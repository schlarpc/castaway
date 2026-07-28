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

#[test]
fn a_screen_is_drawn_at_the_surface_size_not_stretched_to_it() {
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    let (w, h) = (960, 540);
    let mut render = RenderLoop::offscreen(w, h, rx).unwrap();

    tx.try_send(RenderCommand::Home(Box::new(
        pipeline::attract::AttractScene::demo(),
    )))
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
    tx.try_send(RenderCommand::Home(Box::new(
        pipeline::attract::AttractScene::demo(),
    )))
    .unwrap();
    render.pump();
    assert_eq!(
        render.shell_layer_size(),
        Some((320, 180)),
        "the screen should be drawn at the surface size it was given"
    );

    let (w, h) = (1280, 720);
    render.resize(w, h);
    // The kiosk pumps every frame; a resize is followed by a present like anything else.
    render.pump();

    // The property, asserted directly: the texture was *redrawn* at the new size, not
    // handed to the GPU to stretch. Measuring it from the composited pixels is possible
    // and fragile — it depends on what the screen happens to contain that day.
    assert_eq!(
        render.shell_layer_size(),
        Some((w, h)),
        "resize should have redrawn the screen at {w}x{h}"
    );
}
