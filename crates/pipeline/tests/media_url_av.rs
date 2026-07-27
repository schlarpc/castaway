//! The media-URL path decodes *both* streams, and paces itself against the audio.
//!
//! These are the tests the gap they close would have failed: before this, `url_session`
//! took `best(Type::Video)` and skipped every other stream, so a DLNA/Cast/AirPlay video
//! cast played silently and an audio-only URL failed outright with "no video stream" —
//! while the DLNA sink advertised `http-get:*:audio/*:*` to every control point on the LAN.
#![cfg(all(feature = "ffmpeg", feature = "render"))]
#![allow(clippy::unwrap_used)]

use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pipeline::clock::MediaClock;
use pipeline::ffmpeg_decode::{decode_av, MediaLayout};
use pipeline::HwPreference;

/// Build a test file with ffmpeg. Returns `None` when ffmpeg is not on PATH, so the suite
/// still runs in an environment without it rather than failing for the wrong reason.
fn make(args: &[&str], out: &std::path::Path) -> bool {
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(out)
        .status();
    matches!(status, Ok(s) if s.success()) && out.exists()
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("castaway-av-test-{name}"))
}

struct Run {
    layout: MediaLayout,
    frames: usize,
    audio_blocks: usize,
    audio_samples: usize,
    elapsed: Duration,
}

fn run(path: &std::path::Path, want_audio: bool) -> Run {
    let clock = Arc::new(MediaClock::new());
    let (tx, rx) = sync_channel::<castaway_core::PcmFrame>(4096);
    let mut layout = MediaLayout::default();
    let mut frames = 0usize;
    let start = Instant::now();

    // The audio consumer stands in for `audio_session::run_pcm`: it drains and drives the
    // clock, but does *not* pace to real time, so the test finishes in decode time rather
    // than in playback time. Pacing is `Pace`'s job and is tested separately.
    let clock_for_audio = Arc::clone(&clock);
    let collector = std::thread::spawn(move || {
        let (mut blocks, mut samples) = (0usize, 0usize);
        while let Ok(block) = rx.recv() {
            clock_for_audio.observe_audio(block.pts + block.duration());
            blocks += 1;
            samples += block.samples.len();
        }
        (blocks, samples)
    });

    decode_av(
        path.to_str().unwrap(),
        HwPreference::SoftwareOnly,
        &clock,
        want_audio.then_some(tx),
        &|| false,
        |l| layout = l.clone(),
        |_frame| {
            frames += 1;
            true
        },
    )
    .unwrap();

    let (audio_blocks, audio_samples) = collector.join().unwrap();
    Run {
        layout,
        frames,
        audio_blocks,
        audio_samples,
        elapsed: start.elapsed(),
    }
}

/// The headline: a video file's audio track is decoded, not demuxed past. This is the
/// silence that affected DLNA, Cast `LOAD` and AirPlay video alike.
#[test]
fn a_video_file_yields_both_pictures_and_sound() {
    let path = tmp("av.mp4");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }
    let r = run(&path, true);
    assert!(r.layout.has_video && r.layout.has_audio, "{:?}", r.layout);
    assert!(r.frames >= 15, "video frames: {}", r.frames);
    assert!(r.audio_blocks > 0, "no audio was decoded at all");
    // Two seconds of 44.1/48 kHz stereo is tens of thousands of samples; anything much
    // smaller means the stream was opened and then abandoned.
    assert!(
        r.audio_samples > 40_000,
        "audio samples: {}",
        r.audio_samples
    );
}

/// An MP3 is a music session, not an error. It used to fail with "no video stream" while
/// the DLNA sink advertised `audio/*`.
#[test]
fn an_audio_only_url_plays_instead_of_failing() {
    let path = tmp("tone.mp3");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:a",
            "libmp3lame",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }
    let r = run(&path, true);
    assert!(!r.layout.has_video, "an mp3 has no video stream");
    assert!(r.layout.has_audio);
    assert_eq!(r.frames, 0, "and produces no video frames");
    assert!(
        r.audio_samples > 40_000,
        "audio samples: {}",
        r.audio_samples
    );
    // A duration the card's scrubber can be drawn against.
    let d = r.layout.duration.expect("an mp3 knows how long it is");
    assert!(
        d > Duration::from_millis(1_800) && d < Duration::from_millis(2_400),
        "{d:?}"
    );
}

/// Container tags reach the card. A bare URL from Cast or AirPlay carries no metadata of
/// its own, so this is the only thing that stops a music session being a blank rectangle.
#[test]
fn container_tags_are_read_for_the_card() {
    let path = tmp("tagged.mp3");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:a",
            "libmp3lame",
            "-metadata",
            "title=Windowlicker",
            "-metadata",
            "artist=Aphex Twin",
            "-metadata",
            "album=Windowlicker",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }
    let r = run(&path, true);
    assert_eq!(r.layout.title.as_deref(), Some("Windowlicker"));
    assert_eq!(r.layout.artist.as_deref(), Some("Aphex Twin"));
    assert_eq!(r.layout.album.as_deref(), Some("Windowlicker"));
}

/// With no audio to follow, the clock runs off the wall — so a silent file plays at its
/// own frame rate rather than as fast as the CPU can decode it. Before the clock existed
/// this was true of *every* file: a two-hour film decoded in as long as the disk took.
#[test]
fn a_silent_video_is_paced_by_the_wall_clock_not_the_cpu() {
    let path = tmp("silent.mp4");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-c:v",
            "libx264",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }
    let r = run(&path, false);
    assert!(r.frames >= 8, "video frames: {}", r.frames);
    // One second of media should take about a second. Generous lower bound so a loaded
    // CI box cannot fail it, but far above the ~50 ms this took with no clock at all.
    assert!(
        r.elapsed > Duration::from_millis(600),
        "one second of silent video decoded in {:?}, so nothing is pacing it",
        r.elapsed
    );
}
