//! The media-URL path decodes *both* streams, and paces itself against the audio.
//!
//! These are the tests the gap they close would have failed: before this, `url_session`
//! took `best(Type::Video)` and skipped every other stream, so a DLNA/Cast/AirPlay video
//! cast played silently and an audio-only URL failed outright with "no video stream" —
//! while the DLNA sink advertised `http-get:*:audio/*:*` to every control point on the LAN.
#![cfg(all(feature = "ffmpeg", feature = "render"))]
#![allow(clippy::unwrap_used)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

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
    /// The presentation time of the last frame that reached the callback.
    ///
    /// The load-independent half of "the video decoded". How *many* frames arrive is a
    /// property of the host — `drain_paced` drops what is hopelessly late, on purpose —
    /// but how far through the file the decode got is a property of the file (#170).
    last_pts: Duration,
    audio_blocks: usize,
    /// Media time the decoded audio claims, summed per block: the property the sample
    /// count was reaching for. `audio_samples > 40_000` passed a decode that produced a
    /// quarter of a two-second file — half rate, a dropped channel, double speed all fit
    /// under a 4x margin (#234).
    audio_duration: Duration,
    /// Median of `frame.pts - clock.now()` at each presentation, in nanoseconds,
    /// frames past the first second. Positive means the frame was handed over early.
    ///
    /// The A/V sync figure (#234): every structural assertion in this file is satisfied
    /// by a session whose picture runs half a second from its sound, because nothing
    /// compared where a frame landed against the clock the audio drives.
    video_skew_ns: Option<i128>,
    elapsed: Duration,
}

fn run(path: &std::path::Path, want_audio: bool) -> Run {
    let clock = Arc::new(MediaClock::new());
    let (tx, rx) = sync_channel::<castaway_core::PcmFrame>(4096);
    let mut layout = MediaLayout::default();
    let mut frames = 0usize;
    let mut last_pts = Duration::ZERO;
    let mut skews: Vec<i128> = Vec::new();
    let start = Instant::now();

    // The audio consumer stands in for `audio_session::run_pcm`, and the thing it has to
    // be faithful about is *when* it submits — not how fast it can drain.
    //
    // It used to drain instantly and anchor the clock on arrival, on the grounds that
    // pacing is tested separately and this way the test finished in decode time rather
    // than in playback time. That made the clock lie, and #170 is what it cost.
    // `MediaClock` reads `submitted - OUTPUT_LEAD`, so two seconds of audio submitted in
    // fifty milliseconds puts the clock 1.75 s into the media before the video decoder has
    // produced its third frame — and `drain_paced` then drops every frame more than 400 ms
    // behind, exactly as it should. Which thread got further first decided the frame
    // count, so the assertion measured the host.
    //
    // Anchoring in the future does not fix it, and that is worth knowing before trying:
    // `State::running_at` interpolates with `saturating_duration_since`, so an anchor
    // ahead of now does not run backwards — the clock simply reads `submitted - LEAD` and
    // leaps exactly as before.
    //
    // So this paces, which is what the real thing does: `run_pcm` submits at 1x because
    // `MixInput::write` blocks while it is already `LEAD` ahead of the speakers (#111).
    // Audio through media time M is therefore handed over at about `start + M - LEAD`, and
    // the clock then reads wall-clock-since-start whatever order the threads run in. The
    // decode takes as long as the media, which is the honest price and is what
    // `a_silent_video_is_paced_by_the_wall_clock_not_the_cpu` already pays.
    let clock_for_audio = Arc::clone(&clock);
    let collector = std::thread::spawn(move || {
        let mut blocks = 0usize;
        let mut duration = Duration::ZERO;
        while let Ok(block) = rx.recv() {
            let through = block.pts + block.duration();
            let due = start + through.saturating_sub(pipeline::clock::OUTPUT_LEAD);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            clock_for_audio.observe_audio(through);
            blocks += 1;
            duration += block.duration();
        }
        (blocks, duration)
    });

    decode_av(
        path.to_str().unwrap(),
        HwPreference::SoftwareOnly,
        &clock,
        None,
        want_audio.then_some(tx),
        &|| false,
        |l| layout = l.clone(),
        |frame| {
            last_pts = last_pts.max(frame.pts);
            frames += 1;
            if frame.pts >= Duration::from_secs(1) {
                if let Some(now) = clock.now() {
                    let (pts_ns, now_ns) = (
                        i128::try_from(frame.pts.as_nanos()).unwrap(),
                        i128::try_from(now.as_nanos()).unwrap(),
                    );
                    skews.push(pts_ns - now_ns);
                }
            }
            true
        },
    )
    .unwrap();

    let (audio_blocks, audio_duration) = collector.join().unwrap();
    skews.sort_unstable();
    let video_skew_ns = (!skews.is_empty()).then(|| skews[skews.len() / 2]);
    Run {
        layout,
        frames,
        last_pts,
        audio_blocks,
        audio_duration,
        video_skew_ns,
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
    // Pictures came out, and the decode ran to the end of the file rather than being
    // opened and abandoned.
    //
    // Deliberately *not* a frame count, which is what this asserted until #170 and which
    // cannot be a guarantee: `drain_paced` drops frames that are hopelessly late, on
    // purpose and correctly, so under load a loaded box legitimately presents fewer of
    // them. `frames >= 15` of 20 was therefore a measurement of the host — it was seen at
    // 11. How far *through* the file the decode got is a property of the file, and it is
    // also the thing the assertion was reaching for.
    assert!(r.frames > 0, "no video frames at all");
    assert!(
        r.last_pts >= Duration::from_millis(1_500),
        "the decode stopped at {:?} of a two-second file",
        r.last_pts
    );
    assert!(r.audio_blocks > 0, "no audio was decoded at all");
    // Lip sync, at last: the median distance between a presented frame's pts and the
    // clock the audio drives. `drain_paced` releases a frame when the clock reaches
    // `pts - VIDEO_DECODE_LEAD` (40 ms), so the healthy reading is a small positive
    // number; a session whose picture ran half a second from its sound — which every
    // other assertion in this file is satisfied by — reads as exactly that distance.
    let skew_ms = r.video_skew_ns.expect("frames past the first second") as f64 / 1e6;
    eprintln!("video skew median: {skew_ms:.1} ms");
    // Healthy is single digits (measured -2.3 ms); `drain_paced` may present up to
    // `VIDEO_DECODE_LEAD` (40 ms) early. The defect this was born catching read -170 ms:
    // the packet gate fed the decoder in presentation order, so a P-frame held to its
    // own instant starved the B-frames behind it and the picture came out in
    // reorder-span bursts.
    assert!(
        (-60.0..=60.0).contains(&skew_ms),
        "the picture presents {skew_ms:.1} ms from the clock its audio drives"
    );
    // The whole two seconds, within codec padding. A band, not a floor: a decode that
    // produces half the audio at the declared rate — half rate, a dropped channel,
    // double speed — sat comfortably above the old `> 40_000` sample count (#234).
    assert!(
        (1.85..=2.2).contains(&r.audio_duration.as_secs_f64()),
        "a two-second file decoded to {:?} of audio",
        r.audio_duration
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
        (1.85..=2.2).contains(&r.audio_duration.as_secs_f64()),
        "a two-second tone decoded to {:?} of audio",
        r.audio_duration
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
    // Progress, not a count, for the same reason as the A/V test above (#170): the
    // drop-late rule means the number of frames a loaded box presents is a property of
    // the box. This test's own claim is the `elapsed` assertion below.
    assert!(r.frames > 0, "no video frames at all");
    assert!(
        r.last_pts >= Duration::from_millis(700),
        "the decode stopped at {:?} of a one-second file",
        r.last_pts
    );
    // One second of media should take about a second. Generous lower bound so a loaded
    // CI box cannot fail it, but far above the ~50 ms this took with no clock at all.
    assert!(
        r.elapsed > Duration::from_millis(600),
        "one second of silent video decoded in {:?}, so nothing is pacing it",
        r.elapsed
    );
}

#[test]
fn a_paced_audio_consumer_does_not_stall_the_demuxer_that_feeds_it() {
    // #52, and the only test here that runs the *real* playback thread rather than a
    // collector that drains instantly. That substitution is what hid this for so long: the
    // bug is a deadlock between two halves of one thread, and a consumer with no clock in
    // the loop cannot express it.
    //
    // Video used to be decoded the instant its packet was read and then held, inside the
    // demux loop, until the media clock said it was due. But the demux thread is the only
    // producer of the audio that drives that clock, and the clock reads
    // `submitted - OUTPUT_LEAD`: to present a frame at T it must first read audio to
    // T + 250 ms, which it cannot do while asleep holding the frame at T. Measured on a
    // VP8 1080p60 + Vorbis WebM: 0.11x, and a picture at 5.5 fps instead of 60.
    let path = tmp("paced.webm");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=6:size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=6",
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-c:a",
            "libvorbis",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }

    let clock = Arc::new(pipeline::clock::MediaClock::new());
    let seek = Arc::new(pipeline::seek::SeekControl::default());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = sync_channel::<castaway_core::PcmFrame>(pipeline::ffmpeg_decode::AUDIO_QUEUE);

    // The real thing: `run_pcm`, paced to real time by the real mixer, over a null sink
    // so this runs anywhere. The mixer keeps time on the wall when the device cannot
    // count, so the pacing under test is the one the panel uses.
    let mixer = pipeline::mixer::AudioMixer::new(Arc::new(|| {
        Box::new(pipeline::audio_out::NullAudioOut::new())
    }));
    pipeline::audio_session::spawn_pcm(
        rx,
        mixer.input(pipeline::mixer::Backpressure::Pull),
        Arc::clone(&stop),
        Some(pipeline::audio_session::PacedSession {
            clock: Arc::clone(&clock),
            seek: Arc::clone(&seek),
        }),
    );

    const WATCH: Duration = Duration::from_secs(3);
    let started = Instant::now();
    let deadline = started + WATCH;
    let mut frames = 0usize;
    decode_av(
        path.to_str().unwrap(),
        HwPreference::SoftwareOnly,
        &clock,
        Some(&seek),
        Some(tx),
        &|| Instant::now() >= deadline,
        |_l| {},
        |_frame| {
            frames += 1;
            true
        },
    )
    .unwrap();
    let wall = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let advanced = clock.now().unwrap_or_default();
    let rate = advanced.as_secs_f64() / wall.as_secs_f64();

    // A wall-clock assertion, deliberately, because "plays in real time" is not a property
    // that can be asserted any other way. The threshold is a *fifth* of real time against
    // a bug that produced a ninth of it and a fix that produces all of it — four times the
    // margin in both directions, so a loaded host cannot flip it (cf. #156).
    assert!(
        rate > 0.2,
        "the media clock advanced {advanced:?} in {wall:?} — {rate:.3}x — so the demuxer is \
         waiting on a clock only it can advance"
    );
    // And the picture that comes with it. 30 fps of source over three seconds is ~90
    // frames; the deadlock produced single digits.
    assert!(
        frames > 30,
        "{frames} frames in {wall:?}: the video is obeying a clock that is crawling"
    );
}

/// Media arrives over **HTTP**, not from a file, on every path that matters: DLNA hands
/// us a control point's URL, Cast `LOAD` hands us a CDN's, AirPlay the same. So the one
/// thing worth proving beyond decoding is that the decoder can *fetch*.
///
/// This is not hypothetical plumbing. libavformat's protocol set is a build-time choice,
/// and an ffmpeg without the `http` protocol compiled in decodes every local file in this
/// suite perfectly and fails every real cast — with "Protocol not found", from inside the
/// decode thread, which reaches a person in the room as a receiver that accepts the cast
/// and shows nothing.
#[test]
fn media_is_fetched_over_http_not_just_opened_from_disk() {
    use std::io::{Read as _, Write as _};

    let path = tmp("served.mp4");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
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
    let body = std::fs::read(&path).unwrap();

    // A single-shot HTTP/1.0 server on an ephemeral port. Enough for libavformat, which
    // issues a plain GET and reads to EOF; anything more would be testing our own server.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = body.clone();
    let server = std::thread::spawn(move || {
        let mut requests = 0usize;
        // Two connections at most: libavformat may probe and then fetch.
        for _ in 0..2 {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            requests += 1;
            let header = format!(
                "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nContent-Type: video/mp4\r\n\r\n",
                served.len()
            );
            if sock.write_all(header.as_bytes()).is_err() || sock.write_all(&served).is_err() {
                break;
            }
            let _ = sock.flush();
            drop(sock);
            if requests >= 1 {
                break;
            }
        }
        requests
    });

    let url = format!("http://127.0.0.1:{port}/served.mp4");
    let clock = Arc::new(MediaClock::new());
    let (tx, rx) = sync_channel::<castaway_core::PcmFrame>(4096);
    let clock_for_audio = Arc::clone(&clock);
    let drain = std::thread::spawn(move || {
        let mut samples = 0usize;
        while let Ok(block) = rx.recv() {
            clock_for_audio.observe_audio(block.pts + block.duration());
            samples += block.samples.len();
        }
        samples
    });

    let mut frames = 0usize;
    let mut layout = MediaLayout::default();
    let result = decode_av(
        &url,
        HwPreference::SoftwareOnly,
        &clock,
        None,
        Some(tx),
        &|| false,
        |l| layout = l.clone(),
        |_frame| {
            frames += 1;
            true
        },
    );
    let samples = drain.join().unwrap();
    let requests = server.join().unwrap();

    result.expect("the media could not be fetched over http");
    assert!(requests > 0, "the decoder never made an HTTP request");
    assert!(layout.has_video && layout.has_audio, "{layout:?}");
    assert!(frames > 0, "no frames came back from an HTTP source");
    assert!(samples > 0, "no audio came back from an HTTP source");
}

/// A seek moves the demuxer, and the frames that come back are from where it was sent.
///
/// Driven as a *start offset* — a seek requested before the first packet is read — because
/// that is both the deterministic way to assert it and a real case in its own right: Cast
/// `LOAD` and AirPlay both carry "resume from here", which used to be accepted and then
/// ignored, so resuming a film restarted it.
#[test]
fn a_seek_lands_where_it_was_sent_rather_than_where_it_was() {
    let path = tmp("seekable.mp4");
    // A key frame every half second, so the seek has somewhere near the target to land.
    // With libx264's default keyint a four-second clip has exactly one, at the start, and
    // every seek would legitimately land back at zero.
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=4:size=160x120:rate=10",
            "-c:v",
            "libx264",
            "-g",
            "5",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }

    let clock = Arc::new(MediaClock::new());
    let seek = Arc::new(pipeline::seek::SeekControl::new());
    seek.request(Duration::from_secs(3));

    let mut times: Vec<Duration> = Vec::new();
    decode_av(
        path.to_str().unwrap(),
        HwPreference::SoftwareOnly,
        &clock,
        Some(&seek),
        None,
        &|| false,
        |_l| {},
        |frame| {
            times.push(frame.pts);
            true
        },
    )
    .unwrap();

    let first = *times.first().expect("no frames came back after a seek");
    assert!(
        first >= Duration::from_millis(2_500),
        "the seek did not move the demuxer: first frame at {first:?}",
    );
    // …and the tail of the file is still there, so the seek landed inside it rather than
    // running it off the end.
    assert!(
        times.len() >= 5,
        "only {} frames after seeking to 3s of a 4s clip",
        times.len()
    );
    assert!(
        times.windows(2).all(|w| w[1] >= w[0]),
        "timestamps went backwards after the seek"
    );
}

/// A seek interrupts a *paused* session. Scrubbing while paused is the ordinary way people
/// find a spot, and it is exactly the state where the clock never advances — so a frame
/// waiting its turn would hold the seek until somebody pressed play.
#[test]
fn a_paused_session_can_still_be_scrubbed() {
    let path = tmp("scrub-paused.mp4");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=4:size=160x120:rate=10",
            "-c:v",
            "libx264",
            "-g",
            "5",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }

    let clock = Arc::new(MediaClock::new());
    let seek = Arc::new(pipeline::seek::SeekControl::new());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Paused from the outset, so the decode thread parks on the very first frame.
    clock.set_paused(true);

    let decoder = std::thread::spawn({
        let (clock, seek, stop, path) = (
            Arc::clone(&clock),
            Arc::clone(&seek),
            Arc::clone(&stop),
            path.clone(),
        );
        move || {
            let mut times: Vec<Duration> = Vec::new();
            let _ = decode_av(
                path.to_str().unwrap(),
                HwPreference::SoftwareOnly,
                &clock,
                Some(&seek),
                None,
                &|| stop.load(std::sync::atomic::Ordering::SeqCst),
                |_l| {},
                |frame| {
                    times.push(frame.pts);
                    true
                },
            );
            times
        }
    });

    // Give it long enough to be well and truly parked on a frame it cannot present, then
    // scrub. A session that could not be interrupted here would never see this.
    std::thread::sleep(Duration::from_millis(300));
    seek.request(Duration::from_secs(3));

    // The seek re-anchors the clock even though the session is still paused, which is what
    // makes the position readout follow the scrub.
    let mut moved = false;
    for _ in 0..400 {
        if clock.now().is_some_and(|p| p >= Duration::from_secs(3)) {
            moved = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let times = decoder.join().unwrap();
    assert!(
        moved,
        "the scrub never reached the decoder; it is stuck behind a frame waiting for a \
         clock that is frozen. last frames: {times:?}",
    );
    assert!(clock.is_paused(), "and the session is still paused");
}

/// The pipeline says how far through it is, and says when it is done.
///
/// Both halves of the same absence. The decode thread used to log and exit, so a DLNA
/// control point was told `PLAYING` / `OK` for the life of the process and drew its
/// scrubber against a sentinel — a phone showing a healthy session over a panel that had
/// gone back to idle, with a queued playlist stuck on its first track.
///
/// Drives the real [`pipeline::RenderPipeline`] rather than the decoder underneath it,
/// because the seam under test is the pipeline's: the clock it owns, and the report it
/// sends when its decode thread finishes.
#[tokio::test(flavor = "multi_thread")]
async fn the_pipeline_reports_where_it_is_and_that_it_finished() {
    use castaway_core::{MediaUri, Pipeline as _, PlaybackEnd, PlaybackReport as _};

    let path = tmp("reported.mp4");
    if !make(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=160x120:rate=10",
            "-c:v",
            "libx264",
        ],
        &path,
    ) {
        eprintln!("skipping: ffmpeg unavailable");
        return;
    }

    let (pipe, _render_rx) = pipeline::RenderPipeline::new(3);
    let (ends_tx, mut ends_rx) = castaway_core::playback::end_channel();
    pipe.set_playback_ends(ends_tx);
    let progress = pipe.playback_handle();

    // Nothing to report before anything is playing, and that is the answer a control
    // point must get: a zero here is drawn as "at the start" of an item that has not begun.
    assert!(progress.progress().is_none());

    let uri = MediaUri::parse(&format!("file://{}", path.display())).unwrap();
    pipe.play(uri, None).await.unwrap();

    // The clock is seeded by the first frame, so a position appears shortly after the
    // decoder opens the file — and it is a *position*, not a total.
    let mut seen = None;
    for _ in 0..600 {
        if let Some(p) = progress.progress() {
            seen = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let seen = seen.expect("the pipeline never reported a position for a playing item");
    assert!(
        seen.duration.is_some(),
        "the container knows how long this is; the scrubber needs it"
    );

    let end = tokio::time::timeout(Duration::from_secs(20), ends_rx.recv())
        .await
        .expect("the pipeline never said the item had ended")
        .expect("the end channel closed instead");
    assert_eq!(end, PlaybackEnd::Finished);
}

/// A URL the box cannot fetch is the failure this reporting exists for. It is
/// indistinguishable, from the phone, from a receiver that is playing perfectly — which is
/// exactly why it has to be reported rather than merely logged.
#[tokio::test(flavor = "multi_thread")]
async fn a_url_that_cannot_be_fetched_is_reported_as_a_failure() {
    use castaway_core::{MediaUri, Pipeline as _, PlaybackEnd};

    let (pipe, _render_rx) = pipeline::RenderPipeline::new(3);
    let (ends_tx, mut ends_rx) = castaway_core::playback::end_channel();
    pipe.set_playback_ends(ends_tx);

    // A port nothing is listening on, on the loopback: refused immediately rather than
    // waiting out a DNS or connect timeout, so the test does not depend on the network.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);

    let uri = MediaUri::parse(&format!("http://127.0.0.1:{port}/nothing.mp4")).unwrap();
    pipe.play(uri, None).await.unwrap();

    let end = tokio::time::timeout(Duration::from_secs(30), ends_rx.recv())
        .await
        .expect("an unfetchable URL was never reported")
        .expect("the end channel closed instead");
    assert!(
        matches!(end, PlaybackEnd::Failed(_)),
        "a refused connection is a failure, not a finish: {end:?}"
    );
}
