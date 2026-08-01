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

/// How a pipeline obtains an audio output device.
///
/// A factory rather than a device, because each session takes its own: two sessions
/// writing to one device fight rather than mix. It is also the seam a test uses to
/// observe that samples actually left the box — see `null::tests`.
pub type AudioOutputFactory = std::sync::Arc<dyn Fn() -> Box<dyn AudioOut> + Send + Sync>;

pub use crate::audio_select::{
    OutputBackendKind, OutputDeviceInfo, OutputSelection, OutputSelector,
};

/// An output honouring `selection`, from the active backend.
///
/// The dispatch mirrors [`active_backend`] exactly; a build where they disagreed would
/// offer devices it then couldn't open.
#[must_use]
pub fn selected_output(selection: &OutputSelection) -> Box<dyn AudioOut> {
    #[cfg(all(feature = "audio-pipewire", target_os = "linux"))]
    {
        Box::new(crate::audio_pw::PipeWireAudioOut::with_selection(
            selection.clone(),
        ))
    }
    #[cfg(all(
        feature = "audio-out",
        not(all(feature = "audio-pipewire", target_os = "linux"))
    ))]
    {
        Box::new(CpalAudioOut::with_selection(selection.clone()))
    }
    #[cfg(not(any(
        feature = "audio-out",
        all(feature = "audio-pipewire", target_os = "linux")
    )))]
    {
        let _ = selection;
        Box::new(NullAudioOut::new())
    }
}

/// A factory whose streams follow `selector` — the one the app installs everywhere, so
/// a device picked on the settings screen reaches every source's next session.
#[must_use]
pub fn output_factory(selector: OutputSelector) -> AudioOutputFactory {
    std::sync::Arc::new(move || selected_output(&selector.get()))
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

/// The cpal host's output devices — `audio_select::list_output_devices`'s real half.
/// Compiled only where cpal is the *selected* backend: with the native PipeWire
/// backend outranking it, this would be a list nobody asks for.
#[cfg(all(
    feature = "audio-out",
    not(all(feature = "audio-pipewire", target_os = "linux"))
))]
pub(crate) fn cpal_devices() -> Result<Vec<OutputDeviceInfo>, PipelineError> {
    cpal_backend::list_output_devices()
}

#[cfg(feature = "audio-out")]
mod cpal_backend {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
    use std::sync::Arc;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use tracing::{info, warn};

    #[cfg(not(all(feature = "audio-pipewire", target_os = "linux")))]
    use super::OutputDeviceInfo;
    use super::{AudioOut, OutputSelection, PcmBlock, PipelineError};
    use crate::resample::Resampler;

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
        selection: OutputSelection,
        samples: Option<SyncSender<Vec<f32>>>,
        shutdown: Option<SyncSender<()>>,
        underruns: Arc<AtomicU64>,
        /// Present only when the device would not take the source's rate. `None` is both
        /// the common case and the fast path — no conversion, no allocation, no quality
        /// question to answer.
        resampler: Option<Resampler>,
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
        /// An output that has not opened a device yet, on the system default.
        #[must_use]
        pub fn new() -> Self {
            Self::with_selection(OutputSelection::SystemDefault)
        }

        /// An output that will open whatever `selection` names.
        #[must_use]
        pub fn with_selection(selection: OutputSelection) -> Self {
            Self {
                selection,
                samples: None,
                shutdown: None,
                underruns: Arc::new(AtomicU64::new(0)),
                resampler: None,
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
            // The thread reports the rate it actually opened at, which is not always the
            // one asked for — see `choose_rate`.
            let (ready_tx, ready_rx) = sync_channel::<Result<(u32, String), String>>(1);
            let underruns = Arc::clone(&self.underruns);
            let selection = self.selection.clone();

            std::thread::spawn(move || {
                let stream =
                    match open_stream(&selection, sample_rate, channels, samples_rx, underruns) {
                        Ok((s, opened_at, device)) => {
                            let _ = ready_tx.send(Ok((opened_at, device)));
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
                Ok(Ok((opened_at, device))) => {
                    self.resampler = if opened_at == sample_rate {
                        info!(%device, sample_rate, channels, "audio output started");
                        None
                    } else {
                        // Say it plainly. The original design refused to resample so that
                        // a pitch shift could never appear from nowhere; the answer to
                        // that concern is to convert *and name it*, not to play nothing.
                        info!(
                            %device,
                            source_rate = sample_rate,
                            device_rate = opened_at,
                            channels,
                            "audio output started; resampling to the device's rate (soxr)"
                        );
                        Some(Resampler::new(sample_rate, opened_at, channels)?)
                    };
                    self.samples = Some(samples_tx);
                    self.shutdown = Some(shutdown_tx);
                    Ok(())
                }
                Ok(Err(e)) => Err(PipelineError::Audio(e)),
                Err(_) => Err(PipelineError::Audio("audio thread died starting up".into())),
            }
        }

        fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
            // Convert before borrowing the sender: resampling needs `&mut self` and is
            // deliberately done here, on the decode thread, rather than in the device
            // callback where allocating would be a dropout.
            let converted = match self.resampler.as_mut() {
                Some(r) => Some(r.convert(block)?),
                None => None,
            };
            let Some(tx) = self.samples.as_ref() else {
                return Err(PipelineError::Audio("audio output not started".into()));
            };
            let payload = converted.unwrap_or_else(|| block.samples.clone());
            match tx.try_send(payload) {
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
            // Push the resampler's tail before dropping the queue, or the last few
            // milliseconds of every resampled session are left in the filter.
            if let (Some(r), Some(tx)) = (self.resampler.as_mut(), self.samples.as_ref()) {
                match r.flush() {
                    Ok(tail) if !tail.is_empty() => {
                        let _ = tx.try_send(tail);
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "audio output: resampler tail was lost"),
                }
            }
            self.resampler = None;
            self.samples = None;
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.try_send(());
            }
        }
    }

    /// The output devices cpal's host can list, by name. Same gate as `cpal_devices`,
    /// its only caller.
    #[cfg(not(all(feature = "audio-pipewire", target_os = "linux")))]
    pub(super) fn list_output_devices() -> Result<Vec<OutputDeviceInfo>, PipelineError> {
        let host = cpal::default_host();
        let devices = host
            .output_devices()
            .map_err(|e| PipelineError::Audio(format!("listing output devices: {e}")))?;
        Ok(devices
            .filter_map(|d| d.name().ok())
            .map(|name| OutputDeviceInfo {
                id: name.clone(),
                label: name,
            })
            .collect())
    }

    /// The device `selection` names, or the default.
    ///
    /// A named device that is not there falls back to the default *with a warning*
    /// rather than failing the session: the case this happens in is a USB DAC that was
    /// unplugged, and a panel that goes silent because its favourite device left is
    /// worse than one that plays from the wrong speakers and says so.
    fn pick_device(host: &cpal::Host, selection: &OutputSelection) -> Result<cpal::Device, String> {
        if let OutputSelection::Device(name) = selection {
            let found = host
                .output_devices()
                .map_err(|e| format!("listing output devices: {e}"))?
                .find(|d| d.name().is_ok_and(|n| n == *name));
            match found {
                Some(device) => return Ok(device),
                None => warn!(device = %name, "configured output device not found; using default"),
            }
        }
        host.default_output_device()
            .ok_or_else(|| "no default output device".to_owned())
    }

    /// The rate to open `device` at, given the source is at `wanted`.
    ///
    /// Asking for exactly what the sender sends is right on Linux and wrong on Windows,
    /// and the asymmetry is the whole reason this function exists. ALSA and PipeWire
    /// accept any rate and convert underneath. **WASAPI shared mode does not**: the
    /// endpoint has one fixed mix format, and a request for anything else fails with
    /// "the requested stream configuration is not supported by the device" — which is
    /// how a phone streaming 44.1 kHz aptX HD onto a 48 kHz panel paired, negotiated,
    /// decoded, and played to nothing at all.
    ///
    /// So: prefer the source's own rate whenever the device will take it (no conversion
    /// at all, which is always the best resampler), and otherwise pick a rate the device
    /// actually offers and convert into it.
    fn choose_rate(device: &cpal::Device, wanted: u32, channels: u16) -> Result<u32, String> {
        let ranges: Vec<_> = device
            .supported_output_configs()
            .map_err(|e| format!("querying output configs: {e}"))?
            .filter(|c| c.channels() == channels)
            .collect();
        if ranges.is_empty() {
            // A channel-count mismatch is a different failure and is not something a
            // resampler fixes, so it is named rather than folded into the rate story.
            return Err(format!(
                "device offers no {channels}-channel output configuration"
            ));
        }

        let wanted_sr = cpal::SampleRate(wanted);
        let supported = |sr: cpal::SampleRate| {
            ranges
                .iter()
                .any(|r| r.min_sample_rate() <= sr && sr <= r.max_sample_rate())
        };
        if supported(wanted_sr) {
            return Ok(wanted);
        }

        // The device's own default is the endpoint mix format on WASAPI — the one rate
        // guaranteed to open in shared mode — so it is tried before anything cleverer.
        if let Ok(default) = device.default_output_config() {
            if supported(default.sample_rate()) {
                return Ok(default.sample_rate().0);
            }
        }

        // Otherwise the offered rate closest to the source, which keeps the conversion
        // ratio small and avoids inventing bandwidth that was never there.
        ranges
            .iter()
            .map(|r| wanted.clamp(r.min_sample_rate().0, r.max_sample_rate().0))
            .min_by_key(|rate| rate.abs_diff(wanted))
            .ok_or_else(|| format!("device offers no usable rate near {wanted} Hz"))
    }

    /// Build and start the output stream. Runs on the audio thread.
    ///
    /// Returns the stream, the rate it actually opened at, and the *name* of the device
    /// it landed on.
    ///
    /// The name is not decoration. Under [`OutputSelection::SystemDefault`] the device is
    /// whatever the OS currently calls default, and this panel has two active render
    /// endpoints — the DELL panel over HDMI, which is the only real speaker, and a Realtek
    /// analog output with nothing plugged into it. A default pointed at the wrong one
    /// plays to silence and looks identical to working, so the identity has to reach the
    /// log (#106).
    fn open_stream(
        selection: &OutputSelection,
        sample_rate: u32,
        channels: u16,
        rx: Receiver<Vec<f32>>,
        underruns: Arc<AtomicU64>,
    ) -> Result<(cpal::Stream, u32, String), String> {
        let host = cpal::default_host();
        let device = pick_device(&host, selection)?;
        let name = device.name().unwrap_or_else(|_| "<unnamed>".to_owned());
        let opened_at = choose_rate(&device, sample_rate, channels)?;

        let config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(opened_at),
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
        Ok((stream, opened_at, name))
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
    fn the_factory_reads_the_selector_per_stream_not_at_creation() {
        // A device picked on the settings screen must reach the *next* session without
        // rebuilding the factory. All this can assert headlessly is that construction
        // succeeds either way; the dispatch itself is compile-time.
        let sel = OutputSelector::default();
        let factory = output_factory(sel.clone());
        let _first = factory();
        sel.set(OutputSelection::Device("dac".into()));
        let _second = factory();
    }

    #[test]
    fn mono_frames_are_counted_per_frame_not_per_sample() {
        let mut out = NullAudioOut::new();
        out.start(44_100, 1).unwrap();
        out.write(&block(44_100, 1, 100)).unwrap();
        assert_eq!(out.frames(), 100);
    }
}
