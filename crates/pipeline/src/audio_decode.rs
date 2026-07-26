//! libav decode for the A2DP codecs: encoded frames in, interleaved f32 PCM out.
//!
//! Four of the five ride on ffmpeg decoders that are already in the build. LDAC is the
//! exception — libav has no decoder and AOSP's `libldac` is encoder-only — so it is
//! gated behind the `ldac` feature and its absence must keep the endpoint out of the
//! advertised table rather than fail at decode time (OPEN-QUESTIONS Q22).
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
#[derive(Debug, Clone, PartialEq)]
pub struct PcmBlock {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Interleaved samples in `-1.0..=1.0`.
    pub samples: Vec<f32>,
    /// Presentation timestamp carried through from the encoded frame.
    pub pts: Duration,
}

impl PcmBlock {
    /// How many sample frames (one per channel-group) this block holds.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// How long this block plays for.
    #[must_use]
    pub fn duration(&self) -> Duration {
        Duration::from_nanos(
            (self.frame_count() as u64)
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(self.sample_rate.max(1)))
                .unwrap_or(0),
        )
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
        // libav has no LDAC decoder and AOSP's libldac is encoder-only, so this needs the
        // reverse-engineered `libldacdec` over FFI. The `ldac` feature reserves the slot;
        // until something is bound behind it there is no decoder, and `can_decode` must
        // say so rather than trusting the flag (Q22).
        AudioCodec::Ldac => Err(PipelineError::Decode(
            "no LDAC decoder is bound in this build; libav has none (Q22)".into(),
        )),
        other => Err(PipelineError::Decode(format!(
            "no audio decoder mapped for {other:?}"
        ))),
    }
}

/// Whether this build can decode `codec`.
///
/// The A2DP endpoint table must be built from this, not from optimism: advertising a
/// codec we cannot decode means the sender picks it and the session is silence rather
/// than a clean fallback to one we can (Q22).
#[must_use]
pub fn can_decode(codec: AudioCodec) -> bool {
    // One source of truth, and it is whether a decoder actually exists. This used to
    // answer `cfg!(feature = "ldac")` for LDAC, which is a different question: the `ldac`
    // feature reserves the slot but binds no decoder, so a build with it on advertised an
    // LDAC endpoint and then failed every packet — the exact silence Q22 is about.
    ensure_init();
    codec_id(codec).is_ok_and(|id| ffmpeg::decoder::find(id).is_some())
}

/// An open decoder for one A2DP stream.
pub struct AudioDecoder {
    decoder: ffmpeg::decoder::Audio,
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
    /// # Errors
    /// [`PipelineError::Decode`] if this build has no decoder for the codec, or the
    /// decoder refuses to open.
    pub fn new(codec: AudioCodec, format: AudioFormat) -> Result<Self, PipelineError> {
        ensure_init();
        let id = codec_id(codec)?;
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

        let decoder = context
            .open_as(found)
            .map_err(map_err)?
            .audio()
            .map_err(map_err)?;

        Ok(Self {
            decoder,
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
        let mut packet = ffmpeg::Packet::copy(&frame.data);
        packet.set_pts(Some(
            i64::try_from(frame.pts.as_micros()).unwrap_or(i64::MAX),
        ));

        // A decoder that refuses everything is a configuration problem, and configuration
        // problems are solved offline against real bytes rather than with a phone in hand.
        // Set CASTAWAY_DUMP_AUDIO to a path to capture the raw frames as they arrive.
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
        if let Err(e) = self.decoder.send_packet(&packet) {
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
        self.drain(&mut on_pcm)
    }

    /// Flush the decoder at end of stream.
    ///
    /// # Errors
    /// [`PipelineError::Decode`] if draining fails.
    pub fn flush(&mut self, mut on_pcm: impl FnMut(PcmBlock)) -> Result<(), PipelineError> {
        let _ = self.decoder.send_eof();
        self.drain(&mut on_pcm)
    }

    fn drain(&mut self, on_pcm: &mut impl FnMut(PcmBlock)) -> Result<(), PipelineError> {
        let mut decoded = ffmpeg::frame::Audio::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let block = self.convert(&decoded)?;
            if !block.samples.is_empty() {
                on_pcm(block);
            }
        }
        Ok(())
    }

    /// Convert whatever the decoder produced into interleaved f32.
    ///
    /// Done by hand rather than through swresample. Every one of these decoders has its
    /// own native layout — aptX comes out planar s32, SBC packed s16, AAC planar float —
    /// and a resampler would be the obvious tool, but ffmpeg 7's channel-layout rework
    /// leaves `swr` comparing a legacy layout mask against a frame carrying the new
    /// `AVChannelLayout`, so it rejects its own decoder's output with "Input changed".
    /// The conversion is a dozen lines and has no state to get out of sync.
    fn convert(&mut self, decoded: &ffmpeg::frame::Audio) -> Result<PcmBlock, PipelineError> {
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
    mut next: N,
    mut on_pcm: F,
) -> Result<(), PipelineError>
where
    N: FnMut() -> Option<EncodedFrame>,
    F: FnMut(PcmBlock) -> bool,
{
    let mut decoder = AudioDecoder::new(codec, format)?;
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
        "no decoder in this build; the endpoint should not have been advertised (Q22)"
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A negotiated format, for tests that already know both numbers are sane.
    fn format(sample_rate: u32, channels: u16) -> AudioFormat {
        AudioFormat::from_hz(sample_rate, channels).unwrap()
    }

    /// A one-second 440 Hz sine at `rate`, interleaved stereo.
    fn sine(rate: u32, frames: usize) -> Vec<i16> {
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
            other => panic!("test helper cannot fill {other:?}"),
        }
    }

    /// Encode PCM with ffmpeg's own encoder so the decode test has real bytes to chew
    /// on rather than a hand-rolled approximation of the format.
    ///
    /// Returns an empty vec if this build has no encoder for the codec, so the tests
    /// skip rather than fail on an ffmpeg compiled without it.
    fn encode(codec: AudioCodec, rate: u32, pcm: &[i16]) -> Vec<EncodedFrame> {
        ensure_init();
        let id = codec_id(codec).unwrap();
        let Some(found) = ffmpeg::encoder::find(id) else {
            return Vec::new();
        };
        let Ok(audio_codec) = found.audio() else {
            return Vec::new();
        };
        // Ask the encoder what it accepts instead of assuming s16.
        let Some(format) = audio_codec.formats().and_then(|mut f| f.next()) else {
            return Vec::new();
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
            return Vec::new();
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
        frames
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

    #[test]
    fn aptx_decodes_to_audio_that_sounds_like_what_went_in() {
        // Asserts on the *audio*, not on the absence of errors — the same discipline as
        // the hwaccel colour test. Nearly every way this path breaks still produces
        // samples: wrong channel layout halves the rate, a missed planar conversion
        // yields one channel of noise, and the wrong sample format is silence.
        let rate = 44_100;
        let pcm = sine(rate, 44_100);
        let frames = encode(AudioCodec::AptX, rate, &pcm);
        if frames.is_empty() {
            eprintln!("this ffmpeg build has no aptX encoder; skipping");
            return;
        }

        let mut decoded: Vec<f32> = Vec::new();
        let mut reported = None;
        let mut decoder = AudioDecoder::new(AudioCodec::AptX, format(rate, 2)).unwrap();
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
        if frames.is_empty() {
            eprintln!("this ffmpeg build has no SBC encoder; skipping");
            return;
        }
        let mut decoded = Vec::new();
        // SBC is self-describing, so the negotiated format is not needed to open it —
        // but passing the wrong one must not break it either.
        let mut decoder = AudioDecoder::new(AudioCodec::Sbc, format(44_100, 2)).unwrap();
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

    #[test]
    fn a_corrupt_packet_is_skipped_rather_than_ending_the_session() {
        // One bad packet off a radio link must not take the music down.
        let mut decoder = AudioDecoder::new(AudioCodec::AptX, format(44_100, 2)).unwrap();
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
    fn ldac_is_not_claimed_as_decodable_just_because_its_feature_is_on() {
        // Q22, and the way it actually went wrong: `can_decode` answered the feature flag
        // rather than "is there a decoder", so a build with `--features ldac` advertised
        // an LDAC endpoint, a phone picked it, and every packet failed. The feature
        // reserves the slot; it does not conjure a decoder.
        assert!(
            !can_decode(AudioCodec::Ldac),
            "no LDAC decoder is bound, whatever the feature says"
        );
        let err = AudioDecoder::new(AudioCodec::Ldac, format(44_100, 2)).unwrap_err();
        assert!(
            format!("{err}").to_lowercase().contains("ldac"),
            "got: {err}"
        );
    }

    #[test]
    fn the_codecs_we_advertise_are_the_codecs_we_can_decode() {
        // The invariant Q22 turns on. If this fails, some phone will negotiate a codec
        // that produces silence.
        for codec in [AudioCodec::Sbc, AudioCodec::AptX, AudioCodec::AptXHd] {
            assert!(can_decode(codec), "{codec:?} must be decodable");
        }
    }

    #[test]
    fn a_whole_session_turns_encoded_frames_into_played_audio() {
        // The end-to-end claim, without a sound card: aptX frames go in one end of the
        // audio session and a second of audio comes out the other, with the duration the
        // input implies. A path that silently produced nothing would pass every test
        // that only checks for errors.
        use crate::audio_out::NullAudioOut;

        let rate = 44_100;
        let frames = encode(AudioCodec::AptX, rate, &sine(rate, rate as usize));
        if frames.is_empty() {
            eprintln!("this ffmpeg build has no aptX encoder; skipping");
            return;
        }

        let (tx, rx) = tokio::sync::mpsc::channel(frames.len() + 1);
        for frame in frames {
            tx.blocking_send(frame).unwrap();
        }
        drop(tx);

        // Run on a worker thread: the session blocks, which is exactly why it gets one.
        let handle = std::thread::spawn(move || {
            let mut out = NullAudioOut::new();
            // `run` takes ownership, so account through a second sink and report back.
            let counted: BlockLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = CountingOut {
                inner: std::mem::take(&mut out),
                blocks: std::sync::Arc::clone(&counted),
            };
            let stop = std::sync::atomic::AtomicBool::new(false);
            crate::audio_session::run(rx, format(44_100, 2), Box::new(sink), &stop);
            let blocks = counted.lock().unwrap().clone();
            blocks
        });
        let blocks = handle.join().unwrap();

        assert!(!blocks.is_empty(), "the session played nothing");
        let total: usize = blocks.iter().map(|(frames, _)| frames).sum();
        let played = Duration::from_nanos((total as u64 * 1_000_000_000) / u64::from(rate));
        // aptX is fixed-rate, so a second in should be about a second out. Allow slack
        // for whatever the encoder held back at the tail.
        assert!(
            played >= Duration::from_millis(900) && played <= Duration::from_millis(1_100),
            "played {played:?}, expected about a second"
        );
        assert_eq!(blocks[0].1, (rate, 2), "format must survive the session");
    }

    /// One block as the test cares about it: how many frames, in what format.
    type BlockLog = std::sync::Arc<std::sync::Mutex<Vec<(usize, (u32, u16))>>>;

    /// A sink that records what it was handed, so the session's output can be asserted.
    struct CountingOut {
        inner: crate::audio_out::NullAudioOut,
        blocks: BlockLog,
    }

    impl crate::audio_out::AudioOut for CountingOut {
        fn start(&mut self, rate: u32, channels: u16) -> Result<(), PipelineError> {
            self.inner.start(rate, channels)
        }
        fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
            self.blocks
                .lock()
                .expect("poisoned")
                .push((block.frame_count(), (block.sample_rate, block.channels)));
            self.inner.write(block)
        }
        fn stop(&mut self) {
            self.inner.stop();
        }
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
