//! Picture-in-picture, and going home when nothing is on (#27, #28, D38).
//!
//! Both come out of the same fact: the shell draws *below* video. Navigating while
//! something plays would be invisible, so the video is demoted rather than stopped —
//! someone pressing Home in the middle of a film has not asked for it to end.
//!
//! The *rules* live in `pipeline::panel`, as unit tests with no GPU in them. What is left
//! here is the seam: that the render loop's public answers — `pip_rect`,
//! `hit_minimized_card`, `RenderCommand::RestPanel` — still agree with the model they are
//! derived from, on a real surface, with real commands flowing through the channel. That
//! agreement is the thing that broke last time.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use castaway_core::{DecodedFrame, FrameImage};
use pipeline::attract::AttractScene;
use pipeline::render_pipeline::{RenderCommand, RenderLoop};

const W: u32 = 1920;
const H: u32 = 1080;

fn frame() -> DecodedFrame {
    DecodedFrame {
        width: W,
        height: H,
        pts: std::time::Duration::ZERO,
        image: FrameImage::Cpu {
            format: castaway_core::PixelFormat::Rgba8,
            data: bytes::Bytes::from(vec![0x40; (W * H * 4) as usize]),
        },
    }
}

fn playing() -> (std::sync::mpsc::SyncSender<RenderCommand>, RenderLoop) {
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    tx.try_send(RenderCommand::Video(frame())).unwrap();
    render.pump();
    (tx, render)
}

#[test]
fn bringing_the_shell_forward_demotes_the_video_rather_than_stopping_it() {
    let (_tx, mut render) = playing();
    assert!(
        render.pip_rect().is_none(),
        "fullscreen while nothing else wants the panel"
    );

    render.set_shell_foreground(true);
    let (ox, oy, sx, sy) = render.pip_rect().expect("the video should be demoted");
    assert!(sx < 0.5 && sy < 0.5, "a corner, not the screen");
    assert!(ox > 0.5 && oy > 0.5, "bottom-right, clear of the home pill");
    render.pump();

    // And the shell is visible again where its own content is.
    let scene = AttractScene::demo();
    let (_, tile) = pipeline::attract::tile_layout(&scene, W, H)
        .into_iter()
        .next()
        .unwrap();
    let (cx, cy) = tile.center();
    assert!(
        render.shell_hit(cx / W as f32, cy / H as f32).is_some(),
        "with the video in a corner, the tiles are reachable again"
    );
}

#[test]
fn tapping_the_demoted_video_is_how_it_comes_back() {
    let (_tx, mut render) = playing();
    render.set_shell_foreground(true);
    let (ox, oy, sx, sy) = render.pip_rect().unwrap();
    assert!(render.hit_pip(ox + sx / 2.0, oy + sy / 2.0));
    assert!(
        !render.hit_pip(0.1, 0.1),
        "the rest of the panel is the shell's"
    );

    render.set_shell_foreground(false);
    assert!(render.pip_rect().is_none());
}

#[test]
fn a_cast_starting_while_someone_navigates_arrives_in_the_corner() {
    // Not covering what they are reading. The alternative — always full-screen on the
    // first frame — is the behaviour that made the shell unusable during playback.
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    render.pump();
    render.set_shell_foreground(true);

    tx.try_send(RenderCommand::Video(frame())).unwrap();
    render.pump();

    assert!(render.pip_rect().is_some(), "it should arrive demoted");
}

#[test]
fn an_ending_session_returns_the_panel_home() {
    let (tx, mut render) = playing();
    render.shell_push(pipeline::shell::Screen::Picker(Box::new(
        pipeline::picker::Picker::loading("Moonlight", "…"),
    )));
    assert_eq!(render.shell_depth(), 2);

    tx.try_send(RenderCommand::RestPanel).unwrap();
    render.pump();

    assert_eq!(render.shell_depth(), 1, "back to a known state");
    assert!(!render.shell_foreground());
}

#[test]
fn but_not_out_from_under_someone_using_it() {
    // The failure this prevents: a track ending while someone is three screens into a
    // picker closes it, which reads as the panel losing their place for no reason.
    let (tx, mut render) = playing();
    render.shell_push(pipeline::shell::Screen::Picker(Box::new(
        pipeline::picker::Picker::loading("Moonlight", "…"),
    )));
    render.note_touch();

    tx.try_send(RenderCommand::RestPanel).unwrap();
    render.pump();

    assert_eq!(render.shell_depth(), 2, "still where they left it");
}

#[test]
fn an_audio_session_minimizes_into_the_widget_slot_and_restores() {
    // The audio twin of the video PiP: a Spotify/Bluetooth session's card sits above
    // the shell, so going home used to change nothing anyone could see. Now the card
    // shrinks into the home screen's widget slot, and a tap there is how it comes back.
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    tx.try_send(RenderCommand::NowPlaying(Box::default()))
        .unwrap();
    render.pump();

    let (cx, cy) = slot_center();
    assert!(
        !render.hit_minimized_card(cx, cy),
        "fullscreen card is not a minimized one"
    );

    render.set_shell_foreground(true);
    assert!(
        render.hit_minimized_card(cx, cy),
        "card should be in the slot"
    );
    assert!(
        !render.hit_minimized_card(0.05, 0.05),
        "the rest of the panel is not the card"
    );

    // Restoring is the slot tap's job (the kiosk routes it); from here it is just the
    // shell going back.
    render.set_shell_foreground(false);
    assert!(!render.hit_minimized_card(cx, cy));
}

/// The middle of where a demoted card sits — taken from the model, so this asserts the
/// render loop agrees with it rather than restating the arithmetic beside it.
fn slot_center() -> (f32, f32) {
    let r = pipeline::panel::demoted_rect(pipeline::panel::Surface::Card, W, H).unwrap();
    (r.x + r.w / 2.0, r.y + r.h / 2.0)
}

fn a_screen() -> pipeline::shell::Screen {
    pipeline::shell::Screen::Picker(Box::new(pipeline::picker::Picker::loading(
        "Moonlight",
        "…",
    )))
}

#[test]
fn a_demoted_card_leaves_the_glass_when_the_shell_goes_deeper_than_home() {
    // The reported bug, through the real thing: commands over the channel, a real surface,
    // and the render loop's own hit test. `Panel` pins the rule; this pins that navigating
    // actually re-places the layers, which is the half that used to be missing — nothing
    // re-placed anything except `set_shell_foreground`.
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    tx.try_send(RenderCommand::NowPlaying(Box::default()))
        .unwrap();
    render.pump();
    render.set_shell_foreground(true);

    let (cx, cy) = slot_center();
    assert!(
        render.hit_minimized_card(cx, cy),
        "at Home it is in the slot"
    );

    render.shell_push(a_screen());
    render.pump();
    assert!(
        !render.hit_minimized_card(cx, cy),
        "a screen above Home owns its whole surface; the card has nowhere to be"
    );
    // …and coming back restores it, without the session having been touched.
    render.shell_back();
    render.pump();
    assert!(
        render.hit_minimized_card(cx, cy),
        "back at Home, the card is in the slot again"
    );
}

#[test]
fn a_demoted_video_leaves_the_glass_the_same_way() {
    // Same rule, the other surface and the other geometry: video demotes to the PiP corner
    // rather than the widget slot, and `hit_pip` is what routes a tap there to "give the
    // panel back" — so a corner that is not drawn must not be hittable either.
    let (_tx, mut render) = playing();
    render.set_shell_foreground(true);
    let (ox, oy, sx, sy) = render.pip_rect().expect("demoted at Home");
    let (px, py) = (ox + sx / 2.0, oy + sy / 2.0);
    assert!(render.hit_pip(px, py));

    render.shell_push(a_screen());
    render.pump();
    assert!(render.pip_rect().is_none(), "no corner on a pushed screen");
    assert!(!render.hit_pip(px, py), "and nothing to tap there");

    render.shell_home();
    render.pump();
    assert!(render.pip_rect().is_some(), "home again, corner again");
}

#[test]
fn a_session_that_restarts_gets_the_panel_back() {
    // What left the panel stuck: nothing but a touch ever took the shell out of the
    // foreground, so a source that ended and started again (a phone reclaiming Spotify)
    // came back minimized into the corner with no way to know it had.
    let (tx, mut render) = playing();
    render.shell_push(a_screen());
    render.set_shell_foreground(true);
    render.pump();
    assert!(render.shell_foreground());

    // What `Pipeline::play`/`play_audio` send when a session starts.
    tx.try_send(RenderCommand::RestPanel).unwrap();
    render.pump();

    assert!(
        !render.shell_foreground(),
        "the starting session owns the panel"
    );
    assert_eq!(render.shell_depth(), 1, "and the shell is back at Home");
}

#[test]
fn but_a_session_starting_does_not_snatch_the_panel_from_a_hand_on_it() {
    // The other half of the same predicate: someone reading a picker keeps it, exactly as
    // they do when a session *ends* underneath them.
    let (tx, mut render) = playing();
    render.shell_push(a_screen());
    render.set_shell_foreground(true);
    render.note_touch();

    tx.try_send(RenderCommand::RestPanel).unwrap();
    render.pump();

    assert!(render.shell_foreground(), "still theirs");
    assert_eq!(render.shell_depth(), 2, "still where they left it");
}

/// Pump and advance motion one frame at a time, calling `each` with the card's live rect.
fn frames(render: &mut RenderLoop, mut each: impl FnMut(&RenderLoop)) {
    for _ in 0..180 {
        render.pump();
        each(render);
        if !render.tick_motion(std::time::Duration::from_millis(16)) {
            break;
        }
    }
}

#[test]
fn demoting_a_session_travels_rather_than_teleporting() {
    // The whole point of `motion`. Before it, `set_shell_foreground` wrote the corner's
    // transform and the card was simply *there* on the next frame; a person watching had no
    // idea the thing they had been looking at was now the thing in the corner.
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    tx.try_send(RenderCommand::NowPlaying(Box::default()))
        .unwrap();
    render.pump();

    render.set_shell_foreground(true);
    let mut widths = Vec::new();
    frames(&mut render, |r| widths.push(r.card_frame().w));

    let first = widths.first().copied().unwrap_or_default();
    let last = widths.last().copied().unwrap_or_default();
    assert!(first > 0.9, "it should start at full width, got {first}");
    let slot = pipeline::panel::demoted_rect(pipeline::panel::Surface::Card, W, H).unwrap();
    assert!(
        (last - slot.w).abs() < 0.002,
        "it should arrive in the slot: {last} vs {}",
        slot.w
    );
    // The assertion that distinguishes a movement from a jump: it was observably part-way,
    // for more than a frame or two.
    let midway = widths
        .iter()
        .filter(|w| **w < first - 0.05 && **w > slot.w + 0.05)
        .count();
    assert!(
        midway >= 4,
        "only {midway} frames were part-way; that is a teleport, not a travel: {widths:?}"
    );
    // And monotone: a card that overshoots into the corner and comes back reads as a bounce,
    // which is right for the summon and wrong for a demote.
    for pair in widths.windows(2) {
        if let [a, b] = pair {
            assert!(b <= &(a + 0.0005), "the demote reversed: {a} -> {b}");
        }
    }
}

#[test]
fn a_session_that_ends_leaves_rather_than_vanishing() {
    // The deferred clear used to drop the layer outright, so the card was composited one
    // frame and gone the next. Now the clear starts an exit and the layer is retired when the
    // motion has finished.
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    tx.try_send(RenderCommand::NowPlaying(Box::default()))
        .unwrap();
    render.pump();
    render.tick_motion(std::time::Duration::from_millis(16));

    tx.try_send(RenderCommand::ClearNowPlaying).unwrap();
    render.pump();
    // Past the grace, so the exit is allowed to begin.
    std::thread::sleep(std::time::Duration::from_millis(1300));

    let mut opacities = Vec::new();
    frames(&mut render, |r| opacities.push(r.card_opacity()));
    let faded = opacities
        .iter()
        .filter(|o| **o > 0.05 && **o < 0.95)
        .count();
    assert!(
        faded >= 2,
        "the card blinked out instead of leaving: {opacities:?}"
    );
    assert!(
        opacities.last().is_some_and(|o| *o < 0.05),
        "it never finished leaving: {opacities:?}"
    );
}

#[test]
fn a_screen_opened_from_a_tile_grows_out_of_that_tile_and_goes_back_into_it() {
    // "Summoned from their icon". A screen that materialises in the middle of the panel has
    // thrown away the one thing the person pressing knew — where they were looking — and a
    // way out that does not reverse the way in makes the tile stop meaning anything.
    let (_tx, mut render) = playing();
    let scene = AttractScene::demo();
    let hit = pipeline::attract::tile_layout(&scene, W, H)
        .into_iter()
        .find(|(id, _)| id == "cast")
        .expect("the demo scene has a cast tile")
        .1;
    let tile = pipeline::panel::NormRect {
        x: hit.x / W as f32,
        y: hit.y / H as f32,
        w: hit.w / W as f32,
        h: hit.h / H as f32,
    };

    render.shell_push_from(a_screen(), Some(tile));
    // The arriving screen is the floor's layer, launched at the tile.
    let (at, _) = render
        .floor_placement()
        .expect("the floor should be placed");
    assert!(
        (at.w - tile.w).abs() < 0.01 && (at.x - tile.x).abs() < 0.01,
        "the screen should start at the tile, got {at:?} vs {tile:?}"
    );

    let mut widths = vec![at.w];
    for _ in 0..180 {
        render.pump();
        if let Some((rect, _)) = render.floor_placement() {
            widths.push(rect.w);
        }
        if !render.tick_motion(std::time::Duration::from_millis(16)) {
            break;
        }
    }
    assert!(
        widths.last().is_some_and(|w| (*w - 1.0).abs() < 0.01),
        "it should arrive filling the panel: {:?}",
        widths.last()
    );
    let midway = widths.iter().filter(|w| **w > 0.3 && **w < 0.9).count();
    assert!(midway >= 3, "it jumped rather than grew: {widths:?}");

    // And back goes into the same tile, rather than off along an axis.
    assert!(render.shell_back());
    let outgoing = render
        .outgoing_screen_rect()
        .expect("a transition is running");
    assert!(
        (outgoing.w - 1.0).abs() < 0.02,
        "the leaving screen starts whole: {outgoing:?}"
    );
    let mut seen = Vec::new();
    for _ in 0..180 {
        if let Some(rect) = render.outgoing_screen_rect() {
            seen.push(rect);
        }
        if !render.tick_transition(std::time::Duration::from_millis(16)) {
            break;
        }
    }
    let last = seen.last().copied().expect("it should have moved");
    assert!(
        (last.x - tile.x).abs() < 0.15 && last.w < 0.5,
        "it should shrink back toward the tile at {tile:?}, ended at {last:?}"
    );
}
