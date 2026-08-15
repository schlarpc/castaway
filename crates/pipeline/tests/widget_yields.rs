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
use pipeline::compositor::{DirtyRect, LayerId};
use pipeline::panel::Surface;
use pipeline::render_pipeline::{FrameEpoch, RenderCommand, RenderLoop};

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

fn idle_with_widget() -> (
    pipeline::RenderTx,
    RenderLoop,
    pipeline::render_clock::ManualClock,
) {
    let (tx, rx) = pipeline::render_channel(8);
    let (clock, time) = pipeline::render_clock::RenderClock::manual();
    let mut render = RenderLoop::offscreen(W, H, rx).unwrap().with_clock(clock);
    tx.send(RenderCommand::Home(Box::new(AttractScene::demo())));
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
    (tx, render, time)
}

#[test]
fn a_now_playing_session_takes_the_widget_off_the_panel_and_gives_it_back() {
    let (tx, mut render, time) = idle_with_widget();

    // An audio-only session: a card, no video. Its rect need not overlap the widget's —
    // that is the point.
    tx.send(RenderCommand::NowPlaying(Box::default()));
    settle(&mut render);
    assert!(
        !widget_visible(&render),
        "a playing session is on the panel; the ornament must leave, \
         not merely be overlapped"
    );

    // Session over: the widget returns from the texture it kept all along — no new
    // upload happened between these pumps. The clear is deferred by the seek-shaped
    // grace (see `CLEAR_GRACE`), so the card lingers briefly before yielding back.
    tx.send(RenderCommand::ClearNowPlaying);
    render.pump();
    // The grace passes in virtual time; this used to be a real 1.3 s sleep (#236).
    time.advance(std::time::Duration::from_millis(1300));
    settle(&mut render);
    assert!(
        widget_visible(&render),
        "with the session gone the widget should be back, from its warm texture"
    );
}

#[test]
fn a_pushed_shell_screen_takes_the_widget_with_it() {
    let (_tx, mut render, _time) = idle_with_widget();

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

#[test]
fn the_mascot_leans_on_the_slot_and_leaves_only_for_a_full_panel_session() {
    // She leans on the *slot*, not on the clock: the card frame is painted into the scene and
    // persists, so a session demoted into that slot is something for her arms to land on
    // rather than something that buries them. What gets her out of the way is an occupant
    // that has taken the whole panel — a matter of degree, so the change is a fade.
    let (tx, mut render, _time) = idle_with_widget();
    assert_eq!(
        render.mascot_opacity(),
        Some(1.0),
        "on the idle screen she is fully there"
    );

    // A session takes the panel: she is nowhere near the slot now, so she goes.
    tx.send(RenderCommand::NowPlaying(Box::default()));
    settle(&mut render);
    assert!(
        render.mascot_opacity().is_some_and(|o| o < 0.05),
        "a full-panel session is no place for an ornament: {:?}",
        render.mascot_opacity()
    );

    // Demote it into the slot. Now she is leaning on it, so she comes back — and she is
    // *above* it, which is what makes the lean read.
    render.set_shell_foreground(true);
    settle(&mut render);
    assert!(
        render.mascot_opacity().is_some_and(|o| o > 0.8),
        "with the session in the slot she leans on it: {:?}",
        render.mascot_opacity()
    );
    assert!(
        LayerId::MascotOverlay > LayerId::NowPlaying,
        "and her arms have to land in front of what the slot holds"
    );

    // Summoned back to full panel: she gets out of the way again, gradually.
    let mut seen = Vec::new();
    render.panel_restore();
    for _ in 0..180 {
        render.pump();
        seen.push(render.mascot_opacity().unwrap_or_default());
        if !render.tick_motion(std::time::Duration::from_millis(16)) {
            break;
        }
    }
    let partial = seen.iter().filter(|o| **o > 0.05 && **o < 0.95).count();
    assert!(partial >= 2, "she blinked out instead of fading: {seen:?}");
    assert!(seen.last().is_some_and(|o| *o < 0.05), "{seen:?}");
}

#[test]
fn she_stays_whole_when_the_browser_is_off_being_youtube() {
    // The regression this pins (TODO 21): her lower half is baked into the Home floor
    // texture and her upper half is its own layer. With the one browser showing a page
    // (Surface::CastPage) there is no idle-widget *surface* — and the overlay was keyed
    // on exactly that, so coming Home showed half of dma-chan. She leans on the slot's
    // frame, and the frame is there whenever Home is.
    let (_tx, mut render, _time) = idle_with_widget();

    // The browser leaves the clock and becomes a page; the widget surface goes away.
    render.set_surface(Surface::IdleWidget, false);
    render.set_surface(Surface::CastPage, true);

    // Fullscreen page: she is out of the way, as ever.
    render.set_shell_foreground(false);
    settle(&mut render);
    assert!(
        !render.mascot_on_glass(),
        "a full-panel page is no place for an ornament"
    );

    // Back to Home, the page demoted into the slot: the frame is visible, so is she —
    // all of her, not the half that happens to live in the floor texture.
    render.set_shell_foreground(true);
    settle(&mut render);
    assert!(
        render.mascot_on_glass(),
        "Home's floor shows her torso; suppressing the overlay leaves half a mascot"
    );
}

#[test]
fn a_demoted_video_is_nowhere_near_her_and_leaves_her_alone() {
    // Video demotes to the PiP corner, not the slot. Driving her from "is a session present"
    // would have hidden her for a video in the opposite corner of the panel.
    let (tx, mut render, _time) = idle_with_widget();
    tx.send(RenderCommand::Video(
        castaway_core::DecodedFrame {
            width: W,
            height: H,
            pts: std::time::Duration::ZERO,
            image: castaway_core::FrameImage::Cpu {
                format: castaway_core::PixelFormat::Rgba8,
                data: bytes::Bytes::from(vec![0x40; (W * H * 4) as usize]),
            },
        },
        FrameEpoch::ALWAYS_FRESH,
    ));
    settle(&mut render);
    render.set_shell_foreground(true);
    settle(&mut render);
    assert!(
        render.mascot_opacity().is_some_and(|o| o > 0.8),
        "a video in the far corner is no reason for her to go: {:?}",
        render.mascot_opacity()
    );
}
