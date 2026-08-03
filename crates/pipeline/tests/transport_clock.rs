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
    // Every position below is a number this test chose, not a number the host produced.
    // It used to sleep 60 ms and assert the clock had advanced "at least 40 ms" and then
    // re-anchored to "under 40 ms", which is a statement about the machine as much as
    // about the code: under a full `nix flake check`, 388 tests deep against lavapipe, the
    // re-anchored reading came back at 50.5 ms and the gate went red for a reason that had
    // nothing to do with the change under test (#156). With the clock injected,
    // "re-anchored" is asserted as what it actually means — the elapsed time is zero.
    let (tx, rx) = pipeline::render_channel(8);
    let (clock, time) = pipeline::render_clock::RenderClock::manual();
    let Some(mut render) = pipeline::test_gpu::render_loop(640, 360, rx) else {
        return;
    };
    let mut render = render.with_clock(clock);
    let render = &mut render;

    // Track one starts at 0:00 and plays.
    tx.send(RenderCommand::NowPlaying(Box::new(card(
        "one",
        Duration::from_secs(200),
    ))));
    render.pump();
    time.advance(Duration::from_millis(60));
    assert_eq!(
        render.transport_position().unwrap(),
        Duration::from_millis(60),
        "the clock ticks between readings, at exactly the rate time passes"
    );

    // A republish of the *same* reading — a queue update, the device naming itself —
    // must not rewind the clock to the stale base.
    tx.send(RenderCommand::NowPlaying(Box::new(card(
        "one",
        Duration::from_secs(200),
    ))));
    render.pump();
    assert_eq!(
        render.transport_position().unwrap(),
        Duration::from_millis(60),
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
    assert_eq!(
        render.transport_position().unwrap(),
        Duration::ZERO,
        "a new reading re-anchors"
    );

    // And it goes on running from there, rather than being pinned by the re-anchor.
    time.advance(Duration::from_secs(5));
    assert_eq!(
        render.transport_position().unwrap(),
        Duration::from_secs(5),
        "the re-anchored clock ticks like any other"
    );
}

/// A track that can be seeked, paused where the source left it.
///
/// Paused on purpose: it is the case #97 records as the worst of the two. The strip
/// repaints when the visible second changes, and a paused clock has no visible second to
/// change — so a drag over a paused track used to repaint not once a second but *never*.
fn seekable_paused(duration: Duration, position: Duration) -> NowPlayingCard {
    let mut track = NowPlaying::default().with_title("one");
    track.state = PlaybackState::Paused;
    track.position = Some(position);
    track.duration = Some(duration);
    NowPlayingCard {
        track,
        source: castaway_core::SourceDescription::default(),
        up_next: Vec::new(),
        controls: ControlCapabilities::PLAY
            | ControlCapabilities::PAUSE
            | ControlCapabilities::SEEK,
    }
}

/// A point on the scrub track, `fraction` of the way along it, in panel-normalized
/// coordinates — the form a touch event actually arrives in.
fn on_track(render: &pipeline::render_pipeline::RenderLoop, fraction: f32) -> (f32, f32) {
    on_track_of(render, (640, 360), fraction)
}

/// The same, on a panel of a nominated size.
fn on_track_of(
    render: &pipeline::render_pipeline::RenderLoop,
    (w, h): (u32, u32),
    fraction: f32,
) -> (f32, f32) {
    let (ox, oy, sw, sh) = pipeline::transport::placement(w, h);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let layout = pipeline::transport::layout(
        &render.transport_model().expect("a strip is on screen"),
        sw.round() as u32,
        sh.round() as u32,
    );
    let track = layout
        .track_touch
        .expect("a seekable track has a scrub bar");
    let (_, ty) = track.center();
    (
        (ox + track.x + track.w * fraction) / w as f32,
        (oy + ty) / h as f32,
    )
}

#[test]
fn the_bar_follows_the_finger_while_it_is_down_and_asks_for_nothing_until_it_lifts() {
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(640, 360, rx) else {
        return;
    };

    // What the panel looks like when the *source* says 40 s of a 200 s track — the
    // reference the drag has to reproduce, because a preview that painted anything else
    // would be showing the user a position they are not about to get.
    tx.send(RenderCommand::NowPlaying(Box::new(seekable_paused(
        Duration::from_secs(200),
        Duration::from_secs(40),
    ))));
    render.pump();
    let at_forty = render.read_rgba().unwrap();

    // Back to the start, and this time get there with a finger.
    tx.send(RenderCommand::NowPlaying(Box::new(seekable_paused(
        Duration::from_secs(200),
        Duration::ZERO,
    ))));
    render.pump();
    let at_zero = render.read_rgba().unwrap();
    assert_ne!(at_zero, at_forty, "the two readings must look different");

    // A finger goes down at a fifth of the way along and the panel already shows it. The
    // seek has not happened — that is the whole asymmetry the strip is built on.
    let (x, y) = on_track(&render, 0.2);
    assert!(
        render
            .transport_action(x, y, pipeline::transport::TouchPhase::Press)
            .is_none(),
        "a press on the track must not seek: the release position is the chosen one"
    );
    render.pump();
    assert_eq!(
        render.read_rgba().unwrap(),
        at_forty,
        "the bar under the finger must read the same as the source saying so"
    );

    // Sliding moves it, on a paused track, which is the case that never repainted at all.
    let (x, y) = on_track(&render, 0.8);
    assert!(render
        .transport_action(x, y, pipeline::transport::TouchPhase::Drag)
        .is_none());
    render.pump();
    let at_eighty = render.read_rgba().unwrap();
    assert_ne!(
        at_eighty, at_forty,
        "the bar did not follow the finger along the track"
    );

    // And the lift is what finally asks the source for anything.
    let txn = render.transport_action(x, y, pipeline::transport::TouchPhase::Release);
    match txn {
        Some(castaway_core::ControlTxn::Seek(to)) => {
            let off = to.as_secs_f32() - 160.0;
            assert!(
                off.abs() < 2.0,
                "a lift at 80% of 200 s is ~2:40, got {to:?}"
            );
        }
        other => panic!("the lift must seek, got {other:?}"),
    }
    // …after which the strip is the source's picture again, not the finger's. The source
    // has not moved yet — it is being asked to — so this is 0:00 once more.
    render.pump();
    assert_eq!(
        render.read_rgba().unwrap(),
        at_zero,
        "the preview must not outlive the gesture that made it"
    );
}

#[test]
fn a_cancelled_drag_puts_the_bar_back_without_seeking() {
    // A phone that drops off Wi-Fi mid-drag, or a contact the compositor loses. The
    // gesture did not finish, so nothing is asked of the source and the panel goes back
    // to showing what the source last said.
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(640, 360, rx) else {
        return;
    };
    tx.send(RenderCommand::NowPlaying(Box::new(seekable_paused(
        Duration::from_secs(200),
        Duration::ZERO,
    ))));
    render.pump();
    let at_zero = render.read_rgba().unwrap();

    let (x, y) = on_track(&render, 0.7);
    render.transport_action(x, y, pipeline::transport::TouchPhase::Press);
    render.pump();
    assert_ne!(
        render.read_rgba().unwrap(),
        at_zero,
        "the premise: a finger is down and the bar has moved to it"
    );

    render.clear_scrub_preview();
    render.pump();
    assert_eq!(
        render.read_rgba().unwrap(),
        at_zero,
        "a lost contact must leave the bar where the music is"
    );
}

#[test]
fn the_bar_keeps_up_with_the_finger_inside_a_single_second() {
    // The rate limit, which is the half of #97 that is easy to get wrong in a way no
    // ordinary test notices: the strip repaints when the *visible second* changes, and a
    // drag moves the bar far faster than that. Every assertion above happens to cross a
    // second boundary, so a preview honoured at the clock's cadence would still pass them
    // — and on the glass it would be a bar that lurches once a second under a finger that
    // is moving smoothly.
    //
    // A minute-long track on a wide panel is what makes the two cadences separable: a few
    // pixels along the bar is a visible move and less than half a second of music, so a
    // repaint that waited for the second would not happen at all.
    const PANEL: (u32, u32) = (1280, 720);
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(PANEL.0, PANEL.1, rx) else {
        return;
    };
    tx.send(RenderCommand::NowPlaying(Box::new(seekable_paused(
        Duration::from_secs(60),
        Duration::ZERO,
    ))));
    render.pump();

    // 6.30 s of 60 s. Deliberately not 6.00: the fraction is a float, and a press that
    // landed a hair under the boundary would put the two samples in different seconds and
    // quietly turn this into a test of nothing.
    let (x, y) = on_track_of(&render, PANEL, 0.105);
    render.transport_action(x, y, pipeline::transport::TouchPhase::Press);
    render.pump();
    let at_six = render.read_rgba().unwrap();

    // 6.68 s: five pixels along a 794-pixel bar, and the same second on the clock.
    let (x, y) = on_track_of(&render, PANEL, 0.1113);
    render.transport_action(x, y, pipeline::transport::TouchPhase::Drag);
    render.pump();
    assert_ne!(
        render.read_rgba().unwrap(),
        at_six,
        "the bar stopped following the finger between one second and the next"
    );
}

#[test]
fn the_source_publishing_under_a_drag_does_not_snatch_the_bar_back() {
    // Sources publish for reasons that have nothing to do with the finger — a position
    // tick, a queue update, a device naming itself — and a card republished mid-scrub
    // rebuilds the strip's state. If the drag did not survive that, the bar would snap
    // back to the music halfway through a gesture, which reads as the panel fighting the
    // hand rather than as a bug.
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(640, 360, rx) else {
        return;
    };
    tx.send(RenderCommand::NowPlaying(Box::new(seekable_paused(
        Duration::from_secs(200),
        Duration::ZERO,
    ))));
    render.pump();

    let (x, y) = on_track(&render, 0.75);
    render.transport_action(x, y, pipeline::transport::TouchPhase::Press);
    render.pump();
    let under_the_finger = render.read_rgba().unwrap();

    // The source says something, as sources do.
    tx.send(RenderCommand::NowPlaying(Box::new(seekable_paused(
        Duration::from_secs(200),
        Duration::from_secs(3),
    ))));
    render.pump();

    assert!(
        render
            .scrub_preview()
            .is_some_and(|f| (f - 0.75).abs() < 0.02),
        "the finger still has the scrubber: {:?}",
        render.scrub_preview()
    );
    assert_eq!(
        render.read_rgba().unwrap(),
        under_the_finger,
        "the bar must stay under the finger, not jump to what the source just said"
    );
}
