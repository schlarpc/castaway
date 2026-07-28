//! The shell screen is drawn at the size it will be shown.
//!
//! It used to be rasterised once at startup at a hardcoded 3840x2160 and handed to the
//! compositor, which stretched it to whatever the panel measured. That is invisible on a
//! 4K panel and wrong everywhere else — and the idle screen is a dithered gradient with
//! fine text, both of which upscaling smears. The dither exists specifically to avoid
//! banding, so smearing it re-introduces the artefact it was added to remove.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use pipeline::render_pipeline::{RenderCommand, RenderLoop};
use pipeline::shell::Screen;

/// Read the composited surface and report how many distinct colours a horizontal scan
/// through the middle has. A natively-drawn gradient has many; an upscaled one has
/// fewer, in wider steps.
fn distinct_colours_across(render: &RenderLoop, width: u32, height: u32) -> usize {
    let rgba = render.read_rgba().unwrap();
    let row = (height / 2) as usize;
    let mut seen = std::collections::HashSet::new();
    for x in 0..width as usize {
        let i = (row * width as usize + x) * 4;
        seen.insert([rgba[i], rgba[i + 1], rgba[i + 2]]);
    }
    seen.len()
}

#[test]
fn a_screen_is_drawn_at_the_surface_size_not_stretched_to_it() {
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    let (w, h) = (960, 540);
    let mut render = RenderLoop::offscreen(w, h, rx).unwrap();

    tx.try_send(RenderCommand::Screen(Box::new(Screen::Home(
        pipeline::attract::AttractScene::demo(),
    ))))
    .unwrap();
    render.pump();

    let rgba = render.read_rgba().unwrap();
    assert_eq!(rgba.len(), (w * h * 4) as usize);
    // Something was actually drawn: the scene is a gradient, so the top and bottom of
    // the surface must differ.
    let top = &rgba[0..3];
    let bottom = &rgba[((h as usize - 1) * w as usize) * 4..][..3];
    assert_ne!(
        top, bottom,
        "the idle screen should be a gradient, not a flat fill"
    );
}

#[test]
fn resizing_redraws_the_screen_rather_than_stretching_it() {
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    // Start small, then grow — the case that used to leave an upscaled surface behind.
    let mut render = RenderLoop::offscreen(320, 180, rx).unwrap();
    tx.try_send(RenderCommand::Screen(Box::new(Screen::Home(
        pipeline::attract::AttractScene::demo(),
    ))))
    .unwrap();
    render.pump();
    let small = distinct_colours_across(&render, 320, 180);

    let (w, h) = (1280, 720);
    render.resize(w, h);
    // The kiosk pumps every frame; a resize is followed by a present like anything else.
    render.pump();
    let large = distinct_colours_across(&render, w, h);

    // A redraw at the larger size resolves detail the small one could not hold. A
    // stretched 320-wide surface would carry the small one's colour count across four
    // times as many pixels.
    assert!(
        large > small,
        "resize should have redrawn the screen at {w}x{h} (colours: {small} -> {large})"
    );
}
