//! The [`SourceAdapter`]: one async actor that owns the transport and composes every
//! layer beneath it.
//!
//! Everything below this file is pure. This is the only place a socket exists, and its
//! whole job is to move bytes between the transport and the state machines — HCI events
//! to [`HostController`], ACL fragments through [`Reassembler`] to [`Multiplexer`], and
//! L2CAP channel data to whichever of SDP, AVDTP or AVRCP owns that PSM (ground rule 3).

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use castaway_core::{
    Advertisement, AudioFormat, CoreError, EncodedFrame, FrameSource, NowPlaying, ProtocolKind,
    SessionEvent, SessionSink, SourceAdapter, SourceDescription,
};
use substrate_hci::{
    BdAddr, ConnectionHandle, Event, HciPacket, HciTransport, LinkKey, Reassembler,
};
use substrate_l2cap::{ChannelMode, Cid, L2capEvent, L2capPdu, Multiplexer, Psm};
use substrate_sdp::{a2dp_sink, avrcp_controller, avrcp_target, SdpServer};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::acl::AclWriter;
use crate::avctp::{opcode, AvcFrame, AvctpMessage, CommandResponse, Ctype};
use crate::avrcp;
use crate::codec::advertised;
use crate::control::AvrcpControl;
use crate::host::{HostAction, HostConfig, HostController};
use crate::media::Depacketizer;
use crate::obex::{CoverArtSession, FetchState};
use crate::sink::{SinkEvent, SinkSession};
use crate::{avdtp, Message};

/// How many encoded frames may queue before the oldest are dropped.
///
/// Audio, unlike video, must not drop frames casually — a gap is audible where a dropped
/// video frame is not. The buffer is sized generously and a full one is logged rather
/// than silently absorbed, because it means decode is not keeping up.
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

/// Called when a phone pairs, so the caller can persist its link key.
///
/// A callback rather than a path, because this crate must not open files (ground rule
/// 2): where the config directory lives is the app's business, and keeping it out of
/// here is what lets the whole adapter be tested with no filesystem at all.
pub type OnPaired = Arc<dyn Fn(BdAddr, LinkKey) + Send + Sync>;

/// Configuration for the Bluetooth adapter.
#[derive(Clone)]
pub struct BluetoothConfig {
    /// Controller bring-up settings.
    pub host: HostConfig,
    /// What this build can actually turn into sound.
    ///
    /// Not a preference — a capability. A sender takes the first endpoint it supports
    /// from a best-first list, so an endpoint we cannot decode is the one it will pick,
    /// and the session becomes silence rather than a clean fallback (Q22). The app fills
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
    /// Link keys loaded from disk, so repeat guests reconnect silently (Q23).
    pub link_keys: Vec<(BdAddr, LinkKey)>,
    /// Called with each newly paired peer's key. Without one, pairing works for the
    /// current session and every guest re-pairs after a restart.
    pub on_paired: Option<OnPaired>,
}

impl std::fmt::Debug for BluetoothConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BluetoothConfig")
            .field("host", &self.host)
            .field("decodable", &self.decodable)
            .field("codecs", &self.codecs)
            .field("link_keys", &self.link_keys.len())
            .field("persists_keys", &self.on_paired.is_some())
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
    /// in-band rate, so this is the decoder's only source of it (OPEN-QUESTIONS Q25).
    audio_format: Option<AudioFormat>,
    audio_tx: Option<mpsc::Sender<EncodedFrame>>,
    /// Whether a `SessionEvent::Audio` has already been emitted for this link.
    session_open: bool,
    /// Last SBC bitpool we reported, so a change is logged and a steady stream is not.
    reported_bitpool: Option<u8>,
    /// Metadata accumulated for this link, re-emitted as a full snapshot on change.
    now_playing: NowPlaying,
    /// Next AVCTP transaction label.
    avctp_transaction: u8,
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
    /// waits forever (Q29).
    art: Option<(Cid, Box<CoverArtSession>)>,
    /// What we know about this phone: address from link-up, name from the remote-name
    /// request, codec from AVDTP configuration. Each arrives separately.
    description: SourceDescription,
}

impl Link {
    fn new(peer: BdAddr, capabilities: Vec<crate::codec::CodecCapability>) -> Self {
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
            sink: SinkSession::new(capabilities),
            avdtp_signaling: None,
            avdtp_media: None,
            avctp: None,
            depacketizer: None,
            audio_format: None,
            audio_tx: None,
            session_open: false,
            reported_bitpool: None,
            now_playing: NowPlaying::default(),
            avctp_transaction: 0,
            control: None,
            avrcp_control: None,
            media_failures: 0,
            avrcp_reassembly: None,
            art_psm: None,
            art_sdp: None,
            art: None,
            description: SourceDescription::new().with_address(peer.to_string()),
        }
    }

    fn next_transaction(&mut self) -> u8 {
        let t = self.avctp_transaction;
        self.avctp_transaction = (self.avctp_transaction + 1) & 0x0F;
        t
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
        // its volume rocker reaches us (Q24). Publishing one loses half the feature.
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
        // two PDUs interleave their fragments (Q26).
        let acl = AclWriter::spawn(Arc::clone(&self.transport));

        // Retransmission timers are the one thing in this actor that is driven by time
        // rather than by bytes. They are advanced from wall clock on *every* wakeup, not
        // only on the timer's own, because a link busy with audio would otherwise never
        // credit its cover-art channel any elapsed time and never notice a peer that has
        // stopped answering.
        let mut last_tick = std::time::Instant::now();
        loop {
            let due = links.values().filter_map(|l| l.mux.next_timeout()).min();
            let received = tokio::select! {
                packet = self.transport.recv() => Some(packet),
                () = sleep_until_due(due) => None,
            };

            let elapsed = last_tick.elapsed();
            last_tick = std::time::Instant::now();
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
                    debug!(?event, "hci event");
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
                                    Link::new(*peer, self.capabilities.clone()),
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

                    // One phone at a time owns the speakers (Q23). When one starts, every
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
                            self.pause_preempted(other_handle, other, &acl);
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
        if let HostAction::Paired { peer, key } = action {
            info!(%peer, "bluetooth: paired");
            // Persistence is the app's job — it owns the config directory — so the key
            // goes out through a callback rather than to a path this crate knows.
            if let Some(on_paired) = &self.config.on_paired {
                on_paired(*peer, *key);
            }
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
                            Self::spawn_control_writer(handle, peer_cid, rx, acl.clone());
                        } else {
                            warn!(%cid, "avctp channel vanished before its writer started");
                        }
                        // Ask for metadata straight away rather than waiting for a
                        // notification; a track already playing produces no change event.
                        // The text only: attribute 8 is asked for once the image server
                        // is connected, because a Target strips it when it is not (Q29).
                        Self::request_metadata(link, cid, &mut out);
                        // …and subscribe, or the card is a snapshot of this instant and
                        // nothing ever moves it again. RegisterNotification answers
                        // INTERIM with the value *now* and CHANGED when it moves, so one
                        // subscription supplies both the initial play state and every
                        // transition after it.
                        for (event, interval) in [
                            (avrcp::event::PLAYBACK_STATUS_CHANGED, 0),
                            (avrcp::event::TRACK_CHANGED, 0),
                            // The interval field is only meaningful for this one, and it
                            // is in *seconds*. BlueZ sets it to `UINT32_MAX / 1000`
                            // because it only wants position to resync a clock it keeps
                            // itself; a panel has no such clock and wants the number, so
                            // one second — the coarsest value that still looks like it is
                            // moving.
                            (avrcp::event::PLAYBACK_POS_CHANGED, POSITION_INTERVAL_SECS),
                        ] {
                            let transaction = link.next_transaction();
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
                        if let Some((_, session)) = &link.art {
                            if let Some(request) = session.next_request() {
                                out.replies.push((cid, request));
                            }
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
                        // The image server went away. The PSM is remembered, so the next
                        // track change brings the session back up rather than giving up
                        // on artwork for the rest of the link.
                        debug!("cover art: the image session closed");
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
                            self.on_media(link, payload).await;
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
    /// AVRCP pause rather than AVDTP suspend, deliberately. Pausing the *player* is what
    /// the person holding the phone sees and understands, and a phone that pauses stops
    /// its own stream and sends us the suspend itself — which keeps our sink state
    /// machine driven by what it receives, rather than diverging from a command we sent.
    /// A phone that ignores the keypress costs nothing: the pipeline has already stopped
    /// decoding it.
    fn pause_preempted(&self, handle: ConnectionHandle, link: &mut Link, acl: &AclWriter) {
        let Some(cid) = link.avctp else { return };
        let Some(peer_cid) = link.mux.channel(cid).map(|c| c.remote_cid) else {
            return;
        };
        info!(peer = %link.peer, "bluetooth: pausing a preempted phone");
        for frame in avrcp::passthrough(avrcp::operation::PAUSE) {
            let transaction = link.next_transaction();
            acl.send(
                handle,
                L2capPdu::new(peer_cid, avctp_body(transaction, &frame)),
            );
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
                SinkEvent::Configured {
                    codec,
                    format,
                    configuration,
                } => {
                    info!(?codec, %format, "bluetooth: stream configured");
                    // A RECONFIGURE mid-session can change the rate or channel count, and
                    // the session that is already open was opened *with* the old one — the
                    // decoder and the output device were both sized by it. Carrying on
                    // would play the new stream at the old pitch, which is Q25 arriving by
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
                    // decoder nobody is listening to (Q23: last writer wins).
                    out.started = Some(link.peer);
                    // START cannot precede SET_CONFIGURATION in the sink state machine,
                    // so a missing format means a bug here rather than a sender problem —
                    // and starting a session without one would decode at a guessed rate,
                    // which is exactly what Q25 was.
                    let Some(format) = link.audio_format else {
                        warn!("bluetooth: stream started with no negotiated format");
                        continue;
                    };
                    if !link.session_open {
                        let (tx, rx) = mpsc::channel(AUDIO_QUEUE_DEPTH);
                        link.audio_tx = Some(tx);
                        link.session_open = true;
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Audio {
                                source: FrameSource::Encoded(rx),
                                format,
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
                SinkEvent::Opened | SinkEvent::Suspended => {}
            }
        }
        Ok(())
    }

    /// A media packet: depacketize and push the frame at the pipeline.
    async fn on_media(&self, link: &mut Link, payload: Bytes) {
        let (Some(depacketizer), Some(tx)) = (link.depacketizer.as_mut(), link.audio_tx.as_ref())
        else {
            return;
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
                // `try_send` rather than `send`: blocking here would stall the whole
                // adapter, including the signaling channel, so a phone could not even
                // pause. A full queue means decode is behind, which is worth saying.
                if tx.try_send(frame).is_err() {
                    warn!("audio queue full; dropping a frame");
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
            // identifier parses as an attribute count of zero and empties the card (Q29).
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
                    // GetElementAttributes carries no play state, so its default would
                    // overwrite whatever the subscription told us. Keep ours.
                    let state = link.now_playing.state;
                    let previous = std::mem::replace(&mut link.now_playing, parsed.now_playing);
                    link.now_playing.state = state;
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
            avrcp::pdu::REGISTER_NOTIFICATION
                if matches!(frame.ctype, Ctype::Interim | Ctype::Changed) =>
            {
                let Some(&event) = vendor.parameters.first() else {
                    return Ok(());
                };
                // CHANGED ends the subscription — AVRCP notifications are one-shot, so a
                // stack that does not re-register hears about exactly one track change
                // and then goes quiet again.
                let changed = frame.ctype == Ctype::Changed;
                if changed {
                    let transaction = link.next_transaction();
                    out.replies.push((
                        cid,
                        avctp_body(transaction, &avrcp::register_notification(event, 0)),
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
            avrcp::pdu::SET_ABSOLUTE_VOLUME if is_command || frame.ctype == Ctype::Accepted => {
                // Q24: the phone is authoritative. Accept and mirror it.
                if let Some(&raw) = vendor.parameters.first() {
                    let fraction = avrcp::volume_to_fraction(raw);
                    if link.session_open {
                        let link_sink = sink.with_instance(link.peer.to_string());
                        link_sink
                            .emit(SessionEvent::Control(castaway_core::ControlTxn::Volume(
                                fraction,
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
    /// second round trip for text it could have had immediately (Q29).
    fn request_metadata(link: &mut Link, cid: Cid, out: &mut Outbox) {
        let ready = link.art.as_ref().is_some_and(|(_, s)| s.is_ready());
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

    /// Bring the peer's image server up, so that attribute 8 becomes worth asking for.
    ///
    /// Two round trips the first time: the image server lives on a PSM only the peer's
    /// SDP record knows, so we have to ask before we can connect. The PSM is cached for
    /// the life of the link.
    fn open_cover_art(&self, link: &mut Link, out: &mut Outbox) {
        if link.art.is_some() || link.art_sdp.is_some() {
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
    /// made this whole path unreachable (Q29). A peer that counter-proposes basic mode
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
                debug!(%psm, "bluetooth: connecting to the image server");
                let max_packet = link.mux.channel(cid).map_or(0x0400, |c| c.local_mtu);
                link.art = Some((cid, Box::new(CoverArtSession::new(max_packet))));
                Self::queue_signalling(events, &mut out.signalling);
            }
            Err(e) => warn!(error = %e, "cover art: no channel for the image server"),
        }
    }

    /// Ask the image server for a handle the peer just gave us.
    fn fetch_cover_art(link: &mut Link, handle: &str, out: &mut Outbox) {
        let Some((cid, session)) = &mut link.art else {
            return;
        };
        let cid = *cid;
        if !session.fetch(handle) {
            // Either the session is still connecting or an image is already coming. A
            // skipped-through album would otherwise queue art for tracks nobody is on.
            debug!(handle, "cover art: not ready for this one");
            return;
        }
        if let Some(request) = session.next_request() {
            debug!(handle, "bluetooth: fetching cover art");
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
        if let Some(control) = &link.avrcp_control {
            let caps = avrcp::capabilities_from_features(features);
            debug!(?features, ?caps, "bluetooth: peer avrcp capabilities");
            control.set_capabilities(caps);
        }
        let psm = query.cover_art_psm().ok().flatten();
        link.art_sdp = None;
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
        let Some((cid, session)) = &mut link.art else {
            return Ok(());
        };
        let cid = *cid;
        // Whether this packet is the one that brings the session up decides what we do
        // next, and it has to be sampled before the packet is fed in.
        let was_connecting = session.state() == FetchState::Connecting;
        let result = session.feed(payload);
        let now_ready = session.is_ready();
        let next = session.next_request();

        match result {
            Ok(Some(artwork)) => {
                info!(bytes = artwork.len(), "bluetooth: cover art fetched");
                link.now_playing.artwork = Some(artwork);
                if link.session_open {
                    let link_sink = sink.with_instance(link.peer.to_string());
                    link_sink
                        .emit(SessionEvent::NowPlaying(link.now_playing.clone()))
                        .await?;
                }
            }
            // OBEX is request/response all the way down: every chunk we take has to be
            // asked for.
            Ok(None) => {
                if let Some(request) = next {
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
    /// their fragments, and basic-mode L2CAP has no way to sort that out (Q26).
    fn spawn_control_writer(
        handle: ConnectionHandle,
        cid: Cid,
        mut rx: mpsc::Receiver<AvcFrame>,
        acl: AclWriter,
    ) {
        tokio::spawn(async move {
            let mut transaction = 0u8;
            while let Some(frame) = rx.recv().await {
                acl.send(handle, avctp_pdu(cid, transaction, &frame));
                transaction = (transaction + 1) & 0x0F;
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
