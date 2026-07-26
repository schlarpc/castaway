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

    /// Pairing succeeded but the account could not be logged in — the blob was stale, or
    /// the account is not Premium. Carries text already phrased for the panel, because
    /// this is the failure a person standing in front of the screen has to act on.
    #[error("{0}")]
    Login(String),

    /// The Connect session runner is no longer accepting work. Either the receiver is
    /// shutting down or the session manager dropped its end.
    #[error("the Spotify session runner has stopped")]
    SessionGone,
}
