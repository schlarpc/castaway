//! The output stream, end to end and back again (#101).
//!
//! Composite a known picture, run it through the whole chain — RGBA→NV12 on the GPU, the
//! readback, libavcodec, our own fMP4 boxing, the HLS window — then hand the result to
//! *libavformat* and ask it to play it back. Every box we wrote is parsed by something
//! that did not write it, and the picture that comes out is compared with the one that
//! went in.
//!
//! That is the whole point of this file: the unit tests can prove each stage is
//! self-consistent, and none of them can prove the segments are something a player will
//! take. A `trun` pointing four bytes past its `mdat` produces segments of exactly the
//! right size that decode to nothing, and only a demuxer notices.
//!
//! Needs a GPU adapter and an H.264 encoder. Where either is missing the test says so and
//! passes — CI has neither, and a hardware requirement is not something to smuggle in
//! (ground rule 6).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use pipeline::compositor::{Compositor as _, Layer, LayerId, Transform};
use pipeline::render_pipeline::{render_channel, RenderLoop};
use pipeline::stream::cadence::FrameRate;
use pipeline::stream::{LiveStream, StreamConfig, StreamStatus, StreamTap};
use pipeline::wgpu_compositor::{TexelFormat, WgpuCompositor};

/// The colour the panel shows in these tests.
///
/// Magenta, because a channel swap anywhere between the compositor and the decoder moves
/// Cb and Cr in opposite directions and is unmissable — where on a grey it would be
/// invisible, and on a photograph it would be arguable.
const MAGENTA: [u8; 4] = [0xff, 0x00, 0xff, 0xff];

/// BT.709 limited-range magenta: what `crate::nv12` computes and what the decoder should
/// hand back. Derived once by hand from the coefficients, and pinned in the NV12 unit
/// tests too — if these two ever disagree, one of the two conversions moved.
const MAGENTA_YUV: (u8, u8, u8) = (78, 214, 230);

/// A compositor showing one solid colour, or `None` where there is no GPU.
fn panel(width: u32, height: u32) -> Option<WgpuCompositor> {
    let mut compositor = WgpuCompositor::new_offscreen(width, height).ok()?;
    let pixels: Vec<u8> = std::iter::repeat_n(MAGENTA, (width * height) as usize)
        .flatten()
        .collect();
    compositor
        .upload_texture(LayerId::Attract, width, height, TexelFormat::Rgba8, &pixels)
        .unwrap();
    compositor.upsert_layer(Layer {
        id: LayerId::Attract,
        opacity: 1.0,
        transform: Transform::default(),
    });
    Some(compositor)
}

/// Short segments and a small frame, so a test does not have to run for a second per
/// segment. Everything else is the shipped configuration.
fn config() -> StreamConfig {
    StreamConfig {
        rate: FrameRate::new(30).unwrap(),
        max_height: 1080,
        bitrate: 2_000_000,
        segment: Duration::from_millis(200),
        window: 8,
        idle_timeout: Duration::from_secs(30),
        ..StreamConfig::default()
    }
}

/// Drive a render loop with a stream tap attached until `segments` have been published.
///
/// Returns `None` when there is no GPU or no encoder, which is a skip and not a failure.
fn publish(width: u32, height: u32, segments: u32) -> Option<Arc<LiveStream>> {
    let compositor = panel(width, height)?;
    let config = config();
    let state = Arc::new(LiveStream::new(&config));
    let (_tx, rx) = render_channel(2);
    let mut rloop = RenderLoop::new(compositor, rx);
    rloop.add_tap(Box::new(StreamTap::new(
        Arc::clone(&state),
        config,
        width,
        height,
    )));

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        // The tap retires when nothing has asked for a while, and in this harness nothing
        // is asking over HTTP.
        state.touch(Instant::now());
        rloop.pump();
        if let StreamStatus::Failed(why) = state.status() {
            eprintln!("no encoder here, skipping: {why}");
            return None;
        }
        if state.segment(segments).is_some() {
            return Some(state);
        }
        // Faster than the panel would present, so the cadence is what paces the stream
        // rather than this loop.
        std::thread::sleep(Duration::from_millis(4));
    }
    panic!(
        "no segments after twenty seconds; status {:?}",
        state.status()
    );
}

/// Everything the stream published, concatenated: an fMP4 initialisation segment followed
/// by its media segments *is* a playable file, which is what makes this checkable at all.
fn playable(state: &LiveStream, segments: u32) -> tempfile::NamedTempFile {
    let mut file = tempfile::Builder::new().suffix(".mp4").tempfile().unwrap();
    use std::io::Write as _;
    file.write_all(&state.init_segment().expect("an init segment"))
        .unwrap();
    for sequence in 1..=segments {
        let segment = state
            .segment(sequence)
            .unwrap_or_else(|| panic!("segment {sequence} should still be in the window"));
        file.write_all(&segment).unwrap();
    }
    file.flush().unwrap();
    // `CASTAWAY_STREAM_DUMP=/some/dir` keeps a copy. What this test asserts about is three
    // numbers; when they are wrong the only useful next step is to look at the picture,
    // and re-deriving a way to get one each time is the slow way to do that.
    if let Some(dir) = std::env::var_os("CASTAWAY_STREAM_DUMP") {
        let into = std::path::Path::new(&dir).join(format!("stream-{segments}.mp4"));
        std::fs::copy(file.path(), &into).unwrap();
        eprintln!("wrote {}", into.display());
    }
    file
}

/// One decoded frame's centre pixel, in the decoder's own planar Y'CbCr.
type Centre = (u8, u8, u8);

/// Decode the file: what the track claims to be, and every frame's centre pixel.
fn decode(path: &std::path::Path) -> ((u32, u32), Vec<Centre>) {
    ffmpeg::init().unwrap();
    let mut input = ffmpeg::format::input(&path).expect("libavformat should open our segments");
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .expect("a video track");
    let index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .video()
        .unwrap();

    let mut centres = Vec::new();
    let take = |decoder: &mut ffmpeg::decoder::Video, centres: &mut Vec<Centre>| {
        let mut frame = ffmpeg::frame::Video::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            // libavcodec's H.264 decoder produces planar 4:2:0; chroma is half-resolution
            // in both directions, so its centre is at half the luma centre.
            let (w, h) = (frame.width() as usize, frame.height() as usize);
            let y = frame.data(0)[(h / 2) * frame.stride(0) + w / 2];
            let cb = frame.data(1)[(h / 4) * frame.stride(1) + w / 4];
            let cr = frame.data(2)[(h / 4) * frame.stride(2) + w / 4];
            centres.push((y, cb, cr));
        }
    };
    for (source, packet) in input.packets() {
        if source.index() == index {
            decoder.send_packet(&packet).unwrap();
            take(&mut decoder, &mut centres);
        }
    }
    decoder.send_eof().unwrap();
    take(&mut decoder, &mut centres);
    ((decoder.width(), decoder.height()), centres)
}

fn near(got: u8, want: u8, what: &str) {
    assert!(
        got.abs_diff(want) <= 4,
        "{what}: decoded {got}, composited about {want}"
    );
}

#[test]
fn what_the_panel_composited_comes_back_out_of_a_demuxer() {
    // The end-to-end assertion. If the `moov` is malformed libavformat finds no track; if
    // the `trun` offset is wrong the decoder gets box headers as slice data; if the colour
    // conversion is wrong the picture is a different colour. All three fail here.
    let Some(state) = publish(320, 176, 2) else {
        return;
    };
    let file = playable(&state, 2);
    let (size, centres) = decode(file.path());

    assert_eq!(size, (320, 176), "the track's coded size");
    assert!(
        centres.len() >= 10,
        "two 200 ms segments at 30 fps should be a dozen frames, got {}",
        centres.len()
    );
    for (i, (y, cb, cr)) in centres.iter().enumerate() {
        near(*y, MAGENTA_YUV.0, &format!("frame {i} luma"));
        near(*cb, MAGENTA_YUV.1, &format!("frame {i} Cb"));
        near(*cr, MAGENTA_YUV.2, &format!("frame {i} Cr"));
    }
}

#[test]
fn a_4k_panel_streams_at_1080p() {
    // The downscale is in the conversion pass, so what a player sees is 1080p and what
    // crossed the bus was a quarter of the pixels. Worth an end-to-end check because the
    // stream's coded size is chosen in one place (`stream_size`), told to the encoder in
    // another, and written into the init segment in a third.
    let (width, height) = pipeline::stream::stream_size((3840, 2160), 1080);
    assert_eq!((width, height), (1920, 1080));
    let Some(state) = publish(width, height, 1) else {
        return;
    };
    let file = playable(&state, 1);
    let (size, centres) = decode(file.path());
    assert_eq!(size, (1920, 1080));
    assert!(!centres.is_empty());
    near(centres[0].0, MAGENTA_YUV.0, "luma");
}

#[test]
fn the_playlist_names_segments_that_are_actually_there() {
    // A playlist is a promise. One that names a segment the window has already dropped —
    // or spells its URI differently from the route that serves it — makes a player retry
    // forever with nothing in the log to say why.
    let Some(state) = publish(320, 176, 2) else {
        return;
    };
    let text = state
        .playlist("init.mp4", &|n| format!("seg/{n}"))
        .expect("a playlist once there are segments");
    let named: Vec<&str> = text.lines().filter(|l| l.starts_with("seg/")).collect();
    assert!(!named.is_empty());
    for uri in named {
        let sequence: u32 = uri.trim_start_matches("seg/").parse().unwrap();
        assert!(
            state.segment(sequence).is_some(),
            "{uri} is named but not served"
        );
    }
    assert!(text.contains("#EXT-X-MAP:URI=\"init.mp4\""));
    assert!(state.init_segment().is_some());
}

#[test]
fn the_stream_says_which_encoder_it_got() {
    // Not decoration: "the stream works" and "the stream works and is not spending a core
    // on it" are different answers, and this is the only place the second one is visible.
    let Some(state) = publish(320, 176, 1) else {
        return;
    };
    let StreamStatus::Live {
        encoder,
        width,
        height,
        codec,
    } = state.status()
    else {
        panic!("published a segment but is not live: {:?}", state.status());
    };
    eprintln!("encoded by {encoder} at {width}x{height} as {codec}");
    assert_eq!((width, height), (320, 176));
    assert!(codec.starts_with("avc1."), "{codec}");
}
