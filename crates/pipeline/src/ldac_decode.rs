//! LDAC decode: the safe boundary over [`ldac_sys`].
//!
//! The only A2DP codec libav cannot decode, and therefore the only reason this pipeline
//! has a second decode backend at all (OPEN-QUESTIONS Q22). Everything here exists to
//! turn a C API with four sharp edges into something [`crate::audio_decode`] can call the
//! same way it calls ffmpeg.
//!
//! # The four sharp edges
//!
//! 1. **The input buffer needs slack past the frame.** `read_unpack_ldac` fetches three
//!    bytes at a time from wherever its bit cursor sits, so reading the last byte of a
//!    frame touches two bytes past it. `ldacBT.h` says to allocate `LDACBT_MAX_NBYTES + 2`;
//!    passing a bare slice pointer is a two-byte overread on every frame. [`Decoder`] owns
//!    a scratch buffer of exactly that size and copies into it.
//! 2. **One packet is many frames.** An A2DP payload holds as many `ldac_transport_frame`s
//!    as the MTU allowed, and `ldacBT_decode` consumes exactly one per call, reporting how
//!    far it got in `used_bytes`. A decoder that calls it once per packet plays a fraction
//!    of the audio and drops the rest — which sounds like a stutter, not like a bug.
//! 3. **Failure does not always mean "no audio".** When a frame header disagrees with the
//!    handle's configuration, `ldacBT_decode` re-initialises itself, decodes the frame
//!    anyway, sets the error code to `LDACBT_ERR_DEC_CONFIG_UPDATED`, and still returns
//!    -1. Treating that as a dropped frame throws away good audio at exactly the moment a
//!    sender changes rate.
//! 4. **The stream, not the negotiation, says what the rate is.** LDAC is the one A2DP
//!    codec that carries its sample rate and channel configuration in every frame header,
//!    and the library re-configures itself from them. So the [`PcmBlock`]s here report
//!    what the *handle* says after each frame rather than the negotiated
//!    [`AudioFormat`] — the opposite of aptX, which has no header and must be told
//!    (Q25). A mismatch between the two is worth a log line and is not worth failing over:
//!    the audio is fine, the endpoint table is what is wrong.
//!
//! `LDACBT_SMPL_FMT_F32` is the format asked for throughout, because it is interleaved
//! `f32` — exactly what [`PcmBlock`] holds. No conversion, and no exposure to the
//! planar/packed plane-length trap that silenced the right channel on every ffmpeg codec
//! in this pipeline (see `audio_decode::pcm_from_frame`).
#![allow(unsafe_code)]

use std::time::Duration;

use castaway_core::{AudioFormat, PcmFrame as PcmBlock};
use ldac_sys as sys;
use tracing::{debug, warn};

use crate::error::PipelineError;

/// Bytes of slack `ldacBT_decode` reads past the frame it is given.
///
/// Not a guess: `read_unpack_ldac` reads `p[0]<<16 | p[1]<<8 | p[2]` from the current byte,
/// so the last byte of a frame is read together with the two after it. The header states
/// the same requirement as `LDACBT_MAX_NBYTES + 2`.
const LOOKAHEAD: usize = 2;

/// The scratch buffer a frame is copied into before decoding.
const INPUT_CAPACITY: usize = sys::LDACBT_MAX_NBYTES as usize + LOOKAHEAD;

/// Bytes of PCM one `ldacBT_decode` call can write.
///
/// `LDACBT_MAX_LSU` samples per channel, two channels, four bytes per `f32` sample. The
/// library writes `frame_samples * channels * 4` and never asks how big the buffer is, so
/// this has to be the maximum rather than the expected size.
const OUTPUT_CAPACITY: usize = sys::LDACBT_MAX_LSU as usize * 2 * 4;

/// Whether this build can decode LDAC.
///
/// Answers by allocating a handle, not by reporting a feature flag. The distinction is the
/// whole of Q22: `can_decode` used to answer `cfg!(feature = "ldac")` while the feature
/// bound no decoder, so a build advertised an LDAC endpoint, a phone picked it, and every
/// packet failed. Asking the library is the only answer that cannot drift.
#[must_use]
pub fn available() -> bool {
    // SAFETY: `ldacBT_get_handle` takes no arguments and either allocates a handle we own
    // or returns null. Freed immediately; nothing else touches it.
    unsafe {
        let handle = sys::ldacBT_get_handle();
        if handle.is_null() {
            return false;
        }
        sys::ldacBT_free_handle(handle);
    }
    true
}

/// An open LDAC decoder for one stream.
pub struct Decoder {
    handle: sys::HANDLE_LDAC_BT,
    /// Frame bytes plus the lookahead the bit reader needs. Reused per frame so a stream
    /// does not allocate per packet.
    input: Box<[u8; INPUT_CAPACITY]>,
    output: Box<[u8; OUTPUT_CAPACITY]>,
    /// The negotiated format, kept only to compare against what the stream turns out to
    /// be. Not used to shape the output — see the module docs.
    negotiated: AudioFormat,
    /// Whether a stream/negotiation disagreement has already been reported, so a mismatch
    /// costs one log line rather than one per frame.
    warned_mismatch: bool,
}

// The library keeps every bit of its state behind the handle — unlike moonlight-common-c,
// there are no process globals — so a `Decoder` is safe to move between threads. It is
// emphatically not `Sync`, and is not marked so: two threads calling `ldacBT_decode` on
// one handle would race on its internal buffers.
//
// SAFETY: `handle` is an owning pointer to state reachable only through this `Decoder`,
// which holds it exclusively; no C-side global is consulted, and nothing else in the
// process holds a copy of the pointer.
unsafe impl Send for Decoder {}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LdacDecoder")
            .field("negotiated", &self.negotiated)
            .field("stream", &self.stream_format())
            .finish_non_exhaustive()
    }
}

impl Decoder {
    /// Open a decoder for a stream AVDTP negotiated at `format`.
    ///
    /// The format is used to *initialise* the handle, because the library wants a starting
    /// configuration — but it is not what the output is trusted to be. The first frame
    /// header may say otherwise, and if it does the library reconfigures and this decoder
    /// follows it.
    ///
    /// # Errors
    /// [`PipelineError::Decode`] if a handle cannot be allocated, or if the library
    /// refuses the rate or channel count — which it does for any rate outside
    /// 44.1/48/88.2/96 kHz, including the 176.4 and 192 kHz an LDAC *capability* is able
    /// to advertise and its bitstream cannot express.
    pub fn new(format: AudioFormat) -> Result<Self, PipelineError> {
        // SAFETY: no arguments, and the returned handle is owned by us from here on. Every
        // early return below frees it.
        let handle = unsafe { sys::ldacBT_get_handle() };
        if handle.is_null() {
            return Err(PipelineError::Decode(
                "could not allocate an LDAC handle".into(),
            ));
        }

        // The A2DP channel *mode* bitfield, which is what `cm` wants — the `LDAC_CCI_*`
        // constants are a different numbering for the same idea and `ldacBT_assert_cm`
        // rejects them. Mono when the negotiation said one channel; stereo otherwise. Dual
        // channel is not chosen here even though we advertise it, because it is the
        // *frame header* that decides: a dual-channel stream re-initialises the handle on
        // its first frame and lands in the right mode either way.
        let channel_mode = if format.channels() <= 1 {
            sys::LDACBT_CHANNEL_MODE_MONO
        } else {
            sys::LDACBT_CHANNEL_MODE_STEREO
        };

        let decoder = Self {
            handle,
            input: Box::new([0; INPUT_CAPACITY]),
            output: Box::new([0; OUTPUT_CAPACITY]),
            negotiated: format,
            warned_mismatch: false,
        };

        let Ok(rate) = i32::try_from(format.sample_rate()) else {
            return Err(PipelineError::Decode(format!(
                "LDAC sample rate {} is out of range",
                format.sample_rate()
            )));
        };
        // SAFETY: `handle` is a freshly allocated handle owned by `decoder`, which drops
        // it on the error path below. `channel_mode` is one of the three values
        // `ldacBT_assert_cm` accepts, and the three trailing arguments are the reserved
        // ones the header requires to be zero.
        let rc = unsafe {
            sys::ldacBT_init_handle_decode(
                decoder.handle,
                i32::try_from(channel_mode).unwrap_or(1),
                rate,
                0,
                0,
                0,
            )
        };
        if rc != 0 {
            let code = decoder.error_code();
            return Err(PipelineError::Decode(format!(
                "LDAC decoder init refused {format:?}: error {code}"
            )));
        }
        Ok(decoder)
    }

    /// The negotiated format the decoder was opened with.
    #[must_use]
    pub const fn negotiated(&self) -> AudioFormat {
        self.negotiated
    }

    /// The format the *stream* is actually in, as the library reports it.
    ///
    /// `None` before the first frame: the rate lives in the frame headers, so until one
    /// has been decoded there is nothing to report but the guess we were opened with.
    #[must_use]
    pub fn stream_format(&self) -> Option<AudioFormat> {
        // SAFETY: `handle` is a live handle initialised for decode; this only reads it.
        let rate = unsafe { sys::ldacBT_get_sampling_freq(self.handle) };
        let rate = u32::try_from(rate).ok()?;
        AudioFormat::from_hz(rate, self.negotiated.channels())
    }

    /// The library's current error code.
    fn error_code(&self) -> i32 {
        // SAFETY: `handle` is a live handle; this only reads its error field.
        unsafe { sys::ldacBT_get_error_code(self.handle) }
    }

    /// Decode one A2DP payload — a sequence of transport frames — calling `on_pcm` for
    /// each block that comes out.
    ///
    /// `pts` is the presentation time of the payload's first frame; each frame after it is
    /// offset by the audio the previous ones produced, so a block's timestamp stays
    /// truthful when several frames arrive in one packet.
    ///
    /// A frame the library refuses is logged and skipped, never fatal: one corrupt packet
    /// off a radio link must not end the session. This mirrors the ffmpeg path exactly.
    ///
    /// # Errors
    /// Never, currently — every failure mode is per frame and recoverable. The `Result` is
    /// here because the ffmpeg backend beside it has failures that are not, and the two
    /// are called through one signature.
    pub fn decode(
        &mut self,
        payload: &[u8],
        pts: Duration,
        mut on_pcm: impl FnMut(PcmBlock),
    ) -> Result<(), PipelineError> {
        let mut offset = 0usize;
        let mut pts = pts;
        // A frame that consumes nothing would spin here forever; the loop below breaks on
        // `used == 0` for that reason, and this bounds the pathological case where every
        // frame in a packet is refused.
        while offset < payload.len() {
            let remaining = &payload[offset..];
            let Some(block) = self.decode_one(remaining, pts, &mut offset) else {
                break;
            };
            pts = pts.saturating_add(block.duration());
            if !block.samples.is_empty() {
                on_pcm(block);
            }
        }
        Ok(())
    }

    /// Decode the frame at the front of `remaining`, advancing `offset` past it.
    ///
    /// Returns `None` when the sequence is finished or unusable — the caller stops.
    fn decode_one(
        &mut self,
        remaining: &[u8],
        pts: Duration,
        offset: &mut usize,
    ) -> Option<PcmBlock> {
        // `ldacBT_decode` refuses anything shorter than a header plus two bytes outright,
        // and a tail that short is padding rather than a frame.
        if remaining.len() < 5 {
            return None;
        }
        let take = remaining.len().min(sys::LDACBT_MAX_NBYTES as usize);

        // The copy that keeps the two-byte overread in bounds. Zeroing the slack matters
        // as well as its presence: the bit reader's value ends up in a `frame_length` the
        // library then trusts.
        self.input[..take].copy_from_slice(&remaining[..take]);
        self.input[take..take + LOOKAHEAD].fill(0);

        let mut used = 0i32;
        let mut wrote = 0i32;
        // SAFETY: `handle` is initialised for decode. `input` is `LDACBT_MAX_NBYTES + 2`
        // bytes with `take <= LDACBT_MAX_NBYTES` bytes of frame and the two lookahead
        // bytes zeroed, which is the allocation `ldacBT.h` requires. `output` is
        // `LDACBT_MAX_LSU * 2 * 4` bytes, the largest PCM block one call can write
        // (frame samples x channels x f32). `bs_bytes` is what we actually copied in, and
        // both out-parameters are live locals.
        let rc = unsafe {
            sys::ldacBT_decode(
                self.handle,
                self.input.as_mut_ptr(),
                self.output.as_mut_ptr(),
                sys::LDACBT_SMPL_FMT_T::LDACBT_SMPL_FMT_F32,
                i32::try_from(take).unwrap_or(i32::MAX),
                &raw mut used,
                &raw mut wrote,
            )
        };

        // -1 with `LDACBT_ERR_DEC_CONFIG_UPDATED` is a *success* that reconfigured the
        // handle: the frame header disagreed with the current configuration, the library
        // re-initialised itself, decoded the frame, and reported failure anyway. Dropping
        // it loses the first frame of every rate change.
        let reconfigured = rc != 0
            && sys::err::api_err(self.error_code())
                == i32::try_from(sys::LDACBT_ERR_DEC_CONFIG_UPDATED).unwrap_or(-1);
        if rc != 0 && !reconfigured {
            debug!(
                error = self.error_code(),
                len = remaining.len(),
                head = %hex_head(remaining),
                "LDAC decoder refused a frame",
            );
            return None;
        }
        if reconfigured {
            debug!(
                rate = ?self.stream_format().map(AudioFormat::sample_rate),
                "LDAC stream changed configuration mid-flight; decoder followed it",
            );
        }

        // A frame that consumed nothing cannot be stepped over, so stop rather than spin.
        let used = usize::try_from(used).unwrap_or(0);
        if used == 0 {
            return None;
        }
        *offset += used;

        let wrote = usize::try_from(wrote).unwrap_or(0).min(OUTPUT_CAPACITY);
        Some(self.pcm(&self.output[..wrote], pts))
    }

    /// Turn the library's interleaved `f32` output into a [`PcmBlock`].
    fn pcm(&self, bytes: &[u8], pts: Duration) -> PcmBlock {
        // Interleaved f32 in native byte order — the one format in this pipeline that
        // needs no conversion at all. Read four bytes at a time rather than casting the
        // buffer, because a `[u8; N]` has no alignment guarantee for `f32`.
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let rate = self
            .stream_format()
            .unwrap_or(self.negotiated)
            .sample_rate();
        PcmBlock {
            sample_rate: rate,
            channels: channels_written(rate, samples.len()),
            samples,
            pts,
        }
    }

    /// Compare the stream against the negotiation, once, and say so if they differ.
    ///
    /// Not a failure. The audio is correct — this decoder follows the frame headers — but a
    /// sender coding at a rate we did not offer means the endpoint table and the session
    /// disagree, and that is worth knowing before somebody debugs the wrong layer.
    pub fn check_against_negotiation(&mut self) {
        if self.warned_mismatch {
            return;
        }
        let Some(stream) = self.stream_format() else {
            return;
        };
        if stream.sample_rate() != self.negotiated.sample_rate() {
            self.warned_mismatch = true;
            warn!(
                negotiated = self.negotiated.sample_rate(),
                stream = stream.sample_rate(),
                "LDAC stream is not at the negotiated rate; following the stream",
            );
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `handle` was allocated by `ldacBT_get_handle`, is non-null (checked at
        // construction), and this is the only place it is released. `ldacBT_free_handle`
        // closes an initialised handle itself, so no separate `close` is needed.
        unsafe { sys::ldacBT_free_handle(self.handle) };
    }
}

/// How many channels a decoded block holds, derived from its own size.
///
/// The library will tell us the sample rate and not the channel count — there is no
/// `ldacBT_get_channel` in the public API — so the obvious thing is to reuse the negotiated
/// count. That is wrong in exactly the case this whole module is careful about: LDAC's frame
/// header carries the channel configuration and the decoder follows it, so a sender that
/// negotiated stereo and codes mono produces half-size blocks. Labelling those as stereo
/// halves the reported duration, doubles the apparent rate, and plays the audio at speed.
///
/// The size is enough to recover it. One call decodes exactly one frame — 128 samples per
/// channel at 44.1/48 kHz, 256 at 88.2/96 — into interleaved `f32`, so the sample count is
/// `frame_samples * channels` and the division is exact. Anything that does not divide is a
/// short final write; two channels is the safer reading, being what every real sender uses.
fn channels_written(rate: u32, samples: usize) -> u16 {
    let frame_samples = if rate >= 88_200 { 256 } else { 128 };
    match samples.checked_div(frame_samples) {
        Some(1) => 1,
        _ => 2,
    }
}

/// The first few bytes as hex, for the log line that tells framing mistakes apart.
///
/// An LDAC frame starts `aa`; a payload whose one-byte A2DP header was not stripped does
/// not, and that is the difference between a corrupt stream and a bug in this repo.
fn hex_head(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}
