//! libav (ffmpeg) decode → RGBA [`DecodedFrame`]s, converted with swscale so frames can
//! be uploaded straight into the [`crate::wgpu_compositor::WgpuCompositor`].
//!
//! Two entry points, for the two shapes media arrives in:
//! - [`decode`] opens a URL/file and demuxes it itself — the `Play(url)` path.
//! - [`decode_stream`] has no container at all: a mirroring adapter has already
//!   depacketized and decrypted, so it hands over frames and names the codec.
//!
//! Decode is blocking and CPU/thread-affine, so both run on a dedicated thread or
//! `spawn_blocking`, never on the tokio runtime (ground rule 4). Frames are pushed:
//! `on_frame` is called per frame and returning `false` stops early (teardown /
//! drop-late). Nothing here touches tokio — [`decode_stream`] *pulls* its input through a
//! caller-supplied closure precisely so the choice of channel stays with the caller.
//! Hardware decode (vaapi/d3d11va) is a later refinement on the same seam.

use std::sync::Once;
use std::time::Duration;

use castaway_core::{DecodedFrame, EncodedFrame, PixelFormat, VideoCodec};
use ffmpeg_next as ffmpeg;
use tracing::warn;

use crate::error::PipelineError;

/// swscale quality/speed tradeoff, shared by both entry points.
const SCALE_FLAGS: ffmpeg::software::scaling::flag::Flags =
    ffmpeg::software::scaling::flag::Flags::BILINEAR;

/// The timebase [`decode_stream`] hands the decoder. [`EncodedFrame::pts`] is already a
/// [`Duration`], so microseconds are just a unit conversion — and letting the decoder
/// carry the timestamp through its reorder buffer means B-frames come back out labelled
/// correctly instead of us guessing which input a decoded frame came from.
const STREAM_TIMEBASE: ffmpeg::Rational = ffmpeg::Rational(1, 1_000_000);

static INIT: Once = Once::new();

fn ensure_init() {
    INIT.call_once(|| {
        // Errors here are effectively unreachable; log via ffmpeg's own registration.
        let _ = ffmpeg::init();
    });
}

fn map_err(e: ffmpeg::Error) -> PipelineError {
    PipelineError::Decode(e.to_string())
}

/// Decode the video stream at `uri`, invoking `on_frame` for each RGBA frame. Returns
/// when the stream ends or `on_frame` returns `false`.
///
/// # Errors
/// [`PipelineError::Decode`] on open/decode failure.
pub fn decode<F>(uri: &str, mut on_frame: F) -> Result<(), PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
{
    ensure_init();

    let mut ictx = ffmpeg::format::input(&uri).map_err(map_err)?;
    let input = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(PipelineError::Decode("no video stream".into()))?;
    let stream_index = input.index();
    let time_base = input.time_base();

    let decoder_ctx =
        ffmpeg::codec::context::Context::from_parameters(input.parameters()).map_err(map_err)?;
    let mut decoder = decoder_ctx.decoder().video().map_err(map_err)?;

    let mut scaler = ffmpeg::software::scaling::context::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg::format::Pixel::RGBA,
        decoder.width(),
        decoder.height(),
        SCALE_FLAGS,
    )
    .map_err(map_err)?;

    let mut keep_going = true;

    let drain = |decoder: &mut ffmpeg::decoder::Video,
                 scaler: &mut ffmpeg::software::scaling::Context,
                 on_frame: &mut F|
     -> Result<bool, PipelineError> {
        let mut decoded = ffmpeg::frame::Video::empty();
        while decoder.receive_frame(&mut decoded).is_ok() {
            let mut rgba = ffmpeg::frame::Video::empty();
            scaler.run(&decoded, &mut rgba).map_err(map_err)?;
            let frame = to_decoded_frame(&rgba, decoded.pts(), time_base);
            if !on_frame(frame) {
                return Ok(false);
            }
        }
        Ok(true)
    };

    for (stream, packet) in ictx.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(map_err)?;
        if !drain(&mut decoder, &mut scaler, &mut on_frame)? {
            keep_going = false;
            break;
        }
    }
    if keep_going {
        decoder.send_eof().map_err(map_err)?;
        drain(&mut decoder, &mut scaler, &mut on_frame)?;
    }
    Ok(())
}

/// Which ffmpeg decoder a negotiated codec asks for.
fn codec_id(codec: VideoCodec) -> Result<ffmpeg::codec::Id, PipelineError> {
    match codec {
        VideoCodec::H264 => Ok(ffmpeg::codec::Id::H264),
        VideoCodec::Hevc => Ok(ffmpeg::codec::Id::HEVC),
        VideoCodec::Vp8 => Ok(ffmpeg::codec::Id::VP8),
        // `VideoCodec` is `#[non_exhaustive]`, so a codec added to `core` cannot break
        // this build — but it must not silently render black either. Fail loudly.
        other => Err(PipelineError::Decode(format!(
            "no ffmpeg decoder mapped for {other:?}"
        ))),
    }
}

/// Decode a live stream of already-depacketized [`EncodedFrame`]s (the mirroring path).
///
/// There is no container and no demuxer here: the protocol adapter negotiated `codec`
/// and owns the frame boundaries, so all that is left is to feed the decoder. `next` is
/// called for each frame and returns `None` when the source is finished; `on_frame` takes
/// each decoded RGBA frame and returns `false` to stop.
///
/// Pull-based on purpose — `next` blocks on whatever channel the caller owns, from a
/// thread the caller picked, which keeps this module free of tokio (ground rule 4).
///
/// Decoding starts at the first key frame: a mirror session is joined mid-stream far more
/// often than not, and feeding a decoder frames that reference pictures it never saw only
/// produces garbage.
///
/// # Errors
/// [`PipelineError::Decode`] if `codec` has no decoder in this ffmpeg build, or the
/// decoder cannot be opened. A frame the decoder rejects is logged and skipped rather
/// than fatal — one corrupt packet must not tear down a live mirror.
pub fn decode_stream<N, F>(
    codec: VideoCodec,
    mut next: N,
    mut on_frame: F,
) -> Result<(), PipelineError>
where
    N: FnMut() -> Option<EncodedFrame>,
    F: FnMut(DecodedFrame) -> bool,
{
    ensure_init();

    let id = codec_id(codec)?;
    let found = ffmpeg::decoder::find(id)
        .ok_or_else(|| PipelineError::Decode(format!("this ffmpeg build has no {id:?} decoder")))?;
    let mut context = ffmpeg::decoder::new();
    context.set_packet_time_base(STREAM_TIMEBASE);
    let mut decoder = context
        .open_as(found)
        .map_err(map_err)?
        .video()
        .map_err(map_err)?;

    // Built on the first decoded frame and rebuilt on resize: without stream parameters
    // the picture size is not known until one comes out.
    let mut scaler: Option<ffmpeg::software::scaling::Context> = None;
    let mut synced = false;
    let mut running = true;

    while let Some(frame) = next() {
        if frame.video_codec != Some(codec) {
            warn!(
                got = ?frame.video_codec,
                want = ?codec,
                "mirror decode: skipping a frame in the wrong codec",
            );
            continue;
        }
        if !synced {
            if !frame.keyframe {
                continue;
            }
            synced = true;
        }

        let mut packet = ffmpeg::codec::packet::Packet::copy(&frame.data);
        packet.set_pts(i64::try_from(frame.pts.as_micros()).ok());
        if frame.keyframe {
            packet.set_flags(ffmpeg::codec::packet::Flags::KEY);
        }

        if let Err(e) = decoder.send_packet(&packet) {
            warn!(error = %e, "mirror decode: decoder rejected a frame, skipping it");
            continue;
        }
        if !drain_stream(&mut decoder, &mut scaler, &mut on_frame)? {
            running = false;
            break;
        }
    }

    if running {
        decoder.send_eof().map_err(map_err)?;
        drain_stream(&mut decoder, &mut scaler, &mut on_frame)?;
    }
    Ok(())
}

/// Pull every frame the decoder is holding, scale it to RGBA, and hand it over. Returns
/// `false` once `on_frame` asks to stop.
fn drain_stream<F>(
    decoder: &mut ffmpeg::decoder::Video,
    scaler: &mut Option<ffmpeg::software::scaling::Context>,
    on_frame: &mut F,
) -> Result<bool, PipelineError>
where
    F: FnMut(DecodedFrame) -> bool,
{
    let mut decoded = ffmpeg::frame::Video::empty();
    while decoder.receive_frame(&mut decoded).is_ok() {
        let (format, width, height) = (decoded.format(), decoded.width(), decoded.height());
        // `cached` is a no-op when the parameters match and a rebuild when they do not,
        // which is what a sender changing resolution mid-mirror needs.
        let fresh = match scaler.take() {
            Some(mut sws) => {
                sws.cached(
                    format,
                    width,
                    height,
                    ffmpeg::format::Pixel::RGBA,
                    width,
                    height,
                    SCALE_FLAGS,
                );
                sws
            }
            None => ffmpeg::software::scaling::context::Context::get(
                format,
                width,
                height,
                ffmpeg::format::Pixel::RGBA,
                width,
                height,
                SCALE_FLAGS,
            )
            .map_err(map_err)?,
        };
        let sws = scaler.insert(fresh);

        let mut rgba = ffmpeg::frame::Video::empty();
        sws.run(&decoded, &mut rgba).map_err(map_err)?;
        if !on_frame(to_decoded_frame(&rgba, decoded.pts(), STREAM_TIMEBASE)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Copy a scaled RGBA `ffmpeg` frame into an owned [`DecodedFrame`], stripping row
/// padding (swscale may pad the stride past `width*4`).
fn to_decoded_frame(
    rgba: &ffmpeg::frame::Video,
    pts: Option<i64>,
    time_base: ffmpeg::Rational,
) -> DecodedFrame {
    let width = rgba.width();
    let height = rgba.height();
    let stride = rgba.stride(0);
    let row_bytes = (width as usize) * 4;
    let src = rgba.data(0);

    let mut data = Vec::with_capacity(row_bytes * height as usize);
    for row in 0..height as usize {
        let start = row * stride;
        data.extend_from_slice(&src[start..start + row_bytes]);
    }

    let secs = pts.map_or(0.0, |p| {
        p as f64 * f64::from(time_base.numerator()) / f64::from(time_base.denominator())
    });
    DecodedFrame {
        width,
        height,
        format: PixelFormat::Rgba8,
        pts: Duration::from_secs_f64(secs.max(0.0)),
        data: bytes::Bytes::from(data),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Generate a tiny test clip with the ffmpeg CLI, or skip if it isn't available.
    fn make_test_clip() -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join("castaway-ffmpeg-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("testsrc.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
            ])
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    #[test]
    fn decodes_testsrc_to_rgba_frames() {
        let Some(path) = make_test_clip() else {
            eprintln!("skipping: ffmpeg CLI not available to make a test clip");
            return;
        };
        let mut frames = 0usize;
        let mut first_dims = None;
        decode(path.to_str().unwrap(), |f| {
            if first_dims.is_none() {
                first_dims = Some((f.width, f.height));
                assert_eq!(f.format, PixelFormat::Rgba8);
                assert_eq!(f.data.len(), (f.width * f.height * 4) as usize);
            }
            frames += 1;
            true
        })
        .unwrap();
        assert!(frames >= 5, "expected several frames, got {frames}");
        assert_eq!(first_dims, Some((64, 48)));
    }

    /// Spacing between the timestamps the test attaches to its frames.
    const FRAME_INTERVAL: Duration = Duration::from_millis(100);

    /// Generate a bare Annex-B H.264 elementary stream — no container, so the SPS/PPS is
    /// in-band exactly as a mirroring sender delivers it. `-bf 0` matches how a real
    /// sender encodes: B-frames trade latency for size, which mirroring never wants.
    fn make_test_stream() -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join("castaway-ffmpeg-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("testsrc.h264");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
            ])
            .args(["-pix_fmt", "yuv420p", "-bf", "0", "-f", "h264"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        status.success().then_some(path)
    }

    /// Split that elementary stream into the per-frame units an adapter would hand us.
    /// Demuxing happens *here*, in the test — the point of [`decode_stream`] is that it
    /// never sees a container.
    ///
    /// Timestamps are the test's own, not the demuxer's: a raw elementary stream carries
    /// none, and a real adapter derives them from RTP anyway.
    fn encoded_frames(path: &std::path::Path) -> Vec<EncodedFrame> {
        ensure_init();
        let mut ictx = ffmpeg::format::input(&path).unwrap();
        let index = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .unwrap()
            .index();

        let mut out: Vec<EncodedFrame> = Vec::new();
        for (stream, packet) in ictx.packets() {
            if stream.index() != index {
                continue;
            }
            let Some(data) = packet.data() else { continue };
            out.push(EncodedFrame {
                video_codec: Some(VideoCodec::H264),
                audio_codec: None,
                pts: FRAME_INTERVAL * u32::try_from(out.len()).unwrap(),
                keyframe: packet.is_key(),
                data: bytes::Bytes::copy_from_slice(data),
            });
        }
        out
    }

    #[test]
    fn stream_decode_turns_pushed_frames_into_rgba() {
        let Some(path) = make_test_stream() else {
            eprintln!("skipping: ffmpeg CLI not available to make a test stream");
            return;
        };
        let mut input = encoded_frames(&path).into_iter();
        assert!(input.len() >= 5, "expected several encoded frames");

        let mut dims = None;
        let mut times = Vec::new();
        decode_stream(
            VideoCodec::H264,
            || input.next(),
            |f| {
                if dims.is_none() {
                    dims = Some((f.width, f.height));
                    assert_eq!(f.format, PixelFormat::Rgba8);
                    assert_eq!(f.data.len(), (f.width * f.height * 4) as usize);
                }
                times.push(f.pts);
                true
            },
        )
        .unwrap();

        assert!(
            times.len() >= 5,
            "expected several decoded frames, got {}",
            times.len()
        );
        assert_eq!(dims, Some((64, 48)));

        // The timestamps we attached come back attached to the right pictures. If the
        // decoder were not carrying them, every frame would land at zero and the
        // compositor would have nothing to pace against.
        let want: Vec<_> = (0..times.len())
            .map(|i| FRAME_INTERVAL * u32::try_from(i).unwrap())
            .collect();
        assert_eq!(times, want);
    }

    #[test]
    fn stream_decode_waits_for_a_key_frame_before_starting() {
        let Some(path) = make_test_stream() else {
            return;
        };
        // Join mid-stream, as a receiver attaching to a live mirror does. Everything
        // before the next key frame references pictures we never saw, so nothing may be
        // decoded until one arrives — and this clip has only the one, at the start.
        let mut input = encoded_frames(&path).into_iter().skip(1);
        let mut frames = 0usize;
        decode_stream(
            VideoCodec::H264,
            || input.next(),
            |_f| {
                frames += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(frames, 0, "decoded {frames} frames without ever syncing");
    }

    #[test]
    fn stream_decode_skips_frames_in_another_codec() {
        let Some(path) = make_test_stream() else {
            return;
        };
        // An audio frame on the video source, or a sender that switched codecs without
        // renegotiating: feeding those to an H.264 decoder is how you get a hang or a
        // wall of garbage. They must be dropped, not decoded.
        let mut input = encoded_frames(&path).into_iter().map(|mut f| {
            f.video_codec = Some(VideoCodec::Vp8);
            f
        });
        let mut frames = 0usize;
        decode_stream(
            VideoCodec::H264,
            || input.next(),
            |_f| {
                frames += 1;
                true
            },
        )
        .unwrap();
        assert_eq!(frames, 0);
    }

    #[test]
    fn every_negotiable_video_codec_has_a_decoder() {
        // A codec we advertise in an OFFER/ANSWER but cannot actually decode is a black
        // screen at the far end of a successful handshake — the worst kind of failure.
        ensure_init();
        for codec in [VideoCodec::H264, VideoCodec::Hevc, VideoCodec::Vp8] {
            let id = codec_id(codec).unwrap();
            assert!(
                ffmpeg::decoder::find(id).is_some(),
                "{codec:?} is negotiable but this ffmpeg build cannot decode it",
            );
        }
    }

    #[test]
    fn callback_can_stop_early() {
        let Some(path) = make_test_clip() else {
            return;
        };
        let mut frames = 0usize;
        decode(path.to_str().unwrap(), |_f| {
            frames += 1;
            frames < 2 // stop after the second frame
        })
        .unwrap();
        assert_eq!(frames, 2);
    }
}
