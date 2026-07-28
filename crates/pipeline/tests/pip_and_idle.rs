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

    tx.try_send(RenderCommand::ShellHome).unwrap();
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

    tx.try_send(RenderCommand::ShellHome).unwrap();
    render.pump();

    assert_eq!(render.shell_depth(), 2, "still where they left it");
}
