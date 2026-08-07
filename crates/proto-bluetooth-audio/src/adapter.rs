//! The [`SourceAdapter`]: one async actor that owns the transport and composes every
//! layer beneath it.
//!
//! Everything below this file is pure. This is the only place a socket exists, and its
//! whole job is to move bytes between the transport and the state machines — HCI events
//! to [`HostController`], ACL fragments through [`Reassembler`] to [`Multiplexer`], and
//! L2CAP channel data to whichever of SDP, AVDTP or AVRCP owns that PSM (ground rule 3).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use std::time::Duration;

use castaway_core::{
    Advertisement, AudioFormat, ControlCapabilities, CoreError, EncodedFrame, FrameSource,
    LossySend, LossySender, NowPlaying, PlaybackState, ProtocolKind, SessionEvent, SessionSink,
    SourceAdapter, SourceDescription,
};
use substrate_hci::{
    BdAddr, ConnectionHandle, Event, HciPacket, HciTransport, LinkKey, Reassembler,
};
use substrate_l2cap::{ChannelMode, Cid, L2capEvent, L2capPdu, Multiplexer, Psm};
use substrate_sdp::{a2dp_sink, avrcp_controller, avrcp_target, SdpServer};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::acl::AclWriter;
use crate::avctp::{opcode, AvcFrame, AvctpMessage, CommandResponse, Ctype};
use crate::avrcp;
use crate::codec::advertised;
use crate::control::AvrcpControl;
use crate::host::{HostAction, HostConfig, HostController};
use crate::media::Depacketizer;
use crate::obex::{CoverArtSession, FetchState, Fetched};
use crate::sink::{SinkEvent, SinkSession};
use crate::{avdtp, Message};

/// How many encoded frames may queue before arriving ones are dropped.
///
/// Audio, unlike video, must not drop frames casually — a gap is audible where a dropped
/// video frame is not. The buffer is sized generously and a full one is logged rather
/// than silently absorbed, because it means decode is not keeping up.
///
/// A full queue drops the frame *arriving*, not the oldest queued one: this is a bounded
/// `mpsc` and a sender cannot pop its head. The offset that leaves is bounded downstream
/// rather than here — `audio_out`'s device queue is about a third of a second and
/// newest-drops too, so a recovered decoder drains this backlog at CPU speed and most of
/// it is discarded there.
const AUDIO_QUEUE_DEPTH: usize = 256;

/// Ceiling on a reassembled AVRCP response.
///
/// Generous — a track with cover art, seven text attributes and CJK titles is a few
/// kilobytes — but finite, because the peer controls how many fragments it sends and an
/// unbounded buffer keyed on a remote's whim is a buffer a remote can grow forever.
const MAX_AVRCP_REASSEMBLY: usize = 64 * 1024;

/// How often we ask a phone to report where it is in the track.
///
/// The `REGISTER_NOTIFICATION` interval field, in seconds — the only event that uses it
/// is `PLAYBACK_POS_CHANGED`. One second is the coarsest value that still reads as
/// movement on a scrubber, and the cheapest: each report is one small AVCTP frame.
const POSITION_INTERVAL_SECS: u32 = 1;

/// How often to ask a peer that will not report position where it has got to.
///
/// Returns `None` when there is nothing to track, so the poll stops rather than being
/// stopped: the cadence is a function of what is playing, not a flag toggled on
/// transitions, and a state we do not model cannot leave a timer running.
///
/// Paused is deliberately still polled, just slowly. Nothing else reveals a **seek**:
/// `PLAYBACK_STATUS_CHANGED` reports play and pause, `TRACK_CHANGED` reports boundaries,
/// and neither says the person scrubbed — which they may well do with the phone in their
/// hand while it is paused, and which would otherwise leave the scrubber lying until the
/// track changed.
const fn position_poll_interval(state: PlaybackState) -> Option<Duration> {
    match state {
        PlaybackState::Playing | PlaybackState::SeekingForward | PlaybackState::SeekingBackward => {
            Some(Duration::from_secs(POSITION_INTERVAL_SECS as u64))
        }
        // A seek is the only thing that can move it, and nobody is watching it move.
        PlaybackState::Paused => Some(Duration::from_secs(5)),
        // Stopped, errored, or a state this build does not know: nothing to follow.
        _ => None,
    }
}

/// The `REGISTER_NOTIFICATION` interval for one event.
///
/// One function so the initial subscription and the renewal after every CHANGED cannot
/// disagree — AVRCP notifications are one-shot, so the renewal is what a phone spends
/// almost all of its time registered under.
const fn notification_interval(event: u8) -> u32 {
    match event {
        avrcp::event::PLAYBACK_POS_CHANGED => POSITION_INTERVAL_SECS,
        // Every other event we subscribe to is edge-triggered; the field is ignored.
        _ => 0,
    }
}

/// Called when a phone pairs, so the caller can persist its link key.
///
/// A callback rather than a path, because this crate must not open files (ground rule
/// 2): where the config directory lives is the app's business, and keeping it out of
/// here is what lets the whole adapter be tested with no filesystem at all.
/// `None` means "forget this peer's key": it was tried and the peer refused it.
pub type OnPaired = Arc<dyn Fn(BdAddr, Option<LinkKey>) + Send + Sync>;

/// Configuration for the Bluetooth adapter.
#[derive(Clone)]
pub struct BluetoothConfig {
    /// Controller bring-up settings.
    pub host: HostConfig,
    /// What this build can actually turn into sound.
    ///
    /// Not a preference — a capability. A sender takes the first endpoint it supports
    /// from a best-first list, so an endpoint we cannot decode is the one it will pick,
    /// and the session becomes silence rather than a clean fallback (#14). The app fills
    /// this in by asking the pipeline what decoders the build actually has.
    pub decodable: Vec<castaway_core::AudioCodec>,
    /// Restrict the advertised endpoints to these codecs. `None` advertises everything
    /// the build supports, which is what a deployment wants.
    ///
    /// Exists for bring-up: a sender picks the first endpoint it also supports, so the
    /// only way to exercise a *particular* codec against real hardware is to stop
    /// offering the ones it would otherwise prefer. Narrowing this to SBC is how the
    /// mandatory fallback path gets tested at all.
    pub codecs: Option<Vec<castaway_core::AudioCodec>>,
    /// Link keys loaded from disk, so repeat guests reconnect silently (#68).
    pub link_keys: Vec<(BdAddr, LinkKey)>,
    /// Called with each newly paired peer's key. Without one, pairing works for the
    /// current session and every guest re-pairs after a restart.
    pub on_paired: Option<OnPaired>,
    /// How long this receiver holds audio before it is heard, reported to every source
    /// that negotiates delay reporting (#89).
    ///
    /// A promise about the output path rather than a preference: a phone delays its
    /// *video* by this much to keep lip-sync, so a number larger than the truth is as
    /// wrong as one smaller. See [`crate::sink::DEFAULT_SINK_DELAY`].
    pub sink_delay: std::time::Duration,
    /// Ask the peer's image server what forms of the artwork it holds, and fetch the best
    /// one it offers rather than settling for the linked thumbnail.
    ///
    /// **On by default**, because it was measured and it is worth it (#75): an iPhone
    /// offers a 280×280 beside the fixed 200×200 thumbnail, and that larger form is a
    /// genuine render rather than an upscale — 2.5–3.9× the spectral detail an upscale
    /// can contain. Nearly twice the pixels on a two-metre screen, for one extra GET on a
    /// channel that is already open.
    ///
    /// This began life gated, when it was a diagnostic that spent the cover-art path's one
    /// real risk — an extra request on a channel some peers answer disagreements on by
    /// hanging up (#74, [`ART_STRIKES_LIMIT`]). That risk has not vanished, but the strike
    /// limit is what handles it, and it now buys a visible improvement rather than a log
    /// line. The airtime it can spend is bounded by [`MAX_COVER_ART_SIDE`].
    pub fetch_best_cover_art: bool,
}

impl std::fmt::Debug for BluetoothConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BluetoothConfig")
            .field("host", &self.host)
            .field("decodable", &self.decodable)
            .field("codecs", &self.codecs)
            .field("link_keys", &self.link_keys.len())
            .field("persists_keys", &self.on_paired.is_some())
            .field("sink_delay", &self.sink_delay)
            .field("fetch_best_cover_art", &self.fetch_best_cover_art)
            .finish()
    }
}

// Not derivable, despite appearances: an empty `decodable` would advertise SBC alone,
// which is a silent quality regression rather than a compile error. The default is what
// this crate can decode without help; the app narrows it to what the *build* can.
#[allow(clippy::derivable_impls)]
impl Default for BluetoothConfig {
    fn default() -> Self {
        use castaway_core::AudioCodec;
        let mut decodable = vec![
            AudioCodec::Sbc,
            AudioCodec::Aac,
            AudioCodec::AptX,
            AudioCodec::AptXHd,
        ];
        if cfg!(feature = "ldac") {
            decodable.push(AudioCodec::Ldac);
        }
        Self {
            host: HostConfig::default(),
            decodable,
            codecs: None,
            link_keys: Vec::new(),
            on_paired: None,
            sink_delay: crate::sink::DEFAULT_SINK_DELAY,
            fetch_best_cover_art: true,
        }
    }
}

/// What a handler wants sent, and what it wants the caller to know.
///
/// Three separate out-parameters was one too many for a signature, and they travel
/// together anyway: the two send paths differ only in whether the multiplexer has already
/// addressed the PDU.
#[derive(Default)]
struct Outbox {
    /// Protocol replies keyed by *our* channel id; the multiplexer maps each to the
    /// peer's on the way out.
    replies: Vec<(Cid, Bytes)>,
    /// Signalling the multiplexer built, already addressed, and riding the fixed
    /// signalling channel that is not in the channel map.
    signalling: Vec<L2capPdu>,
    /// Set when a link starts streaming, so every other one can be preempted.
    started: Option<BdAddr>,
}

/// Per-ACL-link state.
struct Link {
    peer: BdAddr,
    reassembler: Reassembler,
    mux: Multiplexer,
    sink: SinkSession,
    /// AVDTP opens *two* channels on the same PSM: signaling first, then a separate
    /// media transport channel. They are told apart by arrival order, which is the only
    /// signal the protocol gives — and mixing them up feeds audio to the signaling
    /// parser and produces a stream of "unknown signal" rejects.
    avdtp_signaling: Option<Cid>,
    avdtp_media: Option<Cid>,
    avctp: Option<Cid>,
    depacketizer: Option<Depacketizer>,
    /// What AVDTP negotiated, held from SET_CONFIGURATION until START. aptX carries no
    /// in-band rate, so this is the decoder's only source of it (#70).
    audio_format: Option<AudioFormat>,
    audio_tx: Option<LossySender<EncodedFrame>>,
    /// Whether a `SessionEvent::Audio` has already been emitted for this link.
    session_open: bool,
    /// Last SBC bitpool we reported, so a change is logged and a steady stream is not.
    reported_bitpool: Option<u8>,
    /// Metadata accumulated for this link, re-emitted as a full snapshot on change.
    now_playing: NowPlaying,
    /// Next AVCTP transaction label.
    ///
    /// Shared with the control writer task rather than duplicated: the label is what
    /// correlates a response with its command, and the writer pumps panel passthrough
    /// frames onto the *same* L2CAP channel this loop sends registrations on. Two
    /// counters meant the first keypress reused labels 0 and 1, which the channel-open
    /// RegisterNotifications still had outstanding, and the peer had two in-flight
    /// commands it could not tell apart.
    avctp_transaction: Arc<AtomicU8>,
    /// The handle that lets the panel drive this phone, held until there is a session to
    /// attach it to.
    control: Option<Arc<dyn castaway_core::RemoteControl>>,
    /// The AVRCP control handle, kept concretely so its capabilities can be narrowed
    /// when the peer's SDP record turns up.
    avrcp_control: Option<Arc<AvrcpControl>>,
    /// Media packets this link has failed to depacketize, ever.
    ///
    /// A running count rather than a flag so the log can say whether this is one bad
    /// packet or every packet — the difference between a glitch and a session that is
    /// never going to make a sound.
    media_failures: u64,
    /// The last gap count reported for this link, so the powers-of-two schedule fires
    /// once per threshold rather than on every packet once the count sits on one.
    reported_gaps: u64,
    /// A fragmented AVRCP response being reassembled: the PDU id and what has arrived.
    ///
    /// One at a time, because AVRCP allows exactly one continuation in flight per
    /// direction — the peer holds the remainder keyed by PDU id and hands it over a
    /// fragment per `REQUEST_CONTINUING_RESPONSE`.
    avrcp_reassembly: Option<(u8, bytes::BytesMut)>,
    /// Where the peer serves cover art, once its SDP record has told us. Cached for the
    /// life of the link: it does not move between tracks, and asking again per track
    /// would put an SDP round trip in front of every image.
    art_psm: Option<u16>,
    /// An SDP query in flight to find that PSM, and the channel carrying it.
    art_sdp: Option<(Cid, Box<substrate_sdp::Query>)>,
    /// The OBEX session to the peer's image server, and the channel carrying it.
    ///
    /// One per link, not one per image, and brought up *before* attribute 8 is ever asked
    /// for: a Target strips the image handle from its metadata response when no BIP
    /// client is connected, so a receiver that waits to see a handle before connecting
    /// waits forever (#74).
    art: Option<(Cid, CoverArt)>,
    /// Fetches this link's peer has answered by closing the image channel, ever.
    ///
    /// A count rather than a flag for the same reason `media_failures` is: the log can
    /// then say whether this was one bad moment or a peer that punishes every fetch —
    /// and at [`ART_STRIKES_LIMIT`] we stop asking for the rest of the link, because
    /// artwork is decoration and the observed worst case for provoking the peer again
    /// was it dropping the whole ACL link (reason 0x13).
    art_strikes: u8,
    /// Time left before the next `GetPlayStatus`, when this peer will not report position
    /// on its own.
    ///
    /// `Some` means the peer answered `NOT_IMPLEMENTED` to `PLAYBACK_POS_CHANGED`, which
    /// an iPhone does — so on the commonest sender we have, this is the *only* thing that
    /// moves the scrubber between track changes (#162). `None` means the peer subscribed
    /// happily and reports position itself, and polling it as well would be two sources
    /// for one number.
    position_poll: Option<Duration>,
    /// The AVCTP label of the outstanding `PLAYBACK_POS_CHANGED` registration.
    ///
    /// A refusal carries no event id — an iPhone answers `NOT IMPLEMENTED` with zero
    /// parameters — so the only thing that says *which* subscription was turned down is
    /// our own memory of the label we sent it under.
    pos_notify_txn: Option<u8>,
    /// The handle of the artwork last asked for, kept so the properties probe has
    /// something to name once the thumbnail it belongs to has arrived.
    art_handle: Option<String>,
    /// The handle whose properties have been asked for, so the listing is taken once per
    /// *track* rather than once per link. Per link was enough to answer "what does this
    /// phone hold" only if every track holds the same thing, which is the assumption #75
    /// was reopened over.
    art_probed: Option<String>,
    /// The handle whose larger form has been fetched, so the upgrade is attempted once.
    art_upgraded: Option<String>,
    /// What this link's player exposes as shuffle/repeat, and with which values.
    ///
    /// Per link rather than per peer: AVRCP settings belong to the *player*, so the
    /// answer changes when someone switches from Apple Music to YouTube Music on the same
    /// phone (#76).
    player_settings: avrcp::PlayerSettings,
    /// How far the one-time settings interrogation has got.
    settings_query: SettingsQuery,
    /// What the peer's SDP record said the panel may offer, before the settings listing
    /// widens it.
    ///
    /// Kept because the two answers arrive from different protocols at different times
    /// and both narrow the same handle: without a base to add to, whichever landed second
    /// would erase the other.
    sdp_capabilities: ControlCapabilities,
    /// What we know about this phone: address from link-up, name from the remote-name
    /// request, codec from AVDTP configuration. Each arrives separately.
    description: SourceDescription,
}

/// How far the one-time player-application-settings interrogation has got.
///
/// It has to be a state machine rather than three parallel requests because of one
/// asymmetry: a `ListPlayerApplicationSettingValues` response does not echo the attribute
/// it is about, so the only thing that says what its value ids mean is our own memory of
/// what we asked. Two in flight at once is two lists we cannot tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsQuery {
    /// Not asked yet — no AVCTP channel.
    Idle,
    /// The 0x11 listing is in flight.
    Attributes,
    /// A 0x12 value listing for this attribute is in flight.
    Values(avrcp::SettingAttribute),
    /// Everything enumerated; the current values have been asked for.
    Settled,
    /// The peer refused the listing. It has no player application settings, and the
    /// panel offers no shuffle or repeat for this link.
    Unsupported,
}

/// The largest cover art worth pulling over a link that is also carrying the audio.
///
/// Not a limit of the panel: the card's art square is over a thousand pixels on a 4K
/// display, so the screen would happily take more. It is a limit on *airtime*. An iPhone's
/// 280x280 arrives in about 43 KB and lands in well under a second beside a live A2DP
/// stream; pixel count and bytes scale together, so 512 is roughly six times the
/// thumbnail's pixels and still bounded, while a peer offering 2048x2048 would spend
/// several seconds of contended radio on decoration and risk the thing it decorates.
///
/// It does not bind on any peer measured so far — iOS tops out at 280x280 from every app
/// tried. It exists for the one that does not.
const MAX_COVER_ART_SIDE: u16 = 512;

/// The most a *declared* cover-art size may be before the form is skipped.
///
/// Only `<variant maxsize=…>` states one, and iOS states none, so this is a backstop
/// rather than a working limit. Checked against what the descriptor claims, never against
/// a guess: inferring bytes from pixels would refuse real images over a compression ratio
/// we invented.
const MAX_COVER_ART_BYTES: u64 = 1024 * 1024;

/// Mid-fetch closures after which cover art is given up for the link. Two rather than
/// one so a single coincidental teardown (a phone switching outputs mid-song) does not
/// cost the whole link its artwork.
const ART_STRIKES_LIMIT: u8 = 2;

/// The image channel, and the OBEX session on it once there is one to have.
///
/// Two states rather than an `Option<Box<CoverArtSession>>` beside a `Cid`, because the
/// gap between them is where a real bug lived: the OBEX `MaxPacketLength` we advertise in
/// the CONNECT is derived from the channel's receive MTU, and that number is *not final*
/// until the channel finishes configuring. Building the session at dial time read the MTU
/// we had proposed and promised the responder a packet size the peer had not yet agreed
/// to. Splitting the states makes a session built on a provisional MTU unrepresentable:
/// there is no session until [`L2capEvent::ChannelOpen`] says what was actually negotiated.
#[derive(Debug)]
enum CoverArt {
    /// Dialled, still configuring. Nothing can be asked of it yet.
    Dialling,
    /// Open, with a session built against the MTU that was really agreed.
    Session(Box<CoverArtSession>),
}

impl CoverArt {
    /// The session, if the channel has got far enough to have one.
    const fn session(&self) -> Option<&CoverArtSession> {
        match self {
            Self::Dialling => None,
            Self::Session(session) => Some(session),
        }
    }

    /// The session, mutably.
    const fn session_mut(&mut self) -> Option<&mut CoverArtSession> {
        match self {
            Self::Dialling => None,
            Self::Session(session) => Some(session),
        }
    }
}

impl Link {
    fn new(
        peer: BdAddr,
        capabilities: Vec<crate::codec::CodecCapability>,
        sink_delay: std::time::Duration,
    ) -> Self {
        // The receive MTU we advertise, and the lever that actually decides SBC quality
        // per unit of airtime. A controller's ACL buffer is 1021 bytes and an L2CAP header
        // is 4, so 1017 is the largest SDU that still lands in one ACL packet.
        //
        // 672 — the L2CAP default — is what we advertised before, and it is expensive: an
        // XQ-grade SBC stream at 184-byte frames fits three frames per packet there, with
        // 107 bytes wasted and ~43% of the airtime spent. The same stream at 1017 packs
        // five frames into one 3-DH5 and spends ~26%. Same bitrate, far less radio, which
        // is the resource a room full of people is short of. AOSP takes the same view from
        // the other side: it gates its high-bitrate SBC tier on the negotiated MTU
        // (`MIN_3MBPS_AVDTP_SAFE_MTU`, 801) rather than on a bitrate number.
        let mut mux = Multiplexer::new(1017);
        mux.listen(Psm::SDP);
        mux.listen(Psm::AVDTP);
        mux.listen(Psm::AVCTP);
        Self {
            peer,
            reassembler: Reassembler::new(),
            mux,
            sink: {
                let mut sink = SinkSession::new(capabilities);
                sink.set_reported_delay(sink_delay);
                sink
            },
            avdtp_signaling: None,
            avdtp_media: None,
            avctp: None,
            depacketizer: None,
            audio_format: None,
            audio_tx: None,
            session_open: false,
            reported_bitpool: None,
            now_playing: NowPlaying::default(),
            avctp_transaction: Arc::new(AtomicU8::new(0)),
            control: None,
            avrcp_control: None,
            media_failures: 0,
            reported_gaps: 0,
            avrcp_reassembly: None,
            art_psm: None,
            art_sdp: None,
            art: None,
            art_strikes: 0,
            position_poll: None,
            pos_notify_txn: None,
            art_handle: None,
            art_probed: None,
            art_upgraded: None,
            player_settings: avrcp::PlayerSettings::default(),
            settings_query: SettingsQuery::Idle,
            sdp_capabilities: avrcp::capabilities_for_passthrough(),
            description: SourceDescription::new().with_address(peer.to_string()),
        }
    }

    /// Push the union of what SDP and the settings listing allow onto the control handle.
    ///
    /// A union, because the two answers come from different protocols at different times
    /// and both write the same field: SDP's `SupportedFeatures` says whether transport
    /// and volume are worth offering, the 0x11 listing says whether shuffle and repeat
    /// are, and whichever arrived second used to erase the other.
    fn publish_capabilities(&self) {
        if let Some(control) = &self.avrcp_control {
            let caps = self.sdp_capabilities | self.player_settings.capabilities();
            debug!(?caps, "bluetooth: panel controls for this link");
            control.set_player_settings(self.player_settings.clone());
            control.set_capabilities(caps);
        }
    }

    /// How long until this link's position poll is due, if it is polling at all.
    fn position_due(&self) -> Option<Duration> {
        let remaining = self.position_poll?;
        // No interval for this playback state means nothing to follow, so no deadline.
        position_poll_interval(self.now_playing.state)?;
        Some(remaining)
    }

    /// Advance the position poll, returning a `GetPlayStatus` when one comes due.
    fn tick_position(&mut self, elapsed: Duration) -> Option<AvcFrame> {
        let remaining = self.position_poll?;
        // A state with no interval parks the timer where it is rather than disarming it:
        // the peer still refuses to report position, and playback may resume.
        let interval = position_poll_interval(self.now_playing.state)?;
        let remaining = remaining.saturating_sub(elapsed);
        if remaining > Duration::ZERO {
            self.position_poll = Some(remaining);
            return None;
        }
        self.position_poll = Some(interval);
        Some(avrcp::get_play_status())
    }

    fn next_transaction(&self) -> u8 {
        // Labels are four bits; `u8` wraps at a multiple of 16, so masking the previous
        // value is the whole cycle.
        self.avctp_transaction.fetch_add(1, Ordering::Relaxed) & 0x0F
    }
}

/// The Bluetooth A2DP sink adapter.
pub struct BluetoothAdapter {
    transport: Arc<dyn HciTransport>,
    config: BluetoothConfig,
    sdp: SdpServer,
    /// The endpoint table every link advertises, resolved once.
    capabilities: Vec<crate::codec::CodecCapability>,
}

impl std::fmt::Debug for BluetoothAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BluetoothAdapter")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl BluetoothAdapter {
    /// Build an adapter over a transport.
    #[must_use]
    pub fn new(transport: Arc<dyn HciTransport>, config: BluetoothConfig) -> Self {
        let name = config.host.name.clone();
        let mut sdp = SdpServer::new();
        sdp.add(a2dp_sink(0x0001_0000, &name));
        // Both AVRCP records: Controller so we can drive the phone's player, Target so
        // its volume rocker reaches us (#69). Publishing one loses half the feature.
        sdp.add(avrcp_controller(0x0001_0001, &name));
        sdp.add(avrcp_target(0x0001_0002, &name));
        let mut capabilities = advertised(&config.decodable);
        if let Some(allowed) = &config.codecs {
            capabilities.retain(|c| allowed.contains(&c.audio_codec()));
        }
        Self {
            transport,
            config,
            sdp,
            capabilities,
        }
    }

    /// The codecs this adapter advertises, in preference order.
    #[must_use]
    pub fn advertised_codecs(&self) -> Vec<castaway_core::AudioCodec> {
        self.capabilities
            .iter()
            .map(crate::codec::CodecCapability::audio_codec)
            .collect()
    }

    /// Send one HCI packet.
    async fn send(&self, packet: HciPacket) -> Result<(), CoreError> {
        self.transport
            .send(packet)
            .await
            .map_err(|e| CoreError::Adapter(e.to_string()))
    }
}

#[async_trait::async_trait]
impl SourceAdapter for BluetoothAdapter {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::Bluetooth
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        // Bluetooth is its own discovery layer: inquiry scan and the SDP records, not
        // mDNS or SSDP. There is nothing for the shared responders to publish.
        Vec::new()
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        let mut host = HostController::new(self.config.host.clone());
        host.load_link_keys(self.config.link_keys.iter().copied());

        for action in host.start() {
            self.apply_host_action(&action, &mut host).await?;
        }

        let mut links: HashMap<u16, Link> = HashMap::new();
        // Every outbound PDU goes through here: one writer, paced by the controller's
        // buffer credits, so nothing is written into a buffer that does not exist and no
        // two PDUs interleave their fragments (#71).
        let acl = AclWriter::spawn(Arc::clone(&self.transport));

        // Retransmission timers are the one thing in this actor that is driven by time
        // rather than by bytes. They are advanced from wall clock on *every* wakeup, not
        // only on the timer's own, because a link busy with audio would otherwise never
        // credit its cover-art channel any elapsed time and never notice a peer that has
        // stopped answering.
        // The runtime's clock, not the system's: every deadline in this loop is a tokio
        // timer, so the elapsed time fed to the state machines has to come from the same
        // clock — which is also what lets a test pause it and prove the watchdog without
        // waiting five real seconds.
        let mut last_tick = tokio::time::Instant::now();
        loop {
            // The host's own deadline belongs in this minimum too, and its absence was
            // the whole of #90: during bring-up `links` is empty, so the loop blocked on
            // `recv()` with no deadline of any kind and a controller that stopped
            // answering stopped the receiver, silently and for good.
            let due = links
                .values()
                .filter_map(|l| l.mux.next_timeout())
                .chain(links.values().filter_map(Link::position_due))
                .chain(host.next_timeout())
                .min();
            let received = tokio::select! {
                packet = self.transport.recv() => Some(packet),
                () = sleep_until_due(due) => None,
            };

            let elapsed = last_tick.elapsed();
            last_tick = tokio::time::Instant::now();
            for action in host.tick(elapsed) {
                self.apply_host_action(&action, &mut host).await?;
            }
            let ticks: Vec<(ConnectionHandle, Vec<L2capEvent>)> = links
                .iter_mut()
                .filter_map(|(raw, link)| {
                    let events = link.mux.tick(elapsed);
                    if events.is_empty() {
                        return None;
                    }
                    ConnectionHandle::new(*raw).ok().map(|h| (h, events))
                })
                .collect();
            for (handle, events) in ticks {
                let link = links.get_mut(&handle.raw());
                self.dispatch(handle, events, link, &sink, &acl).await?;
            }

            // Peers that refuse `PLAYBACK_POS_CHANGED` are asked where they are instead.
            // Collected before sending because addressing a PDU needs `&mut` on the same
            // link the iteration is borrowing (#162).
            let polls: Vec<(ConnectionHandle, Cid, Bytes)> = links
                .iter_mut()
                .filter_map(|(raw, link)| {
                    let cid = link.avctp?;
                    let frame = link.tick_position(elapsed)?;
                    let transaction = link.next_transaction();
                    let handle = ConnectionHandle::new(*raw).ok()?;
                    Some((handle, cid, avctp_body(transaction, &frame)))
                })
                .collect();
            for (handle, cid, body) in polls {
                let Some(link) = links.get_mut(&handle.raw()) else {
                    continue;
                };
                match link.mux.send(cid, body) {
                    Ok(events) => {
                        for event in events {
                            if let L2capEvent::Send(pdu) = event {
                                acl.send(handle, pdu);
                            }
                        }
                    }
                    Err(e) => debug!(error = %e, %cid, "position poll could not be addressed"),
                }
            }

            let Some(received) = received else {
                continue;
            };
            let packet = match received {
                Ok(p) => p,
                Err(e) => {
                    // `Err`, emphatically not `Ok(())`. Returning success here was the
                    // whole failure: the caller could not tell a dead dongle from a clean
                    // shutdown, so it did nothing, and Bluetooth stayed dead for the rest
                    // of the process while the panel looked fine. An unplug, a USB reset,
                    // a stalled endpoint that would not clear — all of them arrive here,
                    // and all of them are recoverable by re-opening the controller. Say
                    // so, and let the supervisor do it.
                    warn!(error = %e, "bluetooth transport ended");
                    return Err(CoreError::Adapter(format!("hci transport ended: {e}")));
                }
            };

            match packet {
                HciPacket::Event { code, params } => {
                    let event = match Event::parse(code, &params) {
                        Ok(ev) => ev,
                        Err(e) => {
                            debug!(error = %e, code, "undecodable HCI event");
                            continue;
                        }
                    };
                    // Last-resort visibility only: every event that matters has its own
                    // structured line in the arms below, and completion credits arrive
                    // once per ACL packet during streaming — at `debug` they bury the
                    // lines a live chase is actually after (#215).
                    if !matches!(event, Event::NumberOfCompletedPackets(_)) {
                        trace!(?event, "hci event");
                    }
                    for action in host.on_event(&event) {
                        match &action {
                            HostAction::Ready {
                                address,
                                acl_credits,
                                acl_mtu,
                            } => {
                                acl.configure(*acl_credits, *acl_mtu).await;
                                info!(
                                    %address,
                                    acl_credits,
                                    acl_mtu,
                                    "bluetooth: discoverable"
                                );
                            }
                            HostAction::Credits { handle, count } => {
                                acl.completed(*handle, *count).await;
                            }
                            HostAction::LinkUp { handle, peer } => {
                                info!(%peer, "bluetooth: link up");
                                // Controllers reuse handles, so a handle marked dead by a
                                // previous link has to be cleared or we would refuse to
                                // write to the phone that just arrived on it.
                                acl.link_up(*handle).await;
                                links.insert(
                                    handle.raw(),
                                    Link::new(
                                        *peer,
                                        self.capabilities.clone(),
                                        self.config.sink_delay,
                                    ),
                                );
                            }
                            HostAction::PeerName { peer, name } => {
                                if let Some(link) = links.values_mut().find(|l| l.peer == *peer) {
                                    link.description = std::mem::take(&mut link.description)
                                        .merged(SourceDescription::new().with_display_name(name));
                                    if link.session_open {
                                        let link_sink = sink.with_instance(peer.to_string());
                                        link_sink
                                            .emit(SessionEvent::SourceInfo(
                                                link.description.clone(),
                                            ))
                                            .await?;
                                    }
                                }
                            }
                            HostAction::LinkDown {
                                handle,
                                peer,
                                reason,
                            } => {
                                // The reason separates "authentication failed" from
                                // "connection timeout" from "the phone walked away".
                                // Without it every failure reads the same.
                                info!(%peer, %reason, "bluetooth: link down");
                                // The controller flushed whatever was queued for this
                                // handle without ever reporting it complete, so the
                                // credits have to be taken back by hand.
                                acl.link_down(*handle).await;
                                if let Some(mut link) = links.remove(&handle.raw()) {
                                    // Reap the whole session: the phone left without a
                                    // teardown handshake, which is the ordinary case.
                                    let _ = link.sink.link_down();
                                    let _ = link.mux.link_down();
                                    if link.session_open {
                                        let link_sink = sink.with_instance(link.peer.to_string());
                                        link_sink.emit(SessionEvent::End).await?;
                                    }
                                }
                            }
                            _ => {}
                        }
                        self.apply_host_action(&action, &mut host).await?;
                    }
                }

                HciPacket::Acl(packet) => {
                    let packet_handle = packet.handle;
                    let Some(link) = links.get_mut(&packet_handle.raw()) else {
                        debug!(handle = %packet_handle, "ACL for an unknown link");
                        continue;
                    };
                    let pdu_bytes = match link.reassembler.push(&packet) {
                        Ok(Some(bytes)) => bytes,
                        Ok(None) => continue,
                        Err(e) => {
                            warn!(error = %e, "ACL reassembly failed");
                            continue;
                        }
                    };
                    let pdu = match L2capPdu::decode(&pdu_bytes) {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "malformed L2CAP PDU");
                            continue;
                        }
                    };
                    let events = match link.mux.handle_pdu(&pdu) {
                        Ok(evs) => evs,
                        Err(e) => {
                            debug!(error = %e, "L2CAP rejected a PDU");
                            continue;
                        }
                    };
                    let started = self
                        .dispatch(
                            packet_handle,
                            events,
                            links.get_mut(&packet_handle.raw()),
                            &sink,
                            &acl,
                        )
                        .await?;

                    // One phone at a time owns the speakers (#68). When one starts, every
                    // other one that is streaming gets told, rather than being left to
                    // play into a decoder that has stopped listening.
                    if let Some(winner) = started {
                        for (raw, other) in links.iter_mut() {
                            if other.peer == winner || !other.session_open {
                                continue;
                            }
                            let Ok(other_handle) = ConnectionHandle::new(*raw) else {
                                continue;
                            };
                            self.pause_peer(other_handle, other, &acl);
                        }
                    }
                }

                other => debug!(?other, "ignoring HCI packet"),
            }
        }
    }
}

impl BluetoothAdapter {
    async fn apply_host_action(
        &self,
        action: &HostAction,
        _host: &mut HostController,
    ) -> Result<(), CoreError> {
        if let HostAction::Send(command) = action {
            let packet = command
                .encode()
                .map_err(|e| CoreError::Adapter(format!("hci encode: {e}")))?;
            self.send(packet).await?;
        }
        match action {
            HostAction::Paired { peer, key } => {
                info!(%peer, "bluetooth: paired");
                // Persistence is the app's job — it owns the config directory — so the
                // key goes out through a callback rather than to a path this crate knows.
                if let Some(on_paired) = &self.config.on_paired {
                    on_paired(*peer, Some(*key));
                }
            }
            HostAction::Unpaired { peer } => {
                // Forgotten in memory already; this drops it from disk, or the phone that
                // could not authenticate with it cannot authenticate after a reboot
                // either — the loop just becomes durable.
                info!(%peer, "bluetooth: forgetting a stale link key");
                if let Some(on_paired) = &self.config.on_paired {
                    on_paired(*peer, None);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Route L2CAP events for one link.
    async fn dispatch(
        &self,
        handle: ConnectionHandle,
        events: Vec<L2capEvent>,
        link: Option<&mut Link>,
        sink: &SessionSink,
        acl: &AclWriter,
    ) -> Result<Option<BdAddr>, CoreError> {
        let Some(link) = link else {
            return Ok(None);
        };
        let mut out = Outbox::default();

        for event in events {
            match event {
                L2capEvent::Send(pdu) => out.signalling.push(pdu),

                L2capEvent::ChannelOpen { cid, psm, .. } => {
                    if psm == Psm::AVDTP {
                        // Order is the only discriminator the protocol offers.
                        if link.avdtp_signaling.is_none() {
                            link.avdtp_signaling = Some(cid);
                        } else {
                            link.avdtp_media = Some(cid);
                        }
                    } else if psm == Psm::AVCTP {
                        link.avctp = Some(cid);
                        // The control channel is up, so the receiver can drive the sender.
                        // It stays its own event because the two really are independent —
                        // but which order they arrive in is the sender's choice, and both
                        // happen: an iPhone opens AVCTP *before* it starts streaming, and
                        // the session manager rejects a control surface for a source that
                        // is not active yet. So hold it, and emit it whenever the session
                        // does exist. Dropping it costs the panel every transport control
                        // it has over that phone.
                        let (tx, rx) = mpsc::channel(32);
                        let avrcp_control = Arc::new(AvrcpControl::passthrough(tx));
                        // Kept as its own type as well as behind the trait object: the
                        // peer's feature bitmask arrives later, over SDP, and narrowing
                        // the set then needs the concrete handle.
                        link.avrcp_control = Some(Arc::clone(&avrcp_control));
                        let control: Arc<dyn castaway_core::RemoteControl> = avrcp_control;
                        if link.session_open {
                            let link_sink = sink.with_instance(link.peer.to_string());
                            link_sink
                                .emit(SessionEvent::ControlSurface(Arc::clone(&control)))
                                .await?;
                        }
                        link.control = Some(control);
                        // Resolve the peer's identifier once, here: the writer task
                        // outlives this borrow and cannot consult the multiplexer later.
                        let peer_cid = link.mux.channel(cid).map(|c| c.remote_cid);
                        if let Some(peer_cid) = peer_cid {
                            Self::spawn_control_writer(
                                handle,
                                peer_cid,
                                rx,
                                acl.clone(),
                                Arc::clone(&link.avctp_transaction),
                            );
                        } else {
                            warn!(%cid, "avctp channel vanished before its writer started");
                        }
                        // Ask for metadata straight away rather than waiting for a
                        // notification; a track already playing produces no change event.
                        // The text only: attribute 8 is asked for once the image server
                        // is connected, because a Target strips it when it is not (#74).
                        Self::request_metadata(link, cid, &mut out);
                        // …and subscribe, or the card is a snapshot of this instant and
                        // nothing ever moves it again. RegisterNotification answers
                        // INTERIM with the value *now* and CHANGED when it moves, so one
                        // subscription supplies both the initial play state and every
                        // transition after it.
                        for event in [
                            avrcp::event::PLAYBACK_STATUS_CHANGED,
                            avrcp::event::TRACK_CHANGED,
                            avrcp::event::PLAYBACK_POS_CHANGED,
                        ] {
                            let interval = notification_interval(event);
                            let transaction = link.next_transaction();
                            if event == avrcp::event::PLAYBACK_POS_CHANGED {
                                // Remembered so a refusal can be attributed: the response
                                // to one names no event (#162).
                                link.pos_notify_txn = Some(transaction);
                            }
                            out.replies.push((
                                cid,
                                avctp_body(
                                    transaction,
                                    &avrcp::register_notification(event, interval),
                                ),
                            ));
                        }
                        // Duration comes from GetPlayStatus, not from the subscription:
                        // POS_CHANGED carries only a position, so without this the card
                        // would know how far in we are and not how far in of what.
                        let transaction = link.next_transaction();
                        out.replies
                            .push((cid, avctp_body(transaction, &avrcp::get_play_status())));
                        // "Which player application settings do you have?" — the one round
                        // trip that decides whether this link gets shuffle and repeat
                        // buttons. Asked per player, and gated on nothing: the answer is
                        // the gate (#76).
                        link.settings_query = SettingsQuery::Attributes;
                        let transaction = link.next_transaction();
                        out.replies.push((
                            cid,
                            avctp_body(transaction, &avrcp::list_setting_attributes()),
                        ));
                        // …and go and find the peer's image server now, rather than when
                        // a handle turns up. This is the ordering the whole cover-art
                        // path hinges on: no BIP client, no attribute 8, no handle to
                        // have gone looking for.
                        self.open_cover_art(link, &mut out);
                    }
                    // Our own outgoing channels: the cover-art chain. Both state
                    // machines are pull-driven, so opening one means "ask your question".
                    if link.art_sdp.as_ref().is_some_and(|(c, _)| *c == cid) {
                        if let Some((_, query)) = &link.art_sdp {
                            if let Some(request) = query.next_request() {
                                out.replies.push((cid, request));
                            }
                        }
                    } else if link.art.as_ref().is_some_and(|(c, _)| *c == cid) {
                        // Only now is the receive MTU final, and only now can the OBEX
                        // `MaxPacketLength` be honest about what we can reassemble.
                        let max_packet = link.mux.channel(cid).map_or(0x0400, |c| c.local_mtu);
                        let mut session = CoverArtSession::new(max_packet);
                        let request = session.next_request();
                        debug!(%cid, max_packet, "cover art: image channel open; connecting obex");
                        link.art = Some((cid, CoverArt::Session(Box::new(session))));
                        if let Some(request) = request {
                            debug!(request = %hex(&request), "cover art: obex tx");
                            out.replies.push((cid, request));
                        }
                    }
                    debug!(%cid, %psm, "l2cap channel open");
                }

                L2capEvent::ChannelClosed { cid, psm } => {
                    if Some(cid) == link.avdtp_media {
                        link.avdtp_media = None;
                        link.audio_tx = None;
                    } else if Some(cid) == link.avdtp_signaling {
                        link.avdtp_signaling = None;
                    } else if Some(cid) == link.avctp {
                        link.avctp = None;
                    } else if link.art_sdp.as_ref().is_some_and(|(c, _)| *c == cid) {
                        link.art_sdp = None;
                    } else if link.art.as_ref().is_some_and(|(c, _)| *c == cid) {
                        // The image server went away. If it went away *mid-fetch*, that
                        // is the peer reacting to our GET — the observed iPhone failure —
                        // and each strike counts toward giving cover art up for this
                        // link. An idle close is routine housekeeping; the PSM is
                        // remembered and the next track brings the session back.
                        let mid_fetch = link.art.as_ref().is_some_and(|(_, art)| {
                            art.session()
                                .is_some_and(|s| s.state() == FetchState::Fetching)
                        });
                        if mid_fetch {
                            link.art_strikes += 1;
                            info!(
                                strikes = link.art_strikes,
                                "cover art: the peer closed the image session mid-fetch"
                            );
                        } else {
                            debug!("cover art: the image session closed");
                        }
                        link.art = None;
                    }
                    debug!(%cid, %psm, "l2cap channel closed");
                }

                L2capEvent::Data { cid, psm, payload } => {
                    // Before the SDP server: this is a channel *we* opened, so what
                    // arrives on it is a response to our query, not a request to answer.
                    if link.art_sdp.as_ref().is_some_and(|(c, _)| *c == cid) {
                        self.on_cover_art_sdp(link, &payload, &mut out);
                    } else if link.art.as_ref().is_some_and(|(c, _)| *c == cid) {
                        self.on_cover_art_data(link, &payload, sink, &mut out)
                            .await?;
                    } else if psm == Psm::SDP {
                        let response = self.sdp.handle(&payload);
                        // Both sides in full: an SDP exchange that a peer walks away from
                        // cannot be diagnosed from our side's opinion of it.
                        debug!(
                            request = %hex(&payload),
                            response = %hex(&response),
                            "sdp exchange",
                        );
                        out.replies.push((cid, response));
                    } else if psm == Psm::AVDTP {
                        if Some(cid) == link.avdtp_media {
                            if self.on_media(link, payload).await {
                                // The pipeline let go of the stream. Telling the phone is
                                // not optional: our AVDTP state machine is responder-only,
                                // so clearing our side leaves the peer's stream STARTED
                                // and every packet it sends afterwards falls on the floor
                                // — pause/play cannot recover it, because the phone was
                                // never in a state that needs re-STARTing. An AVRCP pause
                                // makes it suspend, which is the event that legitimately
                                // clears our side and lets the next play open a new
                                // stream.
                                self.pause_peer(handle, link, acl);
                                let link_sink = sink.with_instance(link.peer.to_string());
                                link_sink.emit(SessionEvent::End).await?;
                            }
                        } else {
                            self.on_avdtp(link, cid, &payload, sink, &mut out).await?;
                        }
                    } else if psm == Psm::AVCTP {
                        self.on_avctp(link, cid, &payload, sink, &mut out).await?;
                    }
                }

                L2capEvent::ConnectFailed { psm, result } => {
                    warn!(%psm, ?result, "outgoing l2cap connect refused");
                }
                // L2capEvent is #[non_exhaustive]; a new variant must be noticed rather
                // than dropped, since every existing one is load-bearing.
                other => debug!(?other, "unhandled l2cap event"),
            }
        }

        for pdu in out.signalling {
            acl.send(handle, pdu);
        }
        // `Multiplexer::send` is the only thing that knows which identifier the peer uses
        // for a channel. Addressing a reply with our own is invisible whenever both ends
        // happen to allocate the same number — which BlueZ did, and an iPhone does not.
        for (cid, payload) in out.replies {
            match link.mux.send(cid, payload) {
                Ok(events) => {
                    for event in events {
                        if let L2capEvent::Send(pdu) = event {
                            acl.send(handle, pdu);
                        }
                    }
                }
                Err(e) => warn!(error = %e, %cid, "dropping a reply we cannot address"),
            }
        }
        Ok(out.started)
    }

    /// Tell a phone we are no longer listening to it.
    ///
    /// Two callers, and the second is the reason this is not called `pause_preempted` any
    /// more: preemption, where another source took the panel, and teardown, where our own
    /// output died under a peer that is still streaming. Both leave the phone sending into
    /// nothing, and both need it to stop for the same reason.
    ///
    /// Both halves, and they are not alternatives.
    ///
    /// **The AVRCP pause is what the person holding the phone sees.** Pausing the *player*
    /// is a thing the phone's own screen reflects and its own lock screen agrees with, and
    /// a phone that pauses sends us the SUSPEND itself — which keeps the sink state
    /// machine driven by what it receives rather than by what we asked for. That is why it
    /// stays, and why it goes first.
    ///
    /// **The AVDTP suspend is what stops the radio.** The keypress was the *only* thing
    /// sent for a long time, and it does not cover the cases this is about: a link with no
    /// AVCTP channel got nothing whatsoever, and a phone that ignores the key went on
    /// transmitting into a session that had already been torn down — still holding its
    /// share of the piconet, still spending ACL credits, against the phone that actually
    /// won. Nothing at default log level said so; the last word about it was "pausing a
    /// preempted phone" (#92).
    ///
    /// SUSPEND rather than CLOSE: the configuration survives, so resuming is a START on
    /// the same stream rather than a renegotiation from DISCOVER.
    fn pause_peer(&self, handle: ConnectionHandle, link: &mut Link, acl: &AclWriter) {
        let paused = self.pause_player(handle, link, acl);
        let suspended = self.suspend_stream(handle, link, acl);
        // The one combination with no mitigation at all, and worth a word: this phone was
        // "preempted" by sending it nothing.
        if !paused && !suspended {
            warn!(
                peer = %link.peer,
                "bluetooth: a preempted phone has neither a control channel nor a stream to \
                 suspend; nothing asked it to stop"
            );
        }
    }

    /// The AVRCP passthrough. Returns whether one went out.
    fn pause_player(&self, handle: ConnectionHandle, link: &mut Link, acl: &AclWriter) -> bool {
        let Some(cid) = link.avctp else { return false };
        let Some(peer_cid) = link.mux.channel(cid).map(|c| c.remote_cid) else {
            return false;
        };
        info!(peer = %link.peer, "bluetooth: pausing a preempted phone");
        for frame in avrcp::passthrough(avrcp::operation::PAUSE) {
            let transaction = link.next_transaction();
            acl.send(
                handle,
                L2capPdu::new(peer_cid, avctp_body(transaction, &frame)),
            );
        }
        true
    }

    /// The AVDTP SUSPEND. Returns whether one went out.
    ///
    /// The session stays `Streaming` until the phone answers — the transition belongs to
    /// its response, not to our asking (see [`SinkSession::suspend_peer`]).
    fn suspend_stream(&self, handle: ConnectionHandle, link: &mut Link, acl: &AclWriter) -> bool {
        let Some(cid) = link.avdtp_signaling else {
            return false;
        };
        let Some(SinkEvent::Command(command)) = link.sink.suspend_peer() else {
            return false;
        };
        // Addressed through the multiplexer, which is the only thing that knows the
        // identifier the *peer* uses for this channel.
        match link.mux.send(cid, command.encode()) {
            Ok(events) => {
                info!(peer = %link.peer, "bluetooth: suspending a preempted phone's stream");
                for event in events {
                    if let L2capEvent::Send(pdu) = event {
                        acl.send(handle, pdu);
                    }
                }
                true
            }
            Err(e) => {
                warn!(error = %e, peer = %link.peer, "bluetooth: could not send SUSPEND");
                false
            }
        }
    }

    /// AVDTP signaling: drive the sink session and act on what it reports.
    async fn on_avdtp(
        &self,
        link: &mut Link,
        cid: Cid,
        payload: &[u8],
        sink: &SessionSink,
        out: &mut Outbox,
    ) -> Result<(), CoreError> {
        let msg = match Message::decode(payload) {
            Ok(m) => m,
            Err(e) => {
                // A signal we do not implement, or a fragmented one. Either way the peer
                // is owed an answer: AVDTP has no "ignored", so silence costs it a signal
                // timeout, a retry, and usually the link.
                if let Some((transaction, signal_id)) = avdtp::Message::refusable_header(payload) {
                    debug!(
                        error = %e,
                        signal_id,
                        "avdtp: refusing a signal we do not implement"
                    );
                    out.replies
                        .push((cid, avdtp::Message::general_reject(transaction, signal_id)));
                } else {
                    debug!(error = %e, "undecodable AVDTP message");
                }
                return Ok(());
            }
        };
        for event in link.sink.handle(&msg) {
            match event {
                SinkEvent::Reply(reply) => out.replies.push((cid, reply.encode())),
                SinkEvent::Command(command) => {
                    // The one command a sink originates. It goes out on the same channel
                    // and by the same path as a reply; what is different is that the
                    // phone now owes *us* a response, which we do not wait for — a
                    // source that rejects it simply keeps assuming zero latency, which
                    // is where it started (#89).
                    debug!(signal = command.signal.name(), "avdtp: sending a command");
                    out.replies.push((cid, command.encode()));
                }
                SinkEvent::Configured {
                    codec,
                    format,
                    configuration,
                } => {
                    info!(?codec, %format, "bluetooth: stream configured");
                    // A RECONFIGURE mid-session can change the rate or channel count, and
                    // the session that is already open was opened *with* the old one — the
                    // decoder and the output device were both sized by it. Carrying on
                    // would play the new stream at the old pitch, which is #70 arriving by
                    // a second route. Dropping the channel ends that audio session; the
                    // START that follows the reconfiguration opens a fresh one with the
                    // right shape.
                    if link.session_open && link.audio_format != Some(format) {
                        info!(
                            was = ?link.audio_format,
                            now = %format,
                            "bluetooth: format changed; restarting the audio session"
                        );
                        link.audio_tx = None;
                        link.session_open = false;
                    }
                    link.audio_format = Some(format);
                    link.depacketizer = Some(Depacketizer::new(codec, format.sample_rate()));
                    link.description = std::mem::take(&mut link.description)
                        .merged(SourceDescription::new().with_link(configuration.describe()));
                }
                SinkEvent::Started => {
                    // Preempt every other phone on this controller, politely. Two A2DP
                    // sources feeding one output do not mix — they fight — and the phone
                    // that loses deserves to be told rather than left streaming into a
                    // decoder nobody is listening to (#68: last writer wins).
                    out.started = Some(link.peer);
                    // START cannot precede SET_CONFIGURATION in the sink state machine,
                    // so a missing format means a bug here rather than a sender problem —
                    // and starting a session without one would decode at a guessed rate,
                    // which is exactly what #70 was.
                    let Some(format) = link.audio_format else {
                        warn!("bluetooth: stream started with no negotiated format");
                        continue;
                    };
                    if !link.session_open {
                        let (tx, rx) = mpsc::channel(AUDIO_QUEUE_DEPTH);
                        link.audio_tx = Some(LossySender::new(tx));
                        link.session_open = true;
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Audio {
                                source: FrameSource::Encoded(rx),
                                format,
                                // Every A2DP codec describes itself in-band.
                                config: None,
                            })
                            .await?;
                        // Only now can the description be delivered: the session
                        // manager rejects source info for a source that is not active,
                        // and this is the moment it becomes active.
                        link_sink
                            .emit(SessionEvent::SourceInfo(link.description.clone()))
                            .await?;
                        // …and the control surface, if AVCTP got in first.
                        if let Some(control) = &link.control {
                            link_sink
                                .emit(SessionEvent::ControlSurface(Arc::clone(control)))
                                .await?;
                        }
                    }
                    // If it did not, open it ourselves. We are the AVRCP *Controller* —
                    // the end that wants metadata and sends transport commands — so
                    // waiting to be connected to is the wrong posture. Android opens
                    // AVCTP; an iPhone streams happily and never does, which left the
                    // now-playing card permanently empty on exactly the phones people
                    // are most likely to walk up with.
                    if link.avctp.is_none() {
                        match link.mux.connect(Psm::AVCTP) {
                            Ok((_, events)) => {
                                debug!("bluetooth: peer opened no avctp; connecting out");
                                Self::queue_signalling(events, &mut out.signalling);
                            }
                            Err(e) => warn!(error = %e, "no channel for avctp"),
                        }
                    }
                }
                SinkEvent::Closed => {
                    link.audio_tx = None;
                    link.depacketizer = None;
                    link.audio_format = None;
                    if link.session_open {
                        link.session_open = false;
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink.emit(SessionEvent::End).await?;
                    }
                }
                SinkEvent::SuspendRefused => {
                    // The end of the line for a preempted phone: it has been sent an AVRCP
                    // pause and an AVDTP SUSPEND and is still transmitting. The winner's
                    // stream is contending with it and the panel looks entirely healthy,
                    // which is exactly the silence #92 is about.
                    warn!(
                        peer = %link.peer,
                        "bluetooth: the phone refused SUSPEND and is still streaming"
                    );
                }
                SinkEvent::Opened | SinkEvent::Suspended => {}
            }
        }
        Ok(())
    }

    /// A media packet: depacketize and push the frame at the pipeline.
    ///
    /// Returns whether the consumer has gone away, in which case the caller must end the
    /// session — there is nothing left to play into and the phone is still sending.
    async fn on_media(&self, link: &mut Link, payload: Bytes) -> bool {
        let mut consumer_gone = false;
        let (Some(depacketizer), Some(tx)) = (link.depacketizer.as_mut(), link.audio_tx.as_ref())
        else {
            return false;
        };
        match depacketizer.push(payload) {
            Ok(frame) => {
                // A sender that is struggling lowers its bitpool silently — there is no
                // renegotiation and nothing else says it happened. Logged on change only,
                // since it is stable for a healthy stream and this is the hot path.
                let bitpool = depacketizer.bitpool();
                if bitpool.is_some() && bitpool != link.reported_bitpool {
                    match link.reported_bitpool {
                        None => info!(?bitpool, "bluetooth: sbc bitpool"),
                        Some(was) => info!(
                            from = was,
                            to = bitpool.unwrap_or(was),
                            "bluetooth: sbc bitpool changed"
                        ),
                    }
                    link.reported_bitpool = bitpool;
                }
                // Packet loss, reported on the same powers-of-two schedule as the
                // depacketize failures below and for the same reason: a burst is worth a
                // line, the thousand packets after it are the same line.
                //
                // This is the number that decides where a break-up came from. The
                // decoder complaining ("Synchronization error", an SBC frame that will
                // not parse) is a symptom of either a lossy radio or a bug in our
                // framing, and cannot distinguish them. A sequence gap can: it is proof
                // the packet never arrived, so the fault is below us.
                let gaps = depacketizer.sequence_gaps();
                if gaps > link.reported_gaps && gaps.is_power_of_two() {
                    warn!(
                        gaps,
                        lost = depacketizer.lost_packets(),
                        codec = ?depacketizer.codec(),
                        "bluetooth: media packets are being lost before they reach us"
                    );
                }
                link.reported_gaps = gaps;
                // A lossy sender rather than a blocking one: blocking here would stall
                // the whole adapter, including the signaling channel, so a phone could
                // not even pause. A full queue means decode is behind, which is worth
                // saying.
                //
                // Full and Closed are told apart deliberately. They used to collapse into
                // one `is_err()`, and the difference is the difference between a hiccup
                // and a dead session: when the decode side went away, *every* subsequent
                // packet logged "audio queue full" — thousands of lines blaming
                // backpressure for a channel with no receiver at all. The `LossySend`
                // enum now makes that collapse uncompilable (#221).
                match tx.send(frame) {
                    LossySend::Sent => {}
                    LossySend::Dropped => {
                        warn!("audio queue full; dropping a frame");
                    }
                    LossySend::Closed => {
                        // The consumer is gone and is not coming back, so stop pretending
                        // there is a session. Logged once, because the whole point is to
                        // stop repeating a per-packet message.
                        warn!("bluetooth: the audio consumer is gone; ending the session");
                        consumer_gone = true;
                    }
                }
            }
            Err(e) => {
                // Counted and reported, not just `debug!`ed. Sustained depacketize
                // failure is the worst diagnostic hole in the media path: an AAC stream
                // with `numSubFrames > 0`, or any other shape we refuse, produces a
                // connected phone, a running session, a populated now-playing card — and
                // total silence, with nothing at default log level to say why.
                //
                // Rate-limited by powers of two rather than by a clock: the first failure
                // is worth a line, and so is "this is still happening 1024 packets
                // later", but the 900 in between are the same line.
                link.media_failures += 1;
                if link.media_failures.is_power_of_two() {
                    warn!(
                        error = %e,
                        failures = link.media_failures,
                        codec = ?link.depacketizer.as_ref().map(Depacketizer::codec),
                        "bluetooth: cannot depacketize this stream; it will be silent"
                    );
                }
            }
        }
        // After the match, so the borrows of `depacketizer` and `audio_tx` above are
        // finished and these fields can be cleared.
        if consumer_gone {
            link.audio_tx = None;
            link.depacketizer = None;
            link.audio_format = None;
            link.session_open = false;
        }
        consumer_gone
    }

    /// AVCTP: metadata responses and volume commands.
    async fn on_avctp(
        &self,
        link: &mut Link,
        cid: Cid,
        payload: &[u8],
        sink: &SessionSink,
        out: &mut Outbox,
    ) -> Result<(), CoreError> {
        let Ok(msg) = AvctpMessage::decode(payload) else {
            return Ok(());
        };
        // A *command* we do not answer is not free. AVCTP has no "ignored" — the peer
        // waits out its transaction timeout, retries, and some stacks abort the link. The
        // spec's answer is `NOT IMPLEMENTED`, and nothing here was ever constructing one:
        // three early returns and a bare `_ => {}` meant every opcode outside
        // GetElementAttributes and SetAbsoluteVolume got silence.
        let is_command = msg.cr == CommandResponse::Command;
        let Ok(frame) = AvcFrame::decode(&msg.body) else {
            if is_command {
                debug!("avrcp: undecodable command frame; answering NOT IMPLEMENTED");
                out.replies.push((cid, refusal(&msg, 0, Bytes::new())));
            }
            return Ok(());
        };

        // Non-vendor opcodes. `VendorPdu::parse` needs seven operand bytes, so these all
        // failed it and returned silently — including the two that stacks gate their
        // AVRCP bring-up on. BlueZ-as-source asks both.
        match frame.opcode {
            opcode::UNIT_INFO if is_command => {
                out.replies
                    .push((cid, avctp_response(&msg, &avrcp::unit_info())));
                return Ok(());
            }
            opcode::SUBUNIT_INFO if is_command => {
                out.replies
                    .push((cid, avctp_response(&msg, &avrcp::subunit_info())));
                return Ok(());
            }
            opcode::VENDOR_DEPENDENT => {}
            other if is_command => {
                debug!(opcode = other, "avrcp: unsupported opcode");
                out.replies
                    .push((cid, refusal(&msg, other, frame.operands.clone())));
                return Ok(());
            }
            _ => return Ok(()),
        }

        let Ok(vendor) = avrcp::VendorPdu::parse(&frame.operands) else {
            if is_command {
                out.replies
                    .push((cid, refusal(&msg, frame.opcode, frame.operands.clone())));
            }
            return Ok(());
        };

        // Reassemble a fragmented *response* before anything reads its parameters.
        // AV/C fixes the packet ceiling at 512 bytes (BlueZ: `AVC_MTU`, avctp.h) and
        // AVRCP spends 7 of them on its own header, so a metadata response fragments on
        // its own terms however large the L2CAP MTU is. Nothing here used to read the
        // packet-type field, so the first fragment was parsed as the whole response,
        // failed as truncated, and was dropped in silence — a long or CJK title, or
        // simply all seven text attributes, left the card blank for that track.
        let vendor = match self.reassemble(link, cid, &vendor, frame.ctype.is_response(), out) {
            Some(complete) => complete,
            // A fragment: absorbed, and a request for the next one is on its way out.
            None => return Ok(()),
        };

        match vendor.pdu_id {
            avrcp::pdu::GET_CAPABILITIES if is_command => {
                // "Which events may I subscribe to on your Target?" A phone that asks and
                // hears nothing does not enable absolute volume, which is the feature
                // this whole surface exists for.
                let response = avrcp::vendor_command(
                    Ctype::Stable,
                    avrcp::pdu::GET_CAPABILITIES,
                    &avrcp::capabilities_response(&vendor.parameters),
                );
                out.replies.push((cid, avctp_response(&msg, &response)));
            }
            // Inbound *command*, not a response to ours. Real GM and Hyundai-Kia head
            // units enumerate attributes 1..=8 unconditionally, and this used to fall
            // into the response branch below — where the request's eight-byte track
            // identifier parses as an attribute count of zero and empties the card (#74).
            avrcp::pdu::GET_ELEMENT_ATTRIBUTES if !frame.ctype.is_response() => {
                let requested = avrcp::parse_attribute_request(&vendor.parameters)
                    .unwrap_or_else(|_| avrcp::attribute::ALL.to_vec());
                debug!(?requested, "bluetooth: a peer is asking us what is playing");
                let response = avrcp::element_attributes_response(&link.now_playing, &requested);
                out.replies.push((
                    cid,
                    AvctpMessage::response(&msg, response.encode()).encode(),
                ));
            }
            avrcp::pdu::GET_ELEMENT_ATTRIBUTES
                if frame.ctype.is_response() && !frame.ctype.is_failure() =>
            {
                if let Ok(parsed) = avrcp::parse_element_attributes(&vendor.parameters) {
                    let changed = !parsed.now_playing.is_same_item(&link.now_playing);
                    // A `GetElementAttributes` response describes the **track** — title,
                    // artist, album, genre, number, duration — and nothing else on the
                    // snapshot. Every other field belongs to the *session* and has its own
                    // source: play state from the subscription, position from
                    // POS_CHANGED or the poll (#162), shuffle and repeat from the player
                    // application settings (#76), artwork from a BIP fetch that finishes
                    // seconds later.
                    //
                    // This replaced the whole snapshot and handed `state` back by itself,
                    // so everything else was silently reset to its default on *every*
                    // metadata read — and phones re-read constantly (an iPhone sent nine
                    // TRACK_CHANGED for three songs). Shuffle and repeat therefore went
                    // back to `None` moments after being learned, which is precisely the
                    // condition the transport strip refuses to draw a button under, so the
                    // controls #76 added never appeared on the panel.
                    let previous = std::mem::replace(&mut link.now_playing, parsed.now_playing);
                    let now = &mut link.now_playing;
                    now.state = previous.state;
                    now.shuffle = previous.shuffle;
                    now.repeat = previous.repeat;
                    // These two describe the track rather than the session, so they
                    // survive a re-read of the same one and are dropped when it really
                    // changes: the new track's art is fetched a moment later and its
                    // position starts again. Blanking them on every re-read was also
                    // making already-fetched artwork disappear.
                    if !changed {
                        now.artwork = previous.artwork.clone();
                        now.position = previous.position;
                    }
                    // A sender may re-notify several times for one track as its metadata
                    // fills in — an iPhone sent nine TRACK_CHANGED for three songs — and
                    // most of those re-reads come back identical. Re-emitting them churns
                    // the card for no reason.
                    let unchanged = link.now_playing == previous;
                    if link.session_open && !unchanged {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                            .await?;
                    }
                    if changed {
                        if let Some(handle) = parsed.cover_art_handle {
                            // The text card is already on screen; the art lands as a
                            // second snapshot whenever it arrives, or never, without
                            // holding anything up.
                            Self::fetch_cover_art(link, &handle, out);
                        }
                    }
                }
            }
            // A refused subscription, which is how an iPhone answers
            // `PLAYBACK_POS_CHANGED`. The response names no event, so the label we sent it
            // under is the only thing that says which one was turned down — and without
            // this the scrubber has no source of movement at all between track changes
            // (#162). `Duration::ZERO` so the first poll goes out immediately rather than
            // a second into a track that is already playing.
            avrcp::pdu::REGISTER_NOTIFICATION
                if frame.ctype.is_response()
                    && frame.ctype.is_failure()
                    && link.pos_notify_txn == Some(msg.transaction) =>
            {
                if link.position_poll.is_none() {
                    info!("bluetooth: peer will not report position; polling play status instead");
                }
                link.pos_notify_txn = None;
                link.position_poll = Some(Duration::ZERO);
            }
            avrcp::pdu::REGISTER_NOTIFICATION
                if matches!(frame.ctype, Ctype::Interim | Ctype::Changed) =>
            {
                let Some(&event) = vendor.parameters.first() else {
                    return Ok(());
                };
                // CHANGED ends the subscription — AVRCP notifications are one-shot, so a
                // stack that does not re-register hears about exactly one track change
                // and then goes quiet again.
                if event == avrcp::event::PLAYBACK_POS_CHANGED && link.position_poll.is_some() {
                    // The peer reports position after all — one number, one source.
                    debug!("bluetooth: peer reports position; stopping the poll");
                    link.position_poll = None;
                }
                let changed = frame.ctype == Ctype::Changed;
                if changed {
                    let transaction = link.next_transaction();
                    if event == avrcp::event::PLAYBACK_POS_CHANGED {
                        link.pos_notify_txn = Some(transaction);
                    }
                    // …with the same interval the first registration used. Renewing
                    // POS_CHANGED at 0 asks a Target that honours the field literally to
                    // never report position again, and the scrubber freezes after the
                    // first update — nothing else polls it.
                    out.replies.push((
                        cid,
                        avctp_body(
                            transaction,
                            &avrcp::register_notification(event, notification_interval(event)),
                        ),
                    ));
                }
                match event {
                    avrcp::event::PLAYBACK_STATUS_CHANGED => {
                        if let Some(&raw) = vendor.parameters.get(1) {
                            let state = avrcp::playback_state(raw);
                            if link.now_playing.state != state {
                                link.now_playing.state = state;
                                debug!(?state, "bluetooth: playback state");
                                if link.session_open {
                                    let link_sink = sink.with_instance(link.peer.to_string());
                                    link_sink
                                        .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                                        .await?;
                                }
                            }
                        }
                    }
                    avrcp::event::PLAYBACK_POS_CHANGED => {
                        // Four bytes of milliseconds after the event id. `0xFFFFFFFF` is
                        // the spec's "not applicable" — a track with no meaningful
                        // position, like a live stream — and must not be shown as 49 days.
                        if let Some(raw) = vendor.parameters.get(1..5) {
                            let ms = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
                            let position = (ms != u32::MAX)
                                .then(|| std::time::Duration::from_millis(u64::from(ms)));
                            if link.now_playing.position != position {
                                link.now_playing.position = position;
                                if link.session_open {
                                    let link_sink = sink.with_instance(link.peer.to_string());
                                    link_sink
                                        .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                                        .await?;
                                }
                            }
                        }
                    }
                    avrcp::event::SETTING_CHANGED => {
                        // The only way a shuffle toggled on the phone's own screen ever
                        // reaches the panel. INTERIM carries the value now, CHANGED every
                        // move after it, and both land here.
                        if let Ok(values) = avrcp::parse_setting_change(&vendor.parameters) {
                            debug!(?values, "bluetooth: player settings changed");
                            if avrcp::apply_settings(&mut link.now_playing, &values)
                                && link.session_open
                            {
                                let link_sink = sink.with_instance(link.peer.to_string());
                                link_sink
                                    .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                                    .await?;
                            }
                        }
                    }
                    avrcp::event::TRACK_CHANGED if changed => {
                        // The notification carries only a track id, so the metadata has
                        // to be asked for again — this is the request that keeps the card
                        // in step with what is actually playing.
                        debug!("bluetooth: track changed; re-reading metadata");
                        Self::request_metadata(link, cid, out);
                        // A new track is a new duration, and POS_CHANGED never carries
                        // one — so without this the scrubber would keep the old track's
                        // length and read as though the new one were nearly over.
                        let transaction = link.next_transaction();
                        out.replies
                            .push((cid, avctp_body(transaction, &avrcp::get_play_status())));
                        // A track change is also the moment to try the image server
                        // again, if it never came up or went away with its channel.
                        self.open_cover_art(link, out);
                    }
                    _ => {}
                }
            }
            avrcp::pdu::GET_PLAY_STATUS
                if frame.ctype.is_response() && !frame.ctype.is_failure() =>
            {
                // The only source of *duration* on this protocol: the metadata attributes
                // carry a playing-time string but not every player fills it in, and the
                // position subscription carries no length at all. Without this the card
                // knows how far in we are and not how far in of what.
                if let Ok((duration, position, _)) = avrcp::parse_play_status(&vendor.parameters) {
                    let mut changed = false;
                    // Only overwrite with something we actually learned: a player that
                    // answers 0xFFFFFFFF ("not applicable") should leave what the
                    // subscription told us alone rather than blanking it.
                    if duration.is_some() && link.now_playing.duration != duration {
                        link.now_playing.duration = duration;
                        changed = true;
                    }
                    if position.is_some() && link.now_playing.position != position {
                        link.now_playing.position = position;
                        changed = true;
                    }
                    // The state byte is deliberately ignored: PLAYBACK_STATUS_CHANGED is
                    // the authority on it, and a stale GetPlayStatus answer racing a
                    // notification would flip the card back.
                    if changed && link.session_open {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                            .await?;
                    }
                }
            }
            avrcp::pdu::LIST_SETTING_ATTRIBUTES if frame.ctype.is_response() => {
                if frame.ctype.is_failure() {
                    // Plenty of players have no settings at all, and a rejection here is
                    // how they say so. Not an error — just a link with no shuffle button.
                    debug!("bluetooth: peer exposes no player application settings");
                    link.settings_query = SettingsQuery::Unsupported;
                    link.publish_capabilities();
                } else if let Ok(attributes) = avrcp::parse_setting_attributes(&vendor.parameters) {
                    debug!(
                        known = ?attributes.known,
                        unknown = ?attributes.unknown,
                        "bluetooth: player application settings"
                    );
                    link.player_settings.attributes = attributes;
                    Self::advance_settings_query(link, cid, out);
                }
            }
            avrcp::pdu::LIST_SETTING_VALUES if frame.ctype.is_response() => {
                // Only meaningful against the attribute we asked about: the response does
                // not echo it, so `SettingsQuery::Values` is the only thing that says what
                // these ids mean. Anything arriving outside that state is unattributable
                // and is dropped rather than guessed at.
                if let SettingsQuery::Values(attribute) = link.settings_query {
                    if !frame.ctype.is_failure() {
                        if let Ok(values) =
                            avrcp::parse_setting_values(attribute, &vendor.parameters)
                        {
                            debug!(?attribute, ?values, "bluetooth: values this player takes");
                            link.player_settings.record_values(&values);
                        }
                    }
                    // Either way the interrogation moves on: a refused value listing costs
                    // this setting its preference, not the whole feature.
                    Self::advance_settings_query(link, cid, out);
                }
            }
            avrcp::pdu::GET_CURRENT_SETTINGS
                if frame.ctype.is_response() && !frame.ctype.is_failure() =>
            {
                if let Ok(values) = avrcp::parse_current_settings(&vendor.parameters) {
                    debug!(?values, "bluetooth: current player settings");
                    if avrcp::apply_settings(&mut link.now_playing, &values) && link.session_open {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                            .await?;
                    }
                }
            }
            avrcp::pdu::SET_ABSOLUTE_VOLUME if is_command || frame.ctype == Ctype::Accepted => {
                // #69: the phone is authoritative. Accept and mirror it.
                if let Some(&raw) = vendor.parameters.first() {
                    let position = avrcp::volume_to_position(raw);
                    if link.session_open {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Control(castaway_core::ControlTxn::Volume(
                                castaway_core::Volume::from_position(position),
                            )))
                            .await?;
                    }
                    // Echo the accepted value back, or the phone's volume UI sticks.
                    let response = avrcp::vendor_command(
                        Ctype::Accepted,
                        avrcp::pdu::SET_ABSOLUTE_VOLUME,
                        &[raw & 0x7F],
                    );
                    out.replies.push((cid, avctp_response(&msg, &response)));
                }
            }
            other if is_command => {
                // A PDU we do not model. Answering keeps the peer's transaction table
                // moving; staying silent costs it a timeout per attempt and, on stacks
                // that treat a stalled AVCTP transaction as fatal, the whole link.
                debug!(pdu = other, "avrcp: unsupported vendor pdu");
                let response = avrcp::vendor_command(Ctype::NotImplemented, other, &[]);
                out.replies.push((cid, avctp_response(&msg, &response)));
            }
            _ => {}
        }
        Ok(())
    }

    /// Fold a fragmented response into a whole one.
    ///
    /// Returns `None` while fragments are still outstanding — a continuation request goes
    /// out instead, because the peer holds the remainder and will not send it unasked.
    ///
    /// Two details taken from BlueZ's Target (`profiles/audio/avrcp.c`), since a phone is
    /// the Target here and its behaviour is what we have to match:
    ///
    /// - `avrcp_handle_request_continuing` matches on `pdu->params[0]`, so the request's
    ///   single parameter is the *original* PDU id, and the fragments that come back are
    ///   labelled with that id too (`pdu->pdu_id = pending->pdu_id`) — not with 0x40.
    ///   Keying reassembly on the original id is therefore right.
    /// - `handle_vendordep_pdu` calls `session_abort_pending_pdu` for any PDU that is not
    ///   GetElementAttributes or a continuation, so sending anything else mid-exchange
    ///   makes the Target throw the remainder away. We cannot prevent that, but it is why
    ///   a `Start` supersedes whatever was in flight rather than being treated as an
    ///   error: after an abort, the next thing we see is a fresh `Start`.
    ///
    /// Commands pass straight through: we never fragment what we send (outbound requests
    /// are small by construction), so an inbound fragmented *command* is not something
    /// this direction has to model.
    fn reassemble(
        &self,
        link: &mut Link,
        cid: Cid,
        vendor: &avrcp::VendorPdu,
        is_response: bool,
        out: &mut Outbox,
    ) -> Option<avrcp::VendorPdu> {
        use avrcp::PacketType;
        // Fragmentation is a property of the AV/C *response*, and the ctype field is what
        // says which this is — 0x0..=0x7 are command types, 0x8..=0xF response codes. The
        // AVCTP command/response bit answers a different question (which transaction table
        // the peer is keeping) and is used for that, above. We never fragment what we
        // send, so an inbound fragmented command is not a shape this direction models.
        if !is_response {
            return Some(vendor.clone());
        }
        match vendor.packet_type {
            PacketType::Single => {
                // A stray single for a PDU we were reassembling means the peer restarted
                // the exchange; the partial is worthless.
                link.avrcp_reassembly = None;
                Some(vendor.clone())
            }
            PacketType::Start | PacketType::Continue => {
                let buffer = match &mut link.avrcp_reassembly {
                    // A `Start` supersedes whatever was in flight.
                    Some((id, _))
                        if *id != vendor.pdu_id || vendor.packet_type == PacketType::Start =>
                    {
                        link.avrcp_reassembly = Some((vendor.pdu_id, bytes::BytesMut::new()));
                        &mut link.avrcp_reassembly.as_mut()?.1
                    }
                    Some((_, buffer)) => buffer,
                    None => {
                        link.avrcp_reassembly = Some((vendor.pdu_id, bytes::BytesMut::new()));
                        &mut link.avrcp_reassembly.as_mut()?.1
                    }
                };
                buffer.extend_from_slice(&vendor.parameters);
                if buffer.len() > MAX_AVRCP_REASSEMBLY {
                    // Give up, and *say so* to the peer: a stack that is never told keeps
                    // the remainder buffered, and some refuse a fresh request for the same
                    // PDU while one is outstanding — which would break metadata for the
                    // rest of the session rather than for one track.
                    warn!(
                        pdu = vendor.pdu_id,
                        bytes = buffer.len(),
                        "avrcp: fragmented response too large; abandoning it"
                    );
                    let pdu_id = vendor.pdu_id;
                    link.avrcp_reassembly = None;
                    let transaction = link.next_transaction();
                    out.replies.push((
                        cid,
                        avctp_body(transaction, &avrcp::abort_continuing(pdu_id)),
                    ));
                    return None;
                }
                debug!(
                    pdu = vendor.pdu_id,
                    have = buffer.len(),
                    "avrcp: asking for the next fragment"
                );
                let transaction = link.next_transaction();
                out.replies.push((
                    cid,
                    avctp_body(transaction, &avrcp::request_continuing(vendor.pdu_id)),
                ));
                None
            }
            PacketType::End => {
                let (id, mut buffer) = link.avrcp_reassembly.take()?;
                if id != vendor.pdu_id {
                    debug!(
                        expected = id,
                        got = vendor.pdu_id,
                        "avrcp: end fragment for a different pdu"
                    );
                    return None;
                }
                buffer.extend_from_slice(&vendor.parameters);
                debug!(
                    pdu = id,
                    bytes = buffer.len(),
                    "avrcp: response reassembled"
                );
                Some(avrcp::VendorPdu {
                    pdu_id: id,
                    packet_type: PacketType::Single,
                    parameters: buffer.freeze(),
                })
            }
        }
    }

    /// Ask for the metadata we can currently make use of.
    ///
    /// Attribute 8 only once the image server is connected. Asking earlier is not merely
    /// useless — AOSP's Target strips the attribute from a response when no BIP client is
    /// connected, so the early request *teaches us nothing* and the card would wait on a
    /// second round trip for text it could have had immediately (#74).
    fn request_metadata(link: &mut Link, cid: Cid, out: &mut Outbox) {
        let ready = link
            .art
            .as_ref()
            .is_some_and(|(_, art)| art.session().is_some_and(CoverArtSession::is_ready));
        let attributes: &[u32] = if ready {
            &avrcp::attribute::ALL
        } else {
            &avrcp::attribute::TEXT
        };
        let transaction = link.next_transaction();
        out.replies.push((
            cid,
            avctp_body(transaction, &avrcp::get_element_attributes(attributes)),
        ));
    }

    /// Ask the next question in the player-application-settings interrogation.
    ///
    /// Serial by necessity: a `ListPlayerApplicationSettingValues` response does not echo
    /// the attribute it is about, so [`SettingsQuery::Values`] is the only record of what
    /// the ids in it mean. Once every listed setting has been enumerated this reads the
    /// current values and subscribes, which is what keeps the strip in step with the
    /// phone's own UI (#76).
    fn advance_settings_query(link: &mut Link, cid: Cid, out: &mut Outbox) {
        use avrcp::SettingAttribute;

        let settings = &link.player_settings;
        let next = [
            (SettingAttribute::Repeat, settings.repeat_values.is_empty()),
            (
                SettingAttribute::Shuffle,
                settings.shuffle_values.is_empty(),
            ),
        ]
        .into_iter()
        .find(|(attribute, unasked)| *unasked && settings.attributes.contains(*attribute))
        .map(|(attribute, _)| attribute);

        if let Some(attribute) = next {
            link.settings_query = SettingsQuery::Values(attribute);
            let transaction = link.next_transaction();
            out.replies.push((
                cid,
                avctp_body(transaction, &avrcp::list_setting_values(attribute)),
            ));
            return;
        }

        link.settings_query = SettingsQuery::Settled;
        // The capability bits are publishable now whether or not anything was listed: a
        // player with no settings has told us to offer no buttons, which is an answer.
        link.publish_capabilities();
        let known = link.player_settings.attributes.known.clone();
        if known.is_empty() {
            return;
        }
        let transaction = link.next_transaction();
        out.replies.push((
            cid,
            avctp_body(transaction, &avrcp::get_current_settings(&known)),
        ));
        // …and subscribe, or the strip is a snapshot of this instant: a shuffle toggled
        // on the phone's own screen reaches us no other way.
        let transaction = link.next_transaction();
        out.replies.push((
            cid,
            avctp_body(
                transaction,
                &avrcp::register_notification(
                    avrcp::event::SETTING_CHANGED,
                    notification_interval(avrcp::event::SETTING_CHANGED),
                ),
            ),
        ));
    }

    /// Ask the image server what forms of the last-fetched artwork it holds.
    ///
    /// Once per link and only when [`BluetoothConfig::fetch_best_cover_art`] is on —
    /// this is a measurement, and it is spending the one risk the cover-art path has.
    fn probe_image_properties(&self, link: &mut Link, out: &mut Outbox) {
        if !self.config.fetch_best_cover_art {
            return;
        }
        let Some(handle) = link.art_handle.clone() else {
            return;
        };
        if link.art_probed.as_deref() == Some(handle.as_str()) {
            return;
        }
        let Some((cid, art)) = &mut link.art else {
            return;
        };
        let cid = *cid;
        let Some(session) = art.session_mut() else {
            return;
        };
        if !session.fetch_properties(handle.clone()) {
            return;
        }
        link.art_probed = Some(handle.clone());
        if let Some(request) = session.next_request() {
            debug!(handle, request = %hex(&request), "cover art: asking what forms exist");
            out.replies.push((cid, request));
        }
    }

    /// Fetch a larger form than the thumbnail, if the peer listed one.
    ///
    /// **Measured, and worth doing** (#75). An iPhone lists a 280×280 variant over a
    /// 200×200 native from every app — VLC on local files, YouTube Music, Apple Music, all
    /// identical — and the 280 is a *genuine render*, not a resample of the 200: it
    /// carries 2.5–3.9× the spectral energy above the 200/280 Nyquist cutoff that a
    /// bicubic upscale can physically contain, and a bicubic upscale scores only 25–31 dB
    /// against it where this project's own blit work calls 57 dB pixel-exact. So the
    /// second fetch buys 1.96× the pixels with real detail in them.
    ///
    /// The listing alone could never have said so, which is why the code went and looked:
    /// BIP defines `native` as the *stored* form and variants as derived from it, but iOS
    /// stores nothing — it renders from `MPMediaItemArtwork` on demand — so the spec's
    /// data model does not describe what the peer is doing.
    ///
    /// Still under the properties gate, which now needs re-weighing: it was written when
    /// this was a diagnostic and it is now a feature.
    fn upgrade_cover_art(
        &self,
        link: &mut Link,
        properties: &crate::obex::ImageProperties,
        out: &mut Outbox,
    ) {
        if !self.config.fetch_best_cover_art {
            return;
        }
        let Some(handle) = link.art_handle.clone() else {
            return;
        };
        if link.art_upgraded.as_deref() == Some(handle.as_str()) {
            return;
        }
        // The best form on offer that is worth the airtime — see [`MAX_COVER_ART_SIDE`].
        // Only one strictly larger than the linked thumbnail earns a second fetch; BIP
        // fixes that at 200×200, so anything at or below it is what we already have.
        let Some((variant, (w, h))) =
            properties.largest_decodable_within(MAX_COVER_ART_SIDE, MAX_COVER_ART_BYTES)
        else {
            debug!("cover art: nothing on offer we can decode within the airtime budget");
            return;
        };
        if u32::from(w) * u32::from(h) <= 200 * 200 {
            debug!(
                w,
                h, "cover art: nothing larger than the thumbnail on offer"
            );
            return;
        }
        let variant = variant.clone();
        let Some((cid, art)) = &mut link.art else {
            return;
        };
        let cid = *cid;
        let Some(session) = art.session_mut() else {
            return;
        };
        if !session.fetch_image(handle.clone(), &variant) {
            return;
        }
        link.art_upgraded = Some(handle.clone());
        if let Some(request) = session.next_request() {
            info!(
                handle,
                w, h, "bluetooth: fetching the larger cover art on offer"
            );
            debug!(request = %hex(&request), "cover art: obex tx");
            out.replies.push((cid, request));
        }
    }

    /// Bring the peer's image server up, so that attribute 8 becomes worth asking for.
    ///
    /// Two round trips the first time: the image server lives on a PSM only the peer's
    /// SDP record knows, so we have to ask before we can connect. The PSM is cached for
    /// the life of the link.
    fn open_cover_art(&self, link: &mut Link, out: &mut Outbox) {
        if link.art.is_some() || link.art_sdp.is_some() {
            return;
        }
        // A peer that has repeatedly hung up on a fetch is telling us something about
        // our requests it cannot say in OBEX. Stop asking for the rest of this link:
        // artwork is decoration, and re-provoking a peer that answers protocol
        // disagreements by dropping channels risks the audio session itself (the
        // observed worst case took the whole ACL link down, reason 0x13).
        if link.art_strikes >= ART_STRIKES_LIMIT {
            debug!("cover art: given up for this link after repeated mid-fetch closures");
            return;
        }
        if let Some(psm) = link.art_psm {
            self.connect_cover_art(link, psm, out);
            return;
        }
        match link.mux.connect(Psm::SDP) {
            Ok((cid, events)) => {
                debug!("bluetooth: asking where cover art lives");
                link.art_sdp = Some((cid, Box::new(substrate_sdp::Query::avrcp_target(1))));
                Self::queue_signalling(events, &mut out.signalling);
            }
            Err(e) => warn!(error = %e, "cover art: no channel for the sdp query"),
        }
    }

    /// Open the image channel itself, once the PSM is known.
    ///
    /// In Enhanced Retransmission Mode, because that is what GOEP 2.0 requires of a cover
    /// art channel — a basic-mode channel here is refused by the responder, which is what
    /// made this whole path unreachable (#74). A peer that counter-proposes basic mode
    /// gets it: GOEP 1.x moves a thumbnail perfectly well.
    fn connect_cover_art(&self, link: &mut Link, psm: u16, out: &mut Outbox) {
        let Ok(psm) = Psm::new(psm) else {
            warn!(psm, "cover art: the peer named a psm that is not one");
            return;
        };
        match link
            .mux
            .connect_with(psm, ChannelMode::EnhancedRetransmission)
        {
            Ok((cid, events)) => {
                debug!(%psm, %cid, "bluetooth: connecting to the image server");
                // No session yet: see [`CoverArt`]. The OBEX CONNECT is built when the
                // channel opens and the MTU it has to be sized against is settled.
                link.art = Some((cid, CoverArt::Dialling));
                Self::queue_signalling(events, &mut out.signalling);
            }
            Err(e) => warn!(error = %e, "cover art: no channel for the image server"),
        }
    }

    /// Ask the image server for a handle the peer just gave us.
    fn fetch_cover_art(link: &mut Link, handle: &str, out: &mut Outbox) {
        // Remembered before the fetch rather than after: the session clears the handle
        // when the object completes, and the properties probe runs from that completion.
        link.art_handle = Some(handle.to_owned());
        let Some((cid, art)) = &mut link.art else {
            return;
        };
        let cid = *cid;
        let Some(session) = art.session_mut() else {
            debug!(handle, "cover art: the image channel is still configuring");
            return;
        };
        if !session.fetch_thumbnail(handle) {
            // Either the session is still connecting or an image is already coming. A
            // skipped-through album would otherwise queue art for tracks nobody is on.
            debug!(handle, "cover art: not ready for this one");
            return;
        }
        if let Some(request) = session.next_request() {
            // The bytes, not just the fact. This request is the one thing in the chain
            // that has never been visible, and it is the only thing that can settle
            // whether a peer that goes silent after it is answering a malformed GET or
            // never received one — see the log window at 05:04:16.619, where a fetch was
            // followed by two seconds of nothing and then a teardown.
            debug!(handle, request = %hex(&request), "bluetooth: fetching cover art");
            out.replies.push((cid, request));
        }
    }

    /// Signalling the multiplexer produced is already addressed to the peer, and rides
    /// the fixed signalling channel — which is not in the channel map, so it must not go
    /// through the reply path that maps our channel ids onto the peer's.
    fn queue_signalling(events: Vec<L2capEvent>, signalling: &mut Vec<L2capPdu>) {
        for event in events {
            if let L2capEvent::Send(pdu) = event {
                signalling.push(pdu);
            }
        }
    }

    /// A response to our "where do you serve images from" query.
    fn on_cover_art_sdp(&self, link: &mut Link, payload: &[u8], out: &mut Outbox) {
        let Some((cid, query)) = &mut link.art_sdp else {
            return;
        };
        let cid = *cid;
        match query.feed(payload) {
            // More to come: SDP responses are continued, not fragmented, so the client
            // asks again with the continuation state the peer handed back.
            Ok(false) => {
                if let Some(request) = query.next_request() {
                    out.replies.push((cid, request));
                }
                return;
            }
            Ok(true) => {}
            Err(e) => {
                debug!(error = %e, "cover art: unreadable sdp response");
                link.art_sdp = None;
                return;
            }
        }
        // The same record carries the peer's `SupportedFeatures`, and the panel should
        // not offer a button the phone will answer `NOT IMPLEMENTED` to. Architecture
        // §11.5 always said capabilities come from this bitmask; until now they did not.
        let features = query.supported_features().ok().flatten();
        let psm = query.cover_art_psm().ok().flatten();
        link.art_sdp = None;
        link.sdp_capabilities = avrcp::capabilities_from_features(features);
        debug!(
            ?features,
            caps = ?link.sdp_capabilities,
            "bluetooth: peer avrcp capabilities"
        );
        // Republished rather than stored: the settings listing may already have widened
        // this handle, and writing the SDP answer over it would take the shuffle button
        // back off a player that has one.
        link.publish_capabilities();
        Self::queue_signalling(
            link.mux.disconnect(cid).unwrap_or_default(),
            &mut out.signalling,
        );

        let Some(psm) = psm else {
            // Plenty of senders publish an AVRCP Target and no image server. Not an
            // error, just no picture — and the card is already on screen with its text.
            debug!("bluetooth: peer serves no cover art");
            return;
        };
        link.art_psm = Some(psm);
        self.connect_cover_art(link, psm, out);
    }

    /// Bytes from the peer's image server.
    async fn on_cover_art_data(
        &self,
        link: &mut Link,
        payload: &[u8],
        sink: &SessionSink,
        out: &mut Outbox,
    ) -> Result<(), CoreError> {
        let Some((cid, art)) = &mut link.art else {
            return Ok(());
        };
        let cid = *cid;
        let Some(session) = art.session_mut() else {
            debug!("cover art: obex bytes on a channel that has no session yet");
            return Ok(());
        };
        // Both halves of the exchange in full. An OBEX conversation that a peer walks away
        // from cannot be diagnosed from our side's opinion of it — the same reasoning the
        // SDP exchange is already logged under.
        debug!(response = %hex(payload), "cover art: obex rx");
        // Whether this packet is the one that brings the session up decides what we do
        // next, and it has to be sampled before the packet is fed in.
        let was_connecting = session.state() == FetchState::Connecting;
        let result = session.feed(payload);
        let now_ready = session.is_ready();
        let next = session.next_request();

        match result {
            Ok(Some(Fetched::Artwork(artwork))) => {
                info!(bytes = artwork.len(), "bluetooth: cover art fetched");
                link.now_playing.artwork = Some(artwork);
                if link.session_open {
                    let link_sink = sink.with_instance(link.peer.to_string());
                    link_sink
                        .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                        .await?;
                }
                // The session is free again, which is the only moment a second GET can go
                // out — it refuses one while a fetch is running, deliberately. Once per
                // link: the answer is a property of the peer's image server, not of the
                // track (#75).
                self.probe_image_properties(link, out);
            }
            // Nothing on the card changes from a properties listing — it says what the
            // peer *could* give us, and we still ask for the thumbnail. It is logged
            // because that answer is the whole of #75, and because a phone offering
            // something larger is the only reason to implement asking for it.
            Ok(Some(Fetched::Properties(properties))) => {
                info!(
                    handle = properties.handle.as_deref().unwrap_or("?"),
                    variants = properties.variants.len(),
                    largest = ?properties.largest_decodable().map(|(_, size)| size),
                    "bluetooth: peer's image properties"
                );
                for variant in &properties.variants {
                    debug!(?variant, "cover art: variant on offer");
                }
                self.upgrade_cover_art(link, &properties, out);
            }
            // OBEX is request/response all the way down: every chunk we take has to be
            // asked for.
            Ok(None) => {
                if let Some(request) = next {
                    debug!(request = %hex(&request), "cover art: obex tx");
                    out.replies.push((cid, request));
                }
            }
            Err(e) => debug!(error = %e, "cover art: fetch failed"),
        }

        if was_connecting && now_ready {
            // The image server is up, which is the moment attribute 8 starts arriving.
            // Re-reading the metadata now is what turns the text card into one with a
            // picture; without this the handle would not appear until the next track.
            if let Some(avctp) = link.avctp {
                debug!("bluetooth: image server up; re-reading metadata for the handle");
                Self::request_metadata(link, avctp, out);
            }
        }
        Ok(())
    }

    /// Pump [`AvrcpControl`] frames onto the AVCTP channel.
    ///
    /// Queues through the same [`AclWriter`] as everything else rather than writing to
    /// the transport directly: two tasks fragmenting onto one handle would interleave
    /// their fragments, and basic-mode L2CAP has no way to sort that out (#71).
    fn spawn_control_writer(
        handle: ConnectionHandle,
        cid: Cid,
        mut rx: mpsc::Receiver<AvcFrame>,
        acl: AclWriter,
        labels: Arc<AtomicU8>,
    ) {
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                // The link's allocator, not a second one: see `Link::avctp_transaction`.
                let transaction = labels.fetch_add(1, Ordering::Relaxed) & 0x0F;
                acl.send(handle, avctp_pdu(cid, transaction, &frame));
            }
        });
    }
}

/// Sleep until a retransmission timer is due, or forever if none is.
///
/// `pending` rather than a poll interval: with no ERTM channel open there is nothing to
/// wake up for, and a receiver sitting idle in a hackerspace should be sitting on its
/// socket rather than counting.
async fn sleep_until_due(due: Option<std::time::Duration>) {
    match due {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

/// Hex for a log line, truncated so a big record does not swamp the journal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes.iter().take(256) {
        let _ = write!(out, "{b:02x}");
    }
    if bytes.len() > 256 {
        let _ = write!(out, "…({} bytes)", bytes.len());
    }
    out
}

/// Wrap an AV/C frame in AVCTP, ready for an L2CAP channel.
fn avctp_body(transaction: u8, frame: &AvcFrame) -> Bytes {
    AvctpMessage::command(transaction, frame.encode()).encode()
}

/// Wrap an AV/C frame as the response to `command`, keeping its transaction label.
///
/// The label is the whole point: AVCTP matches responses to commands by it, so a reply
/// with a fresh label is not an answer — it is a second command the peer did not ask for,
/// and the original still times out.
fn avctp_response(command: &AvctpMessage, frame: &AvcFrame) -> Bytes {
    AvctpMessage::response(command, frame.encode()).encode()
}

/// `NOT IMPLEMENTED`, echoing the opcode and operands the peer sent.
///
/// AV/C wants the refusal to carry the frame it refuses, so the peer can tell which of
/// several in-flight commands was rejected.
fn refusal(command: &AvctpMessage, opcode: u8, operands: Bytes) -> Bytes {
    let frame = AvcFrame::panel(Ctype::NotImplemented, opcode, operands);
    avctp_response(command, &frame)
}

/// Wrap an AV/C frame in AVCTP and an L2CAP PDU addressed to `peer_cid`.
fn avctp_pdu(peer_cid: Cid, transaction: u8, frame: &AvcFrame) -> L2capPdu {
    L2capPdu::new(peer_cid, avctp_body(transaction, frame))
}

/// Re-exported for the adapter's tests and the app's wiring.
pub use avdtp::Signal as AvdtpSignal;
