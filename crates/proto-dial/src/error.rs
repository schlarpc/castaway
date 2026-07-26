//! DIAL / Lounge errors.

use thiserror::Error;

/// Failures in the DIAL / YouTube Lounge adapter.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DialError {
    /// A bind-channel chunk was malformed (bad length prefix or JSON).
    #[error("malformed lounge chunk: {0}")]
    MalformedChunk(&'static str),

    /// A command payload was missing a required field.
    #[error("lounge command missing field: {0}")]
    MissingField(&'static str),

    /// A screen id was not the opaque token the Lounge hands out. It goes straight into
    /// the DIAL app-info XML that every sender on the LAN reads, so it is parsed at the
    /// boundary rather than trusted.
    #[error("not a lounge screen id: {0}")]
    NotAScreenId(&'static str),
}
