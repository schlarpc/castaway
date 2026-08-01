//! The AAC encoder behind the stream's audio track.
//!
//! The same shape as [`super::encoder`] and for the same reason — a candidate list probed
//! at runtime rather than a codec chosen at build time — but a much shorter list, because
//! libavcodec's own `aac` encoder is native, LGPL, and present in every build of ffmpeg
//! this project links, including the one the Windows artifact ships. There is no hardware
//! path worth reaching for: AAC at 128 kbit/s is a rounding error next to the video.
//!
//! Two sample formats are tried per candidate, because that is the whole of the difference
//! between the encoders here: libavcodec's native one takes planar float, Fraunhofer's
//! takes interleaved 16-bit, and asking which is which through the deprecated `sample_fmts`
//! array is more code than opening it twice.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::ffi::{c_int, CString};

use ffmpeg_sys_next as sys;

use super::audio::CHANNELS;
use super::fmp4::{AacConfig, Sample};
use crate::av::av_error;
use crate::error::PipelineError;

/// Encoders to try, best first.
///
/// `aac` is libavcodec's own and is always there. `libfdk_aac` is better at low bitrates
/// and is nonfree, so it is present only in a build somebody deliberately made — if it is
/// there, it was put there to be used.
const CANDIDATES: &[&str] = &["libfdk_aac", "aac"];

/// How the encoder wants its samples laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One plane per channel, 32-bit float. libavcodec's native AAC encoder.
    PlanarFloat,
    /// One interleaved plane, signed 16-bit. Fraunhofer's.
    InterleavedS16,
}

impl Layout {
    const fn sample_format(self) -> sys::AVSampleFormat {
        match self {
            Self::PlanarFloat => sys::AVSampleFormat::AV_SAMPLE_FMT_FLTP,
            Self::InterleavedS16 => sys::AVSampleFormat::AV_SAMPLE_FMT_S16,
        }
    }
}

/// An open AAC encoder.
pub struct AacEncoder {
    ctx: *mut sys::AVCodecContext,
    frame: *mut sys::AVFrame,
    packet: *mut sys::AVPacket,
    name: String,
    layout: Layout,
    config: AacConfig,
    /// Samples per channel in one coded frame — 1024 for AAC-LC, and the unit everything
    /// upstream of here counts in.
    frame_size: usize,
    sample_rate: u32,
    /// The next frame's timestamp, in samples.
    pts: i64,
}

// SAFETY: identical reasoning to `super::encoder::H264Encoder` — every pointer is owned
// solely by this struct, created and destroyed by it, and only ever dereferenced through
// `&mut self`. It lives on the encode thread. Not `Sync`.
unsafe impl Send for AacEncoder {}

impl std::fmt::Debug for AacEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AacEncoder")
            .field("encoder", &self.name)
            .field("frame_size", &self.frame_size)
            .finish_non_exhaustive()
    }
}

impl AacEncoder {
    /// Open the best AAC encoder this box has.
    ///
    /// # Errors
    /// [`PipelineError::Encode`] listing what each candidate said.
    pub fn open(sample_rate: u32, bitrate: u32) -> Result<Self, PipelineError> {
        let mut refused = Vec::new();
        for name in CANDIDATES {
            for layout in [Layout::PlanarFloat, Layout::InterleavedS16] {
                match Self::open_one(name, layout, sample_rate, bitrate) {
                    Ok(encoder) => {
                        tracing::info!(
                            encoder = name,
                            ?layout,
                            sample_rate,
                            frame_size = encoder.frame_size,
                            "output stream audio encoder opened"
                        );
                        return Ok(encoder);
                    }
                    Err(e) => refused.push(format!("{name}/{layout:?}: {e}")),
                }
            }
        }
        Err(PipelineError::Encode(format!(
            "no AAC encoder would open ({})",
            refused.join("; ")
        )))
    }

    fn open_one(
        name: &str,
        layout: Layout,
        sample_rate: u32,
        bitrate: u32,
    ) -> Result<Self, PipelineError> {
        let mut encoder = Self {
            ctx: std::ptr::null_mut(),
            frame: std::ptr::null_mut(),
            packet: std::ptr::null_mut(),
            name: name.to_string(),
            layout,
            config: AacConfig::default(),
            frame_size: 1024,
            sample_rate,
            pts: 0,
        };
        // SAFETY: freshly constructed with every pointer null, which is what `build`
        // requires and what `Drop` tolerates if this returns early.
        unsafe { encoder.build(bitrate) }?;
        Ok(encoder)
    }

    /// # Safety
    /// Must be called exactly once, on a `Self` whose pointers are all null.
    unsafe fn build(&mut self, bitrate: u32) -> Result<(), PipelineError> {
        let name = CString::new(self.name.as_str())
            .map_err(|_| PipelineError::Encode("encoder name is not a C string".into()))?;
        // SAFETY: `name` is NUL-terminated and outlives the call.
        let codec = unsafe { sys::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(PipelineError::Encode("not built into this ffmpeg".into()));
        }
        // SAFETY: `codec` is a static descriptor libavcodec just handed back.
        self.ctx = unsafe { sys::avcodec_alloc_context3(codec) };
        if self.ctx.is_null() {
            return Err(PipelineError::Encode("could not allocate a context".into()));
        }
        // SAFETY: a live, unopened context; every field below is one `avcodec_open2` reads.
        // `av_channel_layout_default` is libavutil's own way to build a layout for a
        // channel count, and it initialises the union it writes into.
        unsafe {
            let ctx = &mut *self.ctx;
            ctx.sample_rate = self.sample_rate as c_int;
            ctx.sample_fmt = self.layout.sample_format();
            ctx.bit_rate = i64::from(bitrate);
            // In samples, matching the timescale the audio track is written on — so a
            // frame is exactly `frame_size` ticks and nothing rounds.
            ctx.time_base = sys::AVRational {
                num: 1,
                den: self.sample_rate as c_int,
            };
            sys::av_channel_layout_default(
                std::ptr::addr_of_mut!(ctx.ch_layout),
                c_int::from(CHANNELS),
            );
            // The `AudioSpecificConfig` out of band, which is where `esds` wants it —
            // and, on this encoder, the difference between raw AAC frames and ADTS-framed
            // ones. ADTS inside an `mp4a` track is a stream every player rejects.
            ctx.flags |= sys::AV_CODEC_FLAG_GLOBAL_HEADER as c_int;
        }

        // SAFETY: context and codec are both live and the context has not been opened.
        let rc = unsafe { sys::avcodec_open2(self.ctx, codec, std::ptr::null_mut()) };
        if rc < 0 {
            return Err(PipelineError::Encode(format!(
                "avcodec_open2 failed ({})",
                av_error(rc)
            )));
        }

        // SAFETY: an opened encoder has filled `extradata` and `frame_size`, or left the
        // pointer null, which the guard catches.
        unsafe {
            let ctx = &*self.ctx;
            if ctx.extradata.is_null() || ctx.extradata_size <= 0 {
                return Err(PipelineError::Encode(
                    "opened but published no AudioSpecificConfig".into(),
                ));
            }
            self.config = AacConfig {
                asc: std::slice::from_raw_parts(ctx.extradata, ctx.extradata_size as usize)
                    .to_vec(),
            };
            if ctx.frame_size > 0 {
                self.frame_size = ctx.frame_size as usize;
            }
        }

        // SAFETY: plain allocations, checked for null before use. The frame owns its own
        // buffers here — unlike the video path, the samples need converting on the way in,
        // so there is nothing to alias.
        unsafe {
            self.frame = sys::av_frame_alloc();
            self.packet = sys::av_packet_alloc();
            if self.frame.is_null() || self.packet.is_null() {
                return Err(PipelineError::Encode("out of memory".into()));
            }
            let frame = &mut *self.frame;
            frame.format = self.layout.sample_format() as c_int;
            frame.nb_samples = self.frame_size as c_int;
            frame.sample_rate = self.sample_rate as c_int;
            sys::av_channel_layout_default(
                std::ptr::addr_of_mut!(frame.ch_layout),
                c_int::from(CHANNELS),
            );
            let rc = sys::av_frame_get_buffer(self.frame, 0);
            if rc < 0 {
                return Err(PipelineError::Encode(format!(
                    "no frame buffer ({})",
                    av_error(rc)
                )));
            }
        }
        Ok(())
    }

    /// Which encoder opened.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `AudioSpecificConfig`, for the init segment's `esds`.
    #[must_use]
    pub const fn config(&self) -> &AacConfig {
        &self.config
    }

    /// Samples per channel in one coded frame.
    #[must_use]
    pub const fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// How many samples of input the encoder swallows before its output catches up.
    ///
    /// Every AAC encoder has one — the filter bank needs a frame of lookahead — and it is
    /// why an untrimmed AAC track lags its video by twenty milliseconds or so. The caller
    /// cancels it by discarding this much of its *input*, which costs nothing and needs no
    /// edit list.
    #[must_use]
    pub fn initial_padding(&self) -> usize {
        // SAFETY: `ctx` is a live opened context and this is a plain scalar field.
        let padding = unsafe { (*self.ctx).initial_padding };
        usize::try_from(padding.max(0)).unwrap_or(0)
    }

    /// Encode exactly one frame of interleaved stereo.
    ///
    /// # Errors
    /// [`PipelineError::Encode`] if the slice is the wrong length or libavcodec refuses.
    pub fn encode(&mut self, stereo: &[f32]) -> Result<Vec<Sample>, PipelineError> {
        let channels = usize::from(CHANNELS);
        if stereo.len() != self.frame_size * channels {
            return Err(PipelineError::Encode(format!(
                "encoder takes {} samples a frame, got {}",
                self.frame_size * channels,
                stereo.len()
            )));
        }
        // SAFETY: the frame is live and owns its buffers; `av_frame_make_writable` is what
        // guarantees libavcodec is not still holding the previous contents.
        let rc = unsafe { sys::av_frame_make_writable(self.frame) };
        if rc < 0 {
            return Err(PipelineError::Encode(format!(
                "frame not writable ({})",
                av_error(rc)
            )));
        }
        // SAFETY: the planes were sized by `av_frame_get_buffer` for `frame_size` samples
        // in this format, and every write below is bounded by that count.
        unsafe {
            let frame = &mut *self.frame;
            match self.layout {
                Layout::PlanarFloat => {
                    for channel in 0..channels {
                        let plane = frame.data[channel].cast::<f32>();
                        for (i, sample) in stereo.iter().skip(channel).step_by(channels).enumerate()
                        {
                            *plane.add(i) = *sample;
                        }
                    }
                }
                Layout::InterleavedS16 => {
                    let plane = frame.data[0].cast::<i16>();
                    for (i, sample) in stereo.iter().enumerate() {
                        // Clamped before scaling: a sum of two sessions can exceed unity,
                        // and wrapping a float that does is a click rather than a clip.
                        *plane.add(i) = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
                    }
                }
            }
            frame.pts = self.pts;
        }
        self.pts += self.frame_size as i64;

        // SAFETY: an opened encoder and a frame it accepts.
        let rc = unsafe { sys::avcodec_send_frame(self.ctx, self.frame) };
        if rc < 0 && rc != sys::AVERROR(sys::EAGAIN) {
            return Err(PipelineError::Encode(format!(
                "send_frame failed ({})",
                av_error(rc)
            )));
        }
        let mut out = Vec::new();
        self.drain(&mut out)?;
        Ok(out)
    }

    /// Push out whatever the encoder is still holding.
    pub fn flush(&mut self) -> Vec<Sample> {
        // SAFETY: an opened encoder; a null frame is the documented flush signal.
        unsafe { sys::avcodec_send_frame(self.ctx, std::ptr::null()) };
        let mut out = Vec::new();
        if let Err(e) = self.drain(&mut out) {
            tracing::debug!(error = %e, "aac flush");
        }
        out
    }

    fn drain(&mut self, out: &mut Vec<Sample>) -> Result<(), PipelineError> {
        loop {
            // SAFETY: an opened encoder and a live packet, which the call unrefs itself
            // before writing.
            let rc = unsafe { sys::avcodec_receive_packet(self.ctx, self.packet) };
            if rc == sys::AVERROR(sys::EAGAIN) || rc == sys::AVERROR_EOF {
                return Ok(());
            }
            if rc < 0 {
                return Err(PipelineError::Encode(format!(
                    "receive_packet failed ({})",
                    av_error(rc)
                )));
            }
            // SAFETY: a successful receive leaves a packet with `size` bytes at `data`.
            let data = unsafe {
                let packet = &*self.packet;
                std::slice::from_raw_parts(packet.data, packet.size.max(0) as usize).to_vec()
            };
            if !data.is_empty() {
                out.push(Sample {
                    data,
                    // In the audio track's own timescale, which is its sample rate — so
                    // this is exact and no rounding accumulates over a long stream.
                    duration: u32::try_from(self.frame_size).unwrap_or(1024),
                    // Every AAC frame is independently decodable. Saying otherwise would
                    // make a player refuse to start a segment on one, which is every
                    // segment.
                    keyframe: true,
                });
            }
            // SAFETY: the packet is live and this releases the reference just taken.
            unsafe { sys::av_packet_unref(self.packet) };
        }
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: each tolerates a null pointer and nulls what it frees, and every pointer
        // is owned solely by this struct.
        unsafe {
            sys::av_packet_free(&raw mut self.packet);
            sys::av_frame_free(&raw mut self.frame);
            sys::avcodec_free_context(&raw mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::audio::RATE;
    use super::*;

    fn encoder() -> Option<AacEncoder> {
        match AacEncoder::open(RATE, 128_000) {
            Ok(e) => Some(e),
            Err(e) => {
                eprintln!("no AAC encoder here, skipping: {e}");
                None
            }
        }
    }

    /// A frame of a 440 Hz tone, so the encoder has something other than silence to chew
    /// on — a pure-silence stream compresses to nearly nothing and would not prove much.
    fn tone(frames: usize, phase: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = (i + phase) as f32 / RATE as f32;
                let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn an_encoder_publishes_its_audio_specific_config() {
        // The init segment's `esds` is written from this, and a track whose `esds` carries
        // nothing is one no decoder will configure itself from.
        let Some(encoder) = encoder() else { return };
        assert!(!encoder.config().asc.is_empty());
        assert_eq!(encoder.frame_size(), 1024, "AAC-LC");
        assert_eq!(
            encoder.config().codec_string(),
            "mp4a.40.2",
            "AAC-LC is object type 2"
        );
    }

    #[test]
    fn frames_come_out_and_every_one_is_a_sync_sample() {
        // Every AAC frame is independently decodable. Marking them otherwise makes a
        // player refuse to start on one, and every segment starts on one.
        let Some(mut encoder) = encoder() else { return };
        let size = encoder.frame_size();
        let mut samples = Vec::new();
        for i in 0..20 {
            samples.extend(encoder.encode(&tone(size, i * size)).unwrap());
        }
        samples.extend(encoder.flush());
        assert!(
            samples.len() >= 18,
            "twenty frames in, {} out",
            samples.len()
        );
        assert!(samples.iter().all(|s| s.keyframe));
        assert!(samples.iter().all(|s| s.duration == size as u32));
        assert!(samples.iter().all(|s| !s.data.is_empty()));
    }

    #[test]
    fn frames_are_raw_aac_not_adts() {
        // An ADTS-framed sample inside an `mp4a` track is a stream every player rejects,
        // and the only thing standing between us and that is `AV_CODEC_FLAG_GLOBAL_HEADER`.
        // ADTS opens with a twelve-bit sync word, so it is unmistakable.
        let Some(mut encoder) = encoder() else { return };
        let size = encoder.frame_size();
        let mut samples = Vec::new();
        for i in 0..8 {
            samples.extend(encoder.encode(&tone(size, i * size)).unwrap());
        }
        samples.extend(encoder.flush());
        for sample in &samples {
            let sync = u16::from_be_bytes([sample.data[0], sample.data[1]]) >> 4;
            assert_ne!(sync, 0xfff, "this frame is ADTS-framed");
        }
    }

    #[test]
    fn a_frame_of_the_wrong_length_is_refused() {
        let Some(mut encoder) = encoder() else { return };
        let err = encoder.encode(&[0.0; 64]).unwrap_err();
        assert!(matches!(err, PipelineError::Encode(_)), "{err:?}");
    }
}
