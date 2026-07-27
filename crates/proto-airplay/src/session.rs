//! The AirPlay RTSP dispatch state machine (pure). Given a request's method, path, and
//! body, it produces the response to send and any [`SessionEvent`] to emit. The socket
//! actor owns the TCP connection, the `substrate-rtsp` framing, and the post-pairing
//! ChaCha20 transform; this core just decides *what* to answer.
//!
//! Pairing (`/pair-setup`, `/pair-verify`) and FairPlay (`/fp-setup`) are the gates in
//! front of mirroring. Pairing is not implemented yet and FairPlay hits its captured-
//! tables boundary ([`crypto_fairplay`], Q1), so those return `501`; everything around
//! them (the transaction shape) is real.

use castaway_core::SessionEvent;
use crypto_fairplay::{FairPlayError, FairPlaySession, Stage};
use tracing::{debug, warn};

use crate::advert::AirPlayIdentity;
use crate::error::AirPlayError;
use crate::info;

/// The binary-plist content type AirPlay uses.
pub const APPLE_PLIST_MIME: &str = "application/x-apple-binary-plist";

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

/// One AirPlay session's control state.
pub struct AirPlaySession {
    identity: AirPlayIdentity,
    fairplay: FairPlaySession,
}

impl AirPlaySession {
    /// Create a session for the given receiver identity.
    #[must_use]
    pub fn new(identity: AirPlayIdentity) -> Self {
        Self {
            identity,
            fairplay: FairPlaySession::new(),
        }
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
            ("ANNOUNCE", _) => AirPlayResponse::ok(),
            ("SETUP", _) => AirPlayResponse::ok_body(APPLE_PLIST_MIME, Vec::new()),
            ("RECORD", _) => AirPlayResponse::ok().header("Audio-Latency", "11025"),
            ("SET_PARAMETER" | "GET_PARAMETER", _) => AirPlayResponse::ok(),
            ("FLUSH", _) => AirPlayResponse::ok(),
            ("TEARDOWN", _) => {
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

    fn fp_setup(&mut self, body: &[u8]) -> AirPlayResponse {
        let result = match self.fairplay.stage() {
            Stage::Idle => self.fairplay.setup1(body),
            Stage::AwaitingSetup2 => self.fairplay.setup2(body),
            Stage::Complete => Ok(Vec::new()),
        };
        match result {
            Ok(reply) => AirPlayResponse::ok_body(APPLE_PLIST_MIME, reply),
            Err(FairPlayError::NotImplemented) => {
                warn!("fp-setup reached the captured-tables boundary (Q1); replying 501");
                AirPlayResponse::status(501)
            }
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
    fn fp_setup_hits_not_implemented_boundary() {
        let mut s = session();
        let mut body = b"FPLY".to_vec();
        body.push(0x03);
        body.extend_from_slice(&[0, 0, 0, 0]);
        let r = s.handle("POST", "/fp-setup", &body).unwrap();
        assert_eq!(r.status, 501);
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

    #[test]
    fn pairing_is_501_for_now() {
        let mut s = session();
        assert_eq!(s.handle("POST", "/pair-setup", &[]).unwrap().status, 501);
    }
}
