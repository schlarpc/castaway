//! Picture-in-picture, and going home when nothing is on (#27, #28, D38).
//!
//! Both come out of the same fact: the shell draws *below* video. Navigating while
//! something plays would be invisible, so the video is demoted rather than stopped —
//! someone pressing Home in the middle of a film has not asked for it to end.
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

    let rect = pipeline::attract::WidgetSlot::RightCard.rect(W, H).unwrap();
    let (cx, cy) = (
        (rect.x as f32 + rect.width as f32 / 2.0) / W as f32,
        (rect.y as f32 + rect.height as f32 / 2.0) / H as f32,
    );
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

/// The middle of the Home screen's widget slot, panel-normalized.
fn slot_center() -> (f32, f32) {
    let rect = pipeline::attract::WidgetSlot::RightCard.rect(W, H).unwrap();
    (
        (rect.x as f32 + rect.width as f32 / 2.0) / W as f32,
        (rect.y as f32 + rect.height as f32 / 2.0) / H as f32,
    )
}

fn a_screen() -> pipeline::shell::Screen {
    pipeline::shell::Screen::Picker(Box::new(pipeline::picker::Picker::loading(
        "Moonlight",
        "…",
    )))
}

#[test]
fn a_demoted_card_leaves_the_glass_when_the_shell_goes_deeper_than_home() {
    // The reported bug: the minimized slot *is* the Home screen's widget slot, so a card
    // demoted into it while someone opened a service screen was drawn over the text they
    // were reading — a PiP on a screen that has no PiP. Nothing in `shell_front` said
    // which screen was current, so nothing stopped it.
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
    // Same rule, the other surface: `hit_pip` is what routes a tap to "give the panel
    // back", so a PiP that is not drawn must not be hittable either.
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
