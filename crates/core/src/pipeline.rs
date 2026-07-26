//! The [`Pipeline`] trait: what the session manager drives for *media*. The real impl
//! (ffmpeg + wgpu + CEF) lives in the `pipeline` crate; a null logging impl lives there
//! too and is what the daily Linux dev loop and tests use.
//!
//! OSD is deliberately NOT here — it's a separate overlay concern that many sources feed
//! (see [`crate::osd`]), not something only the media backend owns.

use std::time::Duration;

use crate::error::CoreError;
use crate::event::ControlTxn;
use crate::nowplaying::{NowPlaying, QueueItem};
use crate::source::SourceDescription;
use crate::types::{AudioFormat, FrameSource, MediaUri};

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

    /// Begin a live audio-only session: decode `source` at `format` and play it out,
    /// with the screen showing the now-playing surface rather than video.
    ///
    /// `format` is not optional and has no default: aptX and aptX HD carry no in-band
    /// configuration, so the negotiated rate has to arrive from the adapter or the stream
    /// plays at the wrong pitch (OPEN-QUESTIONS Q25).
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the audio session can't be established.
    async fn play_audio(&self, source: FrameSource, format: AudioFormat) -> Result<(), CoreError>;

    /// Update the now-playing surface. Called with a full snapshot whenever any part of
    /// the metadata changes, including artwork arriving after the text.
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the surface can't be updated.
    async fn now_playing(&self, snapshot: NowPlaying) -> Result<(), CoreError>;

    /// Update what is queued behind the current track, nearest first.
    ///
    /// An empty list means the queue is empty and the surface should say so; a source
    /// that cannot see its queue never calls this, so the last known list stays on screen
    /// rather than being blanked by a source that simply does not know.
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the surface can't be updated.
    async fn up_next(&self, items: Vec<QueueItem>) -> Result<(), CoreError>;

    /// Update the description of who is connected and how.
    ///
    /// # Errors
    /// [`CoreError::Pipeline`] if the surface can't be updated.
    async fn source_info(&self, source: SourceDescription) -> Result<(), CoreError>;

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
}
