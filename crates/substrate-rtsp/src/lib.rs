//! # substrate-rtsp
//!
//! The shared RTSP framing layer. Both AirPlay and Miracast speak "RTSP", but only the
//! *message codec* — request line, headers, `CSeq` correlation, `Content-Length` body
//! framing — is genuinely common (architecture §1a). That codec is
//! [`rtsp_types`](https://docs.rs/rtsp-types); this crate wraps it with:
//!
//! - [`parse`]/[`write`]: buffer-oriented framing that returns `None` on a short read.
//! - [`cseq`]: pull the `CSeq` for request/response correlation.
//! - [`ByteTransform`]: the one crypto concession the shared layer makes — an identity
//!   transform for Miracast, a ChaCha20 transform for AirPlay 2's encrypted control
//!   channel after pair-verify. Each protocol's method dispatch, body parsers, and
//!   state machine stay in its own `proto-*` crate.
#![forbid(unsafe_code)]

use std::borrow::Cow;

use rtsp_types::headers::CSeq;
use rtsp_types::Message;
use thiserror::Error;

/// Re-export so `proto-*` crates share one `rtsp_types` version.
pub use rtsp_types;

/// A parsed RTSP message with an owned body.
pub type RtspMessage = Message<Vec<u8>>;

/// RTSP framing errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RtspError {
    /// The bytes are not a valid RTSP message (and not merely incomplete).
    #[error("malformed RTSP message: {0}")]
    Malformed(String),

    /// Serializing a message to bytes failed.
    #[error("failed to write RTSP message: {0}")]
    Write(String),
}

/// Try to parse one RTSP message from the front of `buf`.
///
/// Returns `Ok(None)` if `buf` doesn't yet contain a complete message (the caller reads
/// more and retries), or `Ok(Some((msg, consumed)))` with the byte count to drain.
///
/// # Errors
/// [`RtspError::Malformed`] if the bytes are a genuinely invalid message.
pub fn parse(buf: &[u8]) -> Result<Option<(RtspMessage, usize)>, RtspError> {
    // A bare-path request is rewritten before parsing, and the inserted bytes are
    // subtracted back out so `consumed` still indexes the caller's original buffer.
    let (bytes, inserted) = match absolutize_request_uri(buf) {
        Some((rewritten, inserted)) => (Cow::Owned(rewritten), inserted),
        None => (Cow::Borrowed(buf), 0),
    };
    match Message::parse(bytes.as_ref()) {
        Ok((msg, consumed)) => Ok(Some((msg, consumed.saturating_sub(inserted)))),
        Err(rtsp_types::ParseError::Incomplete(_)) => Ok(None),
        Err(e) => Err(RtspError::Malformed(format!("{e:?}"))),
    }
}

/// The synthetic authority spliced in front of a bare-path request-URI.
///
/// `rtsp_types` parses the request-URI as an absolute [`url::Url`], per RFC 7826 — and
/// AirPlay simply doesn't send one: `GET /info RTSP/1.0`. Without this, every AirPlay
/// request fails to parse. Callers never see the authority; [`request_path`] gives back
/// the path the sender actually wrote.
const BARE_PATH_AUTHORITY: &str = "rtsp://rtsp.invalid";

/// If the request-URI on the first line is a bare path, return the buffer with
/// [`BARE_PATH_AUTHORITY`] spliced in, plus the number of bytes inserted.
fn absolutize_request_uri(buf: &[u8]) -> Option<(Vec<u8>, usize)> {
    // Interleaved binary data ($ channel len) has no request line, and scanning it for
    // CRLF would find one in the payload.
    if buf.first() == Some(&b'$') {
        return None;
    }
    let line_end = buf.windows(2).position(|w| w == b"\r\n")?;
    let uri_start = buf[..line_end].iter().position(|b| *b == b' ')? + 1;
    // A response line ("RTSP/1.0 200 OK") and an absolute URI both fail this test, which
    // is the point: only a bare path gets rewritten.
    if buf.get(uri_start) != Some(&b'/') {
        return None;
    }
    let mut out = Vec::with_capacity(buf.len() + BARE_PATH_AUTHORITY.len());
    out.extend_from_slice(&buf[..uri_start]);
    out.extend_from_slice(BARE_PATH_AUTHORITY.as_bytes());
    out.extend_from_slice(&buf[uri_start..]);
    Some((out, BARE_PATH_AUTHORITY.len()))
}

/// The path a request asked for (`/info`, `/fp-setup`, `/12345`), or `*` for the
/// no-URI form of `OPTIONS`. Absolute and bare-path request-URIs both reduce to this.
#[must_use]
pub fn request_path<B>(req: &rtsp_types::Request<B>) -> String {
    req.request_uri()
        .map_or_else(|| "*".to_string(), |uri| uri.path().to_string())
}

/// The method as the token that appeared on the wire. `rtsp_types` models the RFC
/// methods as variants and everything else as `Extension`; AirPlay leans on the latter
/// (`GET`, `POST`, `FLUSH`), so dispatch wants the name either way.
#[must_use]
pub fn method_name(method: &rtsp_types::Method) -> &str {
    use rtsp_types::Method;
    match method {
        Method::Describe => "DESCRIBE",
        Method::GetParameter => "GET_PARAMETER",
        Method::Options => "OPTIONS",
        Method::Pause => "PAUSE",
        Method::Play => "PLAY",
        Method::PlayNotify => "PLAY_NOTIFY",
        Method::Redirect => "REDIRECT",
        Method::Setup => "SETUP",
        Method::SetParameter => "SET_PARAMETER",
        Method::Announce => "ANNOUNCE",
        Method::Record => "RECORD",
        Method::Teardown => "TEARDOWN",
        Method::Extension(name) => name,
    }
}

/// Serialize an RTSP message to bytes.
///
/// # Errors
/// [`RtspError::Write`] if serialization fails.
pub fn write<B: AsRef<[u8]>>(msg: &Message<B>) -> Result<Vec<u8>, RtspError> {
    let mut out = Vec::new();
    msg.write(&mut out)
        .map_err(|e| RtspError::Write(format!("{e:?}")))?;
    Ok(out)
}

/// Extract the `CSeq` header used to correlate a response with its request.
#[must_use]
pub fn cseq<B>(msg: &Message<B>) -> Option<u32> {
    use rtsp_types::headers::TypedHeader;
    let cseq = match msg {
        Message::Request(r) => CSeq::from_headers(r),
        Message::Response(r) => CSeq::from_headers(r),
        Message::Data(_) => return None,
    };
    cseq.ok().flatten().map(|c| *c)
}

/// A transform applied to the raw byte stream in each direction. This is the single
/// crypto concession the shared RTSP layer makes: Miracast uses [`Identity`]; AirPlay 2
/// swaps in a ChaCha20-Poly1305 transform once pair-verify completes.
pub trait ByteTransform: Send {
    /// Transform bytes just read from the socket into cleartext RTSP.
    fn decrypt_inbound(&mut self, buf: &mut Vec<u8>) -> Result<(), RtspError>;

    /// Transform cleartext RTSP about to be written to the socket.
    fn encrypt_outbound(&mut self, buf: &mut Vec<u8>) -> Result<(), RtspError>;
}

/// The no-op transform (Miracast, and AirPlay before pairing).
#[derive(Debug, Default, Clone, Copy)]
pub struct Identity;

impl ByteTransform for Identity {
    fn decrypt_inbound(&mut self, _buf: &mut Vec<u8>) -> Result<(), RtspError> {
        Ok(())
    }
    fn encrypt_outbound(&mut self, _buf: &mut Vec<u8>) -> Result<(), RtspError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rtsp_types::{Method, Version};

    const OPTIONS: &[u8] = b"OPTIONS rtsp://10.0.0.1:7000/ RTSP/1.0\r\nCSeq: 3\r\n\r\n";

    #[test]
    fn parses_request_and_cseq() {
        let (msg, consumed) = parse(OPTIONS).unwrap().unwrap();
        assert_eq!(consumed, OPTIONS.len());
        match &msg {
            Message::Request(r) => assert_eq!(r.method(), Method::Options),
            _ => panic!("expected request"),
        }
        assert_eq!(cseq(&msg), Some(3));
    }

    #[test]
    fn incomplete_returns_none() {
        let partial = &OPTIONS[..10];
        assert!(parse(partial).unwrap().is_none());
    }

    /// AirPlay's actual wire form: a bare path where RFC 7826 wants an absolute URI,
    /// and a body framed by Content-Length.
    const AIRPLAY_POST: &[u8] =
        b"POST /fp-setup RTSP/1.0\r\nCSeq: 7\r\nContent-Length: 4\r\n\r\n\x46\x50\x4c\x59";

    #[test]
    fn parses_a_bare_path_request_and_reports_the_path() {
        let (msg, consumed) = parse(AIRPLAY_POST).unwrap().unwrap();
        // The count must index the caller's buffer, not the rewritten one.
        assert_eq!(consumed, AIRPLAY_POST.len());
        match &msg {
            Message::Request(r) => {
                assert_eq!(method_name(r.method()), "POST");
                assert_eq!(request_path(r), "/fp-setup");
                assert_eq!(r.body(), b"FPLY");
            }
            _ => panic!("expected request"),
        }
        assert_eq!(cseq(&msg), Some(7));
    }

    #[test]
    fn an_absolute_request_uri_is_left_alone() {
        let raw = b"SETUP rtsp://10.0.0.1:7000/12345 RTSP/1.0\r\nCSeq: 4\r\n\r\n";
        let (msg, consumed) = parse(raw).unwrap().unwrap();
        assert_eq!(consumed, raw.len());
        match &msg {
            Message::Request(r) => assert_eq!(request_path(r), "/12345"),
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn options_star_has_no_uri() {
        let raw = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (msg, _) = parse(raw).unwrap().unwrap();
        match &msg {
            Message::Request(r) => assert_eq!(request_path(r), "*"),
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn two_bare_path_requests_are_consumed_one_at_a_time() {
        let mut stream = AIRPLAY_POST.to_vec();
        stream.extend_from_slice(b"GET /info RTSP/1.0\r\nCSeq: 8\r\n\r\n");
        let (first, consumed) = parse(&stream).unwrap().unwrap();
        assert_eq!(cseq(&first), Some(7));
        // Draining by the reported count must leave the next request exactly at the front.
        let (second, _) = parse(&stream[consumed..]).unwrap().unwrap();
        assert_eq!(cseq(&second), Some(8));
        match &second {
            Message::Request(r) => assert_eq!(request_path(r), "/info"),
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn roundtrips_a_response() {
        let resp = rtsp_types::Response::builder(Version::V1_0, rtsp_types::StatusCode::Ok)
            .header(rtsp_types::headers::CSEQ, "3")
            .empty();
        let msg: Message<Vec<u8>> = resp.map_body(|_| Vec::new()).into();
        let bytes = write(&msg).unwrap();
        let (back, _) = parse(&bytes).unwrap().unwrap();
        assert_eq!(cseq(&back), Some(3));
    }

    #[test]
    fn identity_transform_is_noop() {
        let mut t = Identity;
        let mut buf = b"hello".to_vec();
        t.decrypt_inbound(&mut buf).unwrap();
        t.encrypt_outbound(&mut buf).unwrap();
        assert_eq!(buf, b"hello");
    }
}
