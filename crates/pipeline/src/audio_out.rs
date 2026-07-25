//! PCM output: the last hop, where decoded samples become sound.
//!
//! Two backends behind one trait, on the same pattern as the compositor. [`NullAudioOut`]
//! accounts for everything and plays nothing, which is what the daily dev loop and CI
//! use — a headless build box has no sound card, and a test that needs one is a test
//! that does not run. [`CpalAudioOut`] is the real device (ALSA on Linux, WASAPI on
//! Windows) behind the `audio-out` feature.
//!
//! The queue between decode and the device callback is a bounded channel plus a local
//! buffer inside the callback. No mutex: an audio callback that blocks on a lock held by
//! a decode thread produces a dropout, and dropouts are exactly what this path exists to
//! avoid.

use std::time::Duration;

use crate::audio_decode::PcmBlock;
use crate::error::PipelineError;

/// Where decoded audio goes.
pub trait AudioOut: Send {
    /// Prepare the device for a stream of this shape.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if no device will accept the format.
    fn start(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError>;

    /// Queue a block for playback.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if the device has gone away.
    fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError>;

    /// Stop and release the device.
    fn stop(&mut self);
}

/// Counts what it is given and plays nothing.
#[derive(Debug, Default)]
pub struct NullAudioOut {
    started: Option<(u32, u16)>,
    frames: u64,
    blocks: u64,
}

impl NullAudioOut {
    /// A null sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total sample frames accepted.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    /// Total blocks accepted.
    #[must_use]
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }

    /// The format the stream was started with, if any.
    #[must_use]
    pub const fn format(&self) -> Option<(u32, u16)> {
        self.started
    }

    /// How much audio has been accepted, in wall-clock terms.
    #[must_use]
    pub fn played(&self) -> Duration {
        let rate = self.started.map_or(1, |(r, _)| r.max(1));
        Duration::from_nanos(
            self.frames
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(rate))
                .unwrap_or(0),
        )
    }
}

impl AudioOut for NullAudioOut {
    fn start(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError> {
        self.started = Some((sample_rate, channels));
        self.frames = 0;
        self.blocks = 0;
        Ok(())
    }

    fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
        self.blocks += 1;
        self.frames += block.frame_count() as u64;
        Ok(())
    }

    fn stop(&mut self) {
        self.started = None;
    }
}

#[cfg(feature = "audio-out")]
pub use cpal_backend::CpalAudioOut;

#[cfg(feature = "audio-out")]
mod cpal_backend {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
    use std::sync::Arc;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use tracing::{info, warn};

    use super::{AudioOut, PcmBlock, PipelineError};

    /// How many blocks may queue before the decoder is told to slow down.
    ///
    /// At ~128 frames a block and 44.1 kHz that is roughly a third of a second — enough
    /// to ride out a scheduling hiccup, short enough that a phone's pause is not audible
    /// a beat later.
    const QUEUE_BLOCKS: usize = 96;

    /// A real output device.
    ///
    /// The `cpal::Stream` handle is deliberately *not* stored here. It is not `Send` on
    /// every host, and [`AudioOut`] requires `Send` — so rather than assert a soundness
    /// property cpal declines to, the stream is created on and owned by a thread of its
    /// own, which parks until told to stop. This type then holds nothing but channels and
    /// is `Send` because it genuinely is.
    pub struct CpalAudioOut {
        samples: Option<SyncSender<Vec<f32>>>,
        shutdown: Option<SyncSender<()>>,
        underruns: Arc<AtomicU64>,
    }

    impl std::fmt::Debug for CpalAudioOut {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("CpalAudioOut")
                .field("open", &self.samples.is_some())
                .field("underruns", &self.underruns.load(Ordering::Relaxed))
                .finish()
        }
    }

    impl Default for CpalAudioOut {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CpalAudioOut {
        /// An output that has not opened a device yet.
        #[must_use]
        pub fn new() -> Self {
            Self {
                samples: None,
                shutdown: None,
                underruns: Arc::new(AtomicU64::new(0)),
            }
        }

        /// How many times the device callback ran dry.
        ///
        /// Non-zero means the decoder is not keeping up, which is audible. Worth
        /// surfacing rather than hiding, because the symptom otherwise is just "the
        /// music sounds bad" with nothing in any log.
        #[must_use]
        pub fn underruns(&self) -> u64 {
            self.underruns.load(Ordering::Relaxed)
        }
    }

    impl AudioOut for CpalAudioOut {
        fn start(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError> {
            self.stop();
            let (samples_tx, samples_rx) = sync_channel::<Vec<f32>>(QUEUE_BLOCKS);
            let (shutdown_tx, shutdown_rx) = sync_channel::<()>(1);
            // The thread reports whether the device opened, so `start` can still fail
            // synchronously — a receiver that pairs and then silently plays nothing is
            // the failure this whole path exists to avoid.
            let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
            let underruns = Arc::clone(&self.underruns);

            std::thread::spawn(move || {
                let stream = match open_stream(sample_rate, channels, samples_rx, underruns) {
                    Ok(s) => {
                        let _ = ready_tx.send(Ok(()));
                        s
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                // Park until stop() drops the sender or sends. The stream lives exactly
                // as long as this scope, on this thread, and never crosses a boundary.
                let _ = shutdown_rx.recv();
                drop(stream);
            });

            match ready_rx.recv() {
                Ok(Ok(())) => {
                    info!(sample_rate, channels, "audio output started");
                    self.samples = Some(samples_tx);
                    self.shutdown = Some(shutdown_tx);
                    Ok(())
                }
                Ok(Err(e)) => Err(PipelineError::Audio(e)),
                Err(_) => Err(PipelineError::Audio("audio thread died starting up".into())),
            }
        }

        fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
            let Some(tx) = self.samples.as_ref() else {
                return Err(PipelineError::Audio("audio output not started".into()));
            };
            match tx.try_send(block.samples.clone()) {
                Ok(()) => Ok(()),
                // A full queue means the device is behind. Dropping the newest and
                // saying so beats blocking the decode thread, which would back up into
                // the adapter and stall the signaling channel too.
                Err(TrySendError::Full(_)) => {
                    warn!("audio output queue full; dropping a block");
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => {
                    Err(PipelineError::Audio("audio device went away".into()))
                }
            }
        }

        fn stop(&mut self) {
            self.samples = None;
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.try_send(());
            }
        }
    }

    /// Build and start the output stream. Runs on the audio thread.
    fn open_stream(
        sample_rate: u32,
        channels: u16,
        rx: Receiver<Vec<f32>>,
        underruns: Arc<AtomicU64>,
    ) -> Result<cpal::Stream, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_owned())?;

        // Ask for exactly what the stream is. A device that refuses gets the
        // conversation resolved here rather than by something downstream silently
        // resampling, where the pitch shift becomes a mystery.
        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let mut pending: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
        let stream = device
            .build_output_stream(
                &config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    // Pull whatever has arrived without ever blocking: this runs on the
                    // audio thread, where waiting is a dropout.
                    while pending.len() < out.len() {
                        match rx.try_recv() {
                            Ok(block) => pending.extend(block),
                            Err(_) => break,
                        }
                    }
                    let mut ran_dry = false;
                    for slot in out.iter_mut() {
                        match pending.pop_front() {
                            Some(sample) => *slot = sample,
                            None => {
                                // Silence beats stale samples: replaying the last buffer
                                // is a buzz, which is worse than a gap.
                                *slot = 0.0;
                                ran_dry = true;
                            }
                        }
                    }
                    if ran_dry {
                        underruns.fetch_add(1, Ordering::Relaxed);
                    }
                },
                move |err| warn!(error = %err, "audio output stream error"),
                None,
            )
            .map_err(|e| format!("build output stream: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("start output stream: {e}"))?;
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn block(rate: u32, channels: u16, frames: usize) -> PcmBlock {
        PcmBlock {
            sample_rate: rate,
            channels,
            samples: vec![0.25; frames * usize::from(channels)],
            pts: Duration::ZERO,
        }
    }

    #[test]
    fn the_null_sink_accounts_for_what_it_was_given() {
        // It plays nothing, but it must not *lie* about nothing — the daily dev loop
        // uses it to prove the audio path ran end to end.
        let mut out = NullAudioOut::new();
        out.start(44_100, 2).unwrap();
        out.write(&block(44_100, 2, 441)).unwrap();
        out.write(&block(44_100, 2, 441)).unwrap();

        assert_eq!(out.blocks(), 2);
        assert_eq!(out.frames(), 882);
        assert_eq!(out.format(), Some((44_100, 2)));
        assert_eq!(out.played(), Duration::from_millis(20));
    }

    #[test]
    fn restarting_a_stream_resets_the_accounting() {
        let mut out = NullAudioOut::new();
        out.start(48_000, 2).unwrap();
        out.write(&block(48_000, 2, 480)).unwrap();
        out.start(44_100, 1).unwrap();
        assert_eq!(out.frames(), 0);
        assert_eq!(out.format(), Some((44_100, 1)));
    }

    #[test]
    fn stopping_clears_the_format() {
        let mut out = NullAudioOut::new();
        out.start(44_100, 2).unwrap();
        out.stop();
        assert_eq!(out.format(), None);
    }

    #[test]
    fn mono_frames_are_counted_per_frame_not_per_sample() {
        let mut out = NullAudioOut::new();
        out.start(44_100, 1).unwrap();
        out.write(&block(44_100, 1, 100)).unwrap();
        assert_eq!(out.frames(), 100);
    }
}
