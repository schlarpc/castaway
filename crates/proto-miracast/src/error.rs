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

    /// The source's M4 named a video format we never advertised.
    ///
    /// Its own category rather than "malformed", because it is a *conforming* source
    /// doing something surprising rather than a broken one: AOSP picks the profile and
    /// level from the lower of the two sides' floors rather than from an intersection,
    /// so it can land on a profile the sink never claimed. See
    /// [`crate::video::pick_best_format`].
    #[error("the source chose a video format the sink never advertised: {0}")]
    UnadvertisedFormat(String),

    /// The M4 named no video format at all, so there is nothing to decode.
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

/// What can go wrong on the Miracast-over-Infrastructure control channel ([MS-MICE]).
///
/// Its own enum rather than variants on [`MiracastError`]: MICE is a different protocol
/// that happens to hand off to the same RTSP session, and folding its failures into the
/// session's would make "the source asked for DTLS" and "the source's M4 named a format we
/// never advertised" the same kind of thing to a caller.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MiceError {
    /// The buffer ends inside a message or a TLV.
    #[error("the message is truncated")]
    Truncated,

    /// A size field smaller than the header it is part of.
    #[error("a message claims to be {0} bytes, which does not cover its own header")]
    ShortMessage(usize),

    /// A message or field too large for its length field.
    #[error("{0} bytes will not fit the field that has to describe it")]
    TooLong(usize),

    /// A version this implementation does not speak. Only `0x01` is defined.
    #[error("unknown MICE message version {0:#04x}")]
    UnknownVersion(u8),

    /// A command outside the six defined. Note `0x01` is `SOURCE_READY` and `0x00` is not
    /// assigned at all.
    #[error("unknown MICE command {0:#04x}")]
    UnknownCommand(u8),

    /// A command arrived without a TLV it cannot mean anything without.
    #[error("the message is missing TLV {0:#04x}")]
    MissingTlv(u8),

    /// A TLV whose length is impossible for its type — or zero, which the spec forbids.
    #[error("TLV {0:#04x} has a length of {1}, which it cannot")]
    BadTlvLength(u8, usize),

    /// A friendly name that is not valid UTF-16.
    #[error("the friendly name is not valid UTF-16")]
    BadFriendlyName,

    /// `SinkDisplaysPin` without `UseDtlsStreamEncryption`, which the spec forbids: the
    /// PIN exchange travels inside the encrypted TLVs, so one without the other describes
    /// something that cannot happen.
    #[error("security options {0:#04x} ask for a PIN with no encryption to carry it")]
    IllegalSecurityOptions(u8),

    /// A PIN response reason outside the three defined.
    #[error("unknown PIN response reason {0:#04x}")]
    UnknownPinReason(u8),

    /// A host name containing a period, which [MS-MICE] §2.2.6.3 says "MUST NOT be used".
    #[error("the MICE host name {0:?} is qualified; it must not contain a period")]
    QualifiedHostName(String),
}
