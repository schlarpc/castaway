//! A service's own screen: what this one is, and what to do on your own device (D38).
//!
//! The idle screen used to carry one instruction row per enabled protocol, so the first
//! thing anyone saw was a wall of text about six things they were not doing. Now it shows
//! a tile per service and the instructions live one tap in — which also gives them room
//! to be readable from across a room instead of compressed onto one line.
//!
//! Pure layout + CPU raster, like every other surface here. The only interactive thing is
//! the back affordance, and it shares its rectangle with [`crate::picker`] so "back" is
//! in the same place on every screen that has one.

use crate::attract::{ServiceDetail, Tile};
use crate::error::PipelineError;
use crate::shape::{self, Rect};
use crate::text::{self, Rgba};
use crate::theme;

/// The design height every dimension scales from, matching the other screens.
const DESIGN_HEIGHT: f32 = 720.0;

/// A service screen: the tile it came from, plus what to say about it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceScreen {
    /// The tile that opened this, for its label, glyph and accent — so the screen looks
    /// like the thing that was pressed.
    pub tile: Tile,
    /// What to say.
    pub detail: ServiceDetail,
}

/// Where the back affordance lands, in device pixels.
///
/// Deliberately identical to [`crate::picker::layout`]'s: a person learns where "back" is
/// once, and a control that moves between screens is one they have to find again.
#[must_use]
pub fn back_rect(_width: u32, height: u32) -> Rect {
    let s = height as f32 / DESIGN_HEIGHT;
    Rect {
        x: 90.0 * s - 20.0 * s,
        y: 60.0 * s,
        w: 200.0 * s,
        h: 96.0 * s,
    }
}

/// Whether a panel-normalized point is on the back affordance.
#[must_use]
pub fn hit_back(width: u32, height: u32, x: f32, y: f32) -> bool {
    back_rect(width, height).contains(x * width as f32, y * height as f32)
}

struct Palette {
    bg_top: Rgba,
    bg_bottom: Rgba,
    title: Rgba,
    headline: Rgba,
    step: Rgba,
    step_num: Rgba,
    back: Rgba,
    look_for_label: Rgba,
    look_for: Rgba,
    plate: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg_top: theme::BG_TOP,
            bg_bottom: theme::BG_BOTTOM,
            title: theme::TEXT,
            headline: theme::ACCENT,
            step: theme::TEXT_BODY,
            step_num: theme::TEXT_FAINT,
            back: theme::TEXT_DIM,
            look_for_label: theme::TEXT_DIM,
            look_for: theme::TEXT,
            plate: theme::PLATE,
        }
    }
}

/// Draw the service screen into a fresh RGBA8 buffer.
///
/// # Errors
/// [`PipelineError`] if the bundled fonts fail to load.
pub fn render(screen: &ServiceScreen, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let f = text::fonts()?;
    let pal = Palette::default();
    let s = height as f32 / DESIGN_HEIGHT;
    let margin = 90.0 * s;

    let mut buf = vec![0u8; (width * height * 4) as usize];
    text::fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom);

    // Back, in the same place as the picker's.
    let back = back_rect(width, height);
    let (bx, by) = (back.x + 34.0 * s, back.y + back.h / 2.0);
    shape::chevron(
        &mut buf,
        width,
        height,
        bx,
        by,
        15.0 * s,
        4.0 * s,
        pal.back,
        shape::Facing::Left,
    );
    text::draw_text(
        &mut buf,
        width,
        height,
        bx + 30.0 * s,
        by + text::ascent(&f.regular, 26.0 * s) * 0.36,
        "Back",
        26.0 * s,
        pal.back,
        &f.regular,
    );

    // The tile's own glyph, large, so the screen is visibly the thing that was pressed.
    let plate = Rect {
        x: margin,
        y: 200.0 * s,
        w: 180.0 * s,
        h: 180.0 * s,
    };
    // The same regulation the tile got, so the screen a tile opens is the colour of the
    // tile that opened it rather than the raw brand colour.
    let accent = crate::theme::regulated(screen.tile.accent);
    let plate_fill = crate::theme::tinted(pal.plate, accent, 0.12);
    shape::rounded_rect(&mut buf, width, height, plate, 26.0 * s, plate_fill);
    shape::rounded_outline(
        &mut buf,
        width,
        height,
        plate,
        26.0 * s,
        (2.0 * s).max(1.0),
        accent,
    );
    let (gx, gy) = plate.center();
    crate::attract::draw_tile_glyph(
        &mut buf,
        (width, height),
        screen.tile.glyph,
        (gx, gy),
        plate.h * 0.30,
        accent,
        plate_fill,
    );

    let text_x = plate.x + plate.w + 56.0 * s;

    // Name and headline.
    text::draw_text(
        &mut buf,
        width,
        height,
        text_x,
        plate.y + 56.0 * s,
        &screen.tile.label,
        56.0 * s,
        pal.title,
        &f.bold,
    );
    text::draw_text(
        &mut buf,
        width,
        height,
        text_x,
        plate.y + 104.0 * s,
        &screen.detail.headline,
        28.0 * s,
        pal.headline,
        &f.regular,
    );

    // Steps, numbered. Numbers rather than bullets because these are in order — "open
    // the app" before "pick the screen" — and a bullet list says they are not.
    let mut y = plate.y + plate.h + 76.0 * s;
    for (i, step) in screen.detail.steps.iter().enumerate() {
        text::draw_text(
            &mut buf,
            width,
            height,
            margin,
            y,
            &format!("{}", i + 1),
            32.0 * s,
            pal.step_num,
            &f.bold,
        );
        text::draw_text(
            &mut buf,
            width,
            height,
            margin + 44.0 * s,
            y,
            step,
            32.0 * s,
            pal.step,
            &f.regular,
        );
        y += 56.0 * s;
    }

    // The name to look for, last and loudest. It is the one string that has to be exactly
    // right and the one nobody can guess, so it gets a plate of its own rather than being
    // another line of grey text.
    if let Some(advertised) = &screen.detail.advertised {
        y += 24.0 * s;
        let label_px = 22.0 * s;
        text::draw_text(
            &mut buf,
            width,
            height,
            margin,
            y,
            "LOOK FOR",
            label_px,
            pal.look_for_label,
            &f.bold,
        );
        y += 20.0 * s;
        let name_px = 40.0 * s;
        let tw = text::measure(&f.bold, advertised, name_px);
        let pad = 22.0 * s;
        let plate = Rect {
            x: margin - pad * 0.6,
            y: y - 4.0 * s,
            w: tw + pad * 1.2,
            h: name_px + pad,
        };
        shape::rounded_rect(&mut buf, width, height, plate, 12.0 * s, pal.plate);
        text::draw_text(
            &mut buf,
            width,
            height,
            margin,
            y + name_px * 0.78,
            advertised,
            name_px,
            pal.look_for,
            &f.bold,
        );
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::attract::TileGlyph;

    fn screen() -> ServiceScreen {
        ServiceScreen {
            tile: Tile {
                id: "cast".into(),
                label: "Google Cast".into(),
                glyph: TileGlyph::Cast,
                accent: [0x42, 0x85, 0xf4, 0xff],
                detail: None,
            },
            detail: ServiceDetail {
                headline: "Cast a tab, or your whole screen.".into(),
                steps: vec!["Open Chrome or Edge".into(), "Menu, then Cast".into()],
                advertised: Some("dma.space/screen#cast".into()),
            },
        }
    }

    #[test]
    fn back_is_where_the_picker_puts_it() {
        // A control that moves between screens is one someone has to find again. This is
        // the assertion that keeps the two in step.
        let (w, h) = (1920, 1080);
        assert_eq!(
            back_rect(w, h),
            crate::picker::layout(&crate::picker::Picker::loading("x", "y"), w, h,).back
        );
    }

    #[test]
    fn the_back_target_is_hittable_from_a_normalized_touch() {
        let (w, h) = (1920, 1080);
        let r = back_rect(w, h);
        let (cx, cy) = r.center();
        assert!(hit_back(w, h, cx / w as f32, cy / h as f32));
        assert!(!hit_back(w, h, 0.9, 0.9));
    }

    #[test]
    fn it_rasterizes_at_panel_scale() {
        let buf = render(&screen(), 1280, 720).unwrap();
        assert_eq!(buf.len(), 1280 * 720 * 4);
    }

    #[test]
    fn a_service_with_no_advertised_name_still_renders() {
        // Not every service has one — GameStream's tile has no detail at all, and a
        // future one might have steps and nothing to look for.
        let mut sc = screen();
        sc.detail.advertised = None;
        sc.detail.steps.clear();
        assert!(render(&sc, 640, 360).is_ok());
    }
}
