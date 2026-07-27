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

    /// One of the RAOP RSA operations failed.
    #[error("raop crypto: {0}")]
    RaopCrypto(#[from] crypto_raop::RaopCryptoError),

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

    /// `a=rsaaeskey:` would not unwrap with the AirPort key.
    ///
    /// Almost always means the sender wrapped the key with FairPlay (`et=3`/`et=5`)
    /// rather than RSA — which it should only do if we advertised those, so seeing this
    /// is a sign the advertisement and the code have drifted apart.
    #[error("could not unwrap the session key from a=rsaaeskey:")]
    KeyUnwrap,

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

/// Failures parsing a `SET_PARAMETER` body.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlError {
    /// No `Content-Type`, so there is no way to know which of the three body formats
    /// this is. Guessing would parse a DMAP blob as text and produce nonsense.
    #[error("SET_PARAMETER has no Content-Type")]
    NoContentType,

    /// A content type this endpoint does not carry.
    #[error("unsupported SET_PARAMETER content type: {0}")]
    UnsupportedContentType(String),

    /// A `text/parameters` body was not text.
    #[error("SET_PARAMETER body is not UTF-8")]
    NotUtf8,

    /// The `volume:` value was not a number.
    #[error("malformed volume value")]
    BadVolume,

    /// The `progress:` value was not three timestamps.
    #[error("malformed progress value")]
    BadProgress,

    /// A DMAP container could not be walked at all.
    #[error("malformed DMAP metadata")]
    MalformedDmap,
}

/// Failures on the mirroring data channel.
///
/// Unlike the audio errors, these are **fatal to the connection**. The video stream is a
/// TCP byte stream with one continuous AES-CTR keystream over it, so a message we cannot
/// frame means sync is lost and nothing after it can be trusted — where a bad UDP audio
/// datagram is just one packet to skip.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum MirrorError {
    /// Fewer bytes than the fixed header.
    #[error("mirror header is {0} bytes, expected 128")]
    ShortHeader(usize),

    /// A payload length that cannot be a real frame.
    #[error("mirror payload length {0} is implausible")]
    PayloadTooLarge(usize),

    /// A payload type this receiver does not model.
    #[error("unknown mirror payload type {0}")]
    UnknownPayloadType(u8),

    /// The AVCC length chain did not walk cleanly to the end of the payload.
    ///
    /// Almost always means the payload did not decrypt: with the wrong key the lengths
    /// are noise and the walk overruns. It is the cheapest wrong-key detector available.
    #[error("malformed access unit (did the payload decrypt?)")]
    MalformedAccessUnit,

    /// The `AVCDecoderConfigurationRecord` was truncated.
    #[error("malformed codec configuration record")]
    MalformedCodecConfig,

    /// An empty codec-config payload: the sender wants a codec we did not advertise.
    ///
    /// In practice HEVC, when feature bit 42 is clear. Naming it is the difference
    /// between a diagnosable refusal and a stream that silently never starts.
    #[error("the sender offered a codec we did not advertise (HEVC?)")]
    CodecRefused,
}

/// Failures parsing a `SETUP` `Transport` header.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportError {
    /// The transport was not `RTP/AVP/UDP`.
    #[error("Transport is not RTP/AVP/UDP")]
    NotUdp,

    /// A port we need in order to talk back was not named.
    #[error("Transport omits {name}")]
    MissingPort {
        /// Which parameter.
        name: &'static str,
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
            // The sender offered a key we cannot read, which is the same shape of
            // problem as declaring half an encryption: it thinks there is a shared key
            // and there is not.
            Self::KeyUnwrap => 456,
            Self::NotUtf8
            | Self::MissingRtpmap
            | Self::BadFmtp
            | Self::BadBase64 { .. }
            | Self::BadLength { .. } => 400,
        }
    }
}
