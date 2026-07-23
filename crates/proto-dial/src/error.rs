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
}
