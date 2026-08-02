//! Typed SDP failures (ground rule 7).

use thiserror::Error;

/// Failures parsing or serving SDP.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SdpError {
    /// A buffer ended before the structure it declared.
    #[error("truncated {what}: need {need} bytes, have {have}")]
    Truncated {
        /// What was being parsed.
        what: &'static str,
        /// Bytes expected.
        need: usize,
        /// Bytes present.
        have: usize,
    },

    /// A data element had an illegal type or width.
    #[error("bad data element ({what}: {detail})")]
    BadElement {
        /// Which aspect was wrong.
        what: &'static str,
        /// The offending value.
        detail: usize,
    },

    /// A PDU id we don't implement.
    #[error("unsupported sdp pdu {0:#04x}")]
    UnsupportedPdu(u8),

    /// The peer returned an SDP error response.
    #[error("peer returned sdp error {0:#06x}")]
    PeerError(u16),

    /// A continuation state was longer than the spec allows.
    #[error("continuation state too long: {0} bytes (max 16)")]
    ContinuationTooLong(usize),

    /// A response was structurally valid but didn't contain what was asked for.
    #[error("sdp response missing {0}")]
    Missing(&'static str),

    /// Nested data elements went deeper than the decoder will follow.
    #[error("sdp data element nested deeper than {limit} levels")]
    TooDeep {
        /// The ceiling that was hit.
        limit: usize,
    },

    /// A peer sent more than we agreed to accept.
    #[error("sdp {what} over budget: {got} bytes, limit {limit}")]
    TooLarge {
        /// What overran.
        what: &'static str,
        /// The ceiling that was hit.
        limit: usize,
        /// What the peer actually sent.
        got: usize,
    },
}
