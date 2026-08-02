//! The transport strip's clock: anchored by published readings, ticking between them.
//!
//! The regression this pins (TODO items 25/26): the anchor was carried over whenever
//! the *position* matched the previous model — and since a source that only publishes
//! a position at track start always publishes 0, advancing a track from the phone kept
//! the old track's anchor. The song sat at 0:00 while the scrubber read 1:20.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use castaway_core::{ControlCapabilities, NowPlaying, PlaybackState};
use pipeline::nowplaying_card::NowPlayingCard;
use pipeline::render_pipeline::RenderCommand;

fn card(title: &str, duration: Duration) -> NowPlayingCard {
    let mut track = NowPlaying::default().with_title(title);
    track.state = PlaybackState::Playing;
    track.position = Some(Duration::ZERO);
    track.duration = Some(duration);
    NowPlayingCard {
        track,
        source: castaway_core::SourceDescription::default(),
        up_next: Vec::new(),
        controls: ControlCapabilities::PLAY | ControlCapabilities::PAUSE,
    }
}

#[test]
fn a_new_reading_re_anchors_the_clock_and_a_repeat_does_not() {
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(640, 360, rx) else {
        return;
    };
    let render = &mut render;

    // Track one starts at 0:00 and plays.
    tx.send(RenderCommand::NowPlaying(Box::new(card(
        "one",
        Duration::from_secs(200),
    ))));
    render.pump();
    std::thread::sleep(Duration::from_millis(60));
    let elapsed = render.transport_position().unwrap();
    assert!(
        elapsed >= Duration::from_millis(40),
        "the clock ticks between readings: {elapsed:?}"
    );

    // A republish of the *same* reading — a queue update, the device naming itself —
    // must not rewind the clock to the stale base.
    tx.send(RenderCommand::NowPlaying(Box::new(card(
        "one",
        Duration::from_secs(200),
    ))));
    render.pump();
    assert!(
        render.transport_position().unwrap() >= Duration::from_millis(40),
        "an identical reading keeps its anchor"
    );

    // The phone advances to the next track: also at 0:00, but a different reading
    // (here, a different duration). The clock must restart from zero, not inherit
    // the minute the last track had already played.
    tx.send(RenderCommand::NowPlaying(Box::new(card(
        "two",
        Duration::from_secs(180),
    ))));
    render.pump();
    let fresh = render.transport_position().unwrap();
    assert!(
        fresh < Duration::from_millis(40),
        "a new reading re-anchors: {fresh:?}"
    );
}
