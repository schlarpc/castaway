//! Core error type. Libraries expose typed errors (ground rule 7); only `app` uses `anyhow`.

use thiserror::Error;

/// Failures originating in the core session/pipeline layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CoreError {
    /// A media URI could not be parsed into a supported scheme.
    #[error("invalid media uri: {0}")]
    InvalidUri(String),

    /// A friendly/display name violated its invariants (empty or too long).
    #[error("invalid friendly name: {0}")]
    InvalidName(&'static str),

    /// The session manager received an event for a source that is not the active one
    /// and could not be arbitrated (e.g. control for a backgrounded source).
    #[error("no active session for source {0}")]
    NoActiveSession(String),

    /// The pipeline rejected or failed an operation.
    #[error("pipeline error: {0}")]
    Pipeline(String),

    /// A display-control operation failed.
    #[error("display control error: {0}")]
    Display(String),

    /// The event bus was closed before the operation completed.
    #[error("session channel closed")]
    ChannelClosed,
}
