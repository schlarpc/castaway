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
    publish_with(width, height, segments, None).map(|(state, _)| state)
}

/// The same, with the panel playing something.
///
/// `sound` is handed the same `MixInput` a real session would take, so what reaches the
/// stream reaches it the way a cast's audio does — through the panel's one mixer — rather
/// than through a back door the shipped path does not have.
fn publish_with(
    width: u32,
    height: u32,
    segments: u32,
    sound: Option<Session<'_>>,
) -> Option<(Arc<LiveStream>, Arc<StreamAudio>)> {
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
    // A session, opened the way one really is.
    let mut session = sound.map(|_| mixer.input(pipeline::mixer::Backpressure::Pull));

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        // The tap retires when nothing has asked for a while, and in this harness nothing
        // is asking over HTTP.
        state.touch(Instant::now());
        rloop.pump();
        if let (Some(session), Some(sound)) = (session.as_mut(), sound) {
            sound(session);
        }
        if let StreamStatus::Failed(why) = state.status() {
            eprintln!("no encoder here, skipping: {why}");
            return None;
        }
        if state.segment(segments).is_some() {
            return Some((state, audio));
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

/// A session writing into the stream, poked once per pump.
type Session<'a> = &'a dyn Fn(&mut pipeline::mixer::MixInput);

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
    Some(Sound {
        sample_rate: decoder.rate(),
        channels: decoder.channels(),
        frames,
        peak,
        onset,
    })
}

/// A session that writes real time's worth of audio whenever it is poked: silence until
/// `quiet_for` has passed, a 440 Hz tone after it.
///
/// Paced against the wall clock rather than per call, because the mix places a block where
/// the clock says it belongs — so a source that produced faster than real time would stack
/// its blocks on the same positions and simply get louder.
struct Tone {
    /// Set on the first block written, not at construction: the timeline is anchored by
    /// the first composited frame, and between building this and that there is a GPU
    /// device to open. Starting the clock here puts the source and the timeline within a
    /// frame of each other, which is what the placement test is measuring against.
    started: std::cell::RefCell<Option<Instant>>,
    quiet_for: Duration,
    last: std::cell::RefCell<Option<Instant>>,
    phase: std::cell::Cell<u64>,
}

impl Tone {
    fn after(quiet_for: Duration) -> Self {
        Self {
            started: std::cell::RefCell::new(None),
            quiet_for,
            last: std::cell::RefCell::new(None),
            phase: std::cell::Cell::new(0),
        }
    }

    fn feed(&self, out: &mut pipeline::mixer::MixInput) {
        let now = Instant::now();
        let mut last = self.last.borrow_mut();
        let since = last.map_or(Duration::from_millis(10), |t| {
            now.saturating_duration_since(t)
        });
        if since < Duration::from_millis(10) {
            return;
        }
        *last = Some(now);
        let frames = usize::try_from(since.as_millis() * u128::from(RATE) / 1000).unwrap_or(0);
        let phase = self.phase.get();
        self.phase.set(phase + frames as u64);
        let started = *self.started.borrow_mut().get_or_insert(now);
        let loud = now.saturating_duration_since(started) >= self.quiet_for;
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                if !loud {
                    return [0.0, 0.0];
                }
                let t = (phase + i as u64) as f32 / RATE as f32;
                let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                [s, s]
            })
            .collect();
        let _ = out.write(&pipeline::audio_decode::PcmBlock {
            sample_rate: RATE,
            channels: 2,
            samples,
            pts: Duration::ZERO,
        });
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

#[test]
fn sound_played_on_the_panel_comes_back_in_the_stream() {
    // Through the shipped path: a session takes an output from the tee'd factory and
    // writes to it exactly as a cast does. If the tee, the resampler, the mix, the AAC
    // encoder, the `esds` or the second `traf` is wrong, this is silence.
    let tone = Tone::after(Duration::ZERO);
    let Some((state, _audio)) = publish_with(320, 176, 2, Some(&|out| tone.feed(out))) else {
        return;
    };
    let file = playable(&state, 2);
    let sound = decode_audio(file.path()).expect("an audio track");
    assert_eq!(sound.sample_rate, RATE);
    assert_eq!(sound.channels, 2);
    assert!(
        sound.peak > 0.2,
        "the track decoded to a peak of {}, which is silence",
        sound.peak
    );
}

#[test]
fn a_silent_panel_still_carries_a_continuous_audio_track() {
    // The normal case, and the one that breaks players: a track that stops when nothing
    // is playing is one a browser stalls on. Silence has to be *produced*.
    let Some((state, _audio)) = publish_with(320, 176, 2, None) else {
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
    let Some((state, _audio)) = publish_with(320, 176, segments, None) else {
        return;
    };
    let file = playable(&state, segments);
    let (_, centres) = decode(file.path());
    let sound = decode_audio(file.path()).unwrap();

    let video_secs = centres.len() as f64 / f64::from(FrameRate::DEFAULT.get());
    let audio_secs = sound.frames as f64 / f64::from(RATE);
    let behind = video_secs - audio_secs;
    // The settle window, plus the segment whose audio has not been cut yet.
    let allowed = StreamConfig::default().audio_settle.as_secs_f64() + 0.2;
    assert!(
        (0.0..=allowed).contains(&behind),
        "audio is {behind:.3}s behind the video (video {video_secs:.3}s, audio {audio_secs:.3}s)"
    );
}

#[test]
fn sound_lands_where_on_the_timeline_it_was_played() {
    // The sync assertion. The session is silent for a quarter of a second and then plays,
    // and the tone has to show up a quarter of a second into the decoded audio — not at
    // the start, which is where it would land if the mix placed blocks by arrival, and not
    // shifted by the encoder's priming delay, which is what `initial_padding` cancels.
    let quiet = Duration::from_millis(250);
    let tone = Tone::after(quiet);
    let segments = 4;
    let Some((state, _audio)) = publish_with(320, 176, segments, Some(&|out| tone.feed(out)))
    else {
        return;
    };
    let file = playable(&state, segments);
    let sound = decode_audio(file.path()).unwrap();
    let onset = sound
        .onset
        .expect("the tone should be somewhere in the track");
    let at = onset as f64 / f64::from(RATE);
    assert!(
        (at - quiet.as_secs_f64()).abs() < 0.06,
        "the tone starts {at:.3}s in; it was played at {:.3}s",
        quiet.as_secs_f64()
    );
}

#[test]
fn the_stream_says_it_has_both_tracks() {
    // What an MSE `SourceBuffer` is opened with. One track missing from the codec string
    // and the browser refuses the whole stream.
    let Some((state, _audio)) = publish_with(320, 176, 1, None) else {
        return;
    };
    let StreamStatus::Live { codec, encoder, .. } = state.status() else {
        panic!("published a segment but is not live");
    };
    eprintln!("encoded by {encoder} as {codec}");
    assert!(codec.contains("avc1."), "{codec}");
    assert!(codec.contains("mp4a."), "{codec}");
}
