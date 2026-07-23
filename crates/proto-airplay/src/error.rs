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
}
