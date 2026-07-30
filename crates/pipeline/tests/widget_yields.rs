//! The idle widget leaves the stage — entirely — when something outranks it.
//!
//! Paint order already put a session's card *above* the clock card, but above only
//! settles who wins where the two overlap. A bluetooth session's now-playing card in
//! the middle of the panel left the clock floating beside it, and a pushed shell
//! screen — drawn on the Attract layer, *below* the widget — sat underneath it. Both
//! are the same wrong picture: an ornament outranking the thing the panel is actually
//! doing.
//!
//! Two rules together, and they are deliberately in different places: which of two surfaces
//! in the same slot wins is declared on the layers (`LayerId::yields_to`) because it is a
//! fact about depth, while whether the slot is on the panel at all is
//! `Panel::placement(Surface::IdleWidget)` — the widget belongs to the Home *screen*, so it
//! leaves when the shell does. The compositor applies both per frame.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use pipeline::attract::{AttractScene, WidgetSlot};
use pipeline::browser::BrowserRole;
use pipeline::compositor::DirtyRect;
use pipeline::panel::Surface;
use pipeline::render_pipeline::{RenderCommand, RenderLoop};

const W: u32 = 1920;
const H: u32 = 1080;

/// Magenta in BGRA, chosen like the browser e2e test's blue: unlike any scene colour,
/// channels all different.
const WIDGET_BGRA: [u8; 4] = [0xff, 0x00, 0xff, 0xff];

fn widget_center() -> (usize, usize) {
    let rect = WidgetSlot::RightCard.rect(W, H).unwrap();
    (
        (rect.x + rect.width / 2) as usize,
        (rect.y + rect.height / 2) as usize,
    )
}

/// Pump and advance motion until nothing is moving.
///
/// Needed now that a surface *leaves* rather than vanishing: a card whose session ended is
/// composited for the whole of its exit, so the layer — and therefore the clock's yielding —
/// only goes once the motion has finished. On the panel the kiosk's redraw loop supplies this
/// clock; in a test it has to be asked for.
fn settle(render: &mut RenderLoop) {
    for _ in 0..180 {
        render.pump();
        if !render.tick_motion(std::time::Duration::from_millis(16)) {
            break;
        }
    }
    render.pump();
}

fn widget_visible(render: &RenderLoop) -> bool {
    let shot = render.read_rgba().unwrap();
    let (cx, cy) = widget_center();
    let i = (cy * W as usize + cx) * 4;
    // BGRA in, RGBA out.
    shot[i] > 0xf0 && shot[i + 1] < 0x10 && shot[i + 2] > 0xf0
}

fn idle_with_widget() -> (std::sync::mpsc::SyncSender<RenderCommand>, RenderLoop) {
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap();
    tx.try_send(RenderCommand::Home(Box::new(AttractScene::demo())))
        .unwrap();
    render.pump();

    // Stand in for the browser host: it is the thing that tells the panel a page is up, and
    // this test paints the layer itself rather than starting Electron.
    render.set_surface(Surface::IdleWidget, true);

    let view = BrowserRole::AttractWidget.view((W, H));
    let (w, h) = (view.rect.width, view.rect.height);
    let pixels = WIDGET_BGRA.repeat((w * h) as usize);
    render
        .upload_browser(
            w,
            h,
            &pixels,
            &[DirtyRect::full(w, h)],
            view.transform,
            view.layer,
        )
        .unwrap();
    render.pump();
    assert!(widget_visible(&render), "the widget should start visible");
    (tx, render)
}

#[test]
fn a_now_playing_session_takes_the_widget_off_the_panel_and_gives_it_back() {
    let (tx, mut render) = idle_with_widget();

    // An audio-only session: a card, no video. Its rect need not overlap the widget's —
    // that is the point.
    tx.try_send(RenderCommand::NowPlaying(Box::default()))
        .unwrap();
    settle(&mut render);
    assert!(
        !widget_visible(&render),
        "a playing session is on the panel; the ornament must leave, \
         not merely be overlapped"
    );

    // Session over: the widget returns from the texture it kept all along — no new
    // upload happened between these pumps. The clear is deferred by the seek-shaped
    // grace (see `CLEAR_GRACE`), so the card lingers briefly before yielding back.
    tx.try_send(RenderCommand::ClearNowPlaying).unwrap();
    render.pump();
    std::thread::sleep(std::time::Duration::from_millis(1300));
    settle(&mut render);
    assert!(
        widget_visible(&render),
        "with the session gone the widget should be back, from its warm texture"
    );
}

#[test]
fn a_pushed_shell_screen_takes_the_widget_with_it() {
    let (_tx, mut render) = idle_with_widget();

    // Navigate off Home: a service screen is the whole panel's content, and it is drawn
    // *below* the widget's layer, so paint order alone would leave the clock floating
    // over it.
    let scene = AttractScene::demo();
    let tile = scene
        .tiles
        .iter()
        .find(|t| t.detail.is_some())
        .expect("the demo scene should have a tile with instructions")
        .clone();
    let detail = tile.detail.clone().unwrap();
    render.shell_push(pipeline::shell::Screen::Service(Box::new(
        pipeline::service::ServiceScreen { tile, detail },
    )));
    settle(&mut render);
    assert!(
        !widget_visible(&render),
        "off Home, the Home scene's widget has no business on the panel"
    );

    render.shell_back();
    settle(&mut render);
    assert!(
        widget_visible(&render),
        "back on Home, the widget should be back"
    );
}
