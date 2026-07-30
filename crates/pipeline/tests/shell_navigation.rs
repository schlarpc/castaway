//! Driving the shell the way a finger does (D38).
//!
//! The layouts and hit tests are unit-tested in their own modules. What this covers is
//! the part between them and the panel: that a press at a coordinate reaches the right
//! screen, that navigation goes where it should, and — the one that matters most — that
//! every screen can be left.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use castaway_core::{ControlCapabilities, DecodedFrame, FrameImage, NowPlaying, PlaybackState};
use pipeline::attract::AttractScene;
use pipeline::nowplaying_card::NowPlayingCard;
use pipeline::render_pipeline::{RenderCommand, RenderLoop};
use pipeline::shell::{Screen, ScreenHit, ShellEvent};

const W: u32 = 1920;
const H: u32 = 1080;

fn loop_with_home() -> (pipeline::RenderTx, RenderLoop) {
    let (tx, rx) = pipeline::render_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
    render.pump();
    (tx, render)
}

/// The centre of a tile, in panel-normalized coordinates.
fn tile_centre(scene: &AttractScene, id: &str) -> (f32, f32) {
    let (_, rect) = pipeline::attract::tile_layout(scene, W, H)
        .into_iter()
        .find(|(tid, _)| tid == id)
        .expect("the demo scene has that tile");
    let (cx, cy) = rect.center();
    (cx / W as f32, cy / H as f32)
}

#[test]
fn pressing_a_service_tile_opens_that_service_and_back_returns() {
    let (_tx, mut render) = loop_with_home();
    let scene = AttractScene::demo();
    assert_eq!(render.shell_depth(), 1, "starts at Home");

    let (x, y) = tile_centre(&scene, "cast");
    let hit = render.shell_hit(x, y).expect("the tile should be hittable");
    let ScreenHit::Push { screen, from } = hit else {
        panic!("a service tile should open its own screen, got {hit:?}");
    };
    assert_eq!(screen.name(), "service");
    // The tile's own rect travels with the press, so the screen it opens can grow out of it
    // rather than materialising in the middle of the panel.
    let from = from.expect("a tile press knows where it was");
    let tile = pipeline::attract::tile_layout(&scene, W, H)
        .into_iter()
        .find(|(id, _)| id == "cast")
        .expect("the demo scene has a cast tile")
        .1;
    assert!((from.x - tile.x / W as f32).abs() < 1e-5, "{from:?}");
    assert!((from.w - tile.w / W as f32).abs() < 1e-5, "{from:?}");
    render.shell_push_from(screen, Some(from));
    assert_eq!(render.shell_depth(), 2);

    // Back, from where the service screen draws it.
    let back = pipeline::service::back_rect(W, H);
    let (bx, by) = back.center();
    let hit = render
        .shell_hit(bx / W as f32, by / H as f32)
        .expect("back should be hittable");
    assert_eq!(hit, ScreenHit::Back);
    assert!(render.shell_back());
    assert_eq!(render.shell_depth(), 1, "back returns to Home");
}

#[test]
fn a_tile_with_nothing_behind_it_becomes_an_event_for_the_app() {
    // Moonlight's tile: the panel cannot answer it locally, because only `app` knows
    // what a host is.
    let (_tx, render) = loop_with_home();
    let scene = AttractScene::demo();
    let (x, y) = tile_centre(&scene, "gamestream");
    assert_eq!(
        render.shell_hit(x, y),
        Some(ScreenHit::Event(ShellEvent::Tile("gamestream".into())))
    );
}

#[test]
fn pressing_between_tiles_hits_nothing() {
    let (_tx, render) = loop_with_home();
    // Far right of the panel, past the widget card and any tile.
    assert_eq!(render.shell_hit(0.97, 0.97), None);
}

#[test]
fn every_screen_can_be_left_however_deep_it_got() {
    // The shell's central promise. A screen you can enter and not leave is the failure
    // the stack exists to prevent, and this drives it through the render loop rather
    // than the model alone.
    let (_tx, mut render) = loop_with_home();
    for i in 0..25 {
        render.shell_push(Screen::Picker(Box::new(pipeline::picker::Picker::loading(
            format!("level {i}"),
            "…",
        ))));
    }
    assert_eq!(render.shell_depth(), 26);
    let mut guard = 0;
    while render.shell_back() {
        guard += 1;
        assert!(guard < 1000, "back did not terminate");
    }
    assert_eq!(render.shell_depth(), 1);
}

#[test]
fn going_home_unwinds_in_one_step() {
    let (_tx, mut render) = loop_with_home();
    for _ in 0..5 {
        render.shell_push(Screen::Picker(Box::new(pipeline::picker::Picker::loading(
            "x", "…",
        ))));
    }
    assert!(render.shell_home());
    assert_eq!(render.shell_depth(), 1);
    assert!(!render.shell_home(), "already home moves nothing");
}

#[test]
fn a_refreshing_picker_stays_one_step_from_home() {
    // A picker that answered its own refreshes by pushing would need one `back` per
    // refresh to escape, which on a busy network is unbounded.
    let (tx, mut render) = loop_with_home();
    for i in 0..12 {
        tx.send(RenderCommand::ReplaceScreen(Box::new(Screen::Picker(
            Box::new(pipeline::picker::Picker::loading(
                "Moonlight",
                format!("looking… {i}"),
            )),
        ))));
        render.pump();
    }
    assert_eq!(render.shell_depth(), 2);
    assert!(render.shell_back());
    assert_eq!(render.shell_depth(), 1);
}

#[test]
fn the_shell_does_not_answer_where_a_cast_is_covering_it() {
    // The shell draws at the bottom of the stack. A press on a video belongs to the
    // video, and a tile invisible underneath it must not eat the touch — the same rule
    // that stopped the hidden transport strip swallowing presses.
    let (tx, mut render) = loop_with_home();
    let scene = AttractScene::demo();
    let (x, y) = tile_centre(&scene, "cast");
    assert!(
        render.shell_hit(x, y).is_some(),
        "hittable with nothing over it"
    );

    tx.send(RenderCommand::Video(DecodedFrame {
        width: W,
        height: H,
        pts: std::time::Duration::ZERO,
        image: FrameImage::Cpu {
            format: castaway_core::PixelFormat::Rgba8,
            data: bytes::Bytes::from(vec![0xff; (W * H * 4) as usize]),
        },
    }));
    render.pump();

    assert_eq!(
        render.shell_hit(x, y),
        None,
        "a covered tile must not take the press"
    );
}

#[test]
fn the_now_playing_card_also_covers_the_shell() {
    // An audio session has no pixels of its own but still owns the screen.
    let (tx, mut render) = loop_with_home();
    let scene = AttractScene::demo();
    let (x, y) = tile_centre(&scene, "cast");

    let mut track = NowPlaying::default().with_title("something playing");
    track.state = PlaybackState::Playing;
    tx.send(RenderCommand::NowPlaying(Box::new(NowPlayingCard {
        track,
        source: castaway_core::SourceDescription::default(),
        up_next: Vec::new(),
        controls: ControlCapabilities::PLAY | ControlCapabilities::PAUSE,
    })));
    render.pump();

    assert_eq!(render.shell_hit(x, y), None);
}

#[test]
fn refreshing_home_does_not_close_a_screen_someone_is_reading() {
    // Home is rebuilt whenever the receiver's state changes — a protocol going down, a
    // host appearing. If that reset the stack, discovery would close a picker mid-read.
    let (tx, mut render) = loop_with_home();
    render.shell_push(Screen::Picker(Box::new(pipeline::picker::Picker::loading(
        "Moonlight",
        "…",
    ))));
    assert_eq!(render.shell_depth(), 2);

    tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
    render.pump();

    assert_eq!(render.shell_depth(), 2, "still in the picker");
}
