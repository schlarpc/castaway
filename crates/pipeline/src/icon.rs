//! The panel's own mark, rasterized from the one SVG source of truth.
//!
//! `assets/brand/castaway-icon.svg` is authored in-repo (unlike the dma.space
//! artwork beside it) and is the only place the icon's geometry lives. Everything
//! raster is derived: the winit window icon ([`rasterize`], called by
//! [`crate::kiosk`]), the checked-in PNGs under `assets/brand/icon/` (the
//! `icon_render` example), and the Windows `.ico` assembled from those PNGs
//! (`crates/app/assets/README.md`). One source, so the taskbar, the desktop
//! entry and the .exe can never drift apart.

/// The icon artwork. Public so the generator example renders exactly what the
/// window ships.
pub const ICON_SVG: &str = include_str!("../assets/brand/castaway-icon.svg");

/// The mark at `side`×`side`, straight-alpha RGBA8 — the layout `winit`'s
/// `Icon::from_rgba` and the `png` encoder both take.
///
/// `None` if the artwork will not parse or the size is degenerate, which the
/// kiosk takes as "no icon" rather than "no window": the default icon is a
/// cosmetic failure, a refused window is not.
#[must_use]
pub fn rasterize(side: u32) -> Option<Vec<u8>> {
    use resvg::tiny_skia;
    let tree = resvg::usvg::Tree::from_str(ICON_SVG, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(side, side)?;
    let size = tree.size();
    #[allow(clippy::cast_precision_loss)]
    let scale = (side as f32 / size.width()).min(side as f32 / size.height());
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied alpha; the consumers above want straight.
    Some(
        pixmap
            .pixels()
            .iter()
            .flat_map(|px| {
                let c = px.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_rasterizes_at_every_size_the_platforms_ask_for() {
        // 16 through 256: the hicolor theme sizes, the .ico sizes, and the 64
        // the window icon uses. A parse or layout regression in the SVG should
        // fail here, not as a silently default-iconed window.
        for side in [16u32, 24, 32, 48, 64, 128, 256] {
            let rgba = rasterize(side).expect("icon failed to rasterize");
            assert_eq!(rgba.len(), (side * side * 4) as usize);
            // Something opaque was actually drawn...
            assert!(
                rgba.chunks_exact(4).any(|px| px[3] == 0xff),
                "{side}px raster is fully transparent"
            );
            // ...and the tile's rounded corners stay transparent, so it sits on
            // any taskbar rather than shipping as a hard square.
            assert_eq!(rgba[3], 0, "{side}px raster has an opaque corner");
        }
    }

    #[test]
    fn the_mark_wears_the_panel_palette() {
        // The icon is the theme's colours or it is somebody else's icon: the
        // sea must be ACCENT teal and the signal dma.space BLUE, exactly.
        let side = 256u32;
        let rgba = rasterize(side).expect("icon failed to rasterize");
        let has = |c: crate::theme::Rgba| {
            rgba.chunks_exact(4)
                .any(|px| px[0] == c[0] && px[1] == c[1] && px[2] == c[2] && px[3] == 0xff)
        };
        assert!(has(crate::theme::ACCENT), "no ACCENT teal sea");
        assert!(has(crate::theme::BLUE), "no BLUE signal");
        assert!(has(crate::theme::TEXT), "no white boat");
    }

    #[test]
    fn degenerate_sizes_are_refused_rather_than_panicking() {
        assert!(rasterize(0).is_none());
    }
}
