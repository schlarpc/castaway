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

use castaway_core::{SessionEvent, SourceDescription};
use crypto_fairplay::FairPlaySession;
use tracing::{debug, info as log_info, warn};

use crate::advert::AirPlayIdentity;
use crate::error::AirPlayError;
use crate::info;
use crate::sdp::AnnounceParams;

/// The binary-plist content type AirPlay uses.
pub const APPLE_PLIST_MIME: &str = "application/x-apple-binary-plist";

/// The content type for raw byte bodies — `/fp-setup` replies are not plists.
pub const OCTET_STREAM_MIME: &str = "application/octet-stream";

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
}

impl AirPlaySession {
    /// Create a session for the given receiver identity.
    #[must_use]
    pub fn new(identity: AirPlayIdentity) -> Self {
        Self {
            identity,
            fairplay: FairPlaySession::new(),
            raop: RaopState::default(),
        }
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
    pub fn handle(
        &mut self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<AirPlayResponse, AirPlayError> {
        debug!(%method, %path, body = body.len(), "airplay request");
        let resp = match (method, path) {
            ("OPTIONS", _) => AirPlayResponse::ok().header(
                "Public",
                "ANNOUNCE, SETUP, RECORD, PAUSE, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, \
                 SET_PARAMETER, POST, GET",
            ),
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
            ("SETUP", _) => self.setup(),
            ("RECORD", _) => self.record(),
            ("SET_PARAMETER" | "GET_PARAMETER", _) => AirPlayResponse::ok(),
            ("FLUSH", _) => AirPlayResponse::ok(),
            ("TEARDOWN", _) => {
                self.raop = RaopState::Idle;
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
    fn setup(&mut self) -> AirPlayResponse {
        let (RaopState::Announced(params) | RaopState::SetUp(params)) =
            std::mem::take(&mut self.raop)
        else {
            warn!("SETUP before ANNOUNCE: nothing has been announced to set up");
            return AirPlayResponse::status(451);
        };
        // Transport negotiation — binding the three UDP sockets and naming their ports
        // back — lands with the audio actor. Until then this records the transition and
        // sends no Transport header, which a sender will notice; it is not yet a
        // working SETUP, only a correctly-ordered one.
        self.raop = RaopState::SetUp(params);
        AirPlayResponse::ok()
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

    fn session() -> AirPlaySession {
        AirPlaySession::new(AirPlayIdentity {
            name: "TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
            pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
        })
    }

    #[test]
    fn options_lists_public_methods() {
        let mut s = session();
        let r = s.handle("OPTIONS", "*", &[]).unwrap();
        assert_eq!(r.status, 200);
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| *k == "Public" && v.contains("SETUP")));
    }

    #[test]
    fn info_returns_binary_plist() {
        let mut s = session();
        let r = s.handle("GET", "/info", &[]).unwrap();
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
        let r = s.handle("POST", "/fp-setup", &body).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body.len(), 142);
        assert_eq!(r.content_type.as_deref(), Some(OCTET_STREAM_MIME));
    }

    #[test]
    fn a_malformed_fp_setup_is_a_400_not_a_501() {
        let mut s = session();
        let r = s
            .handle("POST", "/fp-setup", b"not-fairplay-at-all")
            .unwrap();
        assert_eq!(r.status, 400);
    }

    #[test]
    fn teardown_emits_end_and_closes_the_connection() {
        let mut s = session();
        let r = s.handle("TEARDOWN", "rtsp://x/stream", &[]).unwrap();
        assert!(matches!(r.event, Some(SessionEvent::End)));
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| *k == "Connection" && v == "close"),
            "TEARDOWN must tell the sender the connection is over"
        );
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
        s.handle("ANNOUNCE", "rtsp://x/s", ANNOUNCE_BODY.as_bytes())
            .unwrap();
        s.handle("SETUP", "rtsp://x/s", &[]).unwrap();
        s.handle("RECORD", "rtsp://x/s", &[]).unwrap();
        s
    }

    #[test]
    fn announce_keeps_what_it_negotiated() {
        let mut s = session();
        assert!(s.announced().is_none());
        let r = s
            .handle("ANNOUNCE", "rtsp://x/s", ANNOUNCE_BODY.as_bytes())
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
            .handle("ANNOUNCE", "rtsp://x/s", ANNOUNCE_BODY.as_bytes())
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
        assert_eq!(s.handle("SETUP", "rtsp://x/s", &[]).unwrap().status, 451);
    }

    #[test]
    fn record_before_setup_is_refused() {
        let mut s = session();
        s.handle("ANNOUNCE", "rtsp://x/s", ANNOUNCE_BODY.as_bytes())
            .unwrap();
        assert_eq!(s.handle("RECORD", "rtsp://x/s", &[]).unwrap().status, 455);
    }

    #[test]
    fn the_happy_order_gets_through_and_reports_our_latency() {
        let mut s = session();
        assert_eq!(
            s.handle("ANNOUNCE", "rtsp://x/s", ANNOUNCE_BODY.as_bytes())
                .unwrap()
                .status,
            200
        );
        assert_eq!(s.handle("SETUP", "rtsp://x/s", &[]).unwrap().status, 200);
        let rec = s.handle("RECORD", "rtsp://x/s", &[]).unwrap();
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
        s.handle("TEARDOWN", "rtsp://x/s", &[]).unwrap();
        assert!(s.announced().is_none());
        // And SETUP is refused again, because nothing has been announced since.
        assert_eq!(s.handle("SETUP", "rtsp://x/s", &[]).unwrap().status, 451);
    }

    #[test]
    fn a_half_declared_encryption_is_456_not_a_dropped_connection() {
        // The sender believes it negotiated encryption; we would play noise. It gets a
        // status it can act on rather than a closed socket.
        let mut s = session();
        let body = format!("{ANNOUNCE_BODY}a=rsaaeskey:QUJDREVGR0hJSktMTU5PUA\r\n");
        assert_eq!(
            s.handle("ANNOUNCE", "rtsp://x/s", body.as_bytes())
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
            s.handle("ANNOUNCE", "rtsp://x/s", body.as_bytes())
                .unwrap()
                .status,
            415
        );
    }

    #[test]
    fn pairing_is_501_for_now() {
        let mut s = session();
        assert_eq!(s.handle("POST", "/pair-setup", &[]).unwrap().status, 501);
    }
}
