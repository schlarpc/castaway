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

use crate::control::ControlUpdate;
use crate::error::AirPlayError;
use crate::info;
use crate::mirror::{MirrorKeys, StreamConnectionId};
use crate::sdp::{AnnounceParams, SessionKey};
use crate::transport::{ReceiverPorts, SenderPorts};

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

/// The `streams` entry type for the realtime audio that accompanies mirroring.
const MIRROR_AUDIO_STREAM: i64 = 96;

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
    /// The first `SETUP` gave us the wrapped key; waiting for the stream to be named.
    KeyMaterial {
        /// The FairPlay-wrapped AES key from the `ekey` field.
        ekey: Box<[u8; crypto_playfair::EKEY_LEN]>,
    },
    /// The second `SETUP` named the stream; the data channel can start.
    Ready(Box<MirrorKeys>),
}

/// The FairPlay-unwrapped media key for a mirroring session.
///
/// Kept beside [`MirrorState`] rather than inside it because it outlives the video
/// negotiation: a third `SETUP` asking for the session's *audio* needs the same key, and
/// by then the video keys have been handed to the data-channel task.
#[derive(Clone, Copy)]
struct MirrorMediaKey {
    key: SessionKey,
    iv: [u8; 16],
}

impl std::fmt::Debug for MirrorMediaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MirrorMediaKey(<redacted>)")
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
    sender_ports: Option<SenderPorts>,
    mirror: MirrorState,
    /// The TCP port the actor bound for the mirroring data channel.
    mirror_data_port: Option<u16>,
    /// The unwrapped media key, once a mirroring `SETUP` has provided one.
    mirror_media_key: Option<MirrorMediaKey>,
    /// Audio negotiated alongside a mirroring session, waiting for the actor.
    mirror_audio: Option<Box<AnnounceParams>>,
    /// The `eiv` from the first mirroring `SETUP`, until the key is unwrapped.
    pending_eiv: Option<[u8; 16]>,
    /// A `FLUSH` the actor has not yet passed to the audio task.
    pending_flush: Option<crate::audio::FlushPoint>,
}

impl AirPlaySession {
    /// Create a session for the given receiver identity.
    #[must_use]
    pub fn new(identity: AirPlayIdentity) -> Self {
        Self {
            identity,
            fairplay: FairPlaySession::new(),
            raop: RaopState::default(),
            local_ports: None,
            local_addr: None,
            sender_ports: None,
            mirror: MirrorState::default(),
            mirror_data_port: None,
            mirror_media_key: None,
            mirror_audio: None,
            pending_eiv: None,
            pending_flush: None,
        }
    }

    /// Tell the session which address the sender reached us on.
    ///
    /// Needed to answer an `Apple-Challenge`: the signature covers the address and MAC,
    /// so a response captured from one receiver cannot be replayed for another.
    pub fn set_local_addr(&mut self, addr: IpAddr) {
        self.local_addr = Some(addr);
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

    /// Where to send resend and timing requests, once a `SETUP` has said.
    #[must_use]
    pub const fn sender_ports(&self) -> Option<SenderPorts> {
        self.sender_ports
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
        debug!(%method, %path, body = body.len(), "airplay request");
        let resp = match (method, path) {
            ("OPTIONS", _) => self.options(req),
            ("GET", "/info") => {
                AirPlayResponse::ok_body(APPLE_PLIST_MIME, info::info_plist(&self.identity)?)
            }
            ("POST", "/fp-setup") => self.fp_setup(body),
            ("POST", "/pair-setup" | "/pair-verify") => {
                // HomeKit transient pairing (SRP/Curve25519/Ed25519) not implemented yet.
                warn!(%path, "AirPlay pairing not implemented (Q1)");
                AirPlayResponse::status(501)
            }
            ("POST" | "GET", "/feedback") => AirPlayResponse::ok(),
            ("POST", "/audioMode") => AirPlayResponse::ok(),
            ("ANNOUNCE", _) => self.announce(body),
            ("SETUP", _) => self.setup(req),
            ("RECORD", _) => self.record(),
            ("SET_PARAMETER", _) => self.set_parameter(req),
            ("GET_PARAMETER", _) => AirPlayResponse::ok(),
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
            ("TEARDOWN", _) => {
                self.raop = RaopState::Idle;
                self.sender_ports = None;
                // `Connection: close` is not decoration: shairport-sync answers TEARDOWN
                // with it and then closes the socket, and a sender that does not see it
                // may hold the connection open expecting to reuse the session.
                let mut r = AirPlayResponse::ok().header("Connection", "close");
                r.event = Some(SessionEvent::End);
                r
            }
            (m, p) => {
                debug!(method = %m, path = %p, "unhandled AirPlay request; 200 lenient");
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
            Ok(params) => {
                let link = params.describe();
                log_info!(%link, "AirPlay audio announced");
                self.raop = RaopState::Announced(Box::new(params));
                let mut r = AirPlayResponse::ok();
                // What the panel shows. The generation and codec are only knowable
                // here, so this is the one chance to say them.
                r.event = Some(SessionEvent::SourceInfo(
                    SourceDescription::new().with_link(link),
                ));
                r
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

        self.sender_ports = Some(sender);
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

    /// Handle the mirroring `SETUP`, in its two phases.
    ///
    /// The first carries `ekey`/`eiv` and is answered with our timing port. The second
    /// names the stream and is answered with the TCP port its video should be sent to.
    fn mirror_setup(&mut self, body: &[u8]) -> AirPlayResponse {
        let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(body)) else {
            warn!("mirroring SETUP body is not a plist");
            return AirPlayResponse::status(400);
        };
        let Some(dict) = value.as_dictionary() else {
            return AirPlayResponse::status(400);
        };

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
            if named(MIRROR_AUDIO_STREAM).is_some() {
                return self.mirror_audio_stream();
            }
            warn!("mirroring SETUP names no stream we serve");
            return AirPlayResponse::status(400);
        }

        let Some(ekey) = dict.get("ekey").and_then(plist::Value::as_data) else {
            warn!("mirroring SETUP has neither ekey nor streams");
            return AirPlayResponse::status(400);
        };
        let Ok(ekey) = <[u8; crypto_playfair::EKEY_LEN]>::try_from(ekey) else {
            warn!(len = ekey.len(), "ekey is not 72 bytes");
            return AirPlayResponse::status(400);
        };
        // `eiv` is the media IV and arrives unwrapped. It is kept now because the audio
        // stream needs it later, by which time this SETUP is long gone.
        let Some(eiv) = dict
            .get("eiv")
            .and_then(plist::Value::as_data)
            .and_then(|d| <[u8; 16]>::try_from(d).ok())
        else {
            warn!("mirroring SETUP has no 16-byte eiv");
            return AirPlayResponse::status(400);
        };
        self.pending_eiv = Some(eiv);
        self.mirror = MirrorState::KeyMaterial {
            ekey: Box::new(ekey),
        };
        log_info!("AirPlay mirroring key material received");

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

    /// The second mirroring `SETUP`: derive the stream keys and name the data port.
    fn mirror_streams(&mut self, stream: &plist::Dictionary) -> AirPlayResponse {
        let MirrorState::KeyMaterial { ekey } = std::mem::take(&mut self.mirror) else {
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

        let Some(key_message) = self.fairplay.key_message() else {
            warn!("mirroring SETUP before /fp-setup completed");
            return AirPlayResponse::status(451);
        };
        let aes_key = crypto_playfair::decrypt_key(key_message, &ekey);
        // Kept for the audio stream, which uses the same key with the `eiv` verbatim
        // rather than the SHA-512 derivation the video stream needs.
        if let Some(iv) = self.pending_eiv.take() {
            self.mirror_media_key = Some(MirrorMediaKey {
                key: SessionKey::from_bytes(aes_key),
                iv,
            });
        }
        self.mirror = MirrorState::Ready(Box::new(MirrorKeys::derive(&aes_key, id)));
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

    /// Answer a `SETUP` asking for the audio that rides alongside a mirroring session.
    ///
    /// It arrives on the same UDP sockets the AirPlay 1 audio flow uses and obeys the
    /// same payload rules, so the reply names those ports and the depacketiser is the
    /// same one — only the codec and the key differ.
    fn mirror_audio_stream(&mut self) -> AirPlayResponse {
        let Some(media) = self.mirror_media_key else {
            warn!("mirroring audio SETUP before the video stream provided a key");
            return AirPlayResponse::status(451);
        };
        let Some(ports) = self.local_ports else {
            warn!("no audio ports were bound");
            return AirPlayResponse::status(500);
        };
        let params = AnnounceParams::mirror_aac_eld(media.key, media.iv);
        log_info!(link = %params.describe(), "AirPlay mirroring audio negotiated");
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

    /// Handle `RECORD`: the sender is about to stream.
    ///
    /// `Audio-Latency` is the receiver's own minimum latency in frames, which the sender
    /// adds to its own figure. 11025 is a quarter second at 44.1 kHz and is what
    /// shairport-sync reports; it is a promise about the buffer depth the audio path
    /// keeps, so it and that buffer have to agree.
    fn record(&mut self) -> AirPlayResponse {
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
        AirPlayResponse::ok().header("Audio-Latency", "11025")
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
                log_info!(fraction = v.as_fraction(), "AirPlay volume");
                Some(SessionEvent::Control(ControlTxn::Volume(v.as_fraction())))
            }
            ControlUpdate::Metadata(now) => Some(SessionEvent::NowPlaying(*now)),
            ControlUpdate::Progress(progress) => {
                // Progress is in RTP timestamps, so it only means anything once
                // ANNOUNCE has said what the sample rate is.
                self.raop.params().map(|params| {
                    let (position, duration) = progress.as_seconds(params.codec.sample_rate());
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
        // way Bluetooth does with its negotiated codec.
        let mut s = session();
        let r = s
            .handle(&AirPlayRequest::new(
                "ANNOUNCE",
                "rtsp://x/s",
                ANNOUNCE_BODY.as_bytes(),
            ))
            .unwrap();
        let Some(SessionEvent::SourceInfo(desc)) = r.event else {
            panic!("ANNOUNCE should describe the source, got {:?}", r.event)
        };
        assert_eq!(
            desc.link.as_deref(),
            Some("AirPlay 1 · ALAC · 44.1 kHz · stereo")
        );
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
        assert_eq!(s.sender_ports().unwrap().control, 6001);
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
        assert!(s.sender_ports().is_none(), "transport is forgotten too");
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
        assert!((v - 0.5).abs() < 1e-6, "{v}");
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
    fn pairing_is_501_for_now() {
        let mut s = session();
        assert_eq!(
            s.handle(&AirPlayRequest::new("POST", "/pair-setup", &[]))
                .unwrap()
                .status,
            501
        );
    }
}
