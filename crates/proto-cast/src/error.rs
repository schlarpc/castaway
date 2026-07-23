//! Cast errors.

use thiserror::Error;

/// Failures in the Cast protocol layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CastError {
    /// Protobuf encoding failed.
    #[error("cast encode error: {0}")]
    Encode(String),

    /// Protobuf decoding failed.
    #[error("cast decode error: {0}")]
    Decode(String),

    /// A JSON payload could not be parsed.
    #[error("cast json error: {0}")]
    Json(String),

    /// A `contentId` was not a valid media URI.
    #[error("invalid media in LOAD: {0}")]
    InvalidMedia(String),

    /// Device-auth signing failed.
    #[error("device auth error: {0}")]
    Auth(String),

    /// A mirroring OFFER could not be negotiated.
    #[error("mirror negotiation error: {0}")]
    Mirror(&'static str),
}
