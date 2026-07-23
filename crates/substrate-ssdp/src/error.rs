//! SSDP substrate errors.

use thiserror::Error;

/// Failures in the SSDP responder / message layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SsdpError {
    /// A datagram was not a well-formed SSDP (HTTP-over-UDP) message.
    #[error("malformed SSDP message: {0}")]
    Malformed(&'static str),

    /// Socket setup or I/O failed.
    #[error("SSDP socket error: {0}")]
    Io(#[from] std::io::Error),
}
