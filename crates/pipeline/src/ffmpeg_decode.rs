//! libav (ffmpeg) decode → RGBA [`DecodedFrame`]s. Opens a URL/file, decodes the best
//! video stream, and converts each frame to RGBA8 with swscale so it can be uploaded
//! straight into the [`crate::wgpu_compositor::WgpuCompositor`].
//!
//! Decode is blocking and CPU/thread-affine, so this runs on a dedicated thread or
//! `spawn_blocking`, never on the tokio runtime (ground rule 4). It's a push model:
//! [`decode`] calls `on_frame` per frame and stops early when the callback returns
//! `false` (teardown / drop-late). Hardware decode (vaapi/d3d11va) is a later
//! refinement on the same seam.

use std::sync::Once;
use std::time::Duration;

use castaway_core::{DecodedFrame, PixelFormat};
use ffmpeg_next as ffmpeg;

use crate::error::PipelineError;

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
        ffmpeg::software::scaling::flag::Flags::BILINEAR,
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
