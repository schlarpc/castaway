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
//! Needs a GPU adapter and an H.264 encoder. Where either is honestly missing these skip
//! and say so — a hardware requirement is not something to smuggle in (ground rule 6) —
//! but both skips go through the tripwires, so a build that *promised* an adapter
//! (`CASTAWAY_REQUIRE_GPU`) or ffmpeg (`CASTAWAY_REQUIRE_FFMPEG`) and then could not
//! supply one fails instead. That is not hypothetical: this file skip-passed by design
//! until #182, so even once a check compiled it, it could have asserted nothing.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use pipeline::compositor::{Compositor as _, Layer, LayerId, Transform};
use pipeline::render_pipeline::{render_channel, RenderLoop};
use pipeline::stream::audio::RATE;
use pipeline::stream::cadence::FrameRate;
use pipeline::stream::{LiveStream, StreamAudio, StreamConfig, StreamStatus, StreamTap};
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
    // Through the tripwire, not `.ok()?`: `checks.test` supplies lavapipe and sets
    // `CASTAWAY_REQUIRE_GPU`, and a `?` here would let an ICD path that stopped resolving
    // turn every assertion in this file back into a green nothing.
    let mut compositor = pipeline::test_gpu::compositor(width, height)?;
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

/// A stream that has published what was asked of it, with the loop that produced it
/// **still attached**.
///
/// The render loop is held rather than dropped, and that is the whole reason this type
/// exists. Dropping it drops the [`StreamTap`]; `StreamTap::drop` clears the job sender;
/// the encode thread's `recv` then fails and the tail of `encode_loop` flushes, publishes
/// the last segment and calls `LiveStream::stopped`, which moves the status
/// `Live | Starting → Idle`. That thread is spawned and never joined, so all of it happens
/// *after* the harness returns — and a test reading `status()` afterwards was racing its
/// own teardown. `published a segment but is not live` is what winning that race looks
/// like, and the two tests that read the status are the only two that have ever shown it
/// (#208).
struct Published {
    state: Arc<LiveStream>,
    /// Never read. Held so the tap outlives the assertions; see the type's docs.
    _render: RenderLoop,
    /// The audio half, so a failing assertion can say what the mix did rather than
    /// leaving "peak of 0" to be re-diagnosed from scratch each time (#208).
    audio: Arc<StreamAudio>,
    /// Held for the same reason as the render loop — dropping it stops the mixer thread —
    /// and read by [`Self::sound_diagnostics`].
    mixer: pipeline::mixer::AudioMixer,
}

impl Published {
    /// Every counter on the audio path, for a failure message. Which one is nonzero is
    /// the diagnosis: `dropped`/`starved` say the mixer thread was not scheduled,
    /// `clipped`/`rebase_discarded` say the timeline moved underneath the sound.
    fn sound_diagnostics(&self) -> String {
        let mix = self.audio.mix();
        format!(
            "{:?}, invented={} clipped={} rebase_discarded={} position={} held={}",
            self.mixer.counters(),
            mix.invented(),
            mix.clipped(),
            mix.rebase_discarded(),
            mix.position(),
            mix.held(),
        )
    }

    /// Fail unless the harness's own source kept the mixer fed.
    ///
    /// The premise every audio assertion below rests on, checked separately so a box that
    /// could not schedule a thread writing 480 samples every 10 ms says *that* rather than
    /// reporting a sync defect. `starved` is the discriminator: it counts a pass that
    /// found a live input empty, so it separates "the source did not produce" from "the
    /// mix mislaid what it produced" — the two diagnoses #208 spent a week between.
    fn assert_the_source_kept_up(&self) {
        let counters = self.mixer.counters();
        let starved = counters.starved as f64 / f64::from(RATE);
        assert!(
            starved < 0.05,
            "the harness source was descheduled for {starved:.3}s, so this box cannot \
             measure sync — not a defect in the stream. {}",
            self.sound_diagnostics()
        );
    }
}

impl std::ops::Deref for Published {
    type Target = LiveStream;

    fn deref(&self) -> &LiveStream {
        &self.state
    }
}

/// What the panel is doing while the stream records it.
#[derive(Default, Clone, Copy)]
struct Scenario {
    /// A session playing a tone, after this much silence. It takes the same `MixInput` a
    /// real session would, so what reaches the stream reaches it the way a cast's audio
    /// does — through the panel's one mixer — rather than through a back door the shipped
    /// path does not have.
    sound: Option<Duration>,
    /// Hold the render thread still once, mid-run, for this long — a compositor that lost
    /// its GPU to software rasterisation, which is what the CI sandbox is.
    stall: Option<Duration>,
}

/// Drive a render loop with a stream tap attached until `segments` have been published.
///
/// Returns `None` when there is honestly no GPU and no encoder. Both are tripwired, so
/// under a build that promised either, this is a failure rather than a skip (#182).
fn publish(width: u32, height: u32, segments: u32) -> Option<Published> {
    publish_with(width, height, segments, Scenario::default())
}

/// The same, with the panel doing something.
fn publish_with(width: u32, height: u32, segments: u32, scenario: Scenario) -> Option<Published> {
    let compositor = panel(width, height)?;
    let config = config();
    let state = Arc::new(LiveStream::new(&config));
    let audio = Arc::new(StreamAudio::new());
    let (_tx, rx) = render_channel(2);
    let mut rloop = RenderLoop::new(compositor, rx);
    rloop.add_tap(Box::new(StreamTap::new(
        Arc::clone(&state),
        config,
        width,
        height,
        Some(Arc::clone(&audio)),
    )));

    // The panel's one mixer, with the stream tapping it — which is the whole path under
    // test, since #111 made the stream a tap rather than a reconstruction. Held in scope
    // for the loop below: dropping it would stop the mixer thread.
    let mixer = pipeline::mixer::AudioMixer::new(Arc::new(|| {
        Box::new(pipeline::audio_out::NullAudioOut::new())
    }));
    mixer.add_tap(audio.tap());

    // Boot the mixer before the session, because the panel's is. There the browser holds
    // an input from startup, so by the time a cast opens one the device queue is already
    // full and the mixer is pacing to it. A mixer whose sink has *just* opened is a
    // different machine: an empty queue is a whole `DEVICE_LEAD` of headroom, which `plan`
    // takes in back-to-back passes, and a live session's ring is drained faster than real
    // time for as long as that lasts — 60 ms of `starved` in this harness, which is 60 ms
    // of silence in front of the tone.
    //
    // An input nobody ever writes to is exactly that boot condition and needs no new code
    // path: it is `Supply::Surface`, so it opens the device, constrains no pass and is
    // counted nowhere, and the mixer free-runs silence into the queue until it is full.
    let _warm = mixer.input(pipeline::mixer::Backpressure::Pull);
    // A device lead and a half of emitted frames: past that the queue is full and every
    // further pass is paced by the sink rather than by the headroom.
    let warm_enough = frames_at(Duration::from_millis(150));
    let deadline = Instant::now() + Duration::from_secs(10);
    while mixer.counters().emitted < warm_enough {
        assert!(
            Instant::now() < deadline,
            "the mixer never warmed up: {:?}",
            mixer.counters()
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // Pump until the timeline has an origin, and only then let the session start.
    //
    // The origin is the first composited frame, and audio that arrives before it is
    // dropped rather than stacked at position zero (`AudioMix::add`). So a source whose
    // sample zero predates the anchor loses exactly that much of its head, and everything
    // it plays afterwards arrives *early* by the same amount — the placement defect with
    // the opposite sign. It is not hypothetical: on a box contended down to four cores the
    // first pump costs 140 ms of cold wgpu pipeline, and one run in six put the tone at
    // 0.110 s instead of 0.250 s. Ordering these makes the source's sample zero and the
    // timeline's origin the same instant by construction, rather than by being quick.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !audio.timeline().anchored() {
        assert!(
            Instant::now() < deadline,
            "the render loop never composited a frame; status {:?}",
            state.status()
        );
        state.touch(Instant::now());
        rloop.pump();
        if let StreamStatus::Failed(why) = state.status() {
            let _: Option<()> = pipeline::test_media::resolve(
                &format!("opening an H.264 encoder for the output stream ({why})"),
                None,
            );
            return None;
        }
    }
    let source = scenario
        .sound
        .map(|quiet_for| Source::tone(mixer.input(pipeline::mixer::Backpressure::Pull), quiet_for));

    let loop_began = Instant::now();
    let mut stalled = false;

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        // The tap retires when nothing has asked for a while, and in this harness nothing
        // is asking over HTTP.
        state.touch(Instant::now());
        rloop.pump();
        if let Some(stall) = scenario.stall {
            // Once, and far enough in that the tone's own silence is still running: a
            // stall straddling the moment sound starts is the one that used to move it.
            if !stalled && loop_began.elapsed() >= Duration::from_millis(150) {
                stalled = true;
                std::thread::sleep(stall);
            }
        }
        if let StreamStatus::Failed(why) = state.status() {
            // An H.264 encoder is an ffmpeg capability, so it gets ffmpeg's tripwire: with
            // `CASTAWAY_REQUIRE_FFMPEG` set the build has said it has one, and "there is no
            // encoder" is then a build that lost a codec rather than a developer's box.
            let _: Option<()> = pipeline::test_media::resolve(
                &format!("opening an H.264 encoder for the output stream ({why})"),
                None,
            );
            return None;
        }
        if state.segment(segments).is_some() {
            // The source is dropped here, which stops and joins its thread; the mixer
            // rides along so the assertions can read its counters.
            drop(source);
            return Some(Published {
                state,
                _render: rloop,
                audio,
                mixer,
            });
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

/// The video track's timing, straight off the container.
#[derive(Debug)]
struct VideoTiming {
    /// `(pts, duration)` per packet, in the track's own timescale, presentation order.
    packets: Vec<(i64, i64)>,
    /// Ticks per second, as the track declares it.
    timescale: u32,
}

impl VideoTiming {
    /// Seconds of video the `trun`s actually claim — the number
    /// `the_audio_track_trails_the_live_edge` used to fake by assuming the frame rate.
    fn seconds(&self) -> f64 {
        let ticks: i64 = self.packets.iter().map(|(_, d)| d).sum();
        ticks as f64 / f64::from(self.timescale)
    }
}

/// Decode the file: what the track claims to be, and every frame's centre pixel.
fn decode(path: &std::path::Path) -> ((u32, u32), Vec<Centre>) {
    let (size, centres, _) = decode_timed(path);
    (size, centres)
}

/// [`decode`], with the container's own timing alongside (#234): the test picture is a
/// constant colour, so a duplicated or missing frame is invisible to the pixels — the
/// `trun` timing is the only place cadence is checkable at all.
fn decode_timed(path: &std::path::Path) -> ((u32, u32), Vec<Centre>, VideoTiming) {
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

    let timescale = {
        let tb = stream.time_base();
        assert_eq!(tb.numerator(), 1, "a track timescale is 1/N");
        u32::try_from(tb.denominator()).unwrap()
    };
    let mut packets = Vec::new();
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
            packets.push((
                packet.pts().expect("every sample carries a pts"),
                packet.duration(),
            ));
            decoder.send_packet(&packet).unwrap();
            take(&mut decoder, &mut centres);
        }
    }
    decoder.send_eof().unwrap();
    take(&mut decoder, &mut centres);
    (
        (decoder.width(), decoder.height()),
        centres,
        VideoTiming { packets, timescale },
    )
}

/// What the audio track decoded to.
#[derive(Debug)]
struct Sound {
    sample_rate: u32,
    channels: u16,
    /// Total sample frames decoded.
    frames: u64,
    /// The loudest sample anywhere in it. Silence is 0.
    peak: f32,
    /// The frame position of the first sample that is unmistakably not silence.
    onset: Option<u64>,
    /// Peak level of the *quietest* 20 ms window from the onset to the last loud sample.
    ///
    /// The hole detector (#234): `peak` is a maximum and `onset` a first crossing, so a
    /// 40 ms hole in the middle of the tone — a lost block, a fold, an encoder gap —
    /// changes neither. The quietest window is exactly what such a hole is.
    quietest_window: Option<f32>,
}

/// Decode the audio track, or `None` if the file has none.
fn decode_audio(path: &std::path::Path) -> Option<Sound> {
    ffmpeg::init().unwrap();
    let mut input = ffmpeg::format::input(&path).unwrap();
    let stream = input.streams().best(ffmpeg::media::Type::Audio)?;
    let index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .unwrap()
        .decoder()
        .audio()
        .unwrap();

    let mut frames = 0u64;
    let mut peak = 0f32;
    let mut onset = None;
    // The whole first plane, kept so the windows can be cut after the onset is known.
    let mut mono: Vec<f32> = Vec::new();
    let mut take = |decoder: &mut ffmpeg::decoder::Audio| {
        let mut frame = ffmpeg::frame::Audio::empty();
        while decoder.receive_frame(&mut frame).is_ok() {
            // libavcodec's AAC decoder produces planar float; every plane is worth
            // looking at, because a channel-mapping mistake silences exactly one.
            for plane in 0..frame.planes() {
                for (i, sample) in frame.plane::<f32>(plane).iter().enumerate() {
                    peak = peak.max(sample.abs());
                    // A tenth of full scale: well above the encoder's ringing either side
                    // of a hard onset, and well below the tone's own amplitude.
                    if onset.is_none() && sample.abs() > 0.1 {
                        onset = Some(frames + i as u64);
                    }
                }
            }
            mono.extend_from_slice(frame.plane::<f32>(0));
            frames += frame.samples() as u64;
        }
    };
    for (source, packet) in input.packets() {
        if source.index() == index {
            decoder.send_packet(&packet).unwrap();
            take(&mut decoder);
        }
    }
    decoder.send_eof().unwrap();
    take(&mut decoder);
    // The quietest 20 ms between the onset and the last loud sample. Trimmed at both
    // ends, because the track legitimately starts and ends with silence — the hole that
    // matters is one *inside* the sound.
    let quietest_window = onset.and_then(|onset| {
        let last_loud = mono.iter().rposition(|s| s.abs() > 0.1)?;
        let span = mono.get(usize::try_from(onset).ok()?..=last_loud)?;
        let window = decoder.rate() as usize / 50;
        span.chunks(window)
            .filter(|c| c.len() == window)
            .map(|c| c.iter().fold(0.0f32, |a, s| a.max(s.abs())))
            .min_by(f32::total_cmp)
    });
    Some(Sound {
        sample_rate: decoder.rate(),
        channels: decoder.channels(),
        frames,
        peak,
        onset,
        quietest_window,
    })
}

/// A session that writes real time's worth of audio whenever it is poked: silence until
/// `quiet_for` has passed, a 440 Hz tone after it.
///
/// Paced against the wall clock rather than per call, because the mix places a block where
/// the clock says it belongs — so a source that produced faster than real time would stack
/// its blocks on the same positions and simply get louder.
/// How often the source wakes. A decoder hands over about a packet's worth at a time.
const SOURCE_TICK: Duration = Duration::from_millis(10);

/// How many frames of [`RATE`] audio fit in `elapsed`.
fn frames_at(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos() * u128::from(RATE) / 1_000_000_000).unwrap_or(0)
}

/// A session playing into the panel's mixer, the way an adapter is one: on a thread of its
/// own, with its position counted in samples.
///
/// Both halves of that are load-bearing, and neither used to hold (#208). This was a
/// closure called from inside `pump()`, so the "session" stopped producing whenever the
/// render thread did — and the render thread is the expensive one, compositing under
/// lavapipe while libx264 takes the rest of the box. Ground rule 4 says a source is an
/// actor on its own clock precisely so that cannot happen; no shipped adapter is driven
/// this way (the browser's audio comes off `browser-reader`, sessions off their decoders).
///
/// Injecting the stall proved it end to end before this changed: a render thread held
/// still for 100/200/300 ms moved the tone 74/109/207 ms late, bracketing the 135 ms and
/// 212 ms measured in the sandbox. The stall is a scenario now
/// (`a_stalled_render_thread_does_not_move_the_sound`), so what it used to measure by
/// accident it now measures on purpose.
struct Source {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Source {
    /// Play `quiet_for` of silence, then a 440 Hz tone, until dropped.
    fn tone(mut input: pipeline::mixer::MixInput, quiet_for: Duration) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("harness-source".into())
            .spawn({
                let stop = Arc::clone(&stop);
                move || {
                    let quiet = frames_at(quiet_for);
                    let began = Instant::now();
                    let mut written: u64 = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        // What this source owes by now, against a fixed schedule rather
                        // than against the last wake. A thread that was slept on then
                        // delivers the audio it missed instead of shortening it, and the
                        // boundary between silence and tone stays at sample `quiet`
                        // however the scheduler behaved — a wall-clock `loud` flag moved
                        // it, which is half of why the old source was fragile.
                        let owed = frames_at(began.elapsed()).saturating_sub(written);
                        if owed == 0 {
                            std::thread::sleep(SOURCE_TICK);
                            continue;
                        }
                        let samples: Vec<f32> = (0..owed)
                            .flat_map(|i| {
                                let n = written + i;
                                if n < quiet {
                                    return [0.0, 0.0];
                                }
                                let t = (n - quiet) as f32 / RATE as f32;
                                let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                                [s, s]
                            })
                            .collect();
                        written += owed;
                        if input
                            .write(&pipeline::audio_decode::PcmBlock {
                                sample_rate: RATE,
                                channels: 2,
                                samples,
                                pts: Duration::ZERO,
                            })
                            .is_err()
                        {
                            return;
                        }
                        std::thread::sleep(SOURCE_TICK);
                    }
                }
            })
            .expect("a thread for the harness source");
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
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
    let (size, centres, timing) = decode_timed(file.path());

    assert_eq!(size, (320, 176), "the track's coded size");
    assert!(
        centres.len() >= 10,
        "two 200 ms segments at 30 fps should be a dozen frames, got {}",
        centres.len()
    );
    // The cadence, off the container rather than the pixels — which cannot carry it: the
    // picture is a constant colour, so twelve fresh frames and four fresh plus eight
    // duplicates decode identically (#234). Every slot is exactly one frame at the
    // stream's rate; a `trun` that wrote anything else is a stream that judders or runs
    // at the wrong speed against its own audio, in a player, with every byte accounted
    // for.
    let tick = i64::from(timing.timescale / FrameRate::DEFAULT.get());
    for window in timing.packets.windows(2) {
        if let [(a, da), (b, _)] = window {
            assert_eq!(
                b - a,
                tick,
                "consecutive video samples must be one slot apart: {:?}",
                timing.packets
            );
            assert_eq!(*da, tick, "and each must claim exactly one slot");
        }
    }
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

#[test]
fn letting_the_tap_go_takes_the_stream_live_status_with_it() {
    // The mechanism [`Published`] exists to keep out of the other tests' way, asserted
    // head-on so it is a documented property rather than a footnote in a struct (#208).
    //
    // It is also a real requirement: `/stream/*` answers from this status, and a panel
    // that kept saying `Live` after its tap went away would offer a playlist whose next
    // segment nobody is producing.
    let Some(published) = publish(320, 176, 1) else {
        return;
    };
    assert!(matches!(published.status(), StreamStatus::Live { .. }));

    let state = Arc::clone(&published.state);
    drop(published);

    // The encode thread is spawned and never joined, so waiting for it is the only honest
    // way to observe the end of it — which is exactly why a test that read the status
    // straight after the harness returned was racing.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && matches!(state.status(), StreamStatus::Live { .. }) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !matches!(state.status(), StreamStatus::Live { .. }),
        "a tap that has gone should stop the stream, not leave it advertising itself: {:?}",
        state.status()
    );
}

#[test]
fn sound_played_on_the_panel_comes_back_in_the_stream() {
    // Through the shipped path: a session takes an output from the tee'd factory and
    // writes to it exactly as a cast does. If the tee, the resampler, the mix, the AAC
    // encoder, the `esds` or the second `traf` is wrong, this is silence.
    let Some(state) = publish_with(
        320,
        176,
        2,
        Scenario {
            sound: Some(Duration::ZERO),
            ..Scenario::default()
        },
    ) else {
        return;
    };
    let file = playable(&state, 2);
    let sound = decode_audio(file.path()).expect("an audio track");
    assert_eq!(sound.sample_rate, RATE);
    assert_eq!(sound.channels, 2);
    assert!(
        sound.peak > 0.2,
        "the track decoded to a peak of {}, which is silence; {}",
        sound.peak,
        state.sound_diagnostics()
    );
}

#[test]
fn a_silent_panel_still_carries_a_continuous_audio_track() {
    // The normal case, and the one that breaks players: a track that stops when nothing
    // is playing is one a browser stalls on. Silence has to be *produced*.
    let Some(state) = publish_with(320, 176, 2, Scenario::default()) else {
        return;
    };
    let file = playable(&state, 2);
    let sound = decode_audio(file.path()).expect("an audio track even with nothing playing");
    assert_eq!(sound.sample_rate, RATE);
    assert_eq!(sound.peak, 0.0, "nothing was playing");
    assert!(sound.frames > 0, "and yet there are samples");
}

#[test]
fn the_audio_track_trails_the_live_edge_and_never_leads_it() {
    // Not the same thing as being in sync — see the placement test below for that. This is
    // the *shape*: the mixer holds a settle window so a session that hands over its block
    // a few milliseconds late still lands in the right place, which means the audio track
    // is always a little short of the video one at the live edge. What must not happen is
    // audio running *ahead*, which would mean it was being placed by arrival rather than
    // by the clock, and would compound into real drift.
    let segments = 4;
    let Some(state) = publish_with(320, 176, segments, Scenario::default()) else {
        return;
    };
    let file = playable(&state, segments);
    let (_, _, timing) = decode_timed(file.path());
    let sound = decode_audio(file.path()).unwrap();

    // Off the container, not `frames / assumed_fps`: with the rate assumed, a `trun`
    // writing wrong sample durations moved both sides of the subtraction together and
    // the comparison could not fail (#234).
    let video_secs = timing.seconds();
    let audio_secs = sound.frames as f64 / f64::from(RATE);
    let behind = video_secs - audio_secs;
    // The settle window, plus the segment whose audio has not been cut yet.
    let allowed = StreamConfig::default().audio_settle.as_secs_f64() + 0.2;
    assert!(
        (0.0..=allowed).contains(&behind),
        "audio is {behind:.3}s behind the video (video {video_secs:.3}s, audio {audio_secs:.3}s)"
    );
}

/// How long the panel is quiet before the tone, in the two placement tests.
const QUIET: Duration = Duration::from_millis(250);

/// Where the tone came back in the decoded track, or `None` where there is no GPU.
fn tone_onset(scenario: Scenario) -> Option<f64> {
    let segments = 4;
    let state = publish_with(320, 176, segments, scenario)?;
    state.assert_the_source_kept_up();
    let file = playable(&state, segments);
    let sound = decode_audio(file.path()).unwrap();
    let onset = sound.onset.unwrap_or_else(|| {
        panic!(
            "no tone anywhere in the track. {}",
            state.sound_diagnostics()
        )
    });
    let at = onset as f64 / f64::from(RATE);
    eprintln!(
        "onset {at:.3}s; quietest window {:?}; {}",
        sound.quietest_window,
        state.sound_diagnostics()
    );
    // No hole after the onset: a burst folded onto itself, a lost block, an encoder gap
    // are all a quiet window in the middle of a tone that should never dip (#234). The
    // placement assertion below cannot see them — `onset` is a first crossing and `peak`
    // a maximum.
    let quietest = sound.quietest_window.expect("the tone spans whole windows");
    assert!(
        quietest > 0.2,
        "a 20 ms window inside the tone dropped to {quietest}; the sound has a hole. {}",
        state.sound_diagnostics()
    );
    assert!(
        (at - QUIET.as_secs_f64()).abs() < 0.06,
        "the tone starts {at:.3}s in; it was played at {:.3}s. {}",
        QUIET.as_secs_f64(),
        state.sound_diagnostics()
    );
    Some(at)
}

#[test]
fn sound_lands_where_on_the_timeline_it_was_played() {
    // The sync assertion. The session is silent for a quarter of a second and then plays,
    // and the tone has to show up a quarter of a second into the decoded audio — not at
    // the start, which is where it would land if the mix placed blocks by arrival, and not
    // shifted by the encoder's priming delay, which is what `initial_padding` cancels.
    //
    // `#[ignore]`d against #208 for as long as it took to find out which of three
    // candidate defects it was measuring. It was the harness: a "session" driven from
    // inside the render loop (see [`Source`]), which under lavapipe and libx264 stops
    // producing for a fifth of a second at a time. The stream's own defect was real and
    // is fixed too — a burst of mixer passes summed onto one instant — but it was never
    // what this assertion caught, and the sandbox's dependence on the *encoder* was the
    // render thread losing its cores to it, not the audio path consulting it.
    tone_onset(Scenario {
        sound: Some(QUIET),
        ..Scenario::default()
    });
}

#[test]
fn a_stalled_render_thread_does_not_move_the_sound() {
    // The same assertion with the failure injected: the render thread held still for
    // 300 ms straddling the moment the tone starts. Before the source became an actor of
    // its own this moved the onset 207 ms — 100/200/300 ms of stall bought 74/109/207 ms
    // of miss, which brackets the 135 ms and 212 ms the sandbox produced by accident.
    //
    // Kept because it is the only thing here that fails if a "session" is ever wired back
    // onto the render thread, and because the panel really does stall: a browser paint
    // and a compositor pass share that thread with everything else `RenderLoop` does.
    // Sound the panel plays during a stall is sound it played, and it belongs where the
    // clock says.
    tone_onset(Scenario {
        sound: Some(QUIET),
        stall: Some(Duration::from_millis(300)),
    });
}

#[test]
fn the_stream_says_it_has_both_tracks() {
    // What an MSE `SourceBuffer` is opened with. One track missing from the codec string
    // and the browser refuses the whole stream.
    let Some(state) = publish_with(320, 176, 1, Scenario::default()) else {
        return;
    };
    let StreamStatus::Live { codec, encoder, .. } = state.status() else {
        panic!("published a segment but is not live");
    };
    eprintln!("encoded by {encoder} as {codec}");
    assert!(codec.contains("avc1."), "{codec}");
    assert!(codec.contains("mp4a."), "{codec}");
}
