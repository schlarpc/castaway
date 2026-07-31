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
use pipeline::render_pipeline::{RenderCommand, RenderLoop};

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
    tx.send(RenderCommand::Video(video_frame()));
    assert_eq!(
        render.frame(TICK),
        Demand::Frame,
        "an entrance in progress demands the next frame"
    );
}

#[test]
fn a_scheduled_clear_is_a_deadline_not_a_spin() {
    let (tx, mut render) = home();
    tx.send(RenderCommand::Video(video_frame()));
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
