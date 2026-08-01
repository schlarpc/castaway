//! Sample-rate conversion, for when the device will not take what the sender sends.
//!
//! This exists because of a platform split that is invisible until it bites. ALSA and
//! PipeWire accept any rate and convert underneath, so on Linux the output device never
//! refuses. **WASAPI shared mode does not**: the endpoint has one fixed mix format —
//! essentially always 48 kHz — and `build_output_stream` at 44.1 kHz fails outright. A
//! phone that picks aptX HD at 44.1 kHz therefore paired, negotiated, decoded, and played
//! to nothing at all on the Windows panel, while the same code was fine on the dev box.
//!
//! So the conversion has to be ours, and it has to be good: this is the last hop before
//! the speakers, and a cheap resampler is audible as a dull top end on exactly the
//! high-bitrate codecs someone went out of their way to use.
//!
//! ## Why libswresample, and why not through `ffmpeg-next`
//!
//! [`crate::audio_decode::pcm_from_frame`] records that `swr` "rejects its own decoder's
//! output with `Input changed`", and converts by hand instead. That is true of the
//! **`ffmpeg-next` wrapper**, not of the library: `software::resampling::Context` is built
//! through the legacy `swr_alloc_set_opts` API, which takes an `int64` channel-layout
//! *mask*, and then compares that mask against frames carrying ffmpeg 7's new
//! `AVChannelLayout`. The comparison cannot succeed.
//!
//! `swr_alloc_set_opts2` — the layout-aware form — has no such problem, and it is reachable
//! through `ffmpeg-sys-next`, which is already a dependency for the hwaccel path and is
//! already used for `av_channel_layout_default` a few modules over. So this costs no new
//! dependency and no new blob.
//!
//! The engine is **soxr**, not swr's own. Both the Linux ffmpeg and the vendored Windows
//! build are compiled `--enable-libsoxr` (checked, not assumed — it is in the configuration
//! string of `swresample-5.dll` in the artifact), and soxr at 28-bit precision is
//! transparent where swr's default 32-tap filter is not.

// FFI, like every other module that talks to libav directly (ground rule 8): every
// `unsafe` below carries a `// SAFETY:` note naming the invariant it upholds.
#![allow(unsafe_code)]

use ffmpeg_sys_next as sys;

use crate::audio_decode::PcmBlock;
use crate::error::PipelineError;

/// soxr precision in bits. 20 is soxr's "high quality" default; 28 is its very-high
/// setting, and the cost is a few percent of one core on a box that is otherwise idle.
const SOXR_PRECISION: f64 = 28.0;

/// An open libswresample conversion, fixed to one input and output shape.
///
/// Interleaved `f32` in, interleaved `f32` out — the format [`PcmBlock`] already uses, so
/// the only thing being changed is the rate.
pub struct Resampler {
    ctx: *mut sys::SwrContext,
    from_hz: u32,
    to_hz: u32,
    channels: u16,
}

// SAFETY: `SwrContext` is not internally synchronised, and this type never shares it —
// every use goes through `&mut self`, the pointer is never copied out, and `Drop` is the
// only other toucher. Moving that exclusive ownership between threads is sound, which is
// what `Send` claims. It is deliberately not `Sync`.
unsafe impl Send for Resampler {}

impl Resampler {
    /// Open a converter from `from_hz` to `to_hz` at `channels` channels.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if libswresample refuses the shape or cannot initialise —
    /// which includes an ffmpeg built without soxr, and should, because silently falling
    /// back to a worse filter is the kind of thing nobody ever notices twice.
    pub fn new(from_hz: u32, to_hz: u32, channels: u16) -> Result<Self, PipelineError> {
        let rate_in = i32::try_from(from_hz)
            .map_err(|_| PipelineError::Audio(format!("absurd input rate {from_hz}")))?;
        let rate_out = i32::try_from(to_hz)
            .map_err(|_| PipelineError::Audio(format!("absurd output rate {to_hz}")))?;
        let ch = i32::from(channels.max(1));

        let mut ctx: *mut sys::SwrContext = std::ptr::null_mut();
        // SAFETY: both layouts are stack-owned and zeroed before being filled by
        // `av_channel_layout_default`, which is the documented way to build a default
        // layout for a channel count. `swr_alloc_set_opts2` copies what it needs and
        // writes the new context through `ctx`, which is a valid out-pointer.
        let rc = unsafe {
            let mut layout_in: sys::AVChannelLayout = std::mem::zeroed();
            let mut layout_out: sys::AVChannelLayout = std::mem::zeroed();
            sys::av_channel_layout_default(std::ptr::addr_of_mut!(layout_in), ch);
            sys::av_channel_layout_default(std::ptr::addr_of_mut!(layout_out), ch);
            let rc = sys::swr_alloc_set_opts2(
                std::ptr::addr_of_mut!(ctx),
                std::ptr::addr_of!(layout_out),
                sys::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                rate_out,
                std::ptr::addr_of!(layout_in),
                sys::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                rate_in,
                0,
                std::ptr::null_mut(),
            );
            sys::av_channel_layout_uninit(std::ptr::addr_of_mut!(layout_in));
            sys::av_channel_layout_uninit(std::ptr::addr_of_mut!(layout_out));
            rc
        };
        if rc < 0 || ctx.is_null() {
            return Err(PipelineError::Audio(format!(
                "swr_alloc_set_opts2 refused {from_hz} -> {to_hz} Hz at {channels}ch (rc {rc})"
            )));
        }

        // Wrap immediately so every early return below frees the context.
        let this = Self {
            ctx,
            from_hz,
            to_hz,
            channels: channels.max(1),
        };

        // SAFETY: `ctx` is a live context that has not been initialised yet, which is the
        // only window in which these options may be set. Both names are libswresample's
        // own; a build without soxr rejects the first and is reported rather than
        // silently downgraded.
        let opts = unsafe {
            let obj = this.ctx.cast::<std::ffi::c_void>();
            let engine = sys::av_opt_set(obj, c"resampler".as_ptr(), c"soxr".as_ptr(), 0);
            let precision = sys::av_opt_set_double(obj, c"precision".as_ptr(), SOXR_PRECISION, 0);
            (engine, precision)
        };
        if opts.0 < 0 {
            return Err(PipelineError::Audio(format!(
                "this ffmpeg has no soxr resampler (rc {})",
                opts.0
            )));
        }
        if opts.1 < 0 {
            return Err(PipelineError::Audio(format!(
                "soxr rejected a precision of {SOXR_PRECISION} bits (rc {})",
                opts.1
            )));
        }

        // SAFETY: `ctx` is live, fully configured, and not yet initialised.
        let rc = unsafe { sys::swr_init(this.ctx) };
        if rc < 0 {
            return Err(PipelineError::Audio(format!(
                "swr_init failed for {from_hz} -> {to_hz} Hz (rc {rc})"
            )));
        }
        Ok(this)
    }

    /// The rate this converts from.
    #[must_use]
    pub const fn from_hz(&self) -> u32 {
        self.from_hz
    }

    /// The rate this converts to.
    #[must_use]
    pub const fn to_hz(&self) -> u32 {
        self.to_hz
    }

    /// Convert one block, returning interleaved samples at [`Self::to_hz`].
    ///
    /// The result is not a fixed multiple of the input: a resampler carries filter state
    /// across calls, so an output block may be a sample longer or shorter than the ratio
    /// implies. That is correct and callers must not "fix" it by padding — the samples
    /// come out even over any run of blocks.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if the block's shape does not match what this was opened
    /// for, or libswresample fails the conversion.
    pub fn convert(&mut self, block: &PcmBlock) -> Result<Vec<f32>, PipelineError> {
        if block.channels.max(1) != self.channels {
            return Err(PipelineError::Audio(format!(
                "resampler opened for {}ch, got a {}ch block",
                self.channels, block.channels
            )));
        }
        if block.sample_rate != self.from_hz {
            return Err(PipelineError::Audio(format!(
                "resampler opened for {} Hz, got a {} Hz block",
                self.from_hz, block.sample_rate
            )));
        }
        self.convert_samples(&block.samples)
    }

    /// Drain the filter's delay line at the end of a stream.
    ///
    /// A polyphase resampler holds roughly half its filter length in hand at all times, so
    /// without this the last few hundred frames of every session are simply lost. At soxr's
    /// 28-bit precision that is on the order of 15 ms — inaudible as a *gap*, but it is the
    /// difference between a stream that conserves its samples and one that quietly does
    /// not, and the sample-conservation tests are what would otherwise have to be loosened
    /// until they stopped meaning anything.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if libswresample fails the conversion.
    pub fn flush(&mut self) -> Result<Vec<f32>, PipelineError> {
        let channels = usize::from(self.channels);
        // SAFETY: `ctx` is initialised; asking for the output owed on zero further input
        // is exactly what sizes the drain.
        let out_capacity = unsafe { sys::swr_get_out_samples(self.ctx, 0) };
        if out_capacity <= 0 {
            return Ok(Vec::new());
        }
        let mut out = vec![0f32; usize::try_from(out_capacity).unwrap_or(0) * channels];
        // SAFETY: a null input plane is libswresample's documented way to say "no more
        // input" and is what makes it emit the tail rather than wait for more. `out` has
        // room for `out_capacity` frames.
        let produced = unsafe {
            let mut out_ptr = out.as_mut_ptr().cast::<u8>();
            sys::swr_convert(
                self.ctx,
                std::ptr::addr_of_mut!(out_ptr),
                out_capacity,
                std::ptr::null_mut(),
                0,
            )
        };
        if produced < 0 {
            return Err(PipelineError::Audio(format!(
                "swr_convert failed to drain (rc {produced})"
            )));
        }
        out.truncate(usize::try_from(produced).unwrap_or(0) * channels);
        Ok(out)
    }

    /// The same, on bare interleaved samples.
    ///
    /// # Errors
    /// [`PipelineError::Audio`] if libswresample fails the conversion.
    pub fn convert_samples(&mut self, samples: &[f32]) -> Result<Vec<f32>, PipelineError> {
        let channels = usize::from(self.channels);
        let in_frames = samples.len() / channels;
        let in_count = i32::try_from(in_frames)
            .map_err(|_| PipelineError::Audio("audio block too large to resample".into()))?;

        // `swr_get_out_samples` accounts for samples still held in the filter's delay
        // line, so this is an upper bound rather than a ratio — sizing by ratio alone
        // truncates the first block of every session.
        // SAFETY: `ctx` is initialised and `in_count` is non-negative.
        let out_capacity = unsafe { sys::swr_get_out_samples(self.ctx, in_count) };
        if out_capacity < 0 {
            return Err(PipelineError::Audio(format!(
                "swr_get_out_samples failed (rc {out_capacity})"
            )));
        }
        let out_frames = usize::try_from(out_capacity).unwrap_or(0);
        let mut out = vec![0f32; out_frames * channels];

        // SAFETY: both buffers are interleaved `f32` matching the formats the context was
        // opened with, so libswresample reads exactly one input plane and writes one
        // output plane. `out` has room for `out_frames` frames, which is the bound just
        // obtained, and `samples` holds `in_frames`. Empty input is still valid: it
        // drains the delay line rather than reading the pointer.
        let produced = unsafe {
            let mut in_ptr = samples.as_ptr().cast::<u8>();
            let mut out_ptr = out.as_mut_ptr().cast::<u8>();
            sys::swr_convert(
                self.ctx,
                std::ptr::addr_of_mut!(out_ptr),
                out_capacity,
                std::ptr::addr_of_mut!(in_ptr),
                in_count,
            )
        };
        if produced < 0 {
            return Err(PipelineError::Audio(format!(
                "swr_convert failed (rc {produced})"
            )));
        }
        out.truncate(usize::try_from(produced).unwrap_or(0) * channels);
        Ok(out)
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        // SAFETY: `ctx` was allocated by `swr_alloc_set_opts2` and is freed exactly once —
        // `Resampler` is not `Clone`, so no other value holds this pointer.
        unsafe { sys::swr_free(std::ptr::addr_of_mut!(self.ctx)) };
    }
}

impl std::fmt::Debug for Resampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resampler")
            .field("from_hz", &self.from_hz)
            .field("to_hz", &self.to_hz)
            .field("channels", &self.channels)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A stereo sine at `hz`, `seconds` long, as an interleaved block.
    fn sine(rate: u32, hz: f32, seconds: f32) -> PcmBlock {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = (rate as f32 * seconds) as usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            #[allow(clippy::cast_precision_loss)]
            let t = i as f32 / rate as f32;
            let v = (std::f32::consts::TAU * hz * t).sin() * 0.5;
            samples.push(v);
            samples.push(v);
        }
        PcmBlock {
            sample_rate: rate,
            channels: 2,
            samples,
            pts: std::time::Duration::ZERO,
        }
    }

    /// Zero crossings, which is a frequency measurement that needs no FFT.
    fn crossings(samples: &[f32], channels: usize) -> usize {
        samples
            .iter()
            .step_by(channels)
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| (w[0].is_sign_negative()) != (w[1].is_sign_negative()))
            .count()
    }

    #[test]
    fn forty_four_one_becomes_forty_eight_without_changing_the_note() {
        // The exact conversion the Windows panel needs: a phone's 44.1 kHz aptX HD onto a
        // WASAPI endpoint fixed at 48 kHz.
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        let input = sine(44_100, 1_000.0, 1.0);
        let out = r.convert(&input).unwrap();

        // Length follows the ratio, within the filter's delay.
        let out_frames = out.len() / 2;
        let expected = 48_000f64;
        #[allow(clippy::cast_precision_loss)]
        let ratio = out_frames as f64 / expected;
        assert!(
            (0.98..=1.02).contains(&ratio),
            "expected ~{expected} frames, got {out_frames}"
        );

        // It is still a 1 kHz tone: 2 crossings per cycle, ~2000 over a second. A
        // resampler that shifted pitch — the classic "just reinterpret the buffer" bug —
        // lands near 2176 (1000 * 48/44.1 * 2) and fails here.
        let c = crossings(&out, 2);
        assert!(
            (1_960..=2_040).contains(&c),
            "expected ~2000 zero crossings for a 1 kHz tone, got {c}"
        );

        // …and it is audible, not a correctly-shaped block of zeros.
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((0.45..=0.55).contains(&peak), "peak was {peak}");
        assert!(out.iter().all(|s| s.is_finite()), "produced NaN or inf");
    }

    #[test]
    fn a_stream_of_small_blocks_conserves_samples() {
        // A2DP arrives in small blocks, not one-second buffers, and a resampler that
        // resets its filter state per call would both click and lose samples.
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        let mut produced = 0usize;
        for _ in 0..100 {
            let block = sine(44_100, 440.0, 0.01); // 441 frames each
            produced += r.convert(&block).unwrap().len() / 2;
        }
        produced += r.flush().unwrap().len() / 2;
        // 100 * 441 input frames = exactly 1.0 s in, so 48000 out once the delay line is
        // drained. The tight bound is the point: it is what catches a resampler that
        // resets its filter state per call, which would both click and lose samples.
        assert!(
            (47_990..=48_010).contains(&produced),
            "expected ~48000 frames across the stream, got {produced}"
        );
    }

    #[test]
    fn the_other_direction_works_too() {
        // 48 -> 44.1 is what a device fixed at 44.1 would need, and is not symmetric in
        // the filter design, so it gets its own case.
        let mut r = Resampler::new(48_000, 44_100, 2).unwrap();
        let mut out = r.convert(&sine(48_000, 1_000.0, 1.0)).unwrap();
        out.extend(r.flush().unwrap());
        let frames = out.len() / 2;
        assert!((44_090..=44_110).contains(&frames), "got {frames} frames");
        let c = crossings(&out, 2);
        assert!((1_960..=2_040).contains(&c), "got {c} crossings");
    }

    #[test]
    fn mono_is_not_a_special_case() {
        let mut r = Resampler::new(44_100, 48_000, 1).unwrap();
        let block = PcmBlock {
            sample_rate: 44_100,
            channels: 1,
            samples: vec![0.25; 4_410],
            pts: std::time::Duration::ZERO,
        };
        let mut out = r.convert(&block).unwrap();
        out.extend(r.flush().unwrap());
        assert!((4_795..=4_805).contains(&out.len()), "got {}", out.len());
    }

    #[test]
    fn a_block_of_the_wrong_shape_is_refused_rather_than_misread() {
        // Reinterpreting a 1ch block as 2ch halves the pitch and is exactly the kind of
        // thing that would otherwise be discovered by ear.
        let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
        let mono = PcmBlock {
            sample_rate: 44_100,
            channels: 1,
            samples: vec![0.0; 441],
            pts: std::time::Duration::ZERO,
        };
        assert!(matches!(r.convert(&mono), Err(PipelineError::Audio(_))));

        let wrong_rate = sine(48_000, 440.0, 0.01);
        assert!(matches!(
            r.convert(&wrong_rate),
            Err(PipelineError::Audio(_))
        ));
    }
}
