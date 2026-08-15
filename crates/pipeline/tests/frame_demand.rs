//! The render loop knows when it next owes the glass a frame (#59).
//!
//! The property under test is the one the free-running kiosk never had: after a frame,
//! [`RenderLoop::demand`] answers *continuous* while something moves, a *deadline* when
//! the next change is scheduled, and *idle* when nothing will change until an external
//! event arrives — so the kiosk can sleep instead of presenting ~970 identical frames
//! a second at a 60 Hz panel.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use castaway_core::{ControlCapabilities, DecodedFrame, FrameImage, NowPlaying, PlaybackState};
use pipeline::attract::AttractScene;
use pipeline::demand::Demand;
use pipeline::nowplaying_card::NowPlayingCard;
use pipeline::render_pipeline::{FrameEpoch, RenderCommand, RenderLoop};

const W: u32 = 640;
const H: u32 = 360;
const TICK: Duration = Duration::from_millis(16);

fn video_frame() -> DecodedFrame {
    DecodedFrame {
        width: W,
        height: H,
        pts: Duration::ZERO,
        image: FrameImage::Cpu {
            format: castaway_core::PixelFormat::Rgba8,
            data: bytes::Bytes::from(vec![0x40; (W * H * 4) as usize]),
        },
    }
}

fn playing_card(title: &str) -> NowPlayingCard {
    let mut track = NowPlaying::default().with_title(title);
    track.state = PlaybackState::Playing;
    track.position = Some(Duration::ZERO);
    track.duration = Some(Duration::from_secs(200));
    NowPlayingCard {
        track,
        source: castaway_core::SourceDescription::default(),
        up_next: Vec::new(),
        controls: ControlCapabilities::PLAY | ControlCapabilities::PAUSE,
    }
}

fn home() -> (pipeline::RenderTx, RenderLoop) {
    let (tx, rx) = pipeline::render_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
    render.pump();
    (tx, render)
}

/// Frame until the loop stops demanding continuous frames; panics if it never settles.
fn settle(render: &mut RenderLoop) -> Demand {
    for _ in 0..1000 {
        let demand = render.frame(TICK);
        if demand != Demand::Frame {
            return demand;
        }
    }
    panic!("the loop demanded continuous frames for 16 simulated seconds");
}

#[test]
fn an_idle_home_screen_demands_nothing() {
    let (_tx, mut render) = home();
    assert_eq!(
        settle(&mut render),
        Demand::Idle,
        "an idle shell must let the kiosk sleep"
    );
}

#[test]
fn work_arriving_by_command_is_seen_on_that_same_frame() {
    let (tx, mut render) = home();
    assert_eq!(settle(&mut render), Demand::Idle);

    // A video frame starts the surface's entrance motion. `frame` applies commands
    // *before* stepping motions, so the very frame that received it already answers
    // "still moving" — with the opposite order the loop would go back to sleep on
    // exactly the frame something began to move.
    tx.send(RenderCommand::Video(
        video_frame(),
        FrameEpoch::ALWAYS_FRESH,
    ));
    assert_eq!(
        render.frame(TICK),
        Demand::Frame,
        "an entrance in progress demands the next frame"
    );
}

#[test]
fn a_scheduled_clear_is_a_deadline_not_a_spin() {
    let (tx, mut render) = home();
    tx.send(RenderCommand::Video(
        video_frame(),
        FrameEpoch::ALWAYS_FRESH,
    ));
    assert_eq!(settle(&mut render), Demand::Idle, "a still video is static");

    // The clear is deferred by its grace period (see `ClearVideo`); until it falls due
    // nothing on the glass changes, and the loop should sleep on a timer, not spin.
    let before = Instant::now();
    tx.send(RenderCommand::ClearVideo);
    match render.frame(TICK) {
        Demand::At(due) => assert!(
            due > before && due <= before + Duration::from_secs(5),
            "the deadline should be the clear's grace period from now"
        ),
        other => panic!("a pending clear must be a deadline, got {other:?}"),
    }
}

#[test]
fn a_playing_transport_ticks_once_a_second_not_every_frame() {
    let (tx, mut render) = home();
    tx.send(RenderCommand::NowPlaying(Box::new(playing_card("one"))));

    let demand = settle(&mut render);
    // `now` taken *after* settling: the deadline was computed against the clock inside
    // the last frame, so it can only be nearer than a second from here.
    let now = Instant::now();
    match demand {
        Demand::At(due) => assert!(
            due <= now + Duration::from_secs(1),
            "the strip's clock repaints at the next whole second, got {:?} away",
            due.saturating_duration_since(now)
        ),
        other => panic!("a playing strip keeps time on a deadline, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// The return to rest (#23).
//
// The lifecycle had a way home for every *press* — the pill, the edge swipe, a remote's
// Home button, `back` walking out one step at a time — and none for the person who simply
// walks away, which on a wall panel is how most sessions with it end. A panel left two
// screens deep at closing time was still two screens deep the next morning.
//
// It belongs in this file because of *how* it is built rather than what it does: a
// deadline read off standing facts, so the loop sleeps until it falls due instead of
// polling, and so a gesture that moves the panel back simply stops the answer existing.
// ---------------------------------------------------------------------------------------

/// A shell with the clock in the test's hands.
fn home_with_clock() -> (
    pipeline::RenderTx,
    RenderLoop,
    pipeline::render_clock::ManualClock,
) {
    let (tx, rx) = pipeline::render_channel(8);
    let (clock, time) = pipeline::render_clock::RenderClock::manual();
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap().with_clock(clock);
    tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
    render.pump();
    (tx, render, time)
}

fn a_screen() -> pipeline::shell::Screen {
    pipeline::shell::Screen::Picker(Box::new(pipeline::picker::Picker::loading(
        "Moonlight",
        "…",
    )))
}

#[test]
fn a_panel_left_two_screens_deep_returns_home_by_itself() {
    let (tx, mut render, time) = home_with_clock();
    tx.send(RenderCommand::PushScreen(Box::new(a_screen())));
    render.pump();
    render.note_touch();
    // A deadline rather than idle: the panel is away from rest, so it already owes the
    // glass a return — the loop just does not have to stay awake for it.
    assert!(
        matches!(settle(&mut render), Demand::At(_)),
        "an away-from-rest panel owes a return at a known time"
    );
    assert_eq!(render.shell_depth(), 2, "the screen is up");

    // Not yet: a minute of reading a screen is not somebody having left.
    time.advance(Duration::from_secs(60));
    render.frame(TICK);
    assert_eq!(
        render.shell_depth(),
        2,
        "the panel closed a screen out from under someone still reading it"
    );

    // …and then they are gone.
    time.advance(Duration::from_secs(61));
    settle(&mut render);
    assert_eq!(
        render.shell_depth(),
        1,
        "nobody has touched the panel in two minutes; it should be back Home"
    );
}

#[test]
fn the_return_is_a_deadline_the_loop_can_sleep_until() {
    // The point of doing it this way. A poll would hold the kiosk at display rate for the
    // whole two minutes — on an idle panel, which is the state it spends most of its life
    // in and the state #59 was about.
    let (tx, mut render, time) = home_with_clock();
    tx.send(RenderCommand::PushScreen(Box::new(a_screen())));
    render.pump();
    render.note_touch();

    let Demand::At(due) = settle(&mut render) else {
        panic!("a panel away from rest owes the glass a return, at a known time");
    };
    // Far enough out that this is a sleep rather than a spin, and it is the *touch* it is
    // measured from rather than the frame.
    assert!(
        due.duration_since(time.now()) > Duration::from_secs(100),
        "the deadline is only {:?} away; that is a poll, not a sleep",
        due.duration_since(time.now())
    );

    // Touching again puts it off, because the answer is derived rather than armed.
    time.advance(Duration::from_secs(60));
    render.note_touch();
    let Demand::At(again) = settle(&mut render) else {
        panic!("still away from rest, so still owed");
    };
    assert!(again > due, "a touch has to push the return back");
}

#[test]
fn a_panel_already_at_rest_is_owed_no_return() {
    // The failure this guards is quiet and permanent: a predicate that disagreed with
    // `rest` would wake the loop every two minutes forever, run a transition to where the
    // panel already is, and never settle.
    let (_tx, mut render, _time) = home_with_clock();
    render.note_touch();
    assert_eq!(
        settle(&mut render),
        Demand::Idle,
        "a touched panel that is already Home must still let the kiosk sleep"
    );

    // And after a return has happened, it stays settled rather than repeating.
    let (tx, mut render, time) = home_with_clock();
    tx.send(RenderCommand::PushScreen(Box::new(a_screen())));
    render.pump();
    render.note_touch();
    settle(&mut render);
    time.advance(Duration::from_secs(121));
    settle(&mut render);
    assert_eq!(render.shell_depth(), 1);
    assert_eq!(
        settle(&mut render),
        Demand::Idle,
        "the return happened; nothing is owed a second time"
    );
}

// ---------------------------------------------------------------------------------------
// The music visualiser (#15).
//
// Here for the same reason the return to rest is: what is interesting about it from this
// file's angle is that it asks for ~30 Hz rather than for display rate, and that it stops
// asking the moment the music does.
// ---------------------------------------------------------------------------------------

#[cfg(feature = "audio")]
mod visualizer {
    use super::*;
    use pipeline::mixer::MixTap;
    use pipeline::render_clock::{ManualClock, RenderClock};
    use std::sync::Arc;

    /// `frames` of a 1 kHz tone as interleaved stereo at the mix rate.
    ///
    /// A sine on purpose, not `output_stream.rs`'s per-block constant marker (#290): the
    /// analyzer is spectral, so what these tests need is in-band energy the bars can rise
    /// to — a DC marker has none — and nothing here asserts continuity, which is all the
    /// marker is for.
    fn tone(frames: usize) -> Vec<f32> {
        let rate = pipeline::mixer::RATE as f32;
        (0..frames)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / rate).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    /// A shell with a card up, an analyser attached, and the clock in the test's hands.
    ///
    /// The clock is not optional here. The bars are smoothed with time constants — 60 ms
    /// to rise, 400 ms to fall — and a test loop runs thousands of frames per real
    /// second, so on the wall clock `dt` is microseconds and nothing ever moves. This is
    /// the trap #156 was about, arriving from the other direction.
    fn rig() -> (RenderLoop, Arc<pipeline::visualizer::Analyzer>, ManualClock) {
        let (tx, rx) = pipeline::render_channel(8);
        let (clock, time) = RenderClock::manual();
        let mut render = RenderLoop::offscreen(W, H, rx).unwrap().with_clock(clock);
        tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
        render.pump();
        let analyzer = Arc::new(pipeline::visualizer::Analyzer::new());
        let mut render = render.with_visualizer(Arc::clone(&analyzer));
        tx.send(RenderCommand::NowPlaying(Box::new(playing_card("a track"))));
        render.pump();
        settle(&mut render);
        (render, analyzer, time)
    }

    /// Play `blocks` frames' worth of `audio`, one visualiser interval at a time.
    fn play(
        render: &mut RenderLoop,
        analyzer: &Arc<pipeline::visualizer::Analyzer>,
        time: &ManualClock,
        audio: &[f32],
        blocks: usize,
    ) -> Demand {
        let mut demand = Demand::Idle;
        for _ in 0..blocks {
            // The clock moves *before* the frame, so that when this returns `time.now()`
            // is the instant the last frame was drawn at. Advancing afterwards would step
            // straight over the deadline that frame just asked for, and the caller would
            // measure it as zero.
            time.advance(pipeline::visualizer::FRAME_INTERVAL);
            analyzer.mixed(time.now(), audio);
            demand = render.frame(TICK);
        }
        demand
    }

    #[test]
    fn bars_reach_the_panel_while_something_is_playing() {
        let (mut render, analyzer, time) = rig();

        // Nothing has been played, so there is nothing to draw — and, importantly, no
        // layer either: an empty upload every frame would be the cost of the feature with
        // none of the benefit.
        assert!(
            !render.has_visualizer_layer(),
            "a silent session must not put a layer up"
        );

        play(&mut render, &analyzer, &time, &tone(4_800), 20);
        assert!(
            render.has_visualizer_layer(),
            "a playing session should have bars on the panel"
        );
    }

    #[test]
    fn the_bars_ask_for_thirty_frames_a_second_rather_than_display_rate() {
        // The whole reason this is a deadline: an audio session is up for the length of an
        // album, and `Demand::Frame` for that long is the most expensive thing on the box.
        let (mut render, analyzer, time) = rig();
        let demand = play(&mut render, &analyzer, &time, &tone(4_800), 20);
        let Demand::At(due) = demand else {
            panic!("moving bars should ask for a frame at a time, got {demand:?}");
        };
        // Measured against the clock the loop is actually reading.
        let ahead = due.saturating_duration_since(time.now());
        assert!(
            ahead > Duration::from_millis(10),
            "the bars asked for another frame in {ahead:?}; that is display rate, not 30 Hz"
        );
    }

    #[test]
    fn the_layer_goes_away_when_the_music_does() {
        // A paused track is most of the time an audio session is on the panel. The layer
        // going means the loop can sleep, which is the difference between a visualiser and
        // a space heater.
        let (mut render, analyzer, time) = rig();
        play(&mut render, &analyzer, &time, &tone(4_800), 20);
        assert!(render.has_visualizer_layer());

        // The music stops. Silence still *arrives* — the mixer pads with it whenever no
        // source has anything to say — so this is the real shape of a paused session
        // rather than a tap that has been starved.
        let quiet = vec![0.0f32; 9_600];
        play(&mut render, &analyzer, &time, &quiet, 200);
        assert!(
            !render.has_visualizer_layer(),
            "the bars are still up after the music stopped"
        );
    }
}
