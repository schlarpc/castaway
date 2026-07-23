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
    match Message::parse(buf) {
        Ok((msg, consumed)) => Ok(Some((msg, consumed))),
        Err(rtsp_types::ParseError::Incomplete(_)) => Ok(None),
        Err(e) => Err(RtspError::Malformed(format!("{e:?}"))),
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
