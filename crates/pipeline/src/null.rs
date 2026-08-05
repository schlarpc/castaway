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
    /// The panel's one audio output. `None` means a mixer of our own, opened lazily, so
    /// a `NullPipeline::new()` in a test still makes a complete audio path.
    #[cfg(feature = "audio")]
    mixer: std::sync::Mutex<Option<std::sync::Arc<crate::mixer::AudioMixer>>>,
}

impl NullPipeline {
    /// Create a null pipeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Play through `mixer` rather than one of this pipeline's own.
    #[cfg(feature = "audio")]
    #[must_use]
    pub fn with_mixer(self, mixer: std::sync::Arc<crate::mixer::AudioMixer>) -> Self {
        if let Ok(mut slot) = self.mixer.lock() {
            *slot = Some(mixer);
        }
        self
    }

    /// A way into the mix for one session.
    #[cfg(feature = "audio")]
    fn audio_input(&self) -> crate::mixer::MixInput {
        let Ok(mut slot) = self.mixer.lock() else {
            // A poisoned lock costs this session its sound and nothing else; an unattached
            // mixer plays out and is dropped with the input.
            return crate::mixer::AudioMixer::new(std::sync::Arc::new(|| {
                Box::new(crate::audio_out::NullAudioOut::new())
            }))
            .input(crate::mixer::Backpressure::Pull);
        };
        slot.get_or_insert_with(|| {
            std::sync::Arc::new(crate::mixer::AudioMixer::new(
                crate::audio_out::output_factory(crate::audio_select::OutputSelector::default()),
            ))
        })
        .input(crate::mixer::Backpressure::Pull)
    }

    /// End whatever session is running. The panel is single-source by policy, so a new
    /// session retires the old one first — since #111 that is a policy rather than the
    /// device contention it used to be.
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
        // The audio half is *played*, not drained. A headless build is the default one,
        // and a mirror that made no sound because the screen was null would be the same
        // silent failure this whole field had before it carried a format.
        #[cfg(feature = "audio")]
        if let Some(audio) = audio {
            if let FrameSource::Encoded(rx) = audio.source {
                info!(format = %audio.format, "null pipeline: MIRROR audio (decoding)");
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                if let Ok(mut guard) = self.active.lock() {
                    *guard = Some(std::sync::Arc::clone(&stop));
                }
                crate::audio_session::spawn(
                    rx,
                    audio.format,
                    audio.config,
                    self.audio_input(),
                    stop,
                    // The null pipeline has no session manager to tell, so a refused
                    // output is simply logged by the session itself.
                    None,
                );
            } else {
                info!("null pipeline: MIRROR audio is not encoded frames; draining");
                Self::drain(audio.source, "audio");
            }
        }
        #[cfg(not(feature = "audio"))]
        if let Some(audio) = audio {
            Self::drain(audio.source, "audio");
        }
        Self::drain(video, "video");
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
                    self.audio_input(),
                    stop,
                    // The null pipeline has no session manager to tell, so a refused
                    // output is simply logged by the session itself.
                    None,
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

    /// The panel's device, remembering whether anything was actually played through it.
    ///
    /// Counts *audible* frames rather than every frame it is given. Since #111 it sits
    /// under the mixer rather than under the session, and the mixer runs continuously —
    /// it pads with silence whenever no source has anything to say — so a bare frame
    /// count would pass this test on an empty panel.
    #[cfg(feature = "audio")]
    #[derive(Default)]
    struct Speaker {
        frames: std::sync::atomic::AtomicU64,
    }

    #[cfg(feature = "audio")]
    impl crate::audio_out::AudioOut for std::sync::Arc<Speaker> {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), crate::error::PipelineError> {
            Ok(())
        }
        fn write(
            &mut self,
            block: &crate::audio_decode::PcmBlock,
        ) -> Result<(), crate::error::PipelineError> {
            let audible = block.samples.iter().filter(|s| s.abs() > 1e-3).count() as u64;
            self.frames.fetch_add(
                audible / u64::from(crate::mixer::CHANNELS),
                std::sync::atomic::Ordering::SeqCst,
            );
            Ok(())
        }
        fn stop(&mut self) {}
    }

    #[cfg(all(feature = "audio", feature = "ffmpeg"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mirrors_audio_reaches_the_output_as_sound() {
        // The assertion whose absence let a real bug live for the whole life of Cast
        // mirroring: `SessionEvent::Mirror` has always carried an `audio` field, the
        // render pipeline took it as `_audio`, and every frame was discarded. Nothing
        // caught it, because every test around it proved a *layer* — the adapter emits
        // frames, the depacketiser decrypts them, the channel receives them — and a
        // pipeline that accepts frames and bins them satisfies all of those.
        //
        // So this one asserts the only thing that distinguishes wired-up from dropped:
        // that a sample left the box.
        //
        // SBC because it is the codec every build can decode; what is under test is the
        // wiring, not the codec.
        let rate = 44_100;
        let frames = crate::audio_decode::tests::encode(
            castaway_core::AudioCodec::Sbc,
            rate,
            &crate::audio_decode::tests::sine(rate, 44_100),
        );
        if !crate::test_media::available("an SBC encoder", !frames.is_empty()) {
            return;
        }

        let (atx, arx) = mpsc::channel(frames.len() + 1);
        for frame in frames {
            atx.send(frame).await.unwrap();
        }
        drop(atx);

        let speaker = std::sync::Arc::new(Speaker::default());
        let for_factory = std::sync::Arc::clone(&speaker);
        let pipeline =
            NullPipeline::new().with_mixer(std::sync::Arc::new(crate::mixer::AudioMixer::new(
                std::sync::Arc::new(move || Box::new(std::sync::Arc::clone(&for_factory))),
            )));

        // Video is present but empty: a mirror is video by definition, and the audio has
        // to play regardless of whether any picture ever arrives.
        let (_vtx, vrx) = mpsc::channel(1);
        pipeline
            .mirror(
                FrameSource::Encoded(vrx),
                Some(castaway_core::MirrorAudio {
                    source: FrameSource::Encoded(arx),
                    format: crate::audio_decode::tests::format(rate, 2),
                    config: None,
                }),
            )
            .await
            .unwrap();

        // The session runs on its own thread; wait for it to drain rather than sleeping
        // a fixed amount.
        for _ in 0..200 {
            if speaker.frames.load(std::sync::atomic::Ordering::SeqCst) > u64::from(rate) / 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let played = speaker.frames.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            played > 0,
            "not one sample frame of the mirror's audio reached the output"
        );
        // A second in, near enough a second out — a session that decoded one frame and
        // stopped would satisfy a bare `> 0`.
        assert!(
            played > u64::from(rate) / 2,
            "only {played} frames of a one-second clip reached the output"
        );
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
