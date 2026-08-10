//! The Opus encoder behind the remote track's audio (#259).
//!
//! The same shape as [`super::aac`] and for the same reason — an encoder probed at
//! runtime, wrapped thin over `ffmpeg-sys-next` — but a different consumer: WebRTC.
//! Browsers negotiate Opus and nothing else worth having (the alternatives are G.711),
//! while the HLS duplicate's `mp4a` track wants AAC, so the one mix is encoded twice and
//! the two codecs never meet. `libopus` is the only candidate because it is the encoder
//! ffmpeg builds actually carry: libavcodec's native `opus` encoder is still marked
//! experimental, and the pinned BtbN Windows build was inspected for #259 —
//! `--enable-libopus` is in its configuration string, so both platforms have this.
//!
//! Two sample formats are tried, as in [`super::aac`], because that is the whole of the
//! variation: `libopus` takes packed float or packed 16-bit, and asking through the
//! deprecated `sample_fmts` array is more code than opening it twice.
//!
//! There is no priming here, unlike the AAC path. AAC discards `initial_padding` of its
//! *input* so coded frame *k* lands at track position `k × frame_size` — an fMP4 track
//! needs that, because the container addresses samples by position. RTP does not: a
//! WebRTC receiver runs a jitter buffer that swallows Opus's few milliseconds of
//! lookahead without ever knowing it existed, and there is no container to need an edit
//! list.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::ffi::{c_int, CString};

use ffmpeg_sys_next as sys;

use super::audio::CHANNELS;
use crate::av::av_error;
use crate::error::PipelineError;

/// Encoders to try, best (and only) first. A list, not a string, so a second candidate
/// (the native `opus`, if it ever leaves experimental) is one line.
const CANDIDATES: &[&str] = &["libopus"];

/// How the encoder wants its samples laid out. `libopus` is packed-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One interleaved plane, 32-bit float — what the mix already is.
    PackedFloat,
    /// One interleaved plane, signed 16-bit.
    PackedS16,
}

impl Layout {
    const fn sample_format(self) -> sys::AVSampleFormat {
        match self {
            Self::PackedFloat => sys::AVSampleFormat::AV_SAMPLE_FMT_FLT,
            Self::PackedS16 => sys::AVSampleFormat::AV_SAMPLE_FMT_S16,
        }
    }
}

/// One coded Opus frame, ready for the RTP payloader.
///
/// Not [`super::fmp4::Sample`]: nothing here goes in a container, every Opus frame is
/// independently decodable, and the only timing a packetizer needs is the duration.
#[derive(Debug, Clone)]
pub struct OpusFrame {
    /// The Opus packet, exactly as RFC 7587 wants it on the wire.
    pub data: Vec<u8>,
    /// Samples per channel this frame covers — the RTP timestamp step at 48 kHz.
    pub samples: u32,
}

/// An open Opus encoder.
pub struct OpusEncoder {
    ctx: *mut sys::AVCodecContext,
    frame: *mut sys::AVFrame,
    packet: *mut sys::AVPacket,
    name: String,
    layout: Layout,
    /// Samples per channel in one coded frame — 960 (20 ms) at 48 kHz unless the
    /// encoder says otherwise, and the unit the chunker feeds in.
    frame_size: usize,
    sample_rate: u32,
    /// The next frame's timestamp, in samples.
    pts: i64,
}

// SAFETY: identical reasoning to `super::aac::AacEncoder` — every pointer is owned solely
// by this struct, created and destroyed by it, and only ever dereferenced through
// `&mut self`. It lives on the encode thread. Not `Sync`.
unsafe impl Send for OpusEncoder {}

impl std::fmt::Debug for OpusEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusEncoder")
            .field("encoder", &self.name)
            .field("frame_size", &self.frame_size)
            .finish_non_exhaustive()
    }
}

impl OpusEncoder {
    /// Open the best Opus encoder this box has.
    ///
    /// # Errors
    /// [`PipelineError::Encode`] listing what each candidate said.
    pub fn open(sample_rate: u32, bitrate: u32) -> Result<Self, PipelineError> {
        let mut refused = Vec::new();
        for name in CANDIDATES {
            for layout in [Layout::PackedFloat, Layout::PackedS16] {
                match Self::open_one(name, layout, sample_rate, bitrate) {
                    Ok(encoder) => {
                        tracing::info!(
                            encoder = name,
                            ?layout,
                            sample_rate,
                            frame_size = encoder.frame_size,
                            "remote audio encoder opened"
                        );
                        return Ok(encoder);
                    }
                    Err(e) => refused.push(format!("{name}/{layout:?}: {e}")),
                }
            }
        }
        Err(PipelineError::Encode(format!(
            "no Opus encoder would open ({})",
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
            frame_size: 960,
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
        // SAFETY: a live, unopened context; every field below is one `avcodec_open2`
        // reads. `av_channel_layout_default` initialises the union it writes into.
        unsafe {
            let ctx = &mut *self.ctx;
            ctx.sample_rate = self.sample_rate as c_int;
            ctx.sample_fmt = self.layout.sample_format();
            ctx.bit_rate = i64::from(bitrate);
            // In samples, so a frame is exactly `frame_size` ticks and nothing rounds.
            ctx.time_base = sys::AVRational {
                num: 1,
                den: self.sample_rate as c_int,
            };
            sys::av_channel_layout_default(
                std::ptr::addr_of_mut!(ctx.ch_layout),
                c_int::from(CHANNELS),
            );
        }

        // SAFETY: context and codec are both live and the context has not been opened.
        let rc = unsafe { sys::avcodec_open2(self.ctx, codec, std::ptr::null_mut()) };
        if rc < 0 {
            return Err(PipelineError::Encode(format!(
                "avcodec_open2 failed ({})",
                av_error(rc)
            )));
        }

        // SAFETY: an opened encoder has filled `frame_size` (libopus always does — the
        // default frame duration is 20 ms).
        unsafe {
            let ctx = &*self.ctx;
            if ctx.frame_size > 0 {
                self.frame_size = ctx.frame_size as usize;
            }
        }

        // SAFETY: plain allocations, checked for null before use; the frame owns its own
        // buffers because the samples are converted on the way in.
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

    /// Samples per channel in one coded frame.
    #[must_use]
    pub const fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Encode exactly one frame of interleaved stereo.
    ///
    /// # Errors
    /// [`PipelineError::Encode`] if the slice is the wrong length or libavcodec refuses.
    pub fn encode(&mut self, stereo: &[f32]) -> Result<Vec<OpusFrame>, PipelineError> {
        let channels = usize::from(CHANNELS);
        if stereo.len() != self.frame_size * channels {
            return Err(PipelineError::Encode(format!(
                "encoder takes {} samples a frame, got {}",
                self.frame_size * channels,
                stereo.len()
            )));
        }
        // SAFETY: the frame is live and owns its buffers; `av_frame_make_writable` is
        // what guarantees libavcodec is not still holding the previous contents.
        let rc = unsafe { sys::av_frame_make_writable(self.frame) };
        if rc < 0 {
            return Err(PipelineError::Encode(format!(
                "frame not writable ({})",
                av_error(rc)
            )));
        }
        // SAFETY: the plane was sized by `av_frame_get_buffer` for `frame_size` packed
        // samples in this format, and every write below is bounded by that count.
        unsafe {
            let frame = &mut *self.frame;
            match self.layout {
                Layout::PackedFloat => {
                    let plane = frame.data[0].cast::<f32>();
                    for (i, sample) in stereo.iter().enumerate() {
                        *plane.add(i) = *sample;
                    }
                }
                Layout::PackedS16 => {
                    let plane = frame.data[0].cast::<i16>();
                    for (i, sample) in stereo.iter().enumerate() {
                        // Clamped before scaling: a sum of two sessions can exceed
                        // unity, and wrapping a float that does is a click.
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

    fn drain(&mut self, out: &mut Vec<OpusFrame>) -> Result<(), PipelineError> {
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
            // SAFETY: a successful receive leaves a packet with `size` bytes at `data`,
            // and a `duration` in the context's time base (samples).
            let (data, duration) = unsafe {
                let packet = &*self.packet;
                (
                    std::slice::from_raw_parts(packet.data, packet.size.max(0) as usize).to_vec(),
                    packet.duration,
                )
            };
            if !data.is_empty() {
                out.push(OpusFrame {
                    data,
                    samples: u32::try_from(duration)
                        .ok()
                        .filter(|d| *d > 0)
                        .unwrap_or_else(|| u32::try_from(self.frame_size).unwrap_or(960)),
                });
            }
            // SAFETY: the packet is live and this releases the reference just taken.
            unsafe { sys::av_packet_unref(self.packet) };
        }
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: each tolerates a null pointer and nulls what it frees, and every
        // pointer is owned solely by this struct.
        unsafe {
            sys::av_packet_free(&raw mut self.packet);
            sys::av_frame_free(&raw mut self.frame);
            sys::avcodec_free_context(&raw mut self.ctx);
        }
    }
}

/// Regroups the mix's arbitrary-sized draws into the encoder's exact frame size.
///
/// The encode loop drains the mix in AAC-sized bites (1024 samples), the Opus encoder
/// eats 960 at a time, and neither should have to know the other's number. Pure, so the
/// arithmetic that keeps channel interleaving intact across the seam is tested without
/// an encoder in the room.
#[derive(Debug)]
pub struct Chunker {
    /// Interleaved samples per frame handed out: `frame_size × CHANNELS`.
    frame_len: usize,
    held: Vec<f32>,
}

impl Chunker {
    /// A chunker producing frames of `frame_size` samples per channel.
    #[must_use]
    pub fn new(frame_size: usize) -> Self {
        Self {
            frame_len: frame_size.saturating_mul(usize::from(CHANNELS)).max(1),
            held: Vec::new(),
        }
    }

    /// Append a draw of interleaved stereo.
    pub fn push(&mut self, interleaved: &[f32]) {
        self.held.extend_from_slice(interleaved);
    }

    /// The next full frame, if a whole one is held.
    pub fn take(&mut self) -> Option<Vec<f32>> {
        if self.held.len() < self.frame_len {
            return None;
        }
        let rest = self.held.split_off(self.frame_len);
        Some(std::mem::replace(&mut self.held, rest))
    }

    /// Drop whatever is held — for when the consumer went away and the remainder would
    /// otherwise front-run the next session's sound.
    pub fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::audio::RATE;
    use super::*;

    fn encoder() -> Option<OpusEncoder> {
        crate::test_media::resolve(
            "an Opus encoder",
            match OpusEncoder::open(RATE, 128_000) {
                Ok(e) => Some(e),
                Err(e) => {
                    eprintln!("Opus encoder open failed: {e}");
                    None
                }
            },
        )
    }

    /// A frame of a 440 Hz tone; pure silence compresses to nearly nothing and would not
    /// prove much.
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
    fn the_encoder_codes_twenty_millisecond_frames() {
        // 960 samples at 48 kHz is the 20 ms ptime every browser offers for Opus; a
        // different frame size would still be legal RTP but would change the timestamp
        // step this module documents.
        let Some(encoder) = encoder() else { return };
        assert_eq!(encoder.frame_size(), 960);
    }

    #[test]
    fn frames_come_out_and_carry_their_duration() {
        let Some(mut encoder) = encoder() else { return };
        let size = encoder.frame_size();
        let mut frames = Vec::new();
        for i in 0..20 {
            frames.extend(encoder.encode(&tone(size, i * size)).unwrap());
        }
        assert!(frames.len() >= 18, "twenty frames in, {} out", frames.len());
        assert!(frames.iter().all(|f| !f.data.is_empty()));
        assert!(
            frames.iter().all(|f| f.samples == size as u32),
            "every packet covers one frame"
        );
    }

    #[test]
    fn a_frame_of_the_wrong_length_is_refused() {
        let Some(mut encoder) = encoder() else { return };
        let err = encoder.encode(&[0.0; 64]).unwrap_err();
        assert!(matches!(err, PipelineError::Encode(_)), "{err:?}");
    }

    #[test]
    fn the_chunker_regroups_without_tearing_the_interleave() {
        // 1024-sample draws into 960-sample frames: the seam falls mid-draw, and a
        // channel swap there would be inaudible in a test that only counted lengths.
        // Stamp every frame position into the samples — left holds the index, right its
        // negation — and any regrouping error breaks the pairing somewhere.
        let mut chunker = Chunker::new(960);
        let mut sent = 0u32;
        let mut received = 0u32;
        for _ in 0..10 {
            let draw: Vec<f32> = (0..1024)
                .flat_map(|_| {
                    let v = sent as f32;
                    sent += 1;
                    [v, -v]
                })
                .collect();
            chunker.push(&draw);
            while let Some(frame) = chunker.take() {
                assert_eq!(frame.len(), 960 * 2);
                for pair in frame.chunks_exact(2) {
                    assert_eq!(pair[0], received as f32, "left channel in order");
                    assert_eq!(pair[1], -(received as f32), "right stays with its left");
                    received += 1;
                }
            }
        }
        // 10 × 1024 = 10 240 frames in; ten full 960-frames out, 640 still held.
        assert_eq!(received, 9_600);
        chunker.clear();
        chunker.push(&[1.0; 960 * 2]);
        let head = chunker.take().unwrap();
        assert!(
            head.iter().all(|s| (*s - 1.0).abs() < f32::EPSILON),
            "clear dropped the held remainder"
        );
    }

    #[test]
    fn a_short_push_is_held_not_padded() {
        let mut chunker = Chunker::new(960);
        chunker.push(&[0.0; 100]);
        assert!(chunker.take().is_none(), "no whole frame yet");
    }
}
