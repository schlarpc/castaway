//! The AirPlay RTSP dispatch state machine (pure). Given a request's method, path, and
//! body, it produces the response to send and any [`SessionEvent`] to emit. The socket
//! actor owns the TCP connection, the `substrate-rtsp` framing, and the post-pairing
//! ChaCha20 transform; this core just decides *what* to answer.
//!
//! The AirPlay 1 audio flow — `ANNOUNCE` → `SETUP` → `RECORD` — is modelled as a state
//! that carries what each step settled, so a `SETUP` for a format nobody announced is
//! not representable rather than merely refused. It *is* also refused, with the `451`
//! shairport-sync answers: the lenient `200` these used to return told every sender
//! everything was fine and then played nothing, which is the worst of both.
//!
//! `/fp-setup` is answered properly now (it was a table lookup all along — see
//! [`crypto_fairplay`]). Pairing still returns `501`, and neither is on the path to
//! audio: AirPlay 1's key arrives in the `ANNOUNCE` body, not from FairPlay.

use castaway_core::{ControlTxn, NowPlaying, SessionEvent, SourceDescription};
use crypto_fairplay::FairPlaySession;
use tracing::{debug, info as log_info, warn};

use crate::advert::AirPlayIdentity;
use std::net::IpAddr;
use std::num::NonZeroU32;

use crate::control::ControlUpdate;
use crate::error::AirPlayError;
use crate::info;
use crate::mirror::{MirrorKeys, StreamConnectionId};
use crate::sdp::{AnnounceParams, RaopCodec, SessionKey};
use crate::transport::{ReceiverPorts, SenderPeers};

/// Parse an `AA:BB:CC:DD:EE:FF` device id into six bytes.
fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = text.split(':');
    for slot in &mut out {
        *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// The binary-plist content type AirPlay uses.
pub const APPLE_PLIST_MIME: &str = "application/x-apple-binary-plist";

/// The `streams` entry type that means screen mirroring.
const MIRROR_STREAM: i64 = 110;

/// The `streams` entry type for realtime audio — mirroring's, and a media session's.
const MIRROR_AUDIO_STREAM: i64 = 96;

/// The codec a plist `SETUP` stream names in its `ct` field.
///
/// The values are a bitmask elsewhere in the protocol (`cn=0,1,2,3` advertises the same
/// four), but in a stream description exactly one arrives, so this is an enum: a stream
/// has one codec, and "two codecs at once" should not be representable.
///
/// Mirroring says `ct: 8`; a sender casting media says `ct: 2`. Reading it rather than
/// assuming is the difference between those two sessions working and only one of them
/// working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCodec {
    /// `ct: 1` — uncompressed.
    Pcm,
    /// `ct: 2` — Apple Lossless, 352 samples a packet.
    Alac,
    /// `ct: 4` — AAC-LC. Not served: it needs an `AudioSpecificConfig` this path has no
    /// way to know, and no observed sender asks for it.
    AacLc,
    /// `ct: 8` — AAC Enhanced Low Delay, 480 samples a packet. What mirroring uses.
    AacEld,
}

impl StreamCodec {
    fn parse(ct: i64) -> Option<Self> {
        match ct {
            1 => Some(Self::Pcm),
            2 => Some(Self::Alac),
            4 => Some(Self::AacLc),
            8 => Some(Self::AacEld),
            _ => None,
        }
    }

    /// The negotiated stream this codec describes, given the rest of the stream entry.
    fn negotiated(
        self,
        sample_rate: u32,
        frames_per_packet: u32,
        channels: u8,
    ) -> Option<RaopCodec> {
        match self {
            Self::Pcm => Some(RaopCodec::Pcm {
                sample_rate,
                channels,
            }),
            Self::Alac => Some(RaopCodec::Alac(crate::sdp::AlacConfig::airplay(
                frames_per_packet,
                sample_rate,
                channels,
            ))),
            Self::AacEld => Some(RaopCodec::AacEld {
                sample_rate,
                channels,
            }),
            // Refused rather than approximated: the decoder needs a config we would be
            // inventing, and inventing one produces noise rather than an error.
            Self::AacLc => None,
        }
    }
}

/// Serialize a reply dictionary as a binary plist.
fn plist_response(dict: &plist::Dictionary) -> AirPlayResponse {
    let mut buf = Vec::new();
    if plist::to_writer_binary(&mut buf, dict).is_err() {
        warn!("could not serialize a plist reply");
        return AirPlayResponse::status(500);
    }
    AirPlayResponse::ok_body(APPLE_PLIST_MIME, buf)
}

/// The content type for raw byte bodies — `/fp-setup` replies are not plists.
pub const OCTET_STREAM_MIME: &str = "application/octet-stream";

/// The content type both parameter endpoints use, in each direction.
pub const TEXT_PARAMETERS_MIME: &str = "text/parameters";

/// One request, as the actor received it.
///
/// Headers are here because the protocol puts load-bearing values in them and not in
/// bodies: `Transport` carries the ports a `SETUP` negotiates, `Apple-Challenge` the
/// nonce an `OPTIONS` must answer, `RTP-Info` the flush point of a `FLUSH`.
#[derive(Debug, Clone, Copy)]
pub struct AirPlayRequest<'a> {
    /// The method, upper-case (`OPTIONS`, `SETUP`, `POST`…).
    pub method: &'a str,
    /// The request-URI path (`/info`, `/fp-setup`…).
    pub path: &'a str,
    /// The headers, in the order they arrived.
    pub headers: &'a [(String, String)],
    /// The body.
    pub body: &'a [u8],
}

impl<'a> AirPlayRequest<'a> {
    /// A request with no headers — the common shape in tests.
    #[must_use]
    pub const fn new(method: &'a str, path: &'a str, body: &'a [u8]) -> Self {
        Self {
            method,
            path,
            headers: &[],
            body,
        }
    }

    /// Look a header up case-insensitively, as RTSP requires.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The `X-Apple-HKP` header, the cleanest discriminator between the pairing
    /// regimes: legacy pairing sends none, HomeKit's flows always do (research §2).
    /// Returned as the raw string rather than an enum — the values beyond 3 and 4 could
    /// not be confirmed from any source, and inventing names for them would be folklore.
    #[must_use]
    pub fn apple_hkp(&self) -> Option<&'a str> {
        self.header("X-Apple-HKP")
    }
}

/// A response the actor serializes into an RTSP reply.
#[derive(Debug, Default)]
pub struct AirPlayResponse {
    /// RTSP status code.
    pub status: u16,
    /// Extra headers to include. The *names* are `&'static str` on purpose: every header
    /// this state machine emits is one the protocol names at compile time, so the actor
    /// never has to handle "the session asked for a header name that isn't valid ASCII".
    pub headers: Vec<(&'static str, String)>,
    /// Body content type, if a body is present.
    pub content_type: Option<String>,
    /// Response body.
    pub body: Vec<u8>,
    /// A session event to forward, if this request produced one.
    pub event: Option<SessionEvent>,
}

impl AirPlayResponse {
    fn status(code: u16) -> Self {
        Self {
            status: code,
            ..Default::default()
        }
    }

    fn ok() -> Self {
        Self::status(200)
    }

    /// `101 Switching Protocols`, for the `/reverse` event channel.
    fn switching() -> Self {
        Self::status(101)
    }

    fn ok_body(content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: Some(content_type.to_string()),
            body,
            ..Default::default()
        }
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

/// How far a mirroring negotiation has got.
///
/// Separate from [`RaopState`] because the two are alternatives, not stages: a session
/// is audio *or* mirroring, decided by which shape of `SETUP` arrives, and a state that
/// could be both would be a state no sender ever puts us in.
#[derive(Debug, Default)]
enum MirrorState {
    /// No mirroring `SETUP` seen.
    #[default]
    Idle,
    /// The first `SETUP` gave us the key material; waiting for a stream to be named.
    ///
    /// It carries nothing, because the key it used to carry is unwrapped the moment it
    /// arrives and kept in `media_key` — where a stream of *either* plane can reach it.
    KeyMaterial,
    /// The second `SETUP` named the stream; the data channel can start.
    Ready(Box<MirrorKeys>),
}

/// The FairPlay-unwrapped media key for a mirroring session.
///
/// Kept beside [`MirrorState`] rather than inside it because it outlives the video
/// negotiation: a third `SETUP` asking for the session's *audio* needs the same key, and
/// by then the video keys have been handed to the data-channel task.
#[derive(Clone, Copy)]
struct MediaKey {
    key: SessionKey,
    iv: [u8; 16],
}

impl std::fmt::Debug for MediaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaKey(<redacted>)")
    }
}

/// How far the AirPlay 1 audio flow has got.
///
/// `ANNOUNCE` → `SETUP` → `RECORD`, and each step needs what the one before it settled:
/// `SETUP` allocates transport for a stream whose format only `ANNOUNCE` stated, and
/// `RECORD` starts a stream that only `SETUP` gave somewhere to arrive. Carrying the
/// negotiated parameters *inside* the states rather than beside them is what stops a
/// later stage asking for a format that was never announced.
#[derive(Debug, Default)]
enum RaopState {
    /// Nothing negotiated yet.
    #[default]
    Idle,
    /// `ANNOUNCE` parsed; we know the format.
    Announced(Box<AnnounceParams>),
    /// `SETUP` done; transport agreed.
    SetUp(Box<AnnounceParams>),
    /// `RECORD` received; the sender is streaming.
    Recording(Box<AnnounceParams>),
}

impl RaopState {
    /// The negotiated parameters, once there are any.
    const fn params(&self) -> Option<&AnnounceParams> {
        match self {
            Self::Idle => None,
            Self::Announced(p) | Self::SetUp(p) | Self::Recording(p) => Some(p),
        }
    }
}

/// One AirPlay session's control state.
pub struct AirPlaySession {
    identity: AirPlayIdentity,
    fairplay: FairPlaySession,
    raop: RaopState,
    /// The ports the actor bound for this session, set before any SETUP arrives.
    local_ports: Option<ReceiverPorts>,
    /// The address the sender reached us on, for `Apple-Challenge`.
    local_addr: Option<IpAddr>,
    /// Where the sender wants us to send resend and timing requests.
    ///
    /// Two independently-learned ports, because the two negotiation shapes declare them
    /// differently: an RTSP `Transport` header names both at once, while the plist path
    /// names the timing port in the key-material `SETUP` and the control port in the
    /// type-96 stream entry. Deriving this *only* from the `Transport` header is the
    /// #176 defect — every mirroring and `isMedia` session had no timing peer, so the
    /// timing client was built and never fired.
    sender_peers: SenderPeers,
    mirror: MirrorState,
    /// The TCP port the actor bound for the mirroring data channel.
    mirror_data_port: Option<u16>,
    /// The unwrapped media key, once a mirroring `SETUP` has provided one.
    media_key: Option<MediaKey>,
    /// Audio negotiated alongside a mirroring session, waiting for the actor.
    mirror_audio: Option<Box<AnnounceParams>>,
    /// A `FLUSH` the actor has not yet passed to the audio task.
    pending_flush: Option<crate::audio::FlushPoint>,
    /// What the AirPlay *video* path reports to a polling sender (#80).
    playback: crate::video::PlaybackInfo,
    /// This receiver's long-term pairing identity (feature bit 27).
    pairing: crate::pairing::PairingIdentity,
    /// A `/pair-verify` exchange between its two stages. `Some` only in that window:
    /// the type cannot be constructed any other way, so a stage-2 body arriving first
    /// has nothing to verify against and is refused.
    pair_verify: Option<crate::pairing::PairVerify>,
    /// The level this receiver reports to a sender that asks for it.
    ///
    /// Held here rather than read back from the pipeline because it is the *sender's*
    /// number: what a `GET_PARAMETER` wants is the value it last set, so its slider and
    /// ours agree. Full scale until a sender says otherwise.
    volume: crate::control::Volume,
    /// The verified ECDH secret, once a sender has proved itself.
    ///
    /// With bit 27 advertised this is not optional decoration: the audio key becomes
    /// `SHA512(aeskey ‖ shared)[0..16]`, so a session that skipped pairing and one that
    /// completed it derive *different* media keys from the same `rsaaeskey`.
    paired_secret: Option<[u8; 32]>,
    /// What this session has learned about the sender, accumulated as it says it.
    ///
    /// Held rather than emitted at the message that carries it, because every fact
    /// arrives before there is a session to attach it to: the name comes with the first
    /// `SETUP` and the codec with `ANNOUNCE`, both of which precede the stream event that
    /// makes this source the active one — and the session manager drops a description for
    /// a source that is not active yet. The actor emits this once the stream starts.
    sender: SourceDescription,
    /// The sample rate of whatever stream this session negotiated.
    ///
    /// Kept here rather than read back from the negotiation because the plist path's
    /// parameters are *taken* by the actor when the stream starts
    /// ([`Self::take_mirror_audio`]), so by the time a `SET_PARAMETER` arrives there is
    /// nothing left to ask. A `progress` report is in RTP timestamps and means nothing
    /// without it, which is the whole of "the scrubber sits at 0:00 with the right
    /// duration" on a mirroring session.
    audio_rate: Option<NonZeroU32>,
}

impl AirPlaySession {
    /// Create a session for the given receiver identity.
    #[must_use]
    pub fn new(identity: AirPlayIdentity) -> Self {
        let pairing = crate::pairing::PairingIdentity::from_seed(&identity.pairing_id);
        Self {
            identity,
            pairing,
            pair_verify: None,
            paired_secret: None,
            fairplay: FairPlaySession::new(),
            raop: RaopState::default(),
            local_ports: None,
            local_addr: None,
            sender_peers: SenderPeers::default(),
            mirror: MirrorState::default(),
            mirror_data_port: None,
            media_key: None,
            mirror_audio: None,
            pending_flush: None,
            playback: crate::video::PlaybackInfo::default(),
            volume: crate::control::Volume::Level(0.0),
            sender: SourceDescription::new(),
            audio_rate: None,
        }
    }

    /// Tell the session which address the sender reached us on.
    ///
    /// Needed to answer an `Apple-Challenge`: the signature covers the address and MAC,
    /// so a response captured from one receiver cannot be replayed for another.
    pub fn set_local_addr(&mut self, addr: IpAddr) {
        self.local_addr = Some(addr);
    }

    /// Tell the session where the sender connected from.
    ///
    /// The address is what the card falls back to when a sender never names itself, and
    /// it is worth having even when one does: two phones in a room are routinely both
    /// called "iPhone" (see [`SourceDescription::label`]).
    pub fn set_peer_addr(&mut self, addr: IpAddr) {
        self.sender.address = Some(addr.to_string());
    }

    /// What is known about the sender: its name, where it connected from, and what was
    /// negotiated to carry the media.
    ///
    /// Cloned rather than borrowed because the actor turns it into an event, which
    /// outlives the borrow it would otherwise have to hold across an `await`.
    #[must_use]
    pub fn sender_description(&self) -> SourceDescription {
        self.sender.clone()
    }

    /// Record what a negotiation settled: the link description the panel shows, and the
    /// rate a `progress` report needs to mean anything.
    ///
    /// One function for both because they are one fact learned at one moment, and
    /// splitting them is how the plist path came to fill in a link and no rate.
    fn negotiated(&mut self, params: &AnnounceParams) {
        self.sender.link = Some(params.describe());
        self.audio_rate = NonZeroU32::new(params.codec.sample_rate());
    }

    /// Tell the session which TCP port the actor bound for the mirroring data channel.
    pub fn set_mirror_data_port(&mut self, port: u16) {
        self.mirror_data_port = Some(port);
    }

    /// The derived mirroring keys, once a `SETUP` has named the stream.
    ///
    /// Taken rather than borrowed: the actor moves them into the data-channel task, and
    /// a second `SETUP` should re-derive rather than reuse.
    pub fn take_mirror_keys(&mut self) -> Option<Box<MirrorKeys>> {
        match std::mem::take(&mut self.mirror) {
            MirrorState::Ready(keys) => Some(keys),
            other => {
                self.mirror = other;
                None
            }
        }
    }

    /// A `FLUSH` the audio task has not been told about yet.
    pub fn take_flush(&mut self) -> Option<crate::audio::FlushPoint> {
        self.pending_flush.take()
    }

    /// Whether a mirroring session has negotiated audio the actor has not started yet.
    ///
    /// Exists so the actor can ask *before* consuming anything: taking the parameters
    /// when it has nowhere to put them throws away the only copy, which is how mirror
    /// audio came to be negotiated on the wire and silent in the room.
    #[must_use]
    pub const fn has_mirror_audio(&self) -> bool {
        self.mirror_audio.is_some()
    }

    /// The audio stream a mirroring session negotiated, once it has.
    ///
    /// Taken rather than borrowed, for the same reason as the video keys: the actor
    /// moves it into the receive task.
    pub fn take_mirror_audio(&mut self) -> Option<Box<AnnounceParams>> {
        self.mirror_audio.take()
    }

    /// Tell the session which ports the actor bound for it.
    ///
    /// Called before the connection is served, so a `SETUP` can answer with ports that
    /// are already listening rather than numbers we intend to bind later.
    pub fn set_local_ports(&mut self, ports: ReceiverPorts) {
        self.local_ports = Some(ports);
    }

    /// Whether `RECORD` has arrived and the sender is streaming.
    ///
    /// The actor watches this to know when to start the audio tasks: the pure core
    /// cannot start them itself (it owns no sockets) and should not be handed a channel
    /// just to pass one back.
    #[must_use]
    pub const fn is_recording(&self) -> bool {
        matches!(self.raop, RaopState::Recording(_))
    }

    /// Where to send resend and timing requests, as far as any `SETUP` has said.
    #[must_use]
    pub const fn sender_peers(&self) -> SenderPeers {
        self.sender_peers
    }

    /// What `ANNOUNCE` negotiated, if it has happened.
    #[must_use]
    pub fn announced(&self) -> Option<&AnnounceParams> {
        self.raop.params()
    }

    /// Handle one RTSP request. `method` is upper-case (`OPTIONS`, `SETUP`, `POST`…),
    /// `path` is the request-URI path (`/info`, `/fp-setup`…).
    ///
    /// # Errors
    /// [`AirPlayError`] only for genuinely malformed bodies we must reject; handshake
    /// gates that aren't implemented return a `501` response, not an `Err`.
    pub fn handle(&mut self, req: &AirPlayRequest<'_>) -> Result<AirPlayResponse, AirPlayError> {
        let (method, path, body) = (req.method, req.path, req.body);
        // Headers as well as the request line: the protocol puts load-bearing values in
        // them, and a session that dies at a given request cannot be read from the
        // method alone.
        debug!(
            %method,
            %path,
            body = body.len(),
            headers = ?req.headers,
            "airplay request"
        );
        let resp = match (method, path) {
            ("OPTIONS", _) => self.options(req),
            // The body is not decoration: a sender's first request names the TXT record
            // it wants read back, and is answered with that and nothing else.
            ("GET", "/info") => AirPlayResponse::ok_body(
                APPLE_PLIST_MIME,
                info::info_plist(&self.identity, &info::InfoQuery::parse(body))?,
            ),
            ("POST", "/fp-setup") => self.fp_setup(body),
            // Legacy pairing (bit 27). HomeKit's flows arrive at the same paths but
            // carry `X-Apple-HKP`, and we advertise none of their bits — a sender that
            // sends one anyway is asking for a regime this receiver does not serve, and
            // 501 is the honest answer.
            ("POST", "/pair-setup") if req.apple_hkp().is_none() => {
                AirPlayResponse::ok_body(OCTET_STREAM_MIME, self.pairing.pair_setup_response())
            }
            ("POST", "/pair-verify") if req.apple_hkp().is_none() => self.pair_verify(body),
            ("POST", "/pair-setup" | "/pair-verify") => {
                warn!(%path, hkp = ?req.apple_hkp(), "HomeKit pairing is not implemented");
                AirPlayResponse::status(501)
            }
            ("POST" | "GET", "/feedback") => AirPlayResponse::ok(),
            // AirPlay *video* — a different protocol from mirroring that arrives on the
            // same socket, as HTTP/1.1 rather than RTSP (#80). Routed before the lenient
            // fallback below, which is what these used to reach: a `200` that said "fine"
            // and played nothing, on a session that had negotiated perfectly.
            (m, p) if crate::video::VideoEndpoint::route(m, p).is_some() => {
                // `route` just answered `Some`; the `let-else` is for the type, not for a
                // case that can happen.
                let Some(endpoint) = crate::video::VideoEndpoint::route(m, p) else {
                    return Ok(AirPlayResponse::status(500));
                };
                self.video(endpoint, p, body)
            }
            ("POST", "/audioMode") => AirPlayResponse::ok(),
            ("ANNOUNCE", _) => self.announce(body),
            ("SETUP", _) => self.setup(req),
            ("RECORD", _) => self.record(),
            ("SET_PARAMETER", _) => self.set_parameter(req),
            ("GET_PARAMETER", _) => self.get_parameter(req),
            ("FLUSH", _) => {
                // `RTP-Info` is where the sender says the new position starts. Ignoring
                // it was the audible half of a skip: a moment of the old position plays
                // before the new audio arrives.
                if let Some(header) = req.header("RTP-Info") {
                    let point = crate::audio::FlushPoint::parse(header);
                    log_info!(rtp = ?point.rtp, seq = ?point.seq, "AirPlay FLUSH");
                    self.pending_flush = Some(point);
                } else {
                    debug!("FLUSH with no RTP-Info; nothing to discard from");
                }
                AirPlayResponse::ok()
            }
            ("TEARDOWN", _) => self.teardown(body),
            (m, p) => {
                // Lenient, but not silent. A sender asking for something this receiver
                // does not implement gets a bare `200`, which it may well reject — and
                // at `debug` that was invisible, so a session dying on a request we
                // never knew existed looked like a session dying for no reason.
                warn!(method = %m, path = %p, body = body.len(), "unhandled AirPlay request; answering 200");
                AirPlayResponse::ok()
            }
        };
        Ok(resp)
    }

    /// Handle `OPTIONS`, answering an `Apple-Challenge` if the sender sent one.
    ///
    /// iTunes and macOS will not proceed without a valid `Apple-Response`; iOS is more
    /// forgiving. A challenge we cannot sign is left unanswered rather than answered
    /// wrongly — a sender that checks will reject a bad signature anyway, and one that
    /// does not will carry on.
    fn options(&self, req: &AirPlayRequest<'_>) -> AirPlayResponse {
        let mut resp = AirPlayResponse::ok().header(
            "Public",
            "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, \
             SET_PARAMETER, POST, GET",
        );
        let Some(challenge) = req.header("Apple-Challenge") else {
            return resp;
        };
        match self.apple_response(challenge) {
            Ok(response) => resp = resp.header("Apple-Response", &response),
            Err(e) => warn!(error = %e, "could not answer the Apple-Challenge"),
        }
        resp
    }

    /// Sign one `Apple-Challenge`.
    fn apple_response(&self, challenge: &str) -> Result<String, AirPlayError> {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD_NO_PAD;
        let raw = engine
            .decode(challenge.trim().trim_end_matches('='))
            .map_err(|_| AirPlayError::Malformed("Apple-Challenge is not base64"))?;
        let addr = self.local_addr.ok_or(AirPlayError::Malformed(
            "no local address for the signature",
        ))?;
        let mac = parse_mac(&self.identity.device_id)
            .ok_or(AirPlayError::Malformed("device id is not a MAC"))?;
        let signature = crypto_raop::sign_apple_challenge(&raw, addr, mac)?;
        // Senders send and expect the padding stripped.
        Ok(engine.encode(signature))
    }

    /// Handle `ANNOUNCE`: the only message that states the audio format.
    ///
    /// A body we refuse is answered with a status, not a dropped connection — the
    /// sender may reasonably try again with something else, and the status says which
    /// part it got wrong (see [`SdpError::rtsp_status`]).
    ///
    /// [`SdpError::rtsp_status`]: crate::error::SdpError::rtsp_status
    fn announce(&mut self, body: &[u8]) -> AirPlayResponse {
        match AnnounceParams::parse(body) {
            Ok(mut params) => {
                // Bit 27's other half. We advertise legacy pairing, so a sender that
                // paired derives its media key as `SHA512(aeskey ‖ shared)` and expects
                // us to do the same; one that skipped pairing (nothing forces it —
                // `/pair-verify` is the sender's move) uses the unwrapped key as-is.
                // Which of the two happened is exactly `paired_secret`, so the rekey is
                // conditioned on it rather than on the advertisement.
                if let Some(shared) = &self.paired_secret {
                    params.rekey_with(shared);
                }
                log_info!(link = %params.describe(), "AirPlay audio announced");
                // What the panel shows. The generation and codec are only knowable
                // here, so this is the one chance to learn them — but *not* the moment
                // to say them: nothing is playing yet, and a description for a source
                // that is not the active one is dropped by the session manager. The
                // actor emits it when the stream starts.
                self.negotiated(&params);
                self.raop = RaopState::Announced(Box::new(params));
                AirPlayResponse::ok()
            }
            Err(e) => {
                warn!(error = %e, "refusing ANNOUNCE");
                AirPlayResponse::status(e.rtsp_status())
            }
        }
    }

    /// Handle `SETUP`: transport for a stream `ANNOUNCE` already described.
    ///
    /// Answering `451` to a `SETUP` that arrives first is shairport-sync's behaviour,
    /// and it is the honest one: there is no format to set up transport for. The
    /// lenient `200` this used to return told the sender everything was fine and then
    /// nothing ever played.
    fn setup(&mut self, req: &AirPlayRequest<'_>) -> AirPlayResponse {
        // Which media plane this is, is decided by the shape of the request: a binary
        // plist body is the mirroring negotiation, a `Transport` header is RAOP audio.
        // Nothing about the socket says which, because both arrive on the same one.
        if req.body.starts_with(b"bplist00") {
            return self.mirror_setup(req.body);
        }
        let Some(header) = req.header("Transport") else {
            warn!("SETUP with no Transport header");
            return AirPlayResponse::status(400);
        };
        let sender = match crate::transport::parse_transport(header) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "refusing SETUP");
                return AirPlayResponse::status(461);
            }
        };
        let Some(local) = self.local_ports else {
            // The actor binds before a session ever sees a SETUP. Reaching here means
            // the wiring is wrong, not that the sender is.
            warn!("SETUP with no bound ports; the actor did not supply them");
            return AirPlayResponse::status(500);
        };

        let (RaopState::Announced(params) | RaopState::SetUp(params)) =
            std::mem::take(&mut self.raop)
        else {
            warn!("SETUP before ANNOUNCE: nothing has been announced to set up");
            return AirPlayResponse::status(451);
        };

        self.sender_peers = SenderPeers::from(sender);
        self.raop = RaopState::SetUp(params);
        log_info!(
            audio = local.audio,
            control = local.control,
            timing = local.timing,
            "AirPlay transport agreed"
        );
        AirPlayResponse::ok()
            .header("Transport", &crate::transport::format_transport(local))
            .header("Session", "1")
    }

    /// Handle the mirroring `SETUP`: key material, a named stream, or both.
    ///
    /// The key material (`ekey`/`eiv`) is answered with our timing port; a `streams`
    /// entry is answered with the port that plane should be sent to. They normally
    /// arrive in that order in two requests, but nothing in the protocol says they must
    /// — the reference receiver reads both blocks of *one* request — so each block is
    /// read on its own rather than inferred from how far the negotiation has got. This
    /// used to look at `streams` first and return `451` to a request carrying both.
    /// The AirPlay video control surface (#80).
    ///
    /// `POST /play` hands over a URL, which is already a first-class operation here — the
    /// same one DLNA's `SetAVTransportURI` and Cast's `LOAD` reduce to. The rest is the
    /// transport, mapped onto the [`ControlTxn`]s every other protocol uses, and
    /// `/playback-info` answering from what the pipeline last reported.
    fn video(
        &mut self,
        endpoint: crate::video::VideoEndpoint,
        path: &str,
        body: &[u8],
    ) -> AirPlayResponse {
        use crate::video::{Rate, VideoCommand, VideoEndpoint};

        // The two read-only endpoints answer from state rather than parsing anything.
        match endpoint {
            VideoEndpoint::PlaybackInfo => {
                return match self.playback.to_plist() {
                    Ok(body) => AirPlayResponse::ok_body(APPLE_PLIST_MIME, body),
                    Err(e) => {
                        warn!(error = %e, "could not build /playback-info");
                        AirPlayResponse::status(500)
                    }
                };
            }
            VideoEndpoint::Reverse => {
                // The sender opening an event channel back to itself. Answered with the
                // protocol switch it is waiting for; nothing is pushed over it yet, and a
                // sender that gets no events polls `/playback-info` instead, which is what
                // it does anyway. Refusing would be worse: some senders treat a failed
                // `/reverse` as a failed session.
                return AirPlayResponse::switching()
                    .header("Upgrade", "PTTH/1.0")
                    .header("Connection", "Upgrade");
            }
            VideoEndpoint::Scrub if body.is_empty() && !path.contains("position=") => {
                // A read, in the older `text/parameters` shape.
                return AirPlayResponse::ok_body(
                    "text/parameters",
                    self.playback.to_scrub_body().into_bytes(),
                );
            }
            _ => {}
        }

        let command = match crate::video::parse(endpoint, path, body) {
            Ok(Some(command)) => command,
            // Understood and asks for nothing — an `/action` this receiver has no use
            // for, a `/rate` with no value. Distinct from a failure, and answering `200`
            // here is honest rather than lenient.
            Ok(None) => return AirPlayResponse::ok(),
            Err(e) => {
                // The lesson the rest of this file learned the hard way: a request we
                // cannot serve must not be answered "fine". A sender told everything is
                // well shows a spinner over nothing and says nothing about why.
                warn!(error = %e, %path, "refusing an AirPlay video request");
                return AirPlayResponse::status(400);
            }
        };

        let event = match command {
            VideoCommand::Play { source, start } => {
                // A fraction cannot be resolved yet — the duration is the *item's*, and
                // nothing has opened it. Seconds can, and are the form current senders
                // use; a fraction is carried as a seek the pipeline applies once it knows.
                let at = start.and_then(|s| s.resolve(self.playback.duration));
                log_info!(%source, start = ?at, "AirPlay video: play");
                self.playback = crate::video::PlaybackInfo {
                    rate: Some(Rate::Playing),
                    ..Default::default()
                };
                SessionEvent::Play {
                    source: source.into(),
                    start: at,
                }
            }
            VideoCommand::Scrub(to) => {
                log_info!(?to, "AirPlay video: scrub");
                self.playback.position = to;
                SessionEvent::Control(ControlTxn::Seek(to))
            }
            VideoCommand::Rate(rate) => {
                log_info!(?rate, "AirPlay video: rate");
                self.playback.rate = Some(rate);
                SessionEvent::Control(match rate {
                    Rate::Playing => ControlTxn::Play,
                    Rate::Paused => ControlTxn::Pause,
                })
            }
            VideoCommand::Stop => {
                log_info!("AirPlay video: stop");
                self.playback = crate::video::PlaybackInfo::default();
                SessionEvent::Control(ControlTxn::Stop)
            }
        };
        let mut response = AirPlayResponse::ok();
        response.event = Some(event);
        response
    }

    /// Tell the session where playback has reached, so `/playback-info` can answer.
    ///
    /// The receiver is authoritative here and the sender draws its own scrubber from what
    /// this reports — which is the reverse of every other AirPlay surface, where the
    /// sender states and the receiver follows.
    pub fn set_playback(&mut self, info: crate::video::PlaybackInfo) {
        self.playback = info;
    }

    /// What `/playback-info` would currently answer.
    #[must_use]
    pub const fn playback(&self) -> crate::video::PlaybackInfo {
        self.playback
    }

    fn mirror_setup(&mut self, body: &[u8]) -> AirPlayResponse {
        let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(body)) else {
            warn!("mirroring SETUP body is not a plist");
            return AirPlayResponse::status(400);
        };
        let Some(dict) = value.as_dictionary() else {
            return AirPlayResponse::status(400);
        };
        debug!(keys = ?dict.keys().collect::<Vec<_>>(), "mirroring SETUP");
        self.note_timing_regime(dict);
        self.note_sender(dict);

        // Key material first, so a request that carries both blocks has a key by the
        // time the stream derived from it is read.
        let carries_key = dict.contains_key("ekey") || dict.contains_key("eiv");
        if carries_key {
            if let Err(refusal) = self.accept_key_material(dict) {
                return *refusal;
            }
        }

        if let Some(streams) = dict.get("streams").and_then(plist::Value::as_array) {
            // One SETUP names one plane. Video comes first and audio, if the sender wants
            // it, in a later request — so which this is, is read from the entry rather
            // than from how far the negotiation has got.
            let named = |ty: i64| {
                streams
                    .iter()
                    .filter_map(plist::Value::as_dictionary)
                    .find(|d| d.get("type").and_then(plist::Value::as_signed_integer) == Some(ty))
            };
            if let Some(stream) = named(MIRROR_STREAM) {
                return self.mirror_streams(stream);
            }
            if let Some(stream) = named(MIRROR_AUDIO_STREAM) {
                return self.mirror_audio_stream(stream);
            }
            warn!("mirroring SETUP names no stream we serve");
            return AirPlayResponse::status(400);
        }

        if !carries_key {
            warn!("mirroring SETUP has neither ekey nor streams");
            return AirPlayResponse::status(400);
        }

        let mut reply = plist::Dictionary::new();
        // Our own timing port, and no event channel: UxPlay returns 0 here and mirrors
        // from iOS 12 through iOS 18, so implementing one would be work with no customer.
        reply.insert(
            "timingPort".into(),
            plist::Value::Integer(i64::from(self.local_ports.map_or(0, |p| p.timing)).into()),
        );
        reply.insert("eventPort".into(), plist::Value::Integer(0i64.into()));
        plist_response(&reply)
    }

    /// Take the wrapped key and media IV out of a mirroring `SETUP`.
    ///
    /// `Err` carries the refusal to send, so the caller cannot forget to send one.
    fn accept_key_material(
        &mut self,
        dict: &plist::Dictionary,
    ) -> Result<(), Box<AirPlayResponse>> {
        let refuse = |status| Err(Box::new(AirPlayResponse::status(status)));
        let Some(ekey) = dict.get("ekey").and_then(plist::Value::as_data) else {
            warn!("mirroring SETUP has an eiv but no ekey");
            return refuse(400);
        };
        let Ok(ekey) = <[u8; crypto_playfair::EKEY_LEN]>::try_from(ekey) else {
            warn!(len = ekey.len(), "ekey is not 72 bytes");
            return refuse(400);
        };
        // `eiv` is the media IV and arrives unwrapped. It is kept now because the audio
        // stream needs it later, by which time this SETUP is long gone.
        let Some(eiv) = dict
            .get("eiv")
            .and_then(plist::Value::as_data)
            .and_then(|d| <[u8; 16]>::try_from(d).ok())
        else {
            warn!("mirroring SETUP has no 16-byte eiv");
            return refuse(400);
        };
        // Unwrapped here, at the message that carries it, rather than when a *video*
        // stream is named. The key belongs to the session, not to one of its planes: a
        // sender casting media negotiates audio and no video at all, and deferring the
        // unwrap to the video path meant answering that session `451` and being torn
        // down. It is also what the reference receiver does — `fairplay_decrypt` runs in
        // its first-SETUP branch, and both planes are handed the result.
        let Some(key_message) = self.fairplay.key_message() else {
            warn!("SETUP carries key material but /fp-setup has not completed");
            return refuse(451);
        };
        // The derivation mode is byte 12 of a body the sender POSTed to `/fp-setup`, and
        // it selects one of four recovered tables. A value outside that range is a
        // sender that is not speaking FairPlay, not a receiver bug — refuse the SETUP.
        let aes_key = match crypto_playfair::decrypt_key(key_message, &ekey) {
            Ok(key) => key,
            Err(e) => {
                warn!(error = %e, "SETUP key material could not be unwrapped");
                return refuse(451);
            }
        };
        // Bit 27's other half. A sender that completed `/pair-verify` encrypts with
        // `SHA512(aeskey ‖ shared)[0..16]`; one that skipped it uses the unwrapped key
        // as-is. Which happened is exactly `paired_secret`, and getting it wrong renders
        // noise rather than failing.
        let aes_key = match &self.paired_secret {
            Some(shared) => crate::pairing::rekey_media(&aes_key, shared),
            None => aes_key,
        };
        self.media_key = Some(MediaKey {
            key: SessionKey::from_bytes(aes_key),
            iv: eiv,
        });
        self.mirror = MirrorState::KeyMaterial;
        log_info!("AirPlay session key material received");
        Ok(())
    }

    /// Take the sender's own account of itself out of a mirroring `SETUP`.
    ///
    /// The first `SETUP` plist carries `name` ("iPhone"), `model` ("iPhone17,1") and
    /// `deviceID`, and all three were parsed and dropped — which is the whole of the
    /// card saying "Unknown device" for a phone that had already introduced itself.
    ///
    /// Only `name` reaches the card. `model` is a part number and is the *fallback*
    /// rather than an addition: showing it beside a name would be noise, and showing it
    /// instead of nothing is still an improvement. `deviceID` is logged and no more —
    /// the address a person can act on is the one the sender connected from, not an
    /// identifier they cannot see from anywhere else in the room.
    fn note_sender(&mut self, dict: &plist::Dictionary) {
        let text = |key: &str| dict.get(key).and_then(plist::Value::as_string);
        let Some(name) = text("name").or_else(|| text("model")) else {
            return;
        };
        log_info!(
            %name,
            model = ?text("model"),
            device_id = ?text("deviceID"),
            "AirPlay sender named itself"
        );
        self.sender.display_name = Some(name.to_string());
    }

    /// Read the timing regime a plist `SETUP` asks for, and keep the peer it names.
    ///
    /// This is where a plist session's timing peer comes from: the top-level
    /// `timingPort` is the sender's own NTP service, the exact counterpart of the
    /// `Transport` header's `timing_port=`. It used to be logged at `debug` and dropped,
    /// which left every mirroring and `isMedia` session with no timing peer — the
    /// timing client was constructed and never fired, and `clock_samples=0` on every
    /// live session (#176).
    ///
    /// A regime we do not serve is reported, not refused — the sender decides what it
    /// does next, and the reference receiver merely reports both. But a PTP sender's
    /// port is a PTP port, so it is deliberately *not* kept: NTP requests to it would be
    /// noise aimed at a service that never answers.
    fn note_timing_regime(&mut self, dict: &plist::Dictionary) {
        let ntp = match dict.get("timingProtocol").and_then(plist::Value::as_string) {
            Some("NTP") | None => true,
            Some(other) => {
                warn!(
                    timing_protocol = %other,
                    "sender asked for a timing protocol this receiver does not serve (NTP only)"
                );
                false
            }
        };
        if dict
            .get("isRemoteControlOnly")
            .and_then(plist::Value::as_boolean)
            == Some(true)
        {
            warn!("sender asked for the AirPlay 2 remote-control protocol, which is not served");
        }
        if let Some(port) = dict
            .get("timingPort")
            .and_then(plist::Value::as_unsigned_integer)
            .and_then(|p| u16::try_from(p).ok())
        {
            debug!(sender_timing_port = port, "sender's NTP port");
            if ntp {
                // Zero still means "no timing service" — `NonZeroU16::new` drops it.
                self.sender_peers.timing = std::num::NonZeroU16::new(port);
            }
        }
    }

    /// The second mirroring `SETUP`: derive the stream keys and name the data port.
    fn mirror_streams(&mut self, stream: &plist::Dictionary) -> AirPlayResponse {
        let Some(media) = self.media_key else {
            warn!("mirroring streams SETUP before any key material");
            return AirPlayResponse::status(451);
        };
        let Some(port) = self.mirror_data_port else {
            warn!("no mirroring data port was bound");
            return AirPlayResponse::status(500);
        };

        // The plist integer is signed; the id is not. Reinterpreting the bit pattern is
        // the whole point of the newtype (see `StreamConnectionId`).
        let id = stream
            .get("streamConnectionID")
            .and_then(plist::Value::as_signed_integer)
            .map_or_else(
                || StreamConnectionId::new(0),
                StreamConnectionId::from_plist_signed,
            );

        // The video plane's own derivation on top of the session key: SHA-512 over a
        // label and the stream id. The audio plane uses the same key with the `eiv`
        // verbatim instead, which is why the session holds the key rather than either
        // plane holding it for the other.
        self.mirror = MirrorState::Ready(Box::new(MirrorKeys::derive(media.key.expose(), id)));
        log_info!(%id, port, "AirPlay mirroring stream ready");

        let mut stream_reply = plist::Dictionary::new();
        stream_reply.insert("type".into(), plist::Value::Integer(MIRROR_STREAM.into()));
        stream_reply.insert(
            "dataPort".into(),
            plist::Value::Integer(i64::from(port).into()),
        );
        let mut reply = plist::Dictionary::new();
        reply.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(stream_reply)]),
        );
        plist_response(&reply)
    }

    /// Answer a `SETUP` asking for a realtime audio stream (type 96).
    ///
    /// It arrives on the same UDP sockets the AirPlay 1 audio flow uses and obeys the
    /// same payload rules, so the reply names those ports and the depacketiser is the
    /// same one — only the codec and the key differ.
    ///
    /// **Two kinds of session ask for this stream, and they ask for different codecs.**
    /// Mirroring says `ct: 8, spf: 480` — AAC-ELD. A sender casting *media* (AirPlay from
    /// an app's video, rather than the screen) says `ct: 2, spf: 352` — ALAC, and sets up
    /// no video stream at all. Reading the codec from the request rather than assuming
    /// mirroring's is what lets the second session exist; assuming was a `451` and a
    /// teardown one request later.
    fn mirror_audio_stream(&mut self, stream: &plist::Dictionary) -> AirPlayResponse {
        let Some(media) = self.media_key else {
            warn!("audio SETUP before any key material");
            return AirPlayResponse::status(451);
        };
        let Some(ports) = self.local_ports else {
            warn!("no audio ports were bound");
            return AirPlayResponse::status(500);
        };
        let number = |key: &str| stream.get(key).and_then(plist::Value::as_unsigned_integer);
        // The defaults are the values every capture carries and are only reached by a
        // sender that omits the field; the codec itself is never defaulted.
        let sample_rate = u32::try_from(number("sr").unwrap_or(44_100)).unwrap_or(44_100);
        let spf = u32::try_from(number("spf").unwrap_or(480)).unwrap_or(480);
        // The sender's own control port, where our resend requests go — the plist twin
        // of the `Transport` header's `control_port=`. Every capture of this stream
        // carries it (`controlPort: 60284` in the 2026-07-31 one), and dropping it
        // meant a plist session could never ask for a retransmit (#176).
        if let Some(port) = number("controlPort").and_then(|p| u16::try_from(p).ok()) {
            self.sender_peers.control = std::num::NonZeroU16::new(port);
        }
        let Some(codec) = stream
            .get("ct")
            .and_then(plist::Value::as_signed_integer)
            .and_then(StreamCodec::parse)
            .and_then(|c| c.negotiated(sample_rate, spf, 2))
        else {
            warn!(ct = ?stream.get("ct"), "audio SETUP names a codec this receiver cannot decode");
            return AirPlayResponse::status(451);
        };
        // The declared latency bounds, read rather than dropped: the sender states how
        // far ahead it intends to run, and a receiver that ignores it can only bank
        // that lead by accident (#176). `77175` max at 44.1 kHz is the 1.75 s every
        // live log showed.
        let latency = |key: &str| number(key).and_then(|v| u32::try_from(v).ok());
        let params = AnnounceParams::plist_stream(codec, media.key, media.iv)
            .with_declared_latency(latency("latencyMin"), latency("latencyMax"));
        log_info!(link = %params.describe(), spf, "AirPlay audio stream negotiated");
        // Before the move: the actor takes these parameters when it starts the stream,
        // and a `SET_PARAMETER` arriving afterwards still needs the rate they carried.
        self.negotiated(&params);
        self.mirror_audio = Some(Box::new(params));

        let mut stream = plist::Dictionary::new();
        stream.insert(
            "type".into(),
            plist::Value::Integer(MIRROR_AUDIO_STREAM.into()),
        );
        stream.insert(
            "dataPort".into(),
            plist::Value::Integer(i64::from(ports.audio).into()),
        );
        stream.insert(
            "controlPort".into(),
            plist::Value::Integer(i64::from(ports.control).into()),
        );
        let mut reply = plist::Dictionary::new();
        reply.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(stream)]),
        );
        plist_response(&reply)
    }

    /// Handle `TEARDOWN`, which is **two requests wearing one method name**.
    ///
    /// A bare `TEARDOWN` ends the session. A `TEARDOWN` whose body names `streams` ends
    /// only those streams and leaves everything else running — and iOS sends exactly
    /// that in the middle of a healthy session. Starting a video in a mirrored app makes
    /// it switch `audioMode` to `moviePlayback` and tear down **stream type 96, the
    /// mirror's audio**, so it can renegotiate it; a receiver that reads that as "end the
    /// session" takes the picture down with it and the sender reports a failure. That is
    /// the whole of the "opening YouTube disconnects" symptom, captured from the wire:
    ///
    /// ```text
    /// TEARDOWN {"streams": [{"type": 96, "streamID": 0, …}]}
    /// ```
    ///
    /// Neither plane needs anything stopped here, which is why this looks quiet for a
    /// request that sounds so final. Both are torn down by their own transport: the
    /// mirror's data channel is a TCP connection the sender closes, and the audio is UDP
    /// that simply stops arriving — the receive task stays on its sockets and picks the
    /// stream back up when the sender resumes it, on the same ports and the same key.
    fn teardown(&mut self, body: &[u8]) -> AirPlayResponse {
        let named = Self::teardown_streams(body);
        if !named.is_empty() {
            log_info!(streams = ?named, "AirPlay stream teardown; the session continues");
            // Deliberately no `Connection: close` and no `End`. Both would be this
            // receiver ending a session the sender is still using.
            return AirPlayResponse::ok();
        }
        self.raop = RaopState::Idle;
        self.sender_peers = SenderPeers::default();
        self.mirror = MirrorState::Idle;
        self.mirror_audio = None;
        // What was negotiated goes with the negotiation; who the sender is does not —
        // it is the same phone on the same connection, and it may set up again.
        self.sender.link = None;
        self.audio_rate = None;
        // `Connection: close` is not decoration: shairport-sync answers TEARDOWN
        // with it and then closes the socket, and a sender that does not see it
        // may hold the connection open expecting to reuse the session.
        let mut r = AirPlayResponse::ok().header("Connection", "close");
        r.event = Some(SessionEvent::End);
        r
    }

    /// The stream types a `TEARDOWN` body names, empty for a whole-session teardown.
    ///
    /// Types rather than a `bool`: which plane the sender is dropping is worth having in
    /// the log, and a sender that names a stream we never served should be visible as
    /// exactly that rather than as "not a session teardown".
    fn teardown_streams(body: &[u8]) -> Vec<i64> {
        let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(body)) else {
            return Vec::new();
        };
        value
            .as_dictionary()
            .and_then(|d| d.get("streams"))
            .and_then(plist::Value::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(plist::Value::as_dictionary)
                    .filter_map(|s| s.get("type").and_then(plist::Value::as_signed_integer))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Handle `RECORD`: the sender is about to stream.
    ///
    /// `Audio-Latency` is the receiver's own minimum latency in frames, which the sender
    /// adds to its own figure. 11025 is a quarter second at 44.1 kHz and is what
    /// shairport-sync reports; it is a promise about the buffer depth the audio path
    /// keeps, so it and that buffer have to agree.
    ///
    /// **The two planes answer this differently, and conflating them cost a session.** A
    /// mirroring sender sends `RECORD` too — iOS sends it between the `SETUP` carrying
    /// the key material and the one naming the stream — and there is no `ANNOUNCE` or
    /// transport `SETUP` anywhere in that flow to have reached `RaopState::SetUp`. So the
    /// RAOP gate below answered `455` to a perfectly ordinary mirroring request, and the
    /// sender hung up. On the mirror plane this is an acknowledgement and **not** a state
    /// transition: the picture starts when the sender dials the data port, and moving
    /// `RaopState` here would have the actor consume the audio sockets for a stream
    /// nobody announced.
    fn record(&mut self) -> AirPlayResponse {
        if self.mirroring() {
            debug!("RECORD on the mirroring plane; acknowledged");
            return Self::record_ack();
        }
        let (RaopState::SetUp(params) | RaopState::Recording(params)) =
            std::mem::take(&mut self.raop)
        else {
            warn!("RECORD before SETUP: no transport has been agreed");
            // 455 Method Not Valid In This State. Prior art does not cover this case —
            // shairport-sync only documents the SETUP-before-ANNOUNCE refusal — so this
            // is our reading of RTSP's own semantics rather than an observed behaviour.
            return AirPlayResponse::status(455);
        };
        self.raop = RaopState::Recording(params);
        Self::record_ack()
    }

    /// The `RECORD` reply, which is the same on both planes.
    ///
    /// `Audio-Jack-Status` says the output is attached and analogue; every reference
    /// receiver sends it here and senders read it to decide whether there is anything
    /// on the other end to play to.
    fn record_ack() -> AirPlayResponse {
        AirPlayResponse::ok()
            .header("Audio-Latency", "11025")
            .header("Audio-Jack-Status", "connected; type=analog")
    }

    /// Whether a mirroring negotiation is under way on this session.
    ///
    /// The mirror plane's equivalent of `RaopState` being past `Idle`: from the moment
    /// the key material arrives, requests belong to the mirror rather than to an audio
    /// flow that never started.
    const fn mirroring(&self) -> bool {
        matches!(
            self.mirror,
            MirrorState::KeyMaterial | MirrorState::Ready(_)
        )
    }

    /// Handle `GET_PARAMETER`: report a parameter the sender asked us for.
    ///
    /// **This is not optional decoration, and answering it emptily ends the session.**
    /// An iOS mirroring sender asks for `volume` immediately after the mirroring `SETUP`
    /// — it is the last request of the handshake — and hangs up on a `200` that carries
    /// no `volume:` line, with no error anywhere on either side. This endpoint returned
    /// exactly that empty `200` for as long as it has existed, which is why no iPhone
    /// has ever got past the handshake.
    ///
    /// The value reported is the last one a `SET_PARAMETER` set, so the sender's slider
    /// agrees with what it told us; full scale until it says otherwise.
    fn get_parameter(&self, req: &AirPlayRequest<'_>) -> AirPlayResponse {
        let content_type = req.header("Content-Type");
        let parameters = match crate::control::parse_get_parameter(content_type, req.body) {
            Ok(p) => p,
            Err(e) => {
                // 451 "Parameter not understood", which is what the reference receiver
                // answers a `GET_PARAMETER` it cannot read. Unlike `SET_PARAMETER`, a
                // lenient 200 here is a lie: the sender is waiting for a value.
                warn!(error = %e, "refusing a GET_PARAMETER we cannot read");
                return AirPlayResponse::status(451);
            }
        };
        if parameters.is_empty() {
            return AirPlayResponse::ok();
        }
        let body: String = parameters
            .iter()
            .map(|p| p.answer(self.volume))
            .collect::<Vec<_>>()
            .concat();
        debug!(reported = %body.trim_end(), "GET_PARAMETER answered");
        AirPlayResponse::ok_body(TEXT_PARAMETERS_MIME, body.into_bytes())
    }

    /// Handle `SET_PARAMETER`: volume, progress, metadata and artwork.
    ///
    /// A body we cannot use is answered `200` rather than refused. These are the
    /// sender pushing decoration at the now-playing card; none of it is load-bearing,
    /// and a receiver that fails a session over an unrecognised metadata tag is worse
    /// than one that ignores it.
    fn set_parameter(&mut self, req: &AirPlayRequest<'_>) -> AirPlayResponse {
        let content_type = req.header("Content-Type");
        let update = match crate::control::parse_set_parameter(content_type, req.body) {
            Ok(u) => u,
            Err(e) => {
                debug!(error = %e, "ignoring a SET_PARAMETER we cannot use");
                return AirPlayResponse::ok();
            }
        };

        let mut resp = AirPlayResponse::ok();
        resp.event = match update {
            ControlUpdate::Volume(v) => {
                log_info!(dbfs = v.as_dbfs(), "AirPlay volume");
                // Kept so a later `GET_PARAMETER` reports what this sender set rather
                // than a constant, which is the whole point of the sender asking.
                self.volume = v;
                Some(SessionEvent::Control(ControlTxn::Volume(v.as_level())))
            }
            ControlUpdate::Metadata(now) => Some(SessionEvent::NowPlaying(*now)),
            ControlUpdate::Progress(progress) => {
                // Progress is in RTP timestamps, so it only means anything once a
                // negotiation has said what the sample rate is — from *whichever* path
                // negotiated this session, which is why this is not `raop.params()`.
                self.audio_rate.map(|rate| {
                    let (position, duration) = progress.as_seconds(rate);
                    let mut now = NowPlaying::default();
                    now.position = Some(std::time::Duration::from_secs_f64(position.max(0.0)));
                    now.duration = Some(std::time::Duration::from_secs_f64(duration.max(0.0)));
                    SessionEvent::NowPlaying(now)
                })
            }
            // Artwork rides on the next metadata snapshot rather than as an event of its
            // own; it arrives separately from and usually after the tags it belongs to.
            ControlUpdate::Artwork(_) | ControlUpdate::Ignored => None,
        };
        resp
    }

    /// Answer a `/fp-setup` POST.
    ///
    /// Both setup messages are real now, so this no longer guesses which one it is
    /// holding — the sender's sequence byte says, and `crypto-fairplay` reads it. The
    /// remaining FairPlay boundary is the `ekey` unwrap, which happens later at `SETUP`
    /// and not here.
    ///
    /// Note the content type: fp-setup bodies are raw bytes, not plists.
    /// `/pair-verify`, both stages, dispatched on the leading marker byte.
    ///
    /// A refusal is `400`, not `501`: 501 says "this receiver does not do this", which
    /// would be a lie told to a sender we advertised bit 27 to, and some senders retry
    /// a 501 forever.
    fn pair_verify(&mut self, body: &[u8]) -> AirPlayResponse {
        match body.first() {
            Some(0x01) => match crate::pairing::PairVerify::begin(&self.pairing, body) {
                Ok((state, reply)) => {
                    self.pair_verify = Some(state);
                    debug!("pair-verify: stage 1 answered");
                    AirPlayResponse::ok_body(OCTET_STREAM_MIME, reply)
                }
                Err(e) => {
                    warn!(error = %e, "pair-verify: stage 1 refused");
                    AirPlayResponse::status(400)
                }
            },
            Some(0x00) => match self.pair_verify.take() {
                Some(state) => match state.finish(body) {
                    Ok(shared) => {
                        self.paired_secret = Some(shared);
                        tracing::info!("airplay: sender paired");
                        AirPlayResponse::ok()
                    }
                    Err(e) => {
                        warn!(error = %e, "pair-verify: sender failed to prove itself");
                        AirPlayResponse::status(400)
                    }
                },
                None => {
                    warn!("pair-verify: stage 2 with no stage 1 before it");
                    AirPlayResponse::status(400)
                }
            },
            _ => {
                warn!("pair-verify: body with no recognisable stage marker");
                AirPlayResponse::status(400)
            }
        }
    }

    fn fp_setup(&mut self, body: &[u8]) -> AirPlayResponse {
        match self.fairplay.handle(body) {
            Ok(reply) => AirPlayResponse::ok_body(OCTET_STREAM_MIME, reply),
            Err(e) => {
                warn!(error = %e, "fp-setup failed");
                AirPlayResponse::status(400)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    /// The AirPort key is carved at build time rather than checked in, so a build
    /// without it cannot exercise the RSA paths. `nix flake check` always has it.
    fn skip_without_airport_key() -> bool {
        if crypto_raop::has_airport_key() {
            return false;
        }
        eprintln!("skipping: this build has no AirPort key");
        true
    }
    use super::*;

    /// A session with ports already bound, as the actor always supplies.
    fn session() -> AirPlaySession {
        let mut s = bare_session();
        s.set_local_ports(ReceiverPorts {
            audio: 6000,
            control: 6001,
            timing: 6002,
        });
        s
    }

    fn bare_session() -> AirPlaySession {
        AirPlaySession::new(AirPlayIdentity {
            name: "TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
            pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
            offer_hevc: false,
            mirror_height: 1080,
        })
    }

    #[test]
    fn options_lists_public_methods() {
        let mut s = session();
        let r = s.handle(&AirPlayRequest::new("OPTIONS", "*", &[])).unwrap();
        assert_eq!(r.status, 200);
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| *k == "Public" && v.contains("SETUP")));
    }

    #[test]
    fn info_returns_binary_plist() {
        let mut s = session();
        let r = s.handle(&AirPlayRequest::new("GET", "/info", &[])).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type.as_deref(), Some(APPLE_PLIST_MIME));
        assert!(r.body.starts_with(b"bplist00"));
    }

    #[test]
    fn fp_setup_answers_the_handshake_rather_than_refusing_it() {
        // This used to be a 501 on the belief that answering needed captured tables.
        // It needs a table lookup: byte 14 selects one of four canned 142-byte replies.
        let mut s = session();
        let mut body = vec![0u8; 16];
        body[..4].copy_from_slice(b"FPLY");
        body[4] = 0x03; // version
        body[5] = 1; // type
        body[6] = 1; // sequence: SETUP1
        body[14] = 2; // mode
        let r = s
            .handle(&AirPlayRequest::new("POST", "/fp-setup", &body))
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body.len(), 142);
        assert_eq!(r.content_type.as_deref(), Some(OCTET_STREAM_MIME));
    }

    #[test]
    fn a_malformed_fp_setup_is_a_400_not_a_501() {
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new(
                "POST",
                "/fp-setup",
                b"not-fairplay-at-all",
            ))
            .unwrap();
        assert_eq!(r.status, 400);
    }

    /// A `TEARDOWN` body naming one stream type, as iOS sends it.
    fn teardown_body(ty: i64) -> Vec<u8> {
        let mut stream = plist::Dictionary::new();
        stream.insert("type".into(), plist::Value::Integer(ty.into()));
        stream.insert("streamID".into(), plist::Value::Integer(0i64.into()));
        let mut d = plist::Dictionary::new();
        d.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(stream)]),
        );
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &d).unwrap();
        buf
    }

    #[test]
    fn tearing_down_the_mirror_audio_does_not_tear_down_the_session() {
        // Captured from a real session: starting a video in a mirrored app switches
        // `audioMode` to `moviePlayback` and tears down stream type 96 so it can be
        // renegotiated. Reading that as "end the session" takes the picture down with
        // it — which is the whole of the "opening YouTube disconnects" symptom.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, true)).status, 200);
        let r = s
            .handle(&AirPlayRequest::new(
                "TEARDOWN",
                "rtsp://x/1",
                &teardown_body(96),
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.event.is_none(), "the session must survive: {:?}", r.event);
        assert!(
            !r.headers.iter().any(|(k, _)| *k == "Connection"),
            "`Connection: close` would end a session the sender is still using"
        );
    }

    #[test]
    fn tearing_down_the_mirror_video_leaves_the_connection_open_too() {
        // Type 110 is the other half of the same rule. The picture stops because the
        // sender closes the data channel, not because we end the session.
        let mut s = mirroring_session();
        let r = s
            .handle(&AirPlayRequest::new(
                "TEARDOWN",
                "rtsp://x/1",
                &teardown_body(110),
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.event.is_none());
    }

    #[test]
    fn teardown_emits_end_and_closes_the_connection() {
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new("TEARDOWN", "rtsp://x/stream", &[]))
            .unwrap();
        assert!(matches!(r.event, Some(SessionEvent::End)));
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| *k == "Connection" && v == "close"),
            "TEARDOWN must tell the sender the connection is over"
        );
    }

    /// The headers an iOS `SETUP` carries.
    fn setup_headers() -> Vec<(String, String)> {
        vec![(
            "Transport".into(),
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002"
                .into(),
        )]
    }

    /// The SDP body an iOS sender announces ALAC with.
    const ANNOUNCE_BODY: &str = "v=0\r\n\
        o=iTunes 3696222840 0 IN IP4 10.0.0.7\r\n\
        m=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 AppleLossless\r\n\
        a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

    /// Drive a session through ANNOUNCE → SETUP → RECORD.
    fn recording_session() -> AirPlaySession {
        let mut s = session();
        s.handle(&AirPlayRequest::new(
            "ANNOUNCE",
            "rtsp://x/s",
            ANNOUNCE_BODY.as_bytes(),
        ))
        .unwrap();
        do_setup(&mut s);
        s.handle(&AirPlayRequest::new("RECORD", "rtsp://x/s", &[]))
            .unwrap();
        s
    }

    /// Drive one SETUP with a well-formed Transport header.
    fn do_setup(s: &mut AirPlaySession) -> AirPlayResponse {
        let headers = setup_headers();
        s.handle(&AirPlayRequest {
            method: "SETUP",
            path: "rtsp://x/s",
            headers: &headers,
            body: &[],
        })
        .unwrap()
    }

    #[test]
    fn announce_keeps_what_it_negotiated() {
        let mut s = session();
        assert!(s.announced().is_none());
        let r = s
            .handle(&AirPlayRequest::new(
                "ANNOUNCE",
                "rtsp://x/s",
                ANNOUNCE_BODY.as_bytes(),
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(s.announced().unwrap().codec.sample_rate(), 44100);
    }

    #[test]
    fn announce_tells_the_panel_what_is_playing_and_how() {
        // The UI thread of this: an adapter merges this into the source card, the same
        // way Bluetooth does with its negotiated codec. Held rather than emitted here —
        // see `sender_description`; the actor sends it when the stream starts, which is
        // the first moment the session manager will accept it.
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new(
                "ANNOUNCE",
                "rtsp://x/s",
                ANNOUNCE_BODY.as_bytes(),
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(
            r.event.is_none(),
            "nothing is playing yet, so there is no session to describe: {:?}",
            r.event
        );
        assert_eq!(
            s.sender_description().link.as_deref(),
            Some("AirPlay 1 · ALAC · 44.1 kHz · stereo")
        );
    }

    #[test]
    fn the_address_stands_in_for_a_sender_that_never_names_itself() {
        // `label()` falls back to the address, so this is the difference between
        // "Unknown device" and something a person can act on.
        let mut s = session();
        s.set_peer_addr(IpAddr::from([10, 0, 0, 9]));
        assert_eq!(s.sender_description().label(), Some("10.0.0.9"));
    }

    #[test]
    fn setup_before_announce_is_refused_rather_than_answered_ok() {
        // The lenient 200 this used to return said "fine" and then nothing played.
        let mut s = session();
        assert_eq!(do_setup(&mut s).status, 451);
    }

    #[test]
    fn record_before_setup_is_refused() {
        let mut s = session();
        s.handle(&AirPlayRequest::new(
            "ANNOUNCE",
            "rtsp://x/s",
            ANNOUNCE_BODY.as_bytes(),
        ))
        .unwrap();
        assert_eq!(
            s.handle(&AirPlayRequest::new("RECORD", "rtsp://x/s", &[]))
                .unwrap()
                .status,
            455
        );
    }

    #[test]
    fn the_happy_order_gets_through_and_reports_our_latency() {
        let mut s = session();
        assert_eq!(
            s.handle(&AirPlayRequest::new(
                "ANNOUNCE",
                "rtsp://x/s",
                ANNOUNCE_BODY.as_bytes()
            ))
            .unwrap()
            .status,
            200
        );
        let setup = do_setup(&mut s);
        assert_eq!(setup.status, 200);
        // The reply has to name the ports actually bound; a sender that reads
        // server_port=0 treats the session as failed.
        assert!(setup
            .headers
            .iter()
            .any(|(k, v)| *k == "Transport" && v.contains("server_port=6000")));
        assert_eq!(
            s.sender_peers().control.map(std::num::NonZeroU16::get),
            Some(6001)
        );
        assert_eq!(
            s.sender_peers().timing.map(std::num::NonZeroU16::get),
            Some(6002)
        );
        let rec = s
            .handle(&AirPlayRequest::new("RECORD", "rtsp://x/s", &[]))
            .unwrap();
        assert_eq!(rec.status, 200);
        assert!(rec
            .headers
            .iter()
            .any(|(k, v)| *k == "Audio-Latency" && v == "11025"));
    }

    #[test]
    fn teardown_forgets_the_negotiation() {
        // A second session on the same connection must not inherit the first one's
        // format, or a sender that re-announces gets whatever the last one used.
        let mut s = recording_session();
        assert!(s.announced().is_some());
        s.handle(&AirPlayRequest::new("TEARDOWN", "rtsp://x/s", &[]))
            .unwrap();
        assert!(s.announced().is_none());
        // And SETUP is refused again, because nothing has been announced since.
        assert_eq!(do_setup(&mut s).status, 451);
        assert_eq!(
            s.sender_peers(),
            SenderPeers::default(),
            "transport is forgotten too"
        );
    }

    #[test]
    fn a_setup_with_no_transport_header_is_refused() {
        let mut s = session();
        s.handle(&AirPlayRequest::new(
            "ANNOUNCE",
            "rtsp://x/s",
            ANNOUNCE_BODY.as_bytes(),
        ))
        .unwrap();
        assert_eq!(
            s.handle(&AirPlayRequest::new("SETUP", "rtsp://x/s", &[]))
                .unwrap()
                .status,
            400
        );
    }

    #[test]
    fn a_setup_whose_transport_omits_a_port_is_461() {
        // 461 Unsupported Transport. We cannot send resend or timing requests without
        // it, so agreeing would give a session that plays and then drifts.
        let mut s = session();
        s.handle(&AirPlayRequest::new(
            "ANNOUNCE",
            "rtsp://x/s",
            ANNOUNCE_BODY.as_bytes(),
        ))
        .unwrap();
        let headers = vec![(
            "Transport".into(),
            "RTP/AVP/UDP;unicast;mode=record;timing_port=6002".to_string(),
        )];
        let r = s
            .handle(&AirPlayRequest {
                method: "SETUP",
                path: "rtsp://x/s",
                headers: &headers,
                body: &[],
            })
            .unwrap();
        assert_eq!(r.status, 461);
    }

    #[test]
    fn a_session_whose_ports_were_never_bound_says_so_rather_than_promising_zero() {
        // Reaching this means the actor wiring is wrong, not the sender — so it is a
        // 500, and it must never be a Transport header naming ports nobody is on.
        let mut s = bare_session();
        s.handle(&AirPlayRequest::new(
            "ANNOUNCE",
            "rtsp://x/s",
            ANNOUNCE_BODY.as_bytes(),
        ))
        .unwrap();
        assert_eq!(do_setup(&mut s).status, 500);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let headers = vec![("TRANSPORT".to_string(), "x".to_string())];
        let req = AirPlayRequest {
            method: "SETUP",
            path: "*",
            headers: &headers,
            body: &[],
        };
        assert_eq!(req.header("Transport"), Some("x"));
    }

    #[test]
    fn a_half_declared_encryption_is_456_not_a_dropped_connection() {
        // The sender believes it negotiated encryption; we would play noise. It gets a
        // status it can act on rather than a closed socket.
        let mut s = session();
        let body = format!("{ANNOUNCE_BODY}a=rsaaeskey:QUJDREVGR0hJSktMTU5PUA\r\n");
        assert_eq!(
            s.handle(&AirPlayRequest::new(
                "ANNOUNCE",
                "rtsp://x/s",
                body.as_bytes()
            ))
            .unwrap()
            .status,
            456
        );
        assert!(
            s.announced().is_none(),
            "a refused ANNOUNCE settles nothing"
        );
    }

    #[test]
    fn a_codec_we_cannot_decode_is_415() {
        let mut s = session();
        let body = ANNOUNCE_BODY.replace("AppleLossless", "opus/48000/2");
        assert_eq!(
            s.handle(&AirPlayRequest::new(
                "ANNOUNCE",
                "rtsp://x/s",
                body.as_bytes()
            ))
            .unwrap()
            .status,
            415
        );
    }

    #[test]
    fn an_apple_challenge_is_answered_with_a_signature() {
        if skip_without_airport_key() {
            return;
        }
        // iTunes and macOS will not proceed without this.
        let mut s = session();
        s.set_local_addr("10.0.0.9".parse().unwrap());
        let headers = vec![(
            "Apple-Challenge".to_string(),
            "MDEyMzQ1Njc4OWFiY2RlZg".to_string(),
        )];
        let r = s
            .handle(&AirPlayRequest {
                method: "OPTIONS",
                path: "*",
                headers: &headers,
                body: &[],
            })
            .unwrap();
        let response = r
            .headers
            .iter()
            .find(|(k, _)| *k == "Apple-Response")
            .map(|(_, v)| v.clone())
            .expect("a challenge must be answered");
        // 256 bytes of signature, base64 with the padding stripped the way senders send.
        assert!(response.len() > 300, "{response}");
        assert!(!response.contains('='), "padding should be stripped");
    }

    #[test]
    fn an_options_without_a_challenge_is_unchanged() {
        let mut s = session();
        let r = s.handle(&AirPlayRequest::new("OPTIONS", "*", &[])).unwrap();
        assert_eq!(r.status, 200);
        assert!(!r.headers.iter().any(|(k, _)| *k == "Apple-Response"));
    }

    #[test]
    fn a_challenge_we_cannot_bind_to_an_address_is_left_unanswered() {
        // Better than a signature over the wrong address: a sender that checks would
        // reject it anyway, and one that does not carries on regardless.
        let mut s = bare_session();
        let headers = vec![(
            "Apple-Challenge".to_string(),
            "MDEyMzQ1Njc4OWFiY2RlZg".to_string(),
        )];
        let r = s
            .handle(&AirPlayRequest {
                method: "OPTIONS",
                path: "*",
                headers: &headers,
                body: &[],
            })
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(!r.headers.iter().any(|(k, _)| *k == "Apple-Response"));
    }

    #[test]
    fn a_volume_change_reaches_the_pipeline() {
        let mut s = session();
        let headers = vec![("Content-Type".to_string(), "text/parameters".to_string())];
        let r = s
            .handle(&AirPlayRequest {
                method: "SET_PARAMETER",
                path: "rtsp://x/s",
                headers: &headers,
                body: b"volume: -15.0\r\n",
            })
            .unwrap();
        assert_eq!(r.status, 200);
        let Some(SessionEvent::Control(ControlTxn::Volume(v))) = r.event else {
            panic!("expected a volume change, got {:?}", r.event)
        };
        // -15 dBFS is what the sender wrote, so the amplitude is the exact
        // 10^(-15/20) and not the 0.5 slider position it used to be (#85).
        assert!((v.amplitude() - 0.177_828).abs() < 1e-5, "{v:?}");
    }

    #[test]
    fn track_metadata_reaches_the_now_playing_card() {
        let mut s = session();
        let mut items = b"minm".to_vec();
        items.extend_from_slice(&6u32.to_be_bytes());
        items.extend_from_slice(b"Xtal\0\0");
        let mut body = b"mlit".to_vec();
        body.extend_from_slice(&u32::try_from(items.len()).unwrap().to_be_bytes());
        body.extend_from_slice(&items);
        let headers = vec![(
            "Content-Type".to_string(),
            "application/x-dmap-tagged".to_string(),
        )];
        let r = s
            .handle(&AirPlayRequest {
                method: "SET_PARAMETER",
                path: "rtsp://x/s",
                headers: &headers,
                body: &body,
            })
            .unwrap();
        assert!(matches!(r.event, Some(SessionEvent::NowPlaying(_))));
    }

    #[test]
    fn progress_needs_the_rate_announce_settled() {
        // Progress is in RTP timestamps, so before ANNOUNCE there is no rate to convert
        // it with — and inventing 44100 would put a wrong duration on the card.
        let mut s = session();
        let headers = vec![("Content-Type".to_string(), "text/parameters".to_string())];
        let req = AirPlayRequest {
            method: "SET_PARAMETER",
            path: "rtsp://x/s",
            headers: &headers,
            body: b"progress: 1000/45100/265600\r\n",
        };
        assert!(s.handle(&req).unwrap().event.is_none());

        s.handle(&AirPlayRequest::new(
            "ANNOUNCE",
            "rtsp://x/s",
            ANNOUNCE_BODY.as_bytes(),
        ))
        .unwrap();
        let Some(SessionEvent::NowPlaying(now)) = s.handle(&req).unwrap().event else {
            panic!("expected progress once the rate is known")
        };
        assert_eq!(now.position, Some(std::time::Duration::from_secs(1)));
        assert_eq!(now.duration, Some(std::time::Duration::from_secs(6)));
    }

    #[test]
    fn a_set_parameter_we_cannot_use_is_still_answered_ok() {
        // Decoration for the now-playing card, none of it load-bearing. Failing a
        // session over an unrecognised metadata body would be worse than ignoring it.
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new(
                "SET_PARAMETER",
                "rtsp://x/s",
                b"whatever",
            ))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.event.is_none());
    }

    #[test]
    fn a_flush_carries_its_restart_point_to_the_actor() {
        let mut s = session();
        let headers = vec![(
            "RTP-Info".to_string(),
            "seq=1234;rtptime=567890".to_string(),
        )];
        let r = s
            .handle(&AirPlayRequest {
                method: "FLUSH",
                path: "rtsp://x/s",
                headers: &headers,
                body: &[],
            })
            .unwrap();
        assert_eq!(r.status, 200);
        let point = s
            .take_flush()
            .expect("the flush point must reach the audio task");
        assert_eq!(point.rtp, Some(567_890));
        // Taken once: a second read must not replay a flush that has been acted on.
        assert!(s.take_flush().is_none());
    }

    #[test]
    fn a_flush_without_rtp_info_is_still_answered() {
        // Nothing to discard from, but refusing it would end a session over a header a
        // sender is not required to send.
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new("FLUSH", "rtsp://x/s", &[]))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(s.take_flush().is_none());
    }

    #[test]
    fn legacy_pairing_is_served_and_homekit_is_not() {
        // Two regimes at the same paths, told apart by `X-Apple-HKP` (research §2).
        // We advertise bit 27 and no HomeKit bits, so exactly one of these may work.
        let mut s = session();
        let setup = s
            .handle(&AirPlayRequest::new("POST", "/pair-setup", &[]))
            .unwrap();
        assert_eq!(setup.status, 200);
        assert_eq!(setup.body.len(), 32, "our Ed25519 public key, raw");

        let hkp = [("X-Apple-HKP".to_string(), "4".to_string())];
        let req = AirPlayRequest {
            method: "POST",
            path: "/pair-setup",
            headers: &hkp,
            body: &[],
        };
        assert_eq!(s.handle(&req).unwrap().status, 501);
    }

    /// A real `/fp-setup` SETUP2 body, and a wrapped key it unwraps (the same capture
    /// the socket-level test drives a whole mirroring session from).
    const FP_KEY_MESSAGE: &str = "46504c590301030000000098008f1a9ca548fdd57560a52926ff399f2eb154d0a7a0fffc997f58e27e00499eb9f310110d019e550e328047aea54308ab71b647041406878af96e06cf74127ae35941dceb58931b5543b39903f9f76a376248ee52e3656b561e1c1a0106ec6608df0ab4f2df528e65db6d622d3892d5b49c6c025606a574f19ebea7d93500bdd69db23333f22edcb3ccf7a6acde7389f2facabfa61b0b50";
    const FP_EKEY: &str = "46504c59010201000000003c000000006d44ba12b91f48e061eb230fc53abfa2000000108a1060465d51b808df112d08b604501f9e3ea29ce0902f3c43b81d5319d0575f78517e01";

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len() / 2)
            .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    /// The `streamConnectionID` the mirroring `SETUP` names, as a plist sends it.
    const STREAM_ID: i64 = 4_964_383_553_955_644_435;

    /// A session that has completed `/fp-setup` and has a data port, which is what any
    /// mirroring `SETUP` needs behind it.
    fn mirroring_session() -> AirPlaySession {
        let mut s = session();
        s.set_mirror_data_port(7100);
        s.handle(&AirPlayRequest::new(
            "POST",
            "/fp-setup",
            &unhex(FP_KEY_MESSAGE),
        ))
        .unwrap();
        s
    }

    /// A mirroring `SETUP` body carrying whichever blocks are asked for.
    fn mirror_setup_body(key_material: bool, stream: bool) -> Vec<u8> {
        let mut d = plist::Dictionary::new();
        if key_material {
            d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
            d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
            d.insert("timingProtocol".into(), plist::Value::String("NTP".into()));
        }
        if stream {
            let mut s0 = plist::Dictionary::new();
            s0.insert("type".into(), plist::Value::Integer(110i64.into()));
            s0.insert(
                "streamConnectionID".into(),
                plist::Value::Integer(STREAM_ID.into()),
            );
            d.insert(
                "streams".into(),
                plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
            );
        }
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &d).unwrap();
        buf
    }

    fn setup(s: &mut AirPlaySession, body: &[u8]) -> AirPlayResponse {
        s.handle(&AirPlayRequest::new("SETUP", "rtsp://x/1", body))
            .unwrap()
    }

    #[test]
    fn one_setup_may_carry_both_the_key_and_the_stream_it_belongs_to() {
        // The two blocks normally arrive in two requests, and this used to *require*
        // that: `streams` was read first, found no key material, and answered 451 to a
        // sender that had sent the key in the very same body.
        let mut s = mirroring_session();
        let r = setup(&mut s, &mirror_setup_body(true, true));
        assert_eq!(r.status, 200);
        assert!(
            s.take_mirror_keys().is_some(),
            "a combined SETUP must leave the stream ready"
        );
    }

    #[test]
    fn a_paired_sender_mirrors_with_the_rekeyed_media_key() {
        // Bit 27's other half. With the bit advertised every iOS sender completes
        // `/pair-verify`, and then encrypts the mirror with `SHA512(aeskey ‖ shared)` —
        // so a receiver that skips the rehash decrypts to noise, with nothing anywhere
        // reporting an error. `announce` has always done this for the audio plane; the
        // mirroring plane did not.
        let shared = [7u8; 32];
        let mut paired = mirroring_session();
        paired.paired_secret = Some(shared);
        assert_eq!(
            setup(&mut paired, &mirror_setup_body(true, false)).status,
            200
        );
        assert_eq!(
            setup(&mut paired, &mirror_setup_body(false, true)).status,
            200
        );
        let keys = paired.take_mirror_keys().expect("the stream is ready");

        let key_message: [u8; crypto_playfair::KEY_MESSAGE_LEN] =
            unhex(FP_KEY_MESSAGE).try_into().unwrap();
        let ekey: [u8; crypto_playfair::EKEY_LEN] = unhex(FP_EKEY).try_into().unwrap();
        let aes_key = crypto_playfair::decrypt_key(&key_message, &ekey).unwrap();
        let id = StreamConnectionId::from_plist_signed(STREAM_ID);
        let expected = MirrorKeys::derive(&crate::pairing::rekey_media(&aes_key, &shared), id);
        assert_eq!(keys.key, expected.key);
        assert_eq!(keys.iv, expected.iv);
        // And it is genuinely a different key from the unpaired derivation, which is
        // what makes getting this wrong invisible rather than fatal.
        assert_ne!(keys.key, MirrorKeys::derive(&aes_key, id).key);
    }

    #[test]
    fn an_unpaired_sender_mirrors_with_the_unwrapped_key() {
        // Nothing forces a sender to pair — `/pair-verify` is its move — and one that
        // skipped it uses the FairPlay key as-is.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        assert_eq!(setup(&mut s, &mirror_setup_body(false, true)).status, 200);
        let keys = s.take_mirror_keys().expect("the stream is ready");

        let key_message: [u8; crypto_playfair::KEY_MESSAGE_LEN] =
            unhex(FP_KEY_MESSAGE).try_into().unwrap();
        let ekey: [u8; crypto_playfair::EKEY_LEN] = unhex(FP_EKEY).try_into().unwrap();
        let aes_key = crypto_playfair::decrypt_key(&key_message, &ekey).unwrap();
        let expected =
            MirrorKeys::derive(&aes_key, StreamConnectionId::from_plist_signed(STREAM_ID));
        assert_eq!(keys.key, expected.key);
    }

    #[test]
    fn the_first_mirroring_setup_answers_with_a_timing_port_that_is_bound() {
        let mut s = mirroring_session();
        let r = setup(&mut s, &mirror_setup_body(true, false));
        assert_eq!(r.status, 200);
        let reply: plist::Value = plist::from_bytes(&r.body).unwrap();
        let reply = reply.as_dictionary().unwrap();
        assert_eq!(
            reply.get("timingPort").unwrap().as_unsigned_integer(),
            Some(6002)
        );
        // No event channel: UxPlay returns 0 here and mirrors from iOS 12 through 18.
        assert_eq!(
            reply.get("eventPort").unwrap().as_unsigned_integer(),
            Some(0)
        );
    }

    #[test]
    fn the_sender_names_itself_in_the_first_setup_and_the_card_keeps_it() {
        // "Unknown device" was never a missing feature: `name`, `model` and `deviceID`
        // are all in the first mirroring SETUP and were all parsed and dropped (#81).
        let mut s = mirroring_session();
        s.set_peer_addr(IpAddr::from([10, 0, 0, 9]));
        let mut d = plist::Dictionary::new();
        d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
        d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
        d.insert("name".into(), plist::Value::String("Chaz's iPhone".into()));
        d.insert("model".into(), plist::Value::String("iPhone17,1".into()));
        d.insert(
            "deviceID".into(),
            plist::Value::String("AA:BB:CC:DD:EE:FF".into()),
        );
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &d).unwrap();
        assert_eq!(setup(&mut s, &body).status, 200);

        let described = s.sender_description();
        assert_eq!(described.display_name.as_deref(), Some("Chaz's iPhone"));
        // Alongside the name, not instead of it: two phones in a room are routinely
        // both called "iPhone".
        assert_eq!(described.address.as_deref(), Some("10.0.0.9"));
    }

    #[test]
    fn a_sender_that_sends_only_a_model_gets_the_model_on_the_card() {
        // A part number is a poor name and a much better one than nothing.
        let mut s = mirroring_session();
        let mut d = plist::Dictionary::new();
        d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
        d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
        d.insert("model".into(), plist::Value::String("AppleTV6,2".into()));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &d).unwrap();
        assert_eq!(setup(&mut s, &body).status, 200);
        assert_eq!(
            s.sender_description().display_name.as_deref(),
            Some("AppleTV6,2")
        );
    }

    #[test]
    fn progress_works_on_a_session_that_negotiated_through_a_plist() {
        // The scrubber sat at 0:00 with the right duration for exactly one reason: the
        // rate came from `RaopState`, which only the SDP path fills in, so a plist
        // session emitted no progress event at all (#81). The duration survived because
        // it arrives by a different road — the DMAP `astm` tag.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        assert_eq!(setup(&mut s, &media_audio_setup_body()).status, 200);
        // The actor takes the parameters the moment the stream starts; the rate has to
        // outlive that, which is what this pins.
        let _taken = s.take_mirror_audio().expect("audio was negotiated");

        let headers = vec![("Content-Type".to_string(), "text/parameters".to_string())];
        let r = s
            .handle(&AirPlayRequest {
                method: "SET_PARAMETER",
                path: "rtsp://x/s",
                headers: &headers,
                body: b"progress: 1000/45100/265600\r\n",
            })
            .unwrap();
        let Some(SessionEvent::NowPlaying(now)) = r.event else {
            panic!("a plist session's progress must reach the card, got {r:?}")
        };
        assert_eq!(now.position, Some(std::time::Duration::from_secs(1)));
        assert_eq!(now.duration, Some(std::time::Duration::from_secs(6)));
    }

    /// A `GET_PARAMETER` exactly as an iOS mirroring sender sends it — the request the
    /// whole handshake used to die on (`AirPlay/950.7.1`, CSeq 7, right after the
    /// mirroring `SETUP`).
    fn get_volume(s: &mut AirPlaySession) -> AirPlayResponse {
        let headers = [("Content-Type".to_string(), "text/parameters".to_string())];
        s.handle(&AirPlayRequest {
            method: "GET_PARAMETER",
            path: "/14390981983348410438",
            headers: &headers,
            body: b"volume\r\n",
        })
        .unwrap()
    }

    /// The type-96 stream a *media* session sets up, verbatim from a captured
    /// AirPlay-from-YouTube attempt: ALAC at 352 samples a packet, and no video stream
    /// anywhere in the session.
    fn media_audio_setup_body() -> Vec<u8> {
        let mut s0 = plist::Dictionary::new();
        for (k, v) in [
            ("type", 96i64),
            ("ct", 2),
            ("spf", 352),
            ("sr", 44100),
            ("latencyMin", 11025),
            ("latencyMax", 88200),
            ("controlPort", 60284),
            ("audioFormat", 262_144),
        ] {
            s0.insert(k.into(), plist::Value::Integer(v.into()));
        }
        s0.insert("isMedia".into(), plist::Value::Boolean(true));
        let mut d = plist::Dictionary::new();
        d.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
        );
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &d).unwrap();
        buf
    }

    #[test]
    fn a_media_session_sets_up_audio_with_no_video_stream_at_all() {
        // AirPlay from an app's video rather than from the screen: the sender sends the
        // key material, then *one* stream — ALAC audio — and no type 110 ever. Deferring
        // the FairPlay unwrap to the video path meant this session was answered 451 and
        // torn down one request later.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        let r = setup(&mut s, &media_audio_setup_body());
        assert_eq!(r.status, 200, "a media session's audio must be set up");
        let params = s.take_mirror_audio().expect("audio was negotiated");
        // Read from the request, not assumed: mirroring's AAC-ELD would decode this to
        // noise, and nothing downstream would say why.
        assert!(
            matches!(params.codec, RaopCodec::Alac(cfg) if cfg.frame_length == 352),
            "expected ALAC at 352 samples, got {:?}",
            params.codec
        );
        assert_eq!(params.codec.sample_rate(), 44_100);
    }

    #[test]
    fn a_plist_setup_yields_a_timing_peer_like_a_transport_header_does() {
        // The #176 defect in one assertion: `timing_peer` was derived only from the
        // `Transport` header, so a session negotiated through the two-phase plist SETUP
        // had no timing peer, the timing client never fired, and `clock_samples=0` on
        // every mirroring and `isMedia` session.
        let mut s = mirroring_session();
        let mut d = plist::Dictionary::new();
        d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
        d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
        d.insert("timingProtocol".into(), plist::Value::String("NTP".into()));
        d.insert("timingPort".into(), plist::Value::Integer(53669i64.into()));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &d).unwrap();
        assert_eq!(setup(&mut s, &body).status, 200);
        assert_eq!(
            s.sender_peers().timing.map(std::num::NonZeroU16::get),
            Some(53669)
        );
    }

    #[test]
    fn a_ptp_senders_port_is_not_kept_as_an_ntp_peer() {
        // A sender asking for PTP names a PTP port. NTP requests to it would be noise
        // aimed at a service that never answers, so it is reported and not kept.
        let mut s = mirroring_session();
        let mut d = plist::Dictionary::new();
        d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
        d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
        d.insert("timingProtocol".into(), plist::Value::String("PTP".into()));
        d.insert("timingPort".into(), plist::Value::Integer(319i64.into()));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &d).unwrap();
        assert_eq!(setup(&mut s, &body).status, 200);
        assert_eq!(s.sender_peers().timing, None);
    }

    #[test]
    fn a_media_audio_stream_names_the_control_peer_and_declares_its_latency() {
        // Both facts are verbatim in the captured stream entry (`controlPort: 60284`,
        // `latencyMin: 11025`, `latencyMax: 88200`) and both were dropped: no control
        // peer meant no resend request could ever leave, and an unread latency is the
        // sender's declared lead banked as accidental steady-state latency (#176).
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        assert_eq!(setup(&mut s, &media_audio_setup_body()).status, 200);
        assert_eq!(
            s.sender_peers().control.map(std::num::NonZeroU16::get),
            Some(60284)
        );
        let params = s.take_mirror_audio().expect("audio was negotiated");
        assert_eq!(params.min_latency, Some(11025));
        assert_eq!(params.max_latency, Some(88200));
    }

    #[test]
    fn a_mirroring_audio_stream_is_still_aac_eld() {
        // The other sender of the same stream type, so reading `ct` cannot have broken it.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        let mut s0 = plist::Dictionary::new();
        s0.insert("type".into(), plist::Value::Integer(96i64.into()));
        s0.insert("ct".into(), plist::Value::Integer(8i64.into()));
        s0.insert("spf".into(), plist::Value::Integer(480i64.into()));
        let mut d = plist::Dictionary::new();
        d.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
        );
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &d).unwrap();
        assert_eq!(setup(&mut s, &body).status, 200);
        let params = s.take_mirror_audio().unwrap();
        assert!(matches!(params.codec, RaopCodec::AacEld { .. }));
    }

    #[test]
    fn a_codec_we_cannot_decode_is_refused_rather_than_mis_decoded() {
        // AAC-LC needs an AudioSpecificConfig this path has no way to know. Inventing one
        // produces noise; saying so produces a log line.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        let mut s0 = plist::Dictionary::new();
        s0.insert("type".into(), plist::Value::Integer(96i64.into()));
        s0.insert("ct".into(), plist::Value::Integer(4i64.into()));
        let mut d = plist::Dictionary::new();
        d.insert(
            "streams".into(),
            plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
        );
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &d).unwrap();
        assert_eq!(setup(&mut s, &body).status, 451);
    }

    #[test]
    fn a_mirroring_sender_may_record_without_ever_announcing() {
        // Captured order from a real iPhone: SETUP (key material) → GET /info →
        // GET_PARAMETER volume → RECORD → SETUP (streams). There is no ANNOUNCE and no
        // transport SETUP anywhere in a mirroring session, so the RAOP gate answered 455
        // to an ordinary request and the sender hung up.
        let mut s = mirroring_session();
        assert_eq!(setup(&mut s, &mirror_setup_body(true, false)).status, 200);
        let r = s
            .handle(&AirPlayRequest::new("RECORD", "rtsp://x/1", &[]))
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.headers.iter().any(|(k, _)| *k == "Audio-Latency"));
        // And it is an acknowledgement, not a state transition: moving the RAOP state
        // here would have the actor start an audio task for a stream nobody announced.
        assert!(!s.is_recording(), "RECORD must not start the audio plane");
        // The stream still negotiates afterwards, which is the request that follows it.
        assert_eq!(setup(&mut s, &mirror_setup_body(false, true)).status, 200);
        assert!(s.take_mirror_keys().is_some());
    }

    #[test]
    fn a_get_parameter_for_volume_is_answered_with_a_volume() {
        // The defect this replaces ended every iPhone session: an empty `200`. iOS asks
        // for `volume` as the last request of the mirroring handshake and hangs up on an
        // answer that does not contain it — no error, no retry, nothing in any log.
        let mut s = session();
        let r = get_volume(&mut s);
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type.as_deref(), Some(TEXT_PARAMETERS_MIME));
        let body = String::from_utf8(r.body).unwrap();
        assert!(body.starts_with("volume: "), "{body:?}");
        assert!(body.ends_with("\r\n"), "{body:?}");
        // Parsable as the number the sender is actually after.
        let level: f32 = body
            .trim_start_matches("volume: ")
            .trim()
            .parse()
            .expect("a dBFS number");
        assert!(
            (level - 0.0).abs() < 1e-6,
            "full scale until told otherwise"
        );
    }

    #[test]
    fn the_volume_reported_is_the_one_the_sender_set() {
        // Otherwise the sender's slider and ours disagree the moment it moves one.
        let mut s = session();
        let headers = [("Content-Type".to_string(), "text/parameters".to_string())];
        s.handle(&AirPlayRequest {
            method: "SET_PARAMETER",
            path: "rtsp://x/s",
            headers: &headers,
            body: b"volume: -15.000000\r\n",
        })
        .unwrap();
        let body = String::from_utf8(get_volume(&mut s).body).unwrap();
        let level: f32 = body.trim_start_matches("volume: ").trim().parse().unwrap();
        assert!((level - -15.0).abs() < 1e-6, "{body:?}");
    }

    #[test]
    fn a_get_parameter_we_cannot_read_is_refused_rather_than_answered_emptily() {
        // 451 Parameter not understood. Unlike `SET_PARAMETER`, a lenient 200 here is a
        // lie: the sender is waiting for a value, and an empty answer is what it treats
        // as a broken receiver.
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new(
                "GET_PARAMETER",
                "rtsp://x/s",
                b"volume\r\n",
            ))
            .unwrap();
        assert_eq!(r.status, 451, "a GET_PARAMETER with no Content-Type");
    }

    #[test]
    fn a_parameter_we_cannot_report_leaves_the_answer_empty_rather_than_wrong() {
        let mut s = session();
        let headers = [("Content-Type".to_string(), "text/parameters".to_string())];
        let r = s
            .handle(&AirPlayRequest {
                method: "GET_PARAMETER",
                path: "rtsp://x/s",
                headers: &headers,
                body: b"something-we-do-not-have\r\n",
            })
            .unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.is_empty());
    }

    /// The AirPlay *video* session, as iOS opens one: the same RTSP handshake as
    /// mirroring, and then HTTP on the same socket.
    fn video_request(
        s: &mut AirPlaySession,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> AirPlayResponse {
        s.handle(&AirPlayRequest::new(method, path, body)).unwrap()
    }

    fn play_body(location: &str) -> Vec<u8> {
        let mut dict = plist::Dictionary::new();
        dict.insert(
            "Content-Location".into(),
            plist::Value::String(location.into()),
        );
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &dict).unwrap();
        buf
    }

    #[test]
    fn a_play_becomes_the_same_event_a_dlna_or_cast_load_does() {
        // #80's whole point: `POST /play` hands over a URL, and "play the media at this
        // URL" already exists here. Before this it fell into the lenient `200` — answering
        // "fine" to a session that had negotiated perfectly, and then playing nothing.
        let mut s = session();
        let r = video_request(
            &mut s,
            "POST",
            "/play",
            &play_body("http://10.0.0.5/v.m3u8"),
        );
        assert_eq!(r.status, 200);
        match r.event {
            Some(SessionEvent::Play { source, start }) => {
                assert_eq!(source.uri().as_str(), "http://10.0.0.5/v.m3u8");
                assert_eq!(start, None);
            }
            other => panic!("a /play must produce a Play event, got {other:?}"),
        }
    }

    #[test]
    fn the_transport_endpoints_become_the_control_transactions_every_protocol_uses() {
        let mut s = session();
        video_request(
            &mut s,
            "POST",
            "/play",
            &play_body("http://10.0.0.5/v.m3u8"),
        );

        let txn = |r: AirPlayResponse| match r.event {
            Some(SessionEvent::Control(txn)) => txn,
            other => panic!("expected a control transaction, got {other:?}"),
        };
        assert_eq!(
            txn(video_request(&mut s, "POST", "/rate?value=0.000000", b"")),
            ControlTxn::Pause
        );
        assert_eq!(
            txn(video_request(&mut s, "POST", "/rate?value=1.000000", b"")),
            ControlTxn::Play
        );
        assert_eq!(
            txn(video_request(&mut s, "POST", "/scrub?position=42.5", b"")),
            ControlTxn::Seek(Duration::from_millis(42_500))
        );
        assert_eq!(
            txn(video_request(&mut s, "POST", "/stop", b"")),
            ControlTxn::Stop
        );
    }

    #[test]
    fn playback_info_answers_from_what_the_pipeline_reported() {
        // The one AirPlay surface where the receiver is authoritative: the sender draws
        // its own scrubber from this, so an empty `200` is a sender that shows 0:00 over
        // a playing film.
        let mut s = session();
        s.set_playback(crate::video::PlaybackInfo {
            duration: Some(Duration::from_secs(200)),
            position: Duration::from_secs(40),
            rate: Some(crate::video::Rate::Playing),
            ready: true,
        });
        let r = video_request(&mut s, "GET", "/playback-info", b"");
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type.as_deref(), Some(APPLE_PLIST_MIME));
        let value: plist::Value = plist::from_bytes(&r.body).unwrap();
        let dict = value.as_dictionary().unwrap();
        assert_eq!(
            dict.get("position").and_then(plist::Value::as_real),
            Some(40.0)
        );
        assert_eq!(
            dict.get("duration").and_then(plist::Value::as_real),
            Some(200.0)
        );
        assert_eq!(dict.get("rate").and_then(plist::Value::as_real), Some(1.0));
    }

    #[test]
    fn a_bare_scrub_is_a_read_in_the_older_shape() {
        let mut s = session();
        s.set_playback(crate::video::PlaybackInfo {
            duration: Some(Duration::from_secs(200)),
            position: Duration::from_secs(40),
            rate: Some(crate::video::Rate::Playing),
            ready: true,
        });
        let r = video_request(&mut s, "GET", "/scrub", b"");
        assert_eq!(r.status, 200);
        assert_eq!(
            String::from_utf8(r.body).unwrap(),
            "duration: 200\r\nposition: 40\r\n"
        );
        assert!(r.event.is_none(), "a read must not seek");
    }

    #[test]
    fn reverse_gets_the_protocol_switch_it_is_waiting_for() {
        // Nothing is pushed over the channel yet, and a sender that gets no events polls
        // `/playback-info` — which it does anyway. Refusing would be worse: some senders
        // treat a failed `/reverse` as a failed session.
        let mut s = session();
        let r = video_request(&mut s, "POST", "/reverse", b"");
        assert_eq!(r.status, 101);
        assert!(
            r.headers
                .iter()
                .any(|(n, v)| *n == "Upgrade" && v == "PTTH/1.0"),
            "{:?}",
            r.headers
        );
    }

    #[test]
    fn a_play_we_cannot_serve_is_refused_rather_than_answered_fine() {
        // The failure mode this whole file is organised against, arriving on a new path:
        // a lenient `200` tells the sender everything is well and then nothing happens,
        // with nothing anywhere saying why.
        let mut s = session();
        let mut dict = plist::Dictionary::new();
        dict.insert("Start-Position".into(), plist::Value::Real(0.5));
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &dict).unwrap();
        let r = video_request(&mut s, "POST", "/play", &body);
        assert_eq!(r.status, 400, "a /play with no URL is not fine");
        assert!(r.event.is_none());
    }

    #[test]
    fn an_action_is_understood_and_ignored_rather_than_refused() {
        // `/action` carries playlist manipulation and `unhandledURLResponse`, neither of
        // which applies to a receiver that plays one URL at a time. Understood-and-ignored
        // is not the same as unimplemented, and answering 200 here is honest.
        let mut s = session();
        let r = video_request(&mut s, "POST", "/action", b"anything");
        assert_eq!(r.status, 200);
        assert!(r.event.is_none());
    }

    #[test]
    fn a_pair_verify_stage_two_without_a_stage_one_is_refused() {
        // The endpoint is LAN-facing and the body is attacker-chosen; there must be no
        // path from "send me 68 bytes" to a session that believes it paired.
        let mut s = session();
        let body = [0u8; 68];
        assert_eq!(
            s.handle(&AirPlayRequest::new("POST", "/pair-verify", &body))
                .unwrap()
                .status,
            400
        );
    }
}
