//! AirPlay errors.

use thiserror::Error;

/// Failures in the AirPlay adapter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AirPlayError {
    /// Serializing/parsing a plist body failed.
    #[error("plist error: {0}")]
    Plist(String),

    /// A request body wasn't the expected shape for its endpoint.
    #[error("malformed request: {0}")]
    Malformed(&'static str),

    /// The FairPlay-SAP handshake could not complete (see `crypto-fairplay`, Q1).
    #[error("fairplay: {0}")]
    FairPlay(#[from] crypto_fairplay::FairPlayError),

    /// An `ANNOUNCE` session description could not be understood.
    #[error("announce: {0}")]
    Sdp(#[from] SdpError),

    /// The connection is over: a socket error, a byte stream that won't frame, or a
    /// peer claiming a message larger than we will buffer. The cause is rendered rather
    /// than wrapped — nothing downstream can recover differently per variant.
    #[error("connection failed: {0}")]
    Connection(String),
}

/// Failures parsing an `ANNOUNCE` session description.
///
/// Separate from [`AirPlayError`] because these map onto *specific* RTSP status codes a
/// sender understands — a half-declared encryption is a `456`, an unsupported codec is
/// a `415` — where the outer error is mostly "this connection is over".
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SdpError {
    /// The body was not valid UTF-8. SDP is a text protocol.
    #[error("session description is not UTF-8")]
    NotUtf8,

    /// No `a=rtpmap:` line, so the body never said what it was going to send.
    #[error("session description has no a=rtpmap: line")]
    MissingRtpmap,

    /// A codec we do not decode.
    #[error("unsupported codec: {0}")]
    UnsupportedCodec(String),

    /// The `a=fmtp:` parameters were not the integers ALAC needs.
    #[error("malformed a=fmtp: parameters")]
    BadFmtp,

    /// A base64 attribute would not decode.
    #[error("a={attribute}: is not valid base64")]
    BadBase64 {
        /// Which attribute.
        attribute: &'static str,
    },

    /// A decoded attribute was the wrong size for its field.
    #[error("a={attribute}: is the wrong length")]
    BadLength {
        /// Which attribute.
        attribute: &'static str,
    },

    /// Exactly one of `rsaaeskey`/`aesiv` was present.
    ///
    /// Worth its own variant because it is the failure that would otherwise be silent:
    /// treat it as plaintext and the sender hears noise, treat it as encrypted and
    /// there is no key. Neither is recoverable, so the announcement is refused.
    #[error("encryption declared but a={missing}: is missing")]
    HalfEncrypted {
        /// The attribute that should have been there.
        missing: &'static str,
    },
}

impl SdpError {
    /// The RTSP status to answer an `ANNOUNCE` this error came from.
    ///
    /// A refused announcement is not a dead connection — the sender may reasonably
    /// try again with something else — so each of these is a status code rather than a
    /// dropped socket, and the code is chosen to tell the sender *which* thing it got
    /// wrong.
    #[must_use]
    pub const fn rtsp_status(&self) -> u16 {
        match self {
            // 456 Header Field Not Valid for Resource is what shairport-sync answers to
            // a half-declared encryption, and it is the one a sender can act on.
            Self::HalfEncrypted { .. } => 456,
            // 415 Unsupported Media Type: the body was understood, the codec is not one
            // we decode.
            Self::UnsupportedCodec(_) => 415,
            Self::NotUtf8
            | Self::MissingRtpmap
            | Self::BadFmtp
            | Self::BadBase64 { .. }
            | Self::BadLength { .. } => 400,
        }
    }
}
