//! Typed HCI failures (ground rule 7).

use thiserror::Error;

use crate::status::Status;

/// Failures decoding, encoding, or exchanging HCI packets.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum HciError {
    /// A packet ended before its declared length.
    #[error("truncated {what}: need {need} bytes, have {have}")]
    Truncated {
        /// What was being parsed.
        what: &'static str,
        /// Bytes the header said to expect.
        need: usize,
        /// Bytes actually present.
        have: usize,
    },

    /// The leading packet-type indicator was not one of the four HCI types.
    #[error("unknown HCI packet indicator: {0:#04x}")]
    UnknownPacketType(u8),

    /// An event code we do not model. Carried rather than swallowed so the actor can
    /// log it — controllers emit plenty we neither need nor should crash on.
    #[error("unhandled event code {0:#04x}")]
    UnhandledEvent(u8),

    /// A field held a value outside its legal range (a handle above 12 bits, a
    /// reserved enum discriminant).
    #[error("invalid {field}: {value:#06x}")]
    InvalidField {
        /// Name of the offending field.
        field: &'static str,
        /// The value that was rejected.
        value: u16,
    },

    /// The controller answered a command with a non-success status.
    #[error("controller rejected command: {0}")]
    CommandFailed(Status),

    /// A payload exceeded what a single HCI packet can carry.
    #[error("payload too long for {what}: {len} bytes (max {max})")]
    TooLong {
        /// What was being built.
        what: &'static str,
        /// The oversized length.
        len: usize,
        /// The ceiling.
        max: usize,
    },

    /// The transport failed: device gone, USB stall, socket closed.
    #[error("hci transport: {0}")]
    Transport(String),
}
