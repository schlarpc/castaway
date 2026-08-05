//! End-to-end proof that hardware decode lands correct pixels without a copy.
//!
//! This is the test the whole Linux hwaccel slice exists to pass. It runs the real chain
//! — libavcodec on VA-API → `av_hwframe_map` to DMA-BUF → Vulkan external-memory import →
//! NV12 per-plane sampling → offscreen readback — on the dev box's GPU, with no window,
//! no panel, and no human (ground rule 6).
//!
//! It asserts on **colour**, not on the absence of errors, and that is the point. Nearly
//! every way this path can be wrong produces a picture rather than a failure:
//!
//! - a wrong DRM format modifier renders the surface with the wrong tiling — a
//!   recognisable but scrambled image;
//! - swapped plane offsets or pitches shift the chroma;
//! - a BT.601 matrix on BT.709 content, or full-range maths on limited-range samples,
//!   just shifts the colour a little;
//! - and `VK_IMAGE_LAYOUT_UNDEFINED` on the import barrier is *permitted* by the spec to
//!   discard the contents entirely, which would come back black.
//!
//! Decoding a known solid colour and checking what comes out the far end catches all of
//! them. An integration test rather than a unit test because it needs its own process:
//! the compositor records its surface-import capability in a process-global that the
//! in-crate tests would race.
//!
//! Skips cleanly — rather than failing — when the box has no GPU, no VA-API, or no ffmpeg
//! CLI to generate the fixture, so CI on a machine without a render node stays green.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use castaway_core::{EncodedFrame, FrameImage, VideoCodec};
use pipeline::compositor::{Compositor as _, Layer, LayerId, Transform};
use pipeline::hwaccel::{import_capability, HwPreference, SurfaceImport};

/// The picture the fixture is filled with. Chosen away from the greys and primaries that
/// a broken conversion tends to land on by accident, and away from the clipping ends of
/// the limited range where several wrong matrices agree.
const SOURCE_RGB: [u8; 3] = [40, 170, 90];

/// Fixture size. Small enough to decode instantly, large enough that the chroma plane is
/// a meaningful 2×2-subsampled thing rather than a rounding artefact.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// Generate a solid-colour Annex-B H.264 elementary stream, the shape a mirroring adapter
/// delivers: no container, in-band SPS/PPS, no B-frames.
///
/// `name` keeps each test on its own file. `nextest` runs these in concurrent processes,
/// and two of them writing one path races a half-written stream into a decoder — which
/// surfaces as an unrelated-looking assertion about frame counts.
fn solid_colour_stream(name: &str) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join("castaway-hwaccel-test");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{name}.h264"));
    let [r, g, b] = SOURCE_RGB;
    let source =
        format!("color=c=0x{r:02X}{g:02X}{b:02X}:size={WIDTH}x{HEIGHT}:rate=10:duration=1");
    let ok = Command::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i", &source])
        // A flat field is essentially free to encode, so a low CRF keeps the colour exact
        // without the encoder smearing it — any drift the test sees then comes from the
        // decode/import/convert path rather than from compression.
        //
        // Explicitly *not* `-qp 0`: that produces lossless High 4:4:4 Predictive, which no
        // fixed-function decoder accepts, so the fixture itself would force the very
        // software fallback the test exists to rule out.
        .args(["-pix_fmt", "yuv420p", "-bf", "0", "-crf", "10"])
        .args(["-profile:v", "main", "-f", "h264"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    ok.then(|| std::fs::read(&path).ok()).flatten()
}

/// One NAL unit: where its start code begins, and its type.
struct Nal {
    start: usize,
    kind: u8,
}

/// Find every NAL unit in an Annex-B stream.
///
/// Both start-code lengths have to be handled — x264 emits the four-byte form for
/// parameter sets and the three-byte form for slices, in the same stream — and a splitter
/// that only knows about one of them silently produces zero access units.
fn find_nals(stream: &[u8]) -> Vec<Nal> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 3 < stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            // Absorb the leading zero of a four-byte start code so the emitted unit keeps
            // its own prefix intact.
            let start = if i > 0 && stream[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            nals.push(Nal {
                start,
                kind: stream[i + 3] & 0x1f,
            });
            i += 3;
        } else {
            i += 1;
        }
    }
    nals
}

/// Split the elementary stream into the per-frame units an adapter would push: each slice
/// NAL together with the parameter sets and SEI that precede it.
fn split_access_units(stream: &[u8]) -> Vec<EncodedFrame> {
    let nals = find_nals(stream);
    let mut frames: Vec<EncodedFrame> = Vec::new();
    let mut unit_start: Option<usize> = None;

    for (n, nal) in nals.iter().enumerate() {
        let end = nals.get(n + 1).map_or(stream.len(), |next| next.start);
        let from = unit_start.unwrap_or(nal.start);
        match nal.kind {
            // A coded slice closes the access unit: 5 is an IDR, 1 a non-IDR picture.
            1 | 5 => {
                let index = u32::try_from(frames.len()).unwrap_or(0);
                frames.push(EncodedFrame {
                    video_codec: Some(VideoCodec::H264),
                    audio_codec: None,
                    pts: Duration::from_millis(100) * index,
                    keyframe: nal.kind == 5,
                    data: bytes::Bytes::copy_from_slice(&stream[from..end]),
                });
                unit_start = None;
            }
            // SPS/PPS/SEI — carried into the next slice's access unit.
            _ => unit_start = Some(from),
        }
    }
    frames
}

/// Read back the composited image and average the middle of it, ignoring a border where
/// bilinear sampling of the half-resolution chroma plane legitimately blends.
fn centre_colour(pixels: &[u8]) -> [f32; 3] {
    let (mut sum, mut count) = ([0.0f32; 3], 0.0f32);
    let margin = 8;
    for y in margin..(HEIGHT as usize - margin) {
        for x in margin..(WIDTH as usize - margin) {
            let i = (y * WIDTH as usize + x) * 4;
            sum[0] += f32::from(pixels[i]);
            sum[1] += f32::from(pixels[i + 1]);
            sum[2] += f32::from(pixels[i + 2]);
            count += 1.0;
        }
    }
    [sum[0] / count, sum[1] / count, sum[2] / count]
}

#[test]
fn vaapi_decode_imports_zero_copy_and_composites_the_right_colour() {
    let Some(stream) = solid_colour_stream("zero-copy") else {
        eprintln!("skipping: no ffmpeg CLI to generate the fixture");
        return;
    };
    let frames = split_access_units(&stream);
    assert!(
        frames.len() >= 5,
        "fixture should have several access units, got {}",
        frames.len(),
    );

    // Opening the compositor is also what opens the interop-capable device and records
    // the capability the decode side consults.
    let Some(mut compositor) = pipeline::test_gpu::compositor(WIDTH, HEIGHT) else {
        return;
    };
    if compositor.surface_import() == SurfaceImport::Unsupported {
        eprintln!("skipping: this device cannot import GPU surfaces");
        return;
    }
    assert_eq!(
        import_capability(),
        compositor.surface_import(),
        "the compositor must publish its capability for the decode side",
    );

    // `HardwareOnly` so a fallback is a test failure rather than a silent pass: the whole
    // risk with this path is that everything keeps working on the CPU and nobody notices
    // the acceleration stopped.
    let mut source = frames.into_iter();
    let mut gpu_frames = 0usize;
    let mut cpu_frames = 0usize;
    let mut last_surface: Option<Arc<dyn castaway_core::GpuSurface>> = None;
    let mut size = (0, 0);

    let outcome = pipeline::ffmpeg_decode::decode_stream(
        VideoCodec::H264,
        HwPreference::HardwareOnly,
        || source.next(),
        |frame| {
            match &frame.image {
                FrameImage::Gpu(surface) => {
                    gpu_frames += 1;
                    size = (frame.width, frame.height);
                    last_surface = Some(Arc::clone(surface));
                }
                FrameImage::Cpu { .. } => cpu_frames += 1,
            }
            true
        },
    );

    match outcome {
        Ok(()) => {}
        Err(e) => {
            // `HardwareOnly` turns an unavailable device into an error. On a box with no
            // VA-API that is the expected answer, not a defect in this code.
            eprintln!("skipping: hardware decode unavailable ({e})");
            return;
        }
    }

    assert_eq!(
        cpu_frames, 0,
        "HardwareOnly must not produce software frames",
    );
    assert!(gpu_frames > 0, "expected at least one GPU surface");
    let surface = last_surface.expect("a GPU frame should have been captured");

    compositor
        .import_surface(LayerId::Video, size.0, size.1, &surface)
        .expect("importing the decoder's surface");
    compositor.upsert_layer(Layer {
        id: LayerId::Video,
        opacity: 1.0,
        transform: Transform::default(),
    });
    compositor.present();

    let pixels = compositor.read_rgba().expect("offscreen readback");
    let got = centre_colour(&pixels);
    let want = SOURCE_RGB.map(f32::from);

    // A generous tolerance: the fixture round-trips RGB → limited-range YUV 4:2:0 → RGB,
    // and 8-bit chroma subsampling alone costs a couple of levels. It is nowhere near
    // wide enough to admit the wrong matrix (tens of levels), the wrong range (~16), a
    // wrong tiling (noise), or a discarded image (black).
    let tolerance = 6.0;
    for (channel, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        assert!(
            (g - w).abs() <= tolerance,
            "channel {channel}: composited {g:.1} vs source {w:.1} (±{tolerance}); \
             full result {got:?} against {want:?}",
        );
    }
}

/// Generate a stream no fixed-function decoder will accept.
///
/// `-qp 0` is lossless H.264 — High 4:4:4 Predictive — which VA-API, D3D11VA and every
/// other hardware decoder refuses. That makes it a genuine fixture for the give-up path
/// rather than a mocked one: libavcodec really does offer `get_format` a list without the
/// hardware format in it, which is exactly how a driver says "not this profile" in the
/// field.
fn undecodable_by_hardware_stream() -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join("castaway-hwaccel-test");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("lossless.h264");
    let [r, g, b] = SOURCE_RGB;
    let source =
        format!("color=c=0x{r:02X}{g:02X}{b:02X}:size={WIDTH}x{HEIGHT}:rate=10:duration=1");
    let ok = Command::new("ffmpeg")
        .args(["-y", "-f", "lavfi", "-i", &source])
        .args(["-pix_fmt", "yuv444p", "-bf", "0", "-qp", "0", "-f", "h264"])
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .success();
    ok.then(|| std::fs::read(&path).ok()).flatten()
}

#[test]
fn a_stream_the_gpu_refuses_falls_back_to_software_and_keeps_playing() {
    // The behaviour the whole fallback apparatus exists for, driven by a real driver
    // refusal rather than an injected one. `Auto` must not fail, must not go black, and
    // must not stall — it must quietly produce frames on the CPU.
    let Some(stream) = undecodable_by_hardware_stream() else {
        eprintln!("skipping: no ffmpeg CLI to generate the fixture");
        return;
    };
    // Opening a compositor first is what tells the decode side that GPU surfaces have
    // somewhere to go — otherwise the policy declines hardware before ever asking the
    // driver, and this would pass without exercising the refusal at all.
    let Some(compositor) = pipeline::test_gpu::compositor(WIDTH, HEIGHT) else {
        return;
    };
    if compositor.surface_import() == SurfaceImport::Unsupported {
        eprintln!("skipping: this device cannot import GPU surfaces");
        return;
    }

    let mut source = split_access_units(&stream).into_iter();
    assert!(source.len() >= 5, "fixture should have several frames");
    let mut gpu_frames = 0usize;
    let mut cpu_frames = 0usize;

    let log = CapturedLog::default();
    let result = tracing::subscriber::with_default(log.subscriber(), || {
        pipeline::ffmpeg_decode::decode_stream(
            VideoCodec::H264,
            HwPreference::Auto,
            || source.next(),
            |frame| {
                match &frame.image {
                    FrameImage::Gpu(_) => gpu_frames += 1,
                    FrameImage::Cpu { .. } => cpu_frames += 1,
                }
                true
            },
        )
    });
    result.expect("a hardware refusal must not fail the session");

    assert_eq!(gpu_frames, 0, "the GPU cannot decode this profile");
    assert!(
        cpu_frames >= 5,
        "the mirror must survive the fallback; got {cpu_frames} software frames",
    );

    // A silent downgrade is the failure mode this whole path is designed around: the
    // mirror keeps working, so the only symptom is a hot CPU that nobody attributes to
    // anything. The log line is part of the contract, not decoration.
    let captured = log.text();
    assert!(
        captured.contains("falling back to SOFTWARE decode"),
        "the fallback must announce itself; captured log was:\n{captured}",
    );
    assert_eq!(
        captured.matches("falling back to SOFTWARE decode").count(),
        1,
        "…and exactly once, or a live mirror turns the log into a firehose:\n{captured}",
    );
}

/// Collects `tracing` output so a test can assert on what was logged.
#[derive(Clone, Default)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedLog {
    fn subscriber(&self) -> impl tracing::Subscriber {
        tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish()
    }

    fn text(&self) -> String {
        let buffer = self.0.lock().expect("log mutex");
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log mutex").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLog {
    type Writer = Self;
    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn software_only_never_touches_the_hardware_path() {
    // The other half of the runtime-choice contract: the same binary, the same stream,
    // and an explicit preference produces system-memory frames. If this ever yields a GPU
    // frame, `HwPreference` is not actually in control of anything.
    let Some(stream) = solid_colour_stream("software-only") else {
        return;
    };
    let mut source = split_access_units(&stream).into_iter();
    let mut gpu_frames = 0usize;
    let mut cpu_frames = 0usize;

    pipeline::ffmpeg_decode::decode_stream(
        VideoCodec::H264,
        HwPreference::SoftwareOnly,
        || source.next(),
        |frame| {
            match &frame.image {
                FrameImage::Gpu(_) => gpu_frames += 1,
                FrameImage::Cpu { .. } => cpu_frames += 1,
            }
            true
        },
    )
    .expect("software decode must always work");

    assert_eq!(gpu_frames, 0);
    assert!(cpu_frames > 0, "expected decoded frames");
}
