//! Typed A2DP/AVDTP failures (ground rule 7).

use thiserror::Error;

/// Failures parsing or driving the audio profiles.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioError {
    /// A message ended before its fixed layout.
    #[error("truncated {what}: need {need} bytes, have {have}")]
    Truncated {
        /// What was being parsed.
        what: &'static str,
        /// Bytes expected.
        need: usize,
        /// Bytes present.
        have: usize,
    },

    /// A codec or vendor identifier we don't implement.
    #[error("unsupported {what}: {id:#x}")]
    UnsupportedCodec {
        /// Which identifier space.
        what: &'static str,
        /// The value.
        id: u32,
    },

    /// An AVDTP signal identifier outside the set we handle.
    #[error("unknown avdtp signal {0:#04x}")]
    UnknownSignal(u8),

    /// A stream endpoint identifier outside the legal 1..=0x3E range.
    #[error("invalid seid {0}")]
    InvalidSeid(u8),

    /// The peer sent a message that doesn't belong in the current stream state.
    #[error("avdtp signal {signal} is invalid while {state}")]
    WrongState {
        /// What arrived.
        signal: &'static str,
        /// Where the stream was.
        state: &'static str,
    },

    /// A SET_CONFIGURATION named a capability set rather than one configuration.
    #[error("configuration for {codec} is ambiguous: more than one option selected")]
    AmbiguousConfiguration {
        /// Which codec.
        codec: &'static str,
    },

    /// The peer rejected a command.
    #[error("peer rejected {signal}: error {code:#04x}")]
    Rejected {
        /// Which command.
        signal: &'static str,
        /// AVDTP error code.
        code: u8,
    },

    /// A media packet could not be depacketized.
    #[error("bad media packet: {0}")]
    BadMediaPacket(&'static str),

    /// The peer's BIP image-properties document could not be read.
    ///
    /// Owned rather than `&'static str` because the useful part is what the parser
    /// choked on, which is only known at runtime.
    #[error("malformed image properties: {0}")]
    BadImageProperties(String),
}
