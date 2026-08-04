//! Typed failures for the Matter Casting receiver (ground rule 7).

use std::net::SocketAddr;

/// Everything `proto-matter` can fail at.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MatterError {
    /// A UDC datagram did not parse.
    #[error("user directed commissioning: {0}")]
    Udc(#[from] UdcError),

    /// rs-matter refused something: TLV, crypto, the interaction model, the fabric.
    ///
    /// Its `Error` is not `std::error::Error` (the crate is `no_std`-first), so it is
    /// carried as its display form rather than as a source.
    #[error("matter core: {0}")]
    Core(String),

    /// A socket bind or send failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// mDNS advertisement or browsing failed.
    #[error("mdns: {0}")]
    Mdns(#[from] substrate_mdns::MdnsError),

    /// A client asked to be commissioned but never appeared as a commissionable node.
    #[error("commissionable node {instance} never appeared on mDNS")]
    CommissioneeNotFound {
        /// The instance name the client declared in its UDC message.
        instance: String,
    },

    /// Commissioning ran but did not complete.
    #[error("commissioning {instance} at {addr} failed: {reason}")]
    CommissioningFailed {
        /// The client's declared instance name.
        instance: String,
        /// Where we tried to reach it.
        addr: SocketAddr,
        /// What went wrong.
        reason: String,
    },

    /// The session channel to the app closed.
    #[error(transparent)]
    Session(#[from] castaway_core::CoreError),
}

/// Failures parsing or building a User Directed Commissioning datagram.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UdcError {
    /// The datagram is shorter than the header plus the fixed instance-name block.
    #[error("datagram truncated: {got} bytes, need at least {need}")]
    Truncated {
        /// Bytes received.
        got: usize,
        /// Bytes required to get this far.
        need: usize,
    },

    /// The message header says the payload is encrypted. UDC is never encrypted —
    /// there is no session yet, which is the entire point of it.
    #[error("message is encrypted; UDC runs before any session exists")]
    Encrypted,

    /// A Matter message arrived on the UDC port that is not UDC.
    #[error("protocol id {got:#06x}, expected {want:#06x} (user directed commissioning)")]
    WrongProtocol {
        /// What the payload header carried.
        got: u16,
        /// What UDC uses.
        want: u16,
    },

    /// A UDC message with an opcode we do not know.
    #[error("unknown UDC opcode {0:#04x}")]
    UnknownOpcode(u8),

    /// The instance name field was empty or not UTF-8.
    #[error("instance name: {0}")]
    InstanceName(&'static str),

    /// The TLV body did not parse.
    #[error("malformed TLV: {0}")]
    Tlv(&'static str),

    /// A field parsed but was not the type the spec gives it.
    #[error("field {what}: expected {expected}")]
    Field {
        /// The field's name in the reference implementation.
        what: &'static str,
        /// What it should have been.
        expected: &'static str,
    },

    /// A `CommissionerDeclaration` carried an error code outside the spec's enum.
    #[error("unknown commissioner-declaration error code {0}")]
    UnknownErrorCode(u16),

    /// The message did not fit the buffer it was being written into.
    #[error("message does not fit in {0} bytes")]
    TooLong(usize),
}
