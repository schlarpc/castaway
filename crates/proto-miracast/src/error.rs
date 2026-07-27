//! Miracast errors.
//!
//! Split three ways rather than one flat enum, because the three layers fail for
//! unrelated reasons and answer differently: a bad *parameter* is a `400` on a live
//! connection the sender can retry, a bad *IE* is a device we skip during discovery
//! before any connection exists, and a bad *TS packet* is a resync rather than an error
//! at all until the stream stays broken.

use thiserror::Error;

/// Failures in the Miracast sink adapter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MiracastError {
    /// A WFD parameter body could not be understood.
    #[error("wfd parameter: {0}")]
    Param(#[from] ParamError),

    /// RTSP framing failed.
    #[error("rtsp: {0}")]
    Rtsp(#[from] substrate_rtsp::RtspError),

    /// The source sent a message the session state machine has no transition for.
    #[error("unexpected {method} in state {state}")]
    UnexpectedMessage {
        /// The RTSP method that arrived.
        method: String,
        /// The state the session was in.
        state: &'static str,
    },

    /// The source and sink share no video format, so there is nothing to negotiate.
    #[error("no common video format with the source")]
    NoCommonVideoFormat,

    /// The source and sink share no audio format.
    #[error("no common audio format with the source")]
    NoCommonAudioFormat,

    /// The connection is over: a socket error, or a byte stream that will not frame.
    /// Rendered rather than wrapped — nothing downstream recovers differently per cause.
    #[error("connection failed: {0}")]
    Connection(String),

    /// The platform backend could not bring up a Wi-Fi Direct group.
    #[error("wi-fi direct backend: {0}")]
    Backend(String),
}

/// Failures parsing a `wfd_*` parameter.
///
/// Separate from [`MiracastError`] because these map onto a specific RTSP status a
/// source understands — RFC 7826 `451 Parameter Not Understood` for an unknown key,
/// `400` for a malformed value — where the outer error is mostly "this session is over".
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParamError {
    /// The body was not valid UTF-8. The WFD parameter syntax is ASCII text.
    #[error("parameter body is not UTF-8")]
    NotUtf8,

    /// A line had no `:` separating the key from its value.
    #[error("parameter line has no ':': {0}")]
    MissingColon(String),

    /// A value did not have the field count its grammar requires.
    #[error("{key}: expected {expected} fields, found {found}")]
    FieldCount {
        /// Which parameter.
        key: &'static str,
        /// How many space-separated fields the grammar defines.
        expected: usize,
        /// How many the source sent.
        found: usize,
    },

    /// A field that must be hexadecimal was not.
    #[error("{key}: field {field} is not the hex the grammar requires")]
    NotHex {
        /// Which parameter.
        key: &'static str,
        /// Which field, 0-based, in wire order.
        field: usize,
    },

    /// A field held a value outside the range its grammar allows.
    #[error("{key}: {detail}")]
    OutOfRange {
        /// Which parameter.
        key: &'static str,
        /// What was wrong with it.
        detail: &'static str,
    },
}

/// Failures decoding a WFD information element from a P2P frame.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum IeError {
    /// The element is shorter than its own declared subelement lengths.
    #[error("truncated WFD IE")]
    Truncated,

    /// The vendor OUI or OUI type was not Wi-Fi Alliance / WFD.
    #[error("not a WFD information element")]
    NotWfd,

    /// A subelement declared a length its body does not have.
    #[error("subelement {id} declares {declared} bytes, {actual} present")]
    BadSubelementLength {
        /// The subelement id.
        id: u8,
        /// The length in the header.
        declared: usize,
        /// What was actually there.
        actual: usize,
    },
}
