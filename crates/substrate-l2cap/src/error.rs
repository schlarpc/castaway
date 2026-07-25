//! Typed L2CAP failures (ground rule 7).

use thiserror::Error;

use crate::pdu::Cid;

/// Failures parsing or driving L2CAP.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum L2capError {
    /// A PDU or signaling command ended early.
    #[error("truncated {what}: need {need} bytes, have {have}")]
    Truncated {
        /// What was being parsed.
        what: &'static str,
        /// Bytes expected.
        need: usize,
        /// Bytes present.
        have: usize,
    },

    /// A payload exceeded the 16-bit length field.
    #[error("l2cap payload too long: {len} bytes (max {max})")]
    TooLong {
        /// The oversized length.
        len: usize,
        /// The ceiling.
        max: usize,
    },

    /// A PSM broke the odd/bit-8 rule.
    #[error("invalid psm: {0:#06x}")]
    InvalidPsm(u16),

    /// A signaling command code we don't implement.
    #[error("unknown signaling command code {0:#04x}")]
    UnknownSignalingCode(u8),

    /// A PDU arrived for a channel that isn't open.
    #[error("no channel for cid {0}")]
    UnknownChannel(Cid),

    /// An operation was attempted in a state that doesn't allow it — sending on a
    /// channel still configuring, for instance.
    #[error("channel {cid} is {state}, cannot {action}")]
    WrongState {
        /// The channel.
        cid: Cid,
        /// Its current state.
        state: &'static str,
        /// What was attempted.
        action: &'static str,
    },

    /// The peer refused a connection request.
    #[error("peer refused connection to {psm:#06x}: {reason}")]
    ConnectionRefused {
        /// The PSM asked for.
        psm: u16,
        /// Why.
        reason: &'static str,
    },

    /// No dynamic CIDs left to allocate.
    #[error("no free channel identifiers")]
    OutOfCids,
}
