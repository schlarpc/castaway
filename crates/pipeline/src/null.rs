//! The null pipeline: a [`castaway_core::Pipeline`] that logs and drains, used for the
//! daily Linux dev loop and tests (DECISION-LOG D4). It proves the whole protocol stack
//! end-to-end — advertise, negotiate, emit `SessionEvent` — without any GPU, codec, or
//! display present. The real ffmpeg+wgpu pipeline replaces it behind features.

use std::time::Duration;

use async_trait::async_trait;
use castaway_core::{ControlTxn, CoreError, FrameSource, MediaUri, Pipeline};
use tracing::info;

/// A pipeline that logs every operation and drains mirror frame sources (dropping
/// frames) so senders don't stall on a full channel.
#[derive(Default)]
pub struct NullPipeline;

impl NullPipeline {
    /// Create a null pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Spawn a task that drains a frame source, counting frames it drops.
    fn drain(source: FrameSource, label: &'static str) {
        match source {
            FrameSource::Url(uri) => info!(%uri, "null pipeline: mirror-by-url ({label})"),
            FrameSource::Encoded(mut rx) => {
                tokio::spawn(async move {
                    let mut n: u64 = 0;
                    while rx.recv().await.is_some() {
                        n += 1;
                    }
                    info!(frames = n, "null pipeline: encoded {label} source ended");
                });
            }
            FrameSource::Decoded(mut rx) => {
                tokio::spawn(async move {
                    let mut n: u64 = 0;
                    while rx.recv().await.is_some() {
                        n += 1;
                    }
                    info!(frames = n, "null pipeline: decoded {label} source ended");
                });
            }
        }
    }
}

#[async_trait]
impl Pipeline for NullPipeline {
    async fn play(&self, source: MediaUri, start: Option<Duration>) -> Result<(), CoreError> {
        info!(%source, ?start, "null pipeline: PLAY");
        Ok(())
    }

    async fn mirror(
        &self,
        video: FrameSource,
        audio: Option<FrameSource>,
    ) -> Result<(), CoreError> {
        info!("null pipeline: MIRROR begin");
        Self::drain(video, "video");
        if let Some(audio) = audio {
            Self::drain(audio, "audio");
        }
        Ok(())
    }

    async fn control(&self, txn: ControlTxn) -> Result<(), CoreError> {
        info!(?txn, "null pipeline: CONTROL");
        Ok(())
    }

    async fn stop(&self) -> Result<(), CoreError> {
        info!("null pipeline: STOP");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use castaway_core::EncodedFrame;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn play_and_control_succeed() {
        let p = NullPipeline::new();
        p.play(MediaUri::parse("https://x/v.mp4").unwrap(), None)
            .await
            .unwrap();
        p.control(ControlTxn::Pause).await.unwrap();
        p.stop().await.unwrap();
    }

    #[tokio::test]
    async fn mirror_drains_encoded_frames() {
        let p = NullPipeline::new();
        let (tx, rx) = mpsc::channel(4);
        p.mirror(FrameSource::Encoded(rx), None).await.unwrap();
        tx.send(EncodedFrame {
            video_codec: Some(castaway_core::VideoCodec::H264),
            audio_codec: None,
            pts: Duration::ZERO,
            keyframe: true,
            data: bytes::Bytes::from_static(b"nalu"),
        })
        .await
        .unwrap();
        drop(tx);
        // Give the drain task a moment; the test passing (no hang) is the assertion.
        tokio::task::yield_now().await;
    }
}
