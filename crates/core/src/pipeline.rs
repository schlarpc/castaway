//! The [`Pipeline`] trait: what the session manager drives. The real impl (ffmpeg +
//! wgpu + CEF) lives in the `pipeline` crate; a null logging impl lives there too and
//! is what the daily Linux dev loop and tests use.

use std::time::Duration;

use crate::error::CoreError;
use crate::event::ControlTxn;
use crate::types::{FrameSource, MediaUri};

/// An on-screen-display message the session manager can post ("now casting from …").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsdMessage {
    /// The text to show.
    pub text: String,
    /// How long to show it; `None` means until replaced/cleared.
    pub ttl: Option<Duration>,
}

impl OsdMessage {
    /// A transient banner shown for `ttl`.
    #[must_use]
    pub fn banner(text: impl Into<String>, ttl: Duration) -> Self {
        Self {
            text: text.into(),
            ttl: Some(ttl),
        }
    }
}

/// The media/render backend the session drives. One active session maps to one set of
/// these calls. Kept minimal and codec/GPU-agnostic so the session layer stays pure.
#[async_trait::async_trait]
pub trait Pipeline: Send + Sync {
    /// Fetch and play a media URI (the media-URL path).
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] on decode/open failure.
    async fn play(&self, source: MediaUri, start: Option<Duration>) -> Result<(), CoreError>;

    /// Begin live mirroring from a frame source (the pixel path).
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the mirror session can't be established.
    async fn mirror(&self, video: FrameSource, audio: Option<FrameSource>)
        -> Result<(), CoreError>;

    /// Apply a transport-control transaction to the active session.
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the control can't be applied.
    async fn control(&self, txn: ControlTxn) -> Result<(), CoreError>;

    /// Tear down the active session and return to idle.
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] on teardown failure.
    async fn stop(&self) -> Result<(), CoreError>;

    /// Post (or clear, with `None`) an OSD message.
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the OSD layer can't be updated.
    async fn osd(&self, message: Option<OsdMessage>) -> Result<(), CoreError>;
}
