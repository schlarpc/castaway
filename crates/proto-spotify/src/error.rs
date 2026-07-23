//! Spotify Connect errors.

use thiserror::Error;

/// Failures in the Spotify Connect onboarding surface.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SpotifyError {
    /// A pairing crypto step failed (bad length, checksum mismatch, …).
    #[error("spotify crypto error: {0}")]
    Crypto(&'static str),

    /// A required `addUser` form field was missing.
    #[error("missing field: {0}")]
    MissingField(&'static str),

    /// A base64 field could not be decoded.
    #[error("invalid base64 in {0}")]
    Base64(&'static str),
}
