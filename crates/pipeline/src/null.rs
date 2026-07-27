//! The null pipeline: a [`castaway_core::Pipeline`] that logs and drains, used for the
//! daily Linux dev loop and tests (DECISION-LOG D4). It proves the whole protocol stack
//! end-to-end — advertise, negotiate, emit `SessionEvent` — without any GPU, codec, or
//! display present. The real ffmpeg+wgpu pipeline replaces it behind features.

use std::time::Duration;

use async_trait::async_trait;
use castaway_core::{
    ControlTxn, CoreError, FrameSource, MediaUri, NowPlaying, Pipeline, SourceDescription,
};
use tracing::info;

/// A pipeline that logs every operation and drains mirror frame sources (dropping
/// frames) so senders don't stall on a full channel.
#[derive(Default)]
pub struct NullPipeline {
    /// Stop flag for the session in progress, so a preempted one actually ends.
    active: std::sync::Mutex<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>,
}

impl NullPipeline {
    /// Create a null pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// End whatever session is running. Two audio sessions writing to one output device
    /// do not mix, they fight — so a new session must retire the old one first.
    fn preempt(&self) {
        if let Ok(mut guard) = self.active.lock() {
            if let Some(flag) = guard.take() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
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
            FrameSource::Pcm(rx) => {
                // A std channel drained on a blocking task, because both ends of the PCM
                // path are threads — see `FrameSource::Pcm`.
                tokio::task::spawn_blocking(move || {
                    // Count sample frames rather than blocks: block size is a property of
                    // whoever produced them, so "3140 blocks" says nothing about whether
                    // the right amount of audio arrived, and this is the number a silent
                    // session gets diagnosed with.
                    let (mut blocks, mut frames) = (0u64, 0u64);
                    while let Ok(pcm) = rx.recv() {
                        blocks += 1;
                        frames += pcm.frame_count() as u64;
                    }
                    info!(blocks, frames, "null pipeline: pcm {label} source ended");
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
        audio: Option<castaway_core::MirrorAudio>,
    ) -> Result<(), CoreError> {
        info!("null pipeline: MIRROR begin");
        Self::drain(video, "video");
        if let Some(audio) = audio {
            info!(format = %audio.format, "null pipeline: MIRROR audio");
            Self::drain(audio.source, "audio");
        }
        Ok(())
    }

    async fn play_audio(
        &self,
        source: FrameSource,
        format: castaway_core::AudioFormat,
        config: Option<bytes::Bytes>,
    ) -> Result<(), CoreError> {
        // Only the `audio` build has a decoder to configure.
        #[cfg(not(feature = "audio"))]
        let _ = config;
        // Audio is the one path this pipeline does for real when the feature is built.
        // An A2DP sink is audio-only with a now-playing card for a screen, so requiring
        // the wgpu/winit kiosk just to make sound would mean the headless build can
        // negotiate a whole stream and then silently bin it — which is exactly what it
        // did on the first iPhone that connected.
        self.preempt();
        #[cfg(feature = "audio")]
        {
            if let FrameSource::Encoded(rx) = source {
                info!(%format, "null pipeline: AUDIO begin (decoding to the output device)");
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                if let Ok(mut guard) = self.active.lock() {
                    *guard = Some(std::sync::Arc::clone(&stop));
                }
                crate::audio_session::spawn(
                    rx,
                    format,
                    config,
                    crate::audio_session::default_output(),
                    stop,
                    std::sync::Arc::new(crate::audio_session::Gain::default()),
                );
                return Ok(());
            }
            info!(%format, "null pipeline: AUDIO begin (not encoded frames; draining)");
            Self::drain(source, "audio");
            Ok(())
        }
        #[cfg(not(feature = "audio"))]
        {
            info!(%format, "null pipeline: AUDIO begin (no `audio` feature; draining)");
            Self::drain(source, "audio");
            Ok(())
        }
    }

    async fn now_playing(&self, snapshot: NowPlaying) -> Result<(), CoreError> {
        // Log the art's *size* rather than the snapshot wholesale: a `Debug` of the
        // struct would dump a JPEG's worth of bytes into the journal on every track.
        info!(
            title = ?snapshot.title,
            artist = ?snapshot.artist,
            album = ?snapshot.album,
            state = ?snapshot.state,
            artwork_bytes = snapshot.artwork.as_ref().map(castaway_core::Artwork::len),
            "null pipeline: NOW PLAYING"
        );
        Ok(())
    }

    async fn up_next(&self, items: Vec<castaway_core::QueueItem>) -> Result<(), CoreError> {
        // The titles, not just the count: "3 queued" tells you the plumbing works, and
        // the names tell you the *right* queue arrived.
        info!(
            queued = items.len(),
            items = ?items.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "null pipeline: UP NEXT"
        );
        Ok(())
    }

    async fn source_info(&self, source: SourceDescription) -> Result<(), CoreError> {
        info!(%source, "null pipeline: SOURCE");
        Ok(())
    }

    async fn controls(
        &self,
        capabilities: castaway_core::ControlCapabilities,
    ) -> Result<(), CoreError> {
        info!(?capabilities, "null pipeline: CONTROLS");
        Ok(())
    }

    async fn control(&self, txn: ControlTxn) -> Result<(), CoreError> {
        info!(?txn, "null pipeline: CONTROL");
        Ok(())
    }

    async fn stop(&self) -> Result<(), CoreError> {
        self.preempt();
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
