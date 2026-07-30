//! Navigating is animated, and the gesture drags the animation (D38).
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use pipeline::attract::AttractScene;
use pipeline::render_pipeline::{RenderCommand, RenderLoop};
use pipeline::shell::Screen;
use std::time::Duration;

fn shell() -> (pipeline::RenderTx, RenderLoop) {
    let (tx, rx) = pipeline::render_channel(8);
    let mut render = RenderLoop::offscreen(640, 360, rx).unwrap();
    tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
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

#[test]
fn a_carried_navigation_brings_the_incoming_screen_with_it() {
    // Only the outgoing screen used to move, so a half-completed swipe slid one screen aside to
    // reveal the next already sitting there whole — two unrelated things rather than one
    // navigation being carried by a hand.
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    assert_eq!(render.shell_depth(), 2);

    // A back, then carried by a finger rather than left to the clock.
    assert!(render.shell_back());
    let mut incoming = Vec::new();
    let mut outgoing = Vec::new();
    for step in 0..=10 {
        let shown = 1.0 - step as f32 / 10.0;
        render.drive_transition(shown, 0.0);
        incoming.push(render.floor_placement().expect("the floor is placed").0);
        outgoing.push(
            render
                .outgoing_screen_rect()
                .expect("a transition is running"),
        );
    }

    // The outgoing screen leaves.
    let (first_out, last_out) = (outgoing[0], outgoing[10]);
    assert!(
        last_out.x > first_out.x + 0.3 || last_out.w < first_out.w - 0.1,
        "the outgoing screen should have gone: {first_out:?} -> {last_out:?}"
    );
    // And the incoming one arrives *over the same range of the drag*, rather than being whole
    // from the first frame.
    let (first_in, last_in) = (incoming[0], incoming[10]);
    assert!(
        (last_in.w - 1.0).abs() < 0.01 && (last_in.x).abs() < 0.01,
        "it should end filling the panel: {last_in:?}"
    );
    assert!(
        first_in != last_in,
        "the incoming screen never moved: {first_in:?}"
    );
    // Monotone in the drag, which is what "attached to the hand" means: no frame may go
    // backwards while the finger goes forwards.
    for pair in incoming.windows(2) {
        if let [a, b] = pair {
            assert!(
                b.w >= a.w - 1e-4,
                "the carry reversed against the finger: {a:?} -> {b:?}"
            );
        }
    }
}

#[test]
fn letting_go_of_a_carry_hands_it_to_the_spring_rather_than_freezing_it() {
    // The point of driving a position directly while a finger is down: the moment it lifts, the
    // spring takes over from exactly there. A carry that stayed put would leave the panel
    // half-navigated.
    let (_tx, mut render) = shell();
    render.shell_push(picker());
    assert!(render.shell_back());
    render.drive_transition(0.3, 0.0);
    let held = render.floor_placement().expect("placed").0;

    render.release_transition();
    let mut settled = held;
    for _ in 0..180 {
        let moving = render.tick_transition(std::time::Duration::from_millis(16));
        render.tick_motion(std::time::Duration::from_millis(16));
        settled = render.floor_placement().expect("placed").0;
        if !moving {
            break;
        }
    }
    assert!(
        (settled.w - 1.0).abs() < 0.01,
        "released past half way, it should finish arriving: {held:?} -> {settled:?}"
    );
    assert_eq!(render.shell_depth(), 1, "and the navigation is committed");
}
