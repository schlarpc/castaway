//! Navigating is animated, and the gesture drags the animation (D38).
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use pipeline::attract::AttractScene;
use pipeline::render_pipeline::{RenderCommand, RenderLoop};
use pipeline::shell::Screen;
use std::time::Duration;

fn shell() -> (std::sync::mpsc::SyncSender<RenderCommand>, RenderLoop) {
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(640, 360, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    render.pump();
    (tx, render)
}

fn picker() -> Screen {
    Screen::Picker(Box::new(pipeline::picker::Picker::loading(
        "Moonlight",
        "…",
    )))
}

#[test]
fn navigating_starts_a_transition_that_finishes_on_its_own() {
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    assert!(render.transitioning(), "a push should animate");

    // Run it out on the clock, as the kiosk does every frame.
    let mut guard = 0;
    while render.tick_transition(Duration::from_millis(16)) {
        guard += 1;
        assert!(guard < 500, "the transition never finished");
    }
    assert!(!render.transitioning());
    assert_eq!(render.shell_depth(), 2, "and it navigated");
}

#[test]
fn going_back_animates_too() {
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    while render.tick_transition(Duration::from_millis(16)) {}
    assert!(render.shell_back());
    assert!(render.transitioning());
}

#[test]
fn a_driven_transition_does_not_advance_on_the_clock() {
    // While a finger is on the glass the progress is its, not time's — otherwise the
    // panel would run ahead of the hand.
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    render.drive_transition(0.5, 0.0);
    for _ in 0..100 {
        assert!(
            render.tick_transition(Duration::from_millis(16)),
            "a driven transition stays put"
        );
    }
    assert!(render.transitioning());

    // Let go, and the clock takes it from where the finger left it.
    render.release_transition();
    let mut guard = 0;
    while render.tick_transition(Duration::from_millis(16)) {
        guard += 1;
        assert!(guard < 500);
    }
    assert!(!render.transitioning());
}

#[test]
fn a_transition_ends_even_if_it_is_driven_past_its_bounds() {
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    // Out-of-range progress from a wild drag must not wedge it.
    render.drive_transition(-5.0, 0.0);
    render.drive_transition(9.0, 0.0);
    render.release_transition();
    let mut guard = 0;
    while render.tick_transition(Duration::from_millis(16)) {
        guard += 1;
        assert!(guard < 500, "clamping should not prevent completion");
    }
    assert!(!render.transitioning());
}

#[test]
fn letting_go_half_way_springs_back_and_undoes_the_navigation() {
    // The point of a card that follows the hand: an abandoned gesture leaves the panel
    // where it started, rather than committing because a threshold was crossed.
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    while render.tick_transition(Duration::from_millis(16)) {}
    assert_eq!(render.shell_depth(), 2);

    assert!(render.shell_back());
    render.drive_transition(0.8, 0.0); // barely moved
    render.release_transition();
    let mut guard = 0;
    while render.tick_transition(Duration::from_millis(16)) {
        guard += 1;
        assert!(guard < 500);
    }
    assert_eq!(render.shell_depth(), 2, "it went back to where it was");
}

#[test]
fn a_flick_wins_over_where_the_finger_let_go() {
    // Thrown away early still means away: someone who flicked meant it.
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    while render.tick_transition(Duration::from_millis(16)) {}

    assert!(render.shell_back());
    render.drive_transition(0.9, -4.0); // hardly moved, moving fast
    render.release_transition();
    let mut guard = 0;
    while render.tick_transition(Duration::from_millis(16)) {
        guard += 1;
        assert!(guard < 500);
    }
    assert_eq!(render.shell_depth(), 1, "the flick carried it home");
}

#[test]
fn a_transition_never_covers_a_cast() {
    // It draws directly above the shell's own surface and below everything else, so a
    // navigation animating while something plays is invisible under it rather than
    // flashing over it.
    use pipeline::compositor::LayerId;
    assert!(LayerId::ShellPrev > LayerId::Attract);
    assert!(LayerId::ShellPrev < LayerId::Video);
    assert!(LayerId::ShellPrev < LayerId::BrowserFullscreen);
    assert!(LayerId::ShellPrev < LayerId::Transport);
}
