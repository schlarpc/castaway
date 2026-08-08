//! Decode for the A2DP codecs: encoded frames in, interleaved f32 PCM out.
//!
//! Four of the five ride on ffmpeg decoders that are already in the build. LDAC is the
//! exception — libav has no decoder — so it has a **second backend**, Sony's own
//! `libldacBT` behind the `ldac` feature (see [`crate::ldac_decode`]). Which backend a
//! codec uses is settled once, in [`Backend::for_codec`], and the absence of one must keep
//! the endpoint out of the advertised table rather than fail at decode time
//! (#14).
//!
//! AAC arrives here already unwrapped from its LATM multiplex — see
//! `proto_bluetooth_audio::latm` for why that is a separate step and what happens if it is
//! skipped.
//!
//! Two of these codecs have **no in-band configuration at all**: aptX and aptX HD are raw
//! sample streams with no header, so the decoder cannot discover the sample rate or
//! channel count and must be told. Those come from the AVDTP negotiation, which is
//! exactly why [`AudioFormat`] is a parameter here rather than something sniffed.
//! Getting it wrong plays the stream at the wrong pitch instead of failing.
#![allow(unsafe_code)]

use std::sync::Once;
use std::time::Duration;

use castaway_core::{AudioCodec, AudioFormat, EncodedFrame};
use ffmpeg_next as ffmpeg;
use tracing::{debug, warn};

use crate::error::PipelineError;

static INIT: Once = Once::new();

fn ensure_init() {
    INIT.call_once(|| {
        let _ = ffmpeg::init();
    });
}

fn map_err(e: ffmpeg::Error) -> PipelineError {
    PipelineError::Decode(e.to_string())
}

/// A block of decoded audio.
///
/// The type lives in `core` as [`PcmFrame`] because it is no longer only a decoder
/// output: an adapter can hand the pipeline PCM directly ([`FrameSource::Pcm`]), and
/// two structurally identical types either side of that seam would only invite a
/// pointless conversion. The local name stays because "block" is what the decode and
/// output stages have always called it.
///
/// [`FrameSource::Pcm`]: castaway_core::FrameSource::Pcm
pub use castaway_core::PcmFrame as PcmBlock;

/// Which decoder implementation a codec needs.
///
/// An enum rather than an `Option<ffmpeg::codec::Id>` because "no ffmpeg decoder" and "not
/// decodable" stopped being the same thing when LDAC got a backend of its own, and the
/// place that used to conflate them is where #14's silence came from. Every question this
/// module answers about a codec — can it be decoded, what opens it, what closes over the
/// state — routes through here, so there is one decision and not three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// A libav decoder, named by id.
    Ffmpeg(ffmpeg::codec::Id),
    /// Sony's `libldacBT`. Only reachable with the `ldac` feature on: without it there is
    /// no backend for LDAC at all, which is the answer `can_decode` needs.
    #[cfg(feature = "ldac")]
    Ldac,
}

impl Backend {
    /// Which backend decodes `codec` in this build.
    ///
    /// # Errors
    /// [`PipelineError::Decode`] naming the codec, when this build has nothing for it.
    fn for_codec(codec: AudioCodec) -> Result<Self, PipelineError> {
        match codec {
            #[cfg(feature = "ldac")]
            AudioCodec::Ldac => Ok(Self::Ldac),
            // Without the feature there is no LDAC decoder in the process, and saying so
            // here is what keeps the endpoint off the advertised table. The old wording
            // said libav has none and left it at that, which was true and not the point:
            // what matters is that *nothing* does (#14).
            #[cfg(not(feature = "ldac"))]
            AudioCodec::Ldac => Err(PipelineError::Decode(
                "no LDAC decoder in this build: libav has none, and the `ldac` feature \
                 that binds Sony's own library is off (#14)"
                    .into(),
            )),
            other => codec_id(other).map(Self::Ffmpeg),
        }
    }

    /// Whether the backend is actually present, as opposed to merely named.
    fn is_available(self) -> bool {
        match self {
            // An ffmpeg build without the decoder compiled in is a real configuration:
            // `aptx_hd` in particular is not universal, and advertising it on a build
            // without it puts our best endpoint in front of a sender and then fails.
            Self::Ffmpeg(id) => ffmpeg::decoder::find(id).is_some(),
            // Asks the library for a handle rather than trusting the feature flag. See
            // `ldac_decode::available`.
            #[cfg(feature = "ldac")]
            Self::Ldac => crate::ldac_decode::available(),
        }
    }
}

/// Which ffmpeg decoder an A2DP codec asks for.
fn codec_id(codec: AudioCodec) -> Result<ffmpeg::codec::Id, PipelineError> {
    match codec {
        AudioCodec::Sbc => Ok(ffmpeg::codec::Id::SBC),
        // A2DP does carry AAC inside a LATM multiplex (A2DP §4.5.4 → RFC 3016), but the
        // multiplex comes off in the depacketizer and what arrives here is a raw access
        // unit, so this is the plain decoder. `AAC_LATM` is emphatically *not* the right
        // answer despite the name: ffmpeg's is a LOAS decoder whose first act is to check
        // for the 11-bit 0x2B7 syncword that RFC 3016 streams do not carry, so it refuses
        // every packet — and returns AVERROR_INVALIDDATA without logging a thing.
        AudioCodec::Aac => Ok(ffmpeg::codec::Id::AAC),
        AudioCodec::AptX => Ok(ffmpeg::codec::Id::APTX),
        AudioCodec::AptXHd => Ok(ffmpeg::codec::Id::APTX_HD),
        AudioCodec::Alac => Ok(ffmpeg::codec::Id::ALAC),
        AudioCodec::Opus => Ok(ffmpeg::codec::Id::OPUS),
        // LDAC never reaches here: `Backend::for_codec` routes it to its own backend
        // before asking libav, because libav has no LDAC decoder to name. PCM needs no
        // decoder at all — an adapter with samples uses `FrameSource::Pcm`, so a PCM
        // *encoded* frame asking for a decoder is a bug upstream. Named rather than
        // wildcarded so the next codec added to core has to answer here (#213).
        other @ (AudioCodec::Ldac | AudioCodec::Pcm) => Err(PipelineError::Decode(format!(
            "no libav audio decoder mapped for {other:?}"
        ))),
    }
}

/// Whether this build can decode `codec`.
///
/// The A2DP endpoint table must be built from this, not from optimism: advertising a
/// codec we cannot decode means the sender picks it and the session is silence rather
/// than a clean fallback to one we can (#14).
#[must_use]
pub fn can_decode(codec: AudioCodec) -> bool {
    // One source of truth, and it is whether a decoder actually exists. This used to
    // answer `cfg!(feature = "ldac")` for LDAC, which is a different question: the `ldac`
    // feature reserved a slot and bound no decoder, so a build with it on advertised an
    // LDAC endpoint and then failed every packet — the exact silence #14 is about. The
    // feature now binds a real backend, and this still does not ask about the feature.
    ensure_init();
    Backend::for_codec(codec).is_ok_and(Backend::is_available)
}

/// The decoder state for whichever backend a stream landed on.
enum Decoder {
    Ffmpeg(ffmpeg::decoder::Audio),
    #[cfg(feature = "ldac")]
    Ldac(Box<crate::ldac_decode::Decoder>),
}

/// An open decoder for one A2DP stream.
pub struct AudioDecoder {
    decoder: Decoder,
    format: AudioFormat,
    codec: AudioCodec,
}

impl std::fmt::Debug for AudioDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioDecoder")
            .field("codec", &self.codec)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl AudioDecoder {
    /// Open a decoder for `codec` at the negotiated `format`.
    ///
    /// `config` is the codec's out-of-band configuration — libav calls it *extradata*.
    /// Pass `None` for codecs that describe themselves in-band; pass `Some` when the
    /// protocol carried the configuration separately from the samples:
    ///
    /// - **ALAC** (AirPlay 1) *must* have it. libavcodec's `alac_decode_init` checks for
    ///   at least 36 bytes and returns `AVERROR_INVALIDDATA` without them, so an ALAC
    ///   decoder opened with `None` does not fail later, it fails to open at all. The
    ///   bytes are `AlacConfig::magic_cookie` in `proto-airplay`.
    /// - **AAC-ELD** (AirPlay mirroring audio) needs its 4-byte `AudioSpecificConfig`.
    ///
    /// # Errors
    /// [`PipelineError::Decode`] if this build has no decoder for the codec, the
    /// configuration cannot be allocated, or the decoder refuses to open.
    pub fn new(
        codec: AudioCodec,
        format: AudioFormat,
        config: Option<&[u8]>,
    ) -> Result<Self, PipelineError> {
        ensure_init();
        // With `ldac` off the enum has one variant and this match is "infallible" — the
        // shape is for the variant that is cfg'd in, so the lint is scoped out with it.
        #[cfg_attr(not(feature = "ldac"), allow(clippy::infallible_destructuring_match))]
        let id = match Backend::for_codec(codec)? {
            Backend::Ffmpeg(id) => id,
            // LDAC takes none of what follows: no extradata (the configuration is in every
            // frame header), no libav context, and no channel-layout struct to fill in.
            #[cfg(feature = "ldac")]
            Backend::Ldac => {
                return Ok(Self {
                    decoder: Decoder::Ldac(Box::new(crate::ldac_decode::Decoder::new(format)?)),
                    format,
                    codec,
                })
            }
        };
        let found = ffmpeg::decoder::find(id).ok_or_else(|| {
            PipelineError::Decode(format!("this ffmpeg build has no {id:?} decoder"))
        })?;

        let mut context = ffmpeg::decoder::new();
        {
            // The negotiated format is written onto *every* decoder, not just the ones
            // that need it in principle. aptX and aptX HD are bare sample streams with
            // no header at all, so they genuinely cannot open without it — but libav's
            // SBC decoder also fails, returning AVERROR_BUG from `receive_frame` for
            // every packet while `send_packet` reports success, which is an unusually
            // hostile way to say "you did not configure me".
            //
            // SAFETY: `context` is a freshly allocated, not-yet-opened AVCodecContext
            // that we exclusively own, and these are plain scalar/struct fields on it.
            // `av_channel_layout_default` initialises the layout union in place, which
            // is the documented way to set it since the 5.1 channel-layout rework —
            // writing the old `channels`/`channel_layout` fields silently does nothing
            // on this ffmpeg. Nothing is read back until `open_as` below.
            unsafe {
                let raw = context.as_mut_ptr();
                (*raw).sample_rate = i32::try_from(format.sample_rate()).unwrap_or(44_100);
                ffmpeg_sys_next::av_channel_layout_default(
                    std::ptr::addr_of_mut!((*raw).ch_layout),
                    i32::from(format.channels()),
                );
            }
        }

        if let Some(config) = config {
            // libav takes ownership: `avcodec_free_context` calls `av_freep` on
            // `extradata`, so it has to come from libav's allocator rather than being a
            // pointer into our slice. The trailing padding is what the bitstream readers
            // over-read into; `av_mallocz` zeroes it, which is what they expect.
            let padding =
                usize::try_from(ffmpeg_sys_next::AV_INPUT_BUFFER_PADDING_SIZE).unwrap_or(64);
            // SAFETY: `context` is a freshly allocated, not-yet-opened AVCodecContext we
            // exclusively own. `buf` is a fresh libav allocation of `len + padding`
            // bytes, so the copy of `len` bytes is in bounds, and we hand it to the
            // context which becomes its owner. `extradata_size` is set to the copied
            // length only, excluding the padding, as libav requires.
            unsafe {
                let raw = context.as_mut_ptr();
                let len = config.len();
                let buf = ffmpeg_sys_next::av_mallocz(len + padding).cast::<u8>();
                if buf.is_null() {
                    return Err(PipelineError::Decode(
                        "could not allocate decoder extradata".into(),
                    ));
                }
                std::ptr::copy_nonoverlapping(config.as_ptr(), buf, len);
                (*raw).extradata = buf;
                (*raw).extradata_size = i32::try_from(len).unwrap_or(0);
            }
        }

        let decoder = context
            .open_as(found)
            .map_err(map_err)?
            .audio()
            .map_err(map_err)?;

        Ok(Self {
            decoder: Decoder::Ffmpeg(decoder),
            format,
            codec,
        })
    }

    /// The format the stream was negotiated at.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Decode one encoded frame, calling `on_pcm` for each block that comes out.
    ///
    /// A frame the decoder rejects is logged and skipped rather than fatal: one corrupt
    /// packet out of a radio link must not end the session.
    ///
    /// # Errors
    /// [`PipelineError::Decode`] only for failures that make the decoder unusable.
    pub fn decode(
        &mut self,
        frame: &EncodedFrame,
        mut on_pcm: impl FnMut(PcmBlock),
    ) -> Result<(), PipelineError> {
        // A decoder that refuses everything is a configuration problem, and configuration
        // problems are solved offline against real bytes rather than with a phone in hand.
        // Set CASTAWAY_DUMP_AUDIO to a path to capture the raw frames as they arrive.
        // Before the backend split, so a dump is of what arrived rather than of what one
        // backend made of it — and so the LDAC path is dumpable too, which is how the
        // fixtures in `tests/fixtures` are formatted.
        self.dump(frame);

        #[cfg(feature = "ldac")]
        if let Decoder::Ldac(ldac) = &mut self.decoder {
            // One payload is a *sequence* of transport frames, and the wrapper walks it;
            // the timestamps of the frames after the first are derived from the audio the
            // earlier ones produced.
            ldac.decode(&frame.data, frame.pts, on_pcm)?;
            ldac.check_against_negotiation();
            return Ok(());
        }
        // With `ldac` off the pattern is irrefutable — the guard is for the variant that
        // is cfg'd in, so the lint is scoped out with it.
        #[cfg_attr(not(feature = "ldac"), allow(irrefutable_let_patterns))]
        let Decoder::Ffmpeg(decoder) = &mut self.decoder
        else {
            // Unreachable: the only other variant returned above. Written as a guard
            // rather than a `match` so the ffmpeg body below stays unindented and the diff
            // that introduced the second backend stays readable.
            return Ok(());
        };

        let mut packet = ffmpeg::Packet::copy(&frame.data);
        packet.set_pts(Some(
            i64::try_from(frame.pts.as_micros()).unwrap_or(i64::MAX),
        ));
        if let Err(e) = decoder.send_packet(&packet) {
            // The leading bytes identify the framing when a decoder refuses everything:
            // `fff1`/`fff9` is ADTS, `56ex` is LOAS/LATM with a sync stream, and neither
            // is raw LATM. Guessing between them is how a stream decodes to nothing.
            let head: String = frame
                .data
                .iter()
                .take(12)
                .map(|b| format!("{b:02x}"))
                .collect();
            debug!(
                error = %e,
                codec = ?self.codec,
                len = frame.data.len(),
                head = %head,
                "audio decoder rejected a packet",
            );
            return Ok(());
        }
        Self::drain(decoder, &mut on_pcm)
    }

    /// Write the frame to `CASTAWAY_DUMP_AUDIO`, if it is set.
    fn dump(&self, frame: &EncodedFrame) {
        if let Ok(path) = std::env::var("CASTAWAY_DUMP_AUDIO") {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let len = u32::try_from(frame.data.len()).unwrap_or(u32::MAX);
                let _ = f.write_all(&len.to_le_bytes());
                let _ = f.write_all(&frame.data);
            }
        }
    }

    /// Flush the decoder at end of stream.
    ///
    /// Flushing is not optional on the ffmpeg path: libav decoders hold frames back, and a
    /// session that never flushes loses whatever was still in the pipeline when the phone
    /// stopped. `libldacBT` holds nothing — one frame in, one block out, synchronously — so
    /// there is nothing to flush on the LDAC path and nothing to lose by saying so.
    ///
    /// # Errors
    /// [`PipelineError::Decode`] if draining fails.
    pub fn flush(&mut self, mut on_pcm: impl FnMut(PcmBlock)) -> Result<(), PipelineError> {
        match &mut self.decoder {
            Decoder::Ffmpeg(decoder) => {
                let _ = decoder.send_eof();
                Self::drain(decoder, &mut on_pcm)
            }
            #[cfg(feature = "ldac")]
            Decoder::Ldac(_) => Ok(()),
        }
    }

    fn drain(
        decoder: &mut ffmpeg::decoder::Audio,
        on_pcm: &mut impl FnMut(PcmBlock),
    ) -> Result<(), PipelineError> {
        let mut decoded = ffmpeg::frame::Audio::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let block = pcm_from_frame(&decoded)?;
            if !block.samples.is_empty() {
                on_pcm(block);
            }
        }
        Ok(())
    }
}

/// Convert whatever a libav audio decoder produced into interleaved f32.
///
/// Done by hand rather than through swresample. Every decoder has its own native layout —
/// aptX comes out planar s32, SBC packed s16, AAC planar float — and a resampler would be
/// the obvious tool, but ffmpeg 7's channel-layout rework leaves `swr` comparing a legacy
/// layout mask against a frame carrying the new `AVChannelLayout`, so it rejects its own
/// decoder's output with "Input changed". The conversion is a dozen lines and has no state
/// to get out of sync.
///
/// A free function because both audio paths need exactly this: the A2DP decoder above, and
/// the media-URL demuxer in [`crate::ffmpeg_decode`]. The knowledge in the comments below
/// was expensive and should not be discovered twice.
pub(crate) fn pcm_from_frame(decoded: &ffmpeg::frame::Audio) -> Result<PcmBlock, PipelineError> {
    {
        use ffmpeg::format::sample::Type;
        use ffmpeg::format::Sample;

        let channels = usize::from(decoded.channels().max(1));
        let frames = decoded.samples();
        let pts = decoded
            .pts()
            .map(|us| Duration::from_micros(us.unsigned_abs()))
            .unwrap_or_default();

        let mut samples = vec![0f32; frames * channels];
        let format = decoded.format();
        let planar = matches!(
            format,
            Sample::I16(Type::Planar) | Sample::I32(Type::Planar) | Sample::F32(Type::Planar)
        );
        let width = match format {
            Sample::I16(_) => 2usize,
            Sample::I32(_) | Sample::F32(_) => 4,
            other => {
                return Err(PipelineError::Decode(format!(
                    "unsupported decoder output format {other:?}"
                )))
            }
        };

        // Neither accessor ffmpeg-next offers is usable directly. `Frame::plane::<T>()`
        // reports one channel's worth of length for *packed* audio, and `Frame::data(n)`
        // takes its length from `linesize[n]` — which ffmpeg only fills in for plane 0 of
        // planar audio, leaving every other plane an empty slice. Reading a planar frame
        // through it therefore yields audio in the left channel and digital silence in
        // the right, which is audible immediately and invisible to an RMS check over the
        // interleaved buffer.
        //
        // So planar planes are taken from `extended_data` with the length computed here.
        // `extended_data` rather than `data[]` because it is the documented accessor and
        // stays valid past eight channels, where `data[]` runs out.
        for ch in 0..channels {
            let plane: &[u8] = if planar {
                // SAFETY: `decoded` holds a decoder-owned frame that is alive for this
                // borrow. For planar audio ffmpeg guarantees `extended_data[ch]` is a
                // valid buffer for every channel it reported, each holding exactly
                // `samples() * bytes_per_sample` bytes.
                unsafe {
                    let raw = decoded.as_ptr();
                    let ptr = *(*raw).extended_data.add(ch);
                    if ptr.is_null() {
                        continue;
                    }
                    std::slice::from_raw_parts(ptr, frames * width)
                }
            } else {
                decoded.data(0)
            };
            for i in 0..frames {
                let byte = if planar {
                    i * width
                } else {
                    (i * channels + ch) * width
                };
                let Some(chunk) = plane.get(byte..byte + width) else {
                    continue;
                };
                samples[i * channels + ch] = match format {
                    Sample::I16(_) => {
                        f32::from(i16::from_ne_bytes([chunk[0], chunk[1]])) / 32_768.0
                    }
                    Sample::I32(_) => {
                        #[allow(clippy::cast_precision_loss)]
                        let v = i32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32;
                        v / 2_147_483_648.0
                    }
                    _ => f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                };
            }
        }

        Ok(PcmBlock {
            sample_rate: decoded.rate(),
            channels: u16::try_from(channels).unwrap_or(2),
            samples,
            pts,
        })
    }
}

/// Decode a live A2DP stream, pulling frames and pushing PCM.
///
/// Pull-based for the same reason the video path is: `next` blocks on whatever channel
/// the caller owns, from a thread the caller picked, keeping this module free of tokio
/// (ground rule 4). `on_pcm` returns `false` to stop.
///
/// # Errors
/// [`PipelineError::Decode`] if the decoder cannot be opened at all.
pub fn decode_audio_stream<N, F>(
    codec: AudioCodec,
    format: AudioFormat,
    config: Option<&[u8]>,
    mut next: N,
    mut on_pcm: F,
) -> Result<(), PipelineError>
where
    N: FnMut() -> Option<EncodedFrame>,
    F: FnMut(PcmBlock) -> bool,
{
    let mut decoder = AudioDecoder::new(codec, format, config)?;
    let mut running = true;
    while running {
        let Some(frame) = next() else { break };
        decoder.decode(&frame, |block| {
            if running && !on_pcm(block) {
                running = false;
            }
        })?;
    }
    let _ = decoder.flush(|block| {
        let _ = on_pcm(block);
    });
    Ok(())
}

/// Log once per session that a codec could not be decoded, and why.
pub(crate) fn warn_undecodable(codec: AudioCodec) {
    warn!(
        ?codec,
        "no decoder in this build; the endpoint should not have been advertised (#14)"
    );
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A negotiated format, for tests that already know both numbers are sane.
    pub(crate) fn format(sample_rate: u32, channels: u16) -> AudioFormat {
        AudioFormat::from_hz(sample_rate, channels).unwrap()
    }

    /// A one-second 440 Hz sine at `rate`, interleaved stereo.
    pub(crate) fn sine(rate: u32, frames: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            #[allow(clippy::cast_precision_loss)]
            let t = n as f32 / rate as f32;
            #[allow(clippy::cast_possible_truncation)]
            let s = ((t * 440.0 * std::f32::consts::TAU).sin() * 12_000.0) as i16;
            out.push(s);
            out.push(s);
        }
        out
    }

    /// Write interleaved i16 PCM into `frame`, in whatever layout the encoder wants.
    ///
    /// Encoders are picky in ways that are invisible until `open_as` returns EINVAL:
    /// ffmpeg's aptX encoder takes planar **s32**, SBC takes packed s16. Rather than
    /// guessing, the caller asks the codec which formats it supports and this converts
    /// into the chosen one. (`Frame::plane::<T>()` is also unusable for *packed*
    /// multi-channel audio — it reports one channel's worth of length — so packed
    /// layouts are written through the raw byte buffer.)
    fn fill_frame(
        frame: &mut ffmpeg::frame::Audio,
        format: ffmpeg::format::Sample,
        channels: usize,
        interleaved: &[i16],
    ) {
        use ffmpeg::format::sample::Type;
        let frames = interleaved.len() / channels;
        match format {
            ffmpeg::format::Sample::I16(Type::Planar) => {
                for ch in 0..channels {
                    let plane = frame.plane_mut::<i16>(ch);
                    for (i, slot) in plane.iter_mut().take(frames).enumerate() {
                        *slot = interleaved[i * channels + ch];
                    }
                }
            }
            ffmpeg::format::Sample::I32(Type::Planar) => {
                for ch in 0..channels {
                    let plane = frame.plane_mut::<i32>(ch);
                    for (i, slot) in plane.iter_mut().take(frames).enumerate() {
                        // s32 is full-scale in libav, so an i16 promotes by 16 bits.
                        // Shifting by 8 (as though targeting 24-bit) encodes a signal
                        // ~48 dB too quiet, which still decodes cleanly and sounds wrong.
                        *slot = i32::from(interleaved[i * channels + ch]) << 16;
                    }
                }
            }
            ffmpeg::format::Sample::I16(Type::Packed) => {
                let bytes: Vec<u8> = interleaved.iter().flat_map(|s| s.to_ne_bytes()).collect();
                let dst = frame.data_mut(0);
                let n = bytes.len().min(dst.len());
                dst[..n].copy_from_slice(&bytes[..n]);
            }
            ffmpeg::format::Sample::I32(Type::Packed) => {
                let bytes: Vec<u8> = interleaved
                    .iter()
                    .flat_map(|s| (i32::from(*s) << 16).to_ne_bytes())
                    .collect();
                let dst = frame.data_mut(0);
                let n = bytes.len().min(dst.len());
                dst[..n].copy_from_slice(&bytes[..n]);
            }
            // What ffmpeg's native AAC encoder takes. Full scale is ±1.0 rather than
            // ±32767, so this is the one conversion here that is a divide and not a shift
            // — encoding an i16's raw value as a float would clip every sample to the
            // rails and produce a square wave that still decodes.
            ffmpeg::format::Sample::F32(Type::Planar) => {
                for ch in 0..channels {
                    let plane = frame.plane_mut::<f32>(ch);
                    for (i, slot) in plane.iter_mut().take(frames).enumerate() {
                        *slot = f32::from(interleaved[i * channels + ch]) / 32768.0;
                    }
                }
            }
            ffmpeg::format::Sample::F32(Type::Packed) => {
                let bytes: Vec<u8> = interleaved
                    .iter()
                    .flat_map(|s| (f32::from(*s) / 32768.0).to_ne_bytes())
                    .collect();
                let dst = frame.data_mut(0);
                let n = bytes.len().min(dst.len());
                dst[..n].copy_from_slice(&bytes[..n]);
            }
            other => panic!("test helper cannot fill {other:?}"),
        }
    }

    /// Encode PCM with ffmpeg's own encoder so the decode test has real bytes to chew
    /// on rather than a hand-rolled approximation of the format.
    ///
    /// Returns an empty vec if this build has no encoder for the codec, so the tests
    /// skip rather than fail on an ffmpeg compiled without it.
    pub(crate) fn encode(codec: AudioCodec, rate: u32, pcm: &[i16]) -> Vec<EncodedFrame> {
        encode_with_config(codec, rate, pcm).0
    }

    /// The same, and the encoder's own extradata beside it.
    ///
    /// ALAC cannot be decoded without it — libavcodec's `alac_decode_init` refuses to
    /// open on fewer than 36 bytes — and a *hand-built* cookie is not enough either: the
    /// `frame_length` field has to be the one the encoder actually packed, and a cookie
    /// that gets it wrong decodes to silence or static rather than to an error. In the
    /// real path this comes off the sender's SDP (`a=fmtp:96 352 0 16 40 10 14 2 255 …`),
    /// which is the same information from the other side of the wire.
    pub(crate) fn encode_with_config(
        codec: AudioCodec,
        rate: u32,
        pcm: &[i16],
    ) -> (Vec<EncodedFrame>, Vec<u8>) {
        ensure_init();
        let id = codec_id(codec).unwrap();
        let Some(found) = ffmpeg::encoder::find(id) else {
            return (Vec::new(), Vec::new());
        };
        let Ok(audio_codec) = found.audio() else {
            return (Vec::new(), Vec::new());
        };
        // Ask the encoder what it accepts instead of assuming s16.
        let Some(format) = audio_codec.formats().and_then(|mut f| f.next()) else {
            return (Vec::new(), Vec::new());
        };

        let mut context = ffmpeg::codec::context::Context::new_with_codec(found);
        // SAFETY: freshly allocated, exclusively owned, not yet opened; same reasoning
        // as `AudioDecoder::new`.
        unsafe {
            let raw = context.as_mut_ptr();
            (*raw).sample_rate = i32::try_from(rate).unwrap();
            (*raw).sample_fmt = format.into();
            ffmpeg_sys_next::av_channel_layout_default(std::ptr::addr_of_mut!((*raw).ch_layout), 2);
        }
        let Ok(mut encoder) = context.encoder().audio().and_then(|e| e.open_as(found)) else {
            return (Vec::new(), Vec::new());
        };

        // The magic cookie the encoder wrote when it opened. SAFETY: `encoder` is open and
        // exclusively owned, so `extradata` is either null or a valid buffer of
        // `extradata_size` bytes for as long as this borrow lasts.
        let config = unsafe {
            let raw = encoder.as_ptr();
            let (ptr, len) = ((*raw).extradata, (*raw).extradata_size);
            match usize::try_from(len) {
                Ok(len) if !ptr.is_null() && len > 0 => {
                    std::slice::from_raw_parts(ptr, len).to_vec()
                }
                _ => Vec::new(),
            }
        };

        // A frame_size of 0 means "any size"; pick something sane so the loop terminates.
        let block = match encoder.frame_size() {
            0 => 1024,
            n => n as usize,
        };
        let layout = encoder.channel_layout();
        let mut frames = Vec::new();
        let mut pushed = 0i64;
        for chunk in pcm.chunks(block * 2) {
            if chunk.len() < block * 2 {
                break;
            }
            let mut frame = ffmpeg::frame::Audio::new(format, block, layout);
            fill_frame(&mut frame, format, 2, chunk);
            frame.set_pts(Some(pushed));
            pushed += i64::try_from(block).unwrap_or(i64::MAX);
            if encoder.send_frame(&frame).is_err() {
                break;
            }

            let mut packet = ffmpeg::Packet::empty();
            while encoder.receive_packet(&mut packet).is_ok() {
                if let Some(data) = packet.data() {
                    frames.push(EncodedFrame {
                        video_codec: None,
                        audio_codec: Some(codec),
                        pts: Duration::ZERO,
                        keyframe: true,
                        data: bytes::Bytes::copy_from_slice(data),
                    });
                }
            }
        }
        (frames, config)
    }

    /// Root-mean-square of a signal, as a rough loudness measure.
    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let n = samples.len() as f32;
        (samples.iter().map(|s| s * s).sum::<f32>() / n).sqrt()
    }

    /// A multi-tone sweep: what the fidelity tests below send.
    ///
    /// Four tones rather than one sine, because a single sine is the one signal that
    /// survives almost every way a codec path can be wrong. A decode that dropped every
    /// other block, ran at half rate, or reversed time still produces a 440 Hz tone at the
    /// right level — and RMS, which is what these tests used to assert, cannot see any of
    /// it. Spread across the band so a filterbank that lost a subband shows up, and with a
    /// short silent lead-in so "best lag" is a question with an answer.
    pub(crate) fn sweep(rate: u32, seconds: f32) -> Vec<i16> {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let frames = (rate as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(frames * 2);
        for n in 0..frames {
            #[allow(clippy::cast_precision_loss)]
            let t = n as f32 / rate as f32;
            // Silent for the first 50 ms. Lag has to be measurable, and a signal that
            // starts at full amplitude in sample zero gives every alignment the same score
            // over its steady-state region.
            let envelope = if t < 0.05 { 0.0 } else { 1.0 };
            let s: f32 = [220.0f32, 1000.0, 3500.0, 7000.0]
                .iter()
                .map(|f| (t * f * std::f32::consts::TAU).sin())
                .sum::<f32>()
                / 4.0;
            // Left and right differ, deliberately: identical channels make a swap and a
            // mono collapse invisible, and both are real failures — a planar frame read
            // through a packed accessor produces exactly the second.
            #[allow(clippy::cast_possible_truncation)]
            let left = (s * envelope * 11_000.0) as i16;
            #[allow(clippy::cast_possible_truncation)]
            let right = ((t * 660.0 * std::f32::consts::TAU).sin() * envelope * 11_000.0) as i16;
            out.push(left);
            out.push(right);
        }
        out
    }

    /// Normalised cross-correlation of two signals at zero lag, in `[-1, 1]`.
    ///
    /// 1.0 is "the same shape at any scale", 0.0 is unrelated, −1.0 is inverted. Scale
    /// invariance is the point: a codec is allowed to change the level a little and is not
    /// allowed to change the waveform.
    fn correlation(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        if n == 0 {
            return 0.0;
        }
        let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
        for i in 0..n {
            let (x, y) = (f64::from(a[i]), f64::from(b[i]));
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            (dot / (na.sqrt() * nb.sqrt())) as f32
        }
    }

    /// The best correlation over lags in `0..=max_lag`, and the lag that achieved it.
    ///
    /// A lag search rather than a fixed offset because every codec here has its own
    /// latency — SBC's filterbank, aptX's four-sample block, AAC's 1024-sample window plus
    /// encoder priming — and none of them is the thing under test. What *is* under test is
    /// that some single alignment explains the whole signal.
    fn best_alignment(reference: &[f32], decoded: &[f32], max_lag: usize) -> (f32, usize) {
        let mut best = (0.0f32, 0usize);
        for lag in 0..=max_lag {
            if lag >= decoded.len() {
                break;
            }
            let score = correlation(reference, &decoded[lag..]);
            if score > best.0 {
                best = (score, lag);
            }
        }
        best
    }

    /// Split interleaved stereo into its two channels.
    fn channels(interleaved: &[f32]) -> (Vec<f32>, Vec<f32>) {
        (
            interleaved.iter().step_by(2).copied().collect(),
            interleaved.iter().skip(1).step_by(2).copied().collect(),
        )
    }

    /// Decode every frame and flush, collecting the PCM and what the decoder said it was.
    fn decode_all(
        codec: AudioCodec,
        rate: u32,
        frames: &[EncodedFrame],
        config: Option<&[u8]>,
    ) -> (Vec<f32>, Option<(u32, u16)>) {
        let mut decoded = Vec::new();
        let mut reported = None;
        let mut decoder = AudioDecoder::new(codec, format(rate, 2), config).unwrap();
        for frame in frames {
            decoder
                .decode(frame, |b| {
                    reported = Some((b.sample_rate, b.channels));
                    decoded.extend_from_slice(&b.samples);
                })
                .unwrap();
        }
        // Flushing is not optional: libav decoders hold frames back, and a session that
        // never flushes loses whatever was still in the pipeline when the phone stopped.
        decoder
            .flush(|b| decoded.extend_from_slice(&b.samples))
            .unwrap();
        (decoded, reported)
    }

    /// Every codec the panel advertises decodes to the waveform that went in.
    ///
    /// This is the assertion the per-codec tests were reaching for and could not make.
    /// `rms(&decoded) > 0.05` passes for a channel swap, a phase inversion, a wrong sample
    /// rate at the right level, a time-reversed decode, and a decode that dropped every
    /// other block — all of which are silence-or-noise in the room and green here. LDAC's
    /// `EXPECTED_RMS = 0.35..0.65` is the same shape with tighter bounds; aptX's per-channel
    /// balance check was the only one that could see a mono collapse (#187).
    ///
    /// Normalised cross-correlation at the best lag instead, **per channel**, which is what
    /// makes a swap and a collapse visible: the two channels carry different tones, so a
    /// decode that put the left signal in both ears scores badly on the right.
    ///
    /// One caveat stated plainly, because it bounds what this proves: the input is our own
    /// encoder's output, so a misunderstanding shared between our encode and decode
    /// configuration is still invisible. The fix for that is a capture from a real phone,
    /// which for AAC exists and is used by `aac_from_a_real_iphone_decodes_to_audio`.
    #[test]
    fn every_advertised_codec_decodes_to_the_waveform_that_went_in() {
        // 0.98 rather than 0.99: SBC at its default bitpool is genuinely lossy in a way
        // that shows here, and the threshold has to be one the *worst* honest codec
        // clears. Everything this is meant to catch scores far below either number —
        // a channel swap on this signal is ~0.0, an inversion is −1.0, and half-rate is
        // ~0.1. Measured on the dev box: SBC 0.985/0.990, aptX 0.999/0.999,
        // aptX HD 0.9999/0.9999, AAC 0.996/0.997.
        const FLOOR: f32 = 0.98;
        // 20 ms of slack for codec latency, which is generous: the longest here is AAC's
        // 1024-sample window at 44.1 kHz, about 23 ms — so the search window is 40 ms and
        // the assertion is that the best lag is inside 20 ms of *something*, not that it
        // is zero.
        let rate = 44_100;
        let max_lag_frames = rate as usize * 40 / 1000;

        let pcm = sweep(rate, 1.0);
        let (ref_l, ref_r) = channels(
            &pcm.iter()
                .map(|s| f32::from(*s) / 32768.0)
                .collect::<Vec<_>>(),
        );

        for codec in [
            AudioCodec::Sbc,
            AudioCodec::AptX,
            AudioCodec::AptXHd,
            // The one with no decode coverage at all before this, and the one every
            // iPhone picks. It sat 4th in `advertised()` and was offered whenever ffmpeg
            // reported it, so an ffmpeg build without the AAC decoder gave every iPhone in
            // the room a connected session and silence.
            AudioCodec::Aac,
        ] {
            let frames = encode(codec, rate, &pcm);
            if !crate::test_media::available(&format!("a {codec:?} encoder"), !frames.is_empty()) {
                continue;
            }

            let (decoded, reported) = decode_all(codec, rate, &frames, None);
            assert!(!decoded.is_empty(), "{codec:?} produced no audio");
            assert_eq!(
                reported,
                Some((rate, 2)),
                "{codec:?}: rate and channels must survive the round trip"
            );

            let (got_l, got_r) = channels(&decoded);
            let (score_l, lag_l) = best_alignment(&ref_l, &got_l, max_lag_frames);
            let (score_r, lag_r) = best_alignment(&ref_r, &got_r, max_lag_frames);

            assert!(
                score_l >= FLOOR,
                "{codec:?} left channel correlates {score_l:.4} with what was sent \
                 (at lag {lag_l}); anything below {FLOOR} is a different waveform, not a \
                 lossy one"
            );
            assert!(
                score_r >= FLOOR,
                "{codec:?} right channel correlates {score_r:.4} with what was sent \
                 (at lag {lag_r}); the two channels carry different tones, so this is what \
                 fails on a swap or a mono collapse"
            );
            // The same alignment for both, within a millisecond. Channels that need
            // different lags is a decoder that has desynchronised them, which is audible
            // as a smeared image long before it is audible as an error.
            let skew = lag_l.abs_diff(lag_r);
            assert!(
                skew < rate as usize / 1000,
                "{codec:?}: the channels align at different lags ({lag_l} vs {lag_r})"
            );
        }
    }

    /// #14's invariant, for the codec it was most likely to be wrong about.
    ///
    /// `the_codecs_we_advertise_are_the_codecs_we_can_decode` iterated `[Sbc, AptX,
    /// AptXHd]`. AAC was absent — no RMS test, no level test, not even a `can_decode`
    /// assertion — while sitting 4th in `advertised()` and offered whenever ffmpeg reports
    /// it. That is the exact failure #14 exists to prevent, for the most common sender
    /// there is (#187).
    #[test]
    fn aac_is_decodable_because_it_is_what_every_iphone_picks() {
        assert!(
            can_decode(AudioCodec::Aac),
            "AAC is advertised to every phone that asks; a build that cannot decode it \
             gives every iPhone in the room a connected session and silence"
        );
    }

    #[test]
    fn aptx_decodes_to_audio_that_sounds_like_what_went_in() {
        // Asserts on the *audio*, not on the absence of errors — the same discipline as
        // the hwaccel colour test. Nearly every way this path breaks still produces
        // samples: wrong channel layout halves the rate, a missed planar conversion
        // yields one channel of noise, and the wrong sample format is silence.
        let rate = 44_100;
        let pcm = sine(rate, 44_100);
        let frames = encode(AudioCodec::AptX, rate, &pcm);
        if !crate::test_media::available("an aptX encoder", !frames.is_empty()) {
            return;
        }

        let mut decoded: Vec<f32> = Vec::new();
        let mut reported = None;
        let mut decoder = AudioDecoder::new(AudioCodec::AptX, format(rate, 2), None).unwrap();
        for frame in &frames {
            decoder
                .decode(frame, |block| {
                    reported = Some((block.sample_rate, block.channels));
                    decoded.extend_from_slice(&block.samples);
                })
                .unwrap();
        }
        decoder
            .flush(|block| decoded.extend_from_slice(&block.samples))
            .unwrap();

        assert!(!decoded.is_empty(), "aptX produced no audio");
        assert_eq!(reported, Some((rate, 2)), "rate and channels must survive");

        // aptX is 4:1 and quite faithful; the decoded signal should be close in level to
        // the 12000/32768 ≈ 0.366 peak sine that went in (RMS ≈ 0.259).
        let level = rms(&decoded);
        assert!(
            (0.15..0.40).contains(&level),
            "decoded level {level} is not in the range a 440 Hz sine should produce"
        );

        // Silence would also pass a "no error" check. It must not pass this one.
        assert!(
            decoded.iter().any(|s| s.abs() > 0.05),
            "decoded audio is silent"
        );

        // And *both* channels must carry it. aptX decodes planar, and reading a planar
        // frame through an accessor that takes its length from `linesize[n]` gives an
        // empty slice for every plane after the first — audio in the left ear, silence in
        // the right. The interleaved RMS above still passes with half the samples zeroed,
        // which is exactly how that shipped.
        let left = rms(&decoded.iter().step_by(2).copied().collect::<Vec<_>>());
        let right = rms(&decoded
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect::<Vec<_>>());
        assert!(
            right > 0.05,
            "the right channel is silent: left {left}, right {right}"
        );
        assert!(
            (left - right).abs() < 0.05,
            "a mono sine should decode evenly: left {left}, right {right}"
        );
    }

    #[test]
    fn sbc_round_trips_because_every_sender_falls_back_to_it() {
        let rate = 44_100;
        let pcm = sine(rate, 44_100);
        let frames = encode(AudioCodec::Sbc, rate, &pcm);
        if !crate::test_media::available("an SBC encoder", !frames.is_empty()) {
            return;
        }
        let mut decoded = Vec::new();
        // SBC is self-describing, so the negotiated format is not needed to open it —
        // but passing the wrong one must not break it either.
        let mut decoder = AudioDecoder::new(AudioCodec::Sbc, format(44_100, 2), None).unwrap();
        for frame in &frames {
            decoder
                .decode(frame, |b| decoded.extend_from_slice(&b.samples))
                .unwrap();
        }
        // Flushing is not optional: libav decoders hold frames back, and a session that
        // never flushes loses whatever was still in the pipeline when the phone stopped.
        decoder
            .flush(|b| decoded.extend_from_slice(&b.samples))
            .unwrap();
        assert!(!decoded.is_empty(), "SBC produced no audio");
        assert!(rms(&decoded) > 0.05, "SBC output is silent");
    }

    /// The panel's device, for a test that wants to know what actually came out of it.
    ///
    /// It sits under a mixer rather than under a session, because since #111 a session
    /// does not hold a device. That means it also receives the silence the mixer pads
    /// with whenever no source has anything to say — so "how much audio played" is the
    /// *span* between the first and last audible sample, not a running total of frames.
    #[derive(Default)]
    struct Speaker {
        heard: std::sync::Mutex<Vec<f32>>,
        peak: std::sync::Mutex<f32>,
    }

    impl Speaker {
        /// How long the audible part of what reached the device lasted.
        fn played(&self) -> Duration {
            let heard = self.heard.lock().expect("poisoned");
            let audible = |(_, s): &(usize, &f32)| s.abs() > 1e-3;
            let mut hits = heard.iter().enumerate().filter(audible);
            let Some((first, _)) = hits.next() else {
                return Duration::ZERO;
            };
            let last = hits.next_back().map_or(first, |(i, _)| i);
            let frames = (last - first) / usize::from(crate::mixer::CHANNELS);
            Duration::from_nanos(
                (frames as u64).saturating_mul(1_000_000_000) / u64::from(crate::mixer::RATE),
            )
        }

        /// A mixer that plays through this speaker.
        fn mixer(self: &std::sync::Arc<Self>) -> crate::mixer::AudioMixer {
            let device = std::sync::Arc::clone(self);
            crate::mixer::AudioMixer::new(std::sync::Arc::new(move || {
                Box::new(std::sync::Arc::clone(&device))
            }))
        }
    }

    impl crate::audio_out::AudioOut for std::sync::Arc<Speaker> {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), PipelineError> {
            Ok(())
        }
        fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
            self.heard
                .lock()
                .expect("poisoned")
                .extend_from_slice(&block.samples);
            let loudest = block.samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
            if let Ok(mut peak) = self.peak.lock() {
                *peak = peak.max(loudest);
            }
            Ok(())
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn encoded_frames_from_a_phone_reach_the_output_as_sound() {
        // The assertion nothing anywhere was making: that a single audio sample left the
        // box. Every test up to here proved a *layer* — the adapter emits `EncodedFrame`s,
        // the decoder turns bytes into samples, the session does not hang — and none of
        // them proved the join. `selfplay` has the same hole from the other end: it
        // asserts Spotify's cloud reports `is_playing`, which is echoed back from our own
        // state machine and is true of a receiver making no noise at all.
        //
        // SBC because it is the one codec every sender falls back to and the one we are
        // required to support, so this runs on any build that can decode anything.
        let rate = 44_100;
        let frames = encode(AudioCodec::Sbc, rate, &sine(rate, 44_100));
        if !crate::test_media::available("an SBC encoder", !frames.is_empty()) {
            return;
        }

        let (tx, rx) = tokio::sync::mpsc::channel(frames.len() + 1);
        for frame in frames {
            tx.blocking_send(frame).unwrap();
        }
        drop(tx);

        let speaker = std::sync::Arc::new(Speaker::default());
        let mixer = speaker.mixer();
        crate::audio_session::run(
            rx,
            format(rate, 2),
            None,
            mixer.input(crate::mixer::Backpressure::Pull),
            &std::sync::atomic::AtomicBool::new(false),
            None,
        );
        // The session has handed everything over; give the mixer time to play it out.
        std::thread::sleep(Duration::from_millis(300));

        let played = speaker.played();
        assert!(played > Duration::ZERO, "not one sample reached the output");
        // A second of audio in, near enough a second of audio out — a session that
        // decoded one frame and stopped would satisfy a bare `> 0`.
        assert!(
            played > Duration::from_millis(500),
            "only {played:?} of a one-second clip reached the output"
        );
        // …and it was *sound*, not a correctly-shaped block of zeros, which is what a
        // wrong sample format or a mis-set gain produces.
        assert!(
            *speaker.peak.lock().unwrap() > 0.05,
            "the output received silence"
        );
    }

    /// An output that will not open — what a WASAPI endpoint does when asked for a rate
    /// it does not have, and what a PipeWire sink does after the panel has gone to sleep
    /// and taken the HDMI node with it (#55).
    #[derive(Debug)]
    struct RefusingOutput;

    impl crate::audio_out::AudioOut for RefusingOutput {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), PipelineError> {
            Err(PipelineError::Audio(
                "the requested stream configuration is not supported by the device".into(),
            ))
        }
        fn write(&mut self, _block: &PcmBlock) -> Result<(), PipelineError> {
            panic!("nothing may be written to an output that refused to start");
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn a_device_that_refuses_costs_the_sound_and_not_the_session() {
        // This assertion is deliberately the opposite of the one it replaced, and the
        // reversal is the point of #111.
        //
        // The bug that motivated the original, measured on the panel: a phone streaming
        // 44.1 kHz aptX HD at a WASAPI endpoint fixed to 48 kHz. `build_output_stream`
        // refused, the session thread returned — and nothing else was told. The adapter
        // went on accepting media into a dead channel for another 46 seconds, logging
        // "audio queue full" once per packet, while the now-playing card claimed to be
        // playing throughout. The fix then was to end the session loudly.
        //
        // A session no longer holds a device, so it cannot be the thing that notices one
        // refusing. The mixer does, falls back to a sink that still keeps time, and
        // retries underneath every source at once — so the session runs to completion and
        // reports no failure. That is what makes a Bluetooth session survive the display
        // sleeping instead of dying with it (#55), and it is why the sharp end of the old
        // failure cannot come back: there is no longer a per-session device to refuse.
        //
        // Resampling (see `crate::resample`) is what stops the refusal happening at all.
        let rate = 44_100;
        let frames = encode(AudioCodec::Sbc, rate, &sine(rate, rate as usize / 4));
        if !crate::test_media::available("an SBC encoder", !frames.is_empty()) {
            return;
        }

        let (tx, rx) = tokio::sync::mpsc::channel(frames.len() + 1);
        for frame in frames {
            tx.blocking_send(frame).unwrap();
        }
        drop(tx);

        let mixer = crate::mixer::AudioMixer::new(std::sync::Arc::new(|| Box::new(RefusingOutput)));
        let reported: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = std::sync::Arc::clone(&reported);
        let started = std::time::Instant::now();
        crate::audio_session::run(
            rx,
            format(rate, 2),
            None,
            mixer.input(crate::mixer::Backpressure::Pull),
            &std::sync::atomic::AtomicBool::new(false),
            Some(Box::new(move |why| {
                *sink.lock().expect("poisoned") = Some(why);
            })),
        );

        assert_eq!(
            *reported.lock().unwrap(),
            None,
            "a device that will not open must not tear the session down with it"
        );
        // And it drained in real time rather than wedging behind a sink that never
        // consumes — a quarter-second clip, paced, with the lead it is allowed to run
        // ahead by taken off the front.
        let taken = started.elapsed();
        assert!(
            taken < Duration::from_secs(2),
            "the session stalled behind a dead device: {taken:?}"
        );
    }

    /// The 36-byte ALAC magic cookie for 44.1 kHz / 16-bit / stereo, 352-frame packets
    /// — the shape `AlacConfig::magic_cookie` builds from an AirPlay `a=fmtp:` line.
    fn alac_magic_cookie() -> Vec<u8> {
        let mut c = vec![0u8; 36];
        c[0..4].copy_from_slice(&36u32.to_be_bytes());
        c[4..8].copy_from_slice(b"alac");
        c[12..16].copy_from_slice(&352u32.to_be_bytes()); // frameLength
        c[17] = 16; // bitDepth
        c[18] = 40; // pb
        c[19] = 10; // mb
        c[20] = 14; // kb
        c[21] = 2; // channels
        c[22..24].copy_from_slice(&255u16.to_be_bytes()); // maxRun
        c[32..36].copy_from_slice(&44_100u32.to_be_bytes());
        c
    }

    #[test]
    fn alac_opens_with_its_magic_cookie_and_not_without_it() {
        // The whole reason `config` exists. libavcodec's alac_decode_init requires at
        // least 36 bytes of extradata and returns AVERROR_INVALIDDATA otherwise, so the
        // failure is at *open*, not on the first packet — an AirPlay session would have
        // negotiated, decrypted and then had nowhere to send its audio.
        let cookie = alac_magic_cookie();
        assert!(
            AudioDecoder::new(AudioCodec::Alac, format(44_100, 2), Some(&cookie)).is_ok(),
            "ALAC should open when handed its magic cookie"
        );
        assert!(
            AudioDecoder::new(AudioCodec::Alac, format(44_100, 2), None).is_err(),
            "ALAC without extradata must fail loudly at open, not silently later"
        );
    }

    /// ALAC decodes to the waveform that went in, not merely opens.
    ///
    /// `alac_opens_with_its_magic_cookie_and_not_without_it` proves libavcodec *opens* a
    /// decoder given our 36 bytes. It does not prove a frame decodes, and AirPlay's audio
    /// path had nothing that did: `raop_session.rs` is the strongest test in that
    /// subsystem — real sockets, a real FairPlay vector, a genuine AES-CTR keystream
    /// assertion — and the "ALAC" payload it carries is the ASCII string
    /// `an ALAC frame..` (#189).
    ///
    /// So the machinery is proven and the media is not: a correct magic cookie with a
    /// wrong `frame_length` decodes to static, and the field symptom is static with a
    /// green journal.
    #[test]
    fn alac_decodes_to_the_waveform_that_went_in() {
        let rate = 44_100;
        let pcm = sweep(rate, 1.0);
        let (frames, cookie) = encode_with_config(AudioCodec::Alac, rate, &pcm);
        if !crate::test_media::available("an ALAC encoder", !frames.is_empty()) {
            return;
        }
        assert!(
            cookie.len() >= 36,
            "an ALAC encoder that writes no magic cookie leaves a decoder nothing to \
             open with; got {} bytes",
            cookie.len()
        );

        // The cookie describes the stream it is handed, and `frame_length` is the field a
        // hand-built one gets wrong: ffmpeg's encoder packs 4096 samples per frame and
        // AirPlay's RAOP uses 352. A decoder told the wrong number reads the next frame's
        // bits as this one's residuals, which is audible as static and is not an error —
        // which is why this takes the encoder's own cookie rather than reconstructing one.
        let (decoded, reported) = decode_all(AudioCodec::Alac, rate, &frames, Some(&cookie));

        assert!(!decoded.is_empty(), "ALAC produced no audio");
        assert_eq!(reported, Some((rate, 2)));

        let (ref_l, ref_r) = channels(
            &pcm.iter()
                .map(|s| f32::from(*s) / 32768.0)
                .collect::<Vec<_>>(),
        );
        let (got_l, got_r) = channels(&decoded);
        let max_lag = rate as usize * 40 / 1000;

        // ALAC is *lossless*, so the bar is higher than the lossy codecs' 0.98 — anything
        // short of near-unity here is a framing error rather than a codec being a codec.
        for (name, reference, got) in [("left", &ref_l, &got_l), ("right", &ref_r, &got_r)] {
            let (score, lag) = best_alignment(reference, got, max_lag);
            assert!(
                score >= 0.999,
                "ALAC {name} correlates {score:.5} at lag {lag}; it is lossless, so this \
                 is a framing error and not a quality one"
            );
        }
    }

    #[test]
    fn a_corrupt_packet_is_skipped_rather_than_ending_the_session() {
        // One bad packet off a radio link must not take the music down.
        let mut decoder = AudioDecoder::new(AudioCodec::AptX, format(44_100, 2), None).unwrap();
        let garbage = EncodedFrame {
            video_codec: None,
            audio_codec: Some(AudioCodec::AptX),
            pts: Duration::ZERO,
            keyframe: true,
            data: bytes::Bytes::from_static(&[0xFF; 3]),
        };
        assert!(decoder.decode(&garbage, |_| {}).is_ok());
    }

    #[test]
    #[cfg(not(feature = "ldac"))]
    fn a_build_without_the_ldac_backend_does_not_claim_ldac() {
        // #14, and the way it actually went wrong: `can_decode` answered the feature flag
        // rather than "is there a decoder", so a build with `--features ldac` advertised an
        // LDAC endpoint, a phone picked it, and every packet failed. The flag now binds a
        // real backend — but this side of the invariant still has to hold, because
        // `castaway-portable` and the Windows artifact are both built without it.
        assert!(
            !can_decode(AudioCodec::Ldac),
            "there is no LDAC backend in this build"
        );
        let err = AudioDecoder::new(AudioCodec::Ldac, format(44_100, 2), None).unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("ldac"),
            "the error must name the codec; got: {err}"
        );
    }

    #[test]
    #[cfg(feature = "ldac")]
    fn a_build_with_the_ldac_backend_claims_it_and_can_open_it() {
        // The other side, and the part that was a lie for a while: with the feature on
        // there is a decoder, and `can_decode` says so because it *asked the library* — not
        // because the flag is set. `tests/ldac_decode.rs` is where the audio itself is
        // asserted; this is only about the claim.
        assert!(can_decode(AudioCodec::Ldac));
        assert!(AudioDecoder::new(AudioCodec::Ldac, format(44_100, 2), None).is_ok());
    }

    #[test]
    fn the_codecs_we_advertise_are_the_codecs_we_can_decode() {
        // The invariant #14 turns on. If this fails, some phone will negotiate a codec
        // that produces silence.
        for codec in [
            AudioCodec::Sbc,
            AudioCodec::AptX,
            AudioCodec::AptXHd,
            AudioCodec::Aac,
        ] {
            assert!(can_decode(codec), "{codec:?} must be decodable");
        }
    }

    #[test]
    fn a_whole_session_turns_encoded_frames_into_played_audio() {
        // The end-to-end claim, without a sound card: aptX frames go in one end of the
        // audio session and a second of audio comes out the other, with the duration the
        // input implies. A path that silently produced nothing would pass every test
        // that only checks for errors.

        let rate = 44_100;
        let frames = encode(AudioCodec::AptX, rate, &sine(rate, rate as usize));
        if !crate::test_media::available("an aptX encoder", !frames.is_empty()) {
            return;
        }

        let (tx, rx) = tokio::sync::mpsc::channel(frames.len() + 1);
        for frame in frames {
            tx.blocking_send(frame).unwrap();
        }
        drop(tx);

        let speaker = std::sync::Arc::new(Speaker::default());
        // The mixer stays *here*, not in the worker: dropping it stops its thread, and the
        // last of the in-flight audio would never be played out.
        let mixer = speaker.mixer();
        let input = mixer.input(crate::mixer::Backpressure::Pull);
        // Run on a worker thread: the session blocks, which is exactly why it gets one.
        std::thread::spawn(move || {
            let stop = std::sync::atomic::AtomicBool::new(false);
            crate::audio_session::run(rx, format(44_100, 2), None, input, &stop, None);
        })
        .join()
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));

        let played = speaker.played();
        // aptX is fixed-rate, so a second in should be about a second out.
        //
        // The window is tight on purpose, because since #111 this is also where a session
        // inventing a rate would show up. The device runs at the mix rate whatever the
        // source does, so a wrong rate no longer reaches it as a wrong *format* — it
        // reaches it as the wrong *duration*: 44 100 frames declared as 48 kHz skip the
        // resampler and play in 919 ms. That is the #70 failure, and the only thing here
        // that can still see it.
        assert!(
            played >= Duration::from_millis(950) && played <= Duration::from_millis(1_060),
            "played {played:?}, expected about a second — a short read here means the \
             session played the stream at a rate the samples did not state"
        );
    }

    #[test]
    fn a_pcm_block_reports_its_own_duration() {
        let block = PcmBlock {
            sample_rate: 48_000,
            channels: 2,
            samples: vec![0.0; 96_000],
            pts: Duration::ZERO,
        };
        assert_eq!(block.frame_count(), 48_000);
        assert_eq!(block.duration(), Duration::from_secs(1));
    }
}
