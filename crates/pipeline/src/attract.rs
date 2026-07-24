//! The idle "attract" / lobby scene: what the panel shows when nothing is casting. It's
//! the first thing anyone sees, so it names the receiver and tells them how to throw
//! media at it. Rendered on the CPU (the shared [`crate::text`] rasterizer over a
//! gradient) into an RGBA image the compositor shows as a background layer (video covers
//! it when a cast starts).
//!
//! Pure/deterministic, so it unit-tests without a GPU and can be dumped to PNG ([`to_png`]).
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use ab_glyph::FontRef;

use crate::compositor::Transform;
use crate::error::PipelineError;
use crate::text::{self, Rgba};

/// The design resolution the layout is authored against; every dimension scales from it,
/// so the scene looks the same at 720p and 4K but is rasterized natively.
const DESIGN_HEIGHT: f32 = 720.0;

/// A pixel rectangle on the attract surface — where a *live* layer goes. Whole device
/// pixels, because the compositor maps that layer's texels 1:1 onto the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsetRect {
    /// Left edge in device pixels.
    pub x: u32,
    /// Top edge in device pixels.
    pub y: u32,
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
}

impl InsetRect {
    /// The compositor placement for this rect on a `surface_width`×`surface_height`
    /// surface: normalized scale + offset, one texel per device pixel.
    #[must_use]
    pub fn transform(self, surface_width: u32, surface_height: u32) -> Transform {
        let (sw, sh) = (surface_width.max(1) as f32, surface_height.max(1) as f32);
        Transform {
            scale_x: self.width as f32 / sw,
            scale_y: self.height as f32 / sh,
            offset_x: self.x as f32 / sw,
            offset_y: self.y as f32 / sh,
        }
    }
}

/// Whether the idle scene reserves room for the live web widget (the CEF clock). Without
/// a browser there is nothing to put there, so the text uses the full width instead of
/// leaving a hole — hence an enum the renderer must match on rather than a maybe-empty
/// rect threaded through every call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WidgetSlot {
    /// No widget: title and tagline are centered across the whole surface.
    #[default]
    None,
    /// Reserve a card in the top-right corner; the browser layer paints into it and the
    /// text block moves left to make room.
    RightCard,
}

impl WidgetSlot {
    /// The reserved rect for a `width`×`height` surface, or `None` when nothing is
    /// reserved. The single source of truth for the widget's geometry: the renderer draws
    /// the frame around it and the browser host sizes its viewport from it, so the two
    /// cannot drift.
    #[must_use]
    pub fn rect(self, width: u32, height: u32) -> Option<InsetRect> {
        match self {
            Self::None => None,
            Self::RightCard => {
                let (w, h) = (width.max(1), height.max(1));
                let margin = (90.0 * (h as f32 / DESIGN_HEIGHT)) as u32;
                // 16:9 and sized off the width, so a page written for a landscape
                // viewport isn't letterboxed inside a stripe. Clamped so a small or
                // portrait surface yields a valid (if cramped) rect rather than a
                // zero-sized texture.
                let card_w = (w * 3 / 10).clamp(1, w);
                let card_h = (card_w * 9 / 16).clamp(1, h);
                Some(InsetRect {
                    x: w.saturating_sub(margin.saturating_add(card_w)),
                    y: margin.min(h - card_h),
                    width: card_w,
                    height: card_h,
                })
            }
        }
    }
}

/// One "how to cast" row: a colored bullet, a device/app label, and the instruction.
#[derive(Debug, Clone)]
pub struct AttractRow {
    /// Bullet accent color.
    pub accent: Rgba,
    /// The sender (e.g. "Chrome / Edge").
    pub label: String,
    /// What to do (e.g. "Cast → dma.space/screen").
    pub detail: String,
}

impl AttractRow {
    /// Convenience constructor.
    #[must_use]
    pub fn new(accent: Rgba, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            accent,
            label: label.into(),
            detail: detail.into(),
        }
    }
}

/// The full idle scene.
#[derive(Debug, Clone)]
pub struct AttractScene {
    /// Big title — the receiver's friendly name.
    pub title: String,
    /// One-line tagline under the title.
    pub tagline: String,
    /// The "how to cast" rows.
    pub rows: Vec<AttractRow>,
    /// Dim footer (network info).
    pub footer: String,
    /// Room reserved for the live web widget (the CEF clock layer).
    pub widget: WidgetSlot,
}

impl AttractScene {
    /// A representative scene for previews/tests.
    #[must_use]
    pub fn demo() -> Self {
        let name = "dma.space/screen";
        Self {
            title: name.into(),
            tagline: "Throw anything at the wall — no app to install.".into(),
            rows: vec![
                AttractRow::new(
                    [0x42, 0x85, 0xf4, 0xff],
                    "Chrome / Edge",
                    format!("Cast \u{2192} {name}"),
                ),
                AttractRow::new(
                    [0xff, 0xff, 0xff, 0xff],
                    "iPhone / Mac",
                    format!("AirPlay \u{2192} {name}"),
                ),
                AttractRow::new(
                    [0x3d, 0xdc, 0x84, 0xff],
                    "Android / VLC",
                    format!("Cast or DLNA \u{2192} {name}"),
                ),
                AttractRow::new(
                    [0x1d, 0xb9, 0x54, 0xff],
                    "Spotify",
                    format!("Devices \u{2192} {name}"),
                ),
                AttractRow::new(
                    [0xff, 0x00, 0x00, 0xff],
                    "YouTube",
                    "Cast button".to_string(),
                ),
            ],
            footer: "castaway  •  DLNA / mDNS on 10.0.0.5:8080".into(),
            widget: WidgetSlot::RightCard,
        }
    }
}

struct Palette {
    bg_top: Rgba,
    bg_bottom: Rgba,
    title: Rgba,
    tagline: Rgba,
    label: Rgba,
    detail: Rgba,
    footer: Rgba,
    card_edge: Rgba,
    card_bg: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg_top: [0x0d, 0x14, 0x28, 0xff],
            bg_bottom: [0x03, 0x05, 0x0b, 0xff],
            title: [0xff, 0xff, 0xff, 0xff],
            tagline: [0x4f, 0xd1, 0xc5, 0xff],
            label: [0xe8, 0xec, 0xf4, 0xff],
            detail: [0x9a, 0xa4, 0xb8, 0xff],
            footer: [0x55, 0x5e, 0x72, 0xff],
            card_edge: [0x22, 0x2d, 0x44, 0xff],
            card_bg: [0x02, 0x03, 0x07, 0xff],
        }
    }
}

/// Shrink `px` until `text` fits in `avail`. The rasterizer clips at the surface edge,
/// not at the widget card, so a long friendly name would otherwise run straight under it.
fn fit_px(font: &FontRef, text: &str, px: f32, avail: f32) -> f32 {
    let w = text::measure(font, text, px);
    if w <= avail || w <= 0.0 || avail <= 0.0 {
        px
    } else {
        px * avail / w
    }
}

/// Render the scene to an RGBA8 image of `width`×`height`.
///
/// # Errors
/// [`PipelineError`] if the embedded fonts fail to load (never, in practice).
pub fn render(scene: &AttractScene, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let f = text::fonts()?;
    let pal = Palette::default();

    let mut buf = vec![0u8; (width * height * 4) as usize];
    text::fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom);

    let w = width as f32;
    // Scale everything relative to a 720p design so it looks right at any resolution.
    let s = height as f32 / DESIGN_HEIGHT;
    let margin = 90.0 * s;

    // The widget card, framed *around* the reserved rect: the browser layer covers that
    // rect exactly, so a frame drawn inside it would vanish on the first paint. The inner
    // fill is what an un-painted card shows — an empty panel rather than a hole.
    let slot = scene.widget.rect(width, height);
    if let Some(card) = slot {
        let edge = (3.0 * s).max(1.0);
        let (cx, cy) = (card.x as f32, card.y as f32);
        let (cw, ch) = (card.width as f32, card.height as f32);
        text::fill_rect(
            &mut buf,
            width,
            height,
            cx - edge,
            cy - edge,
            cw + edge * 2.0,
            ch + edge * 2.0,
            pal.card_edge,
        );
        text::fill_rect(&mut buf, width, height, cx, cy, cw, ch, pal.card_bg);
    }

    // The text column: the whole surface, or everything left of the widget card. The rows
    // below stay left-aligned either way — the card sits above them.
    let column = slot.map_or(w, |card| card.x as f32 - 40.0 * s);
    let avail = (column - margin * 2.0).max(1.0);

    // Title (bold, centered in the column).
    let title_px = fit_px(&f.bold, &scene.title, 76.0 * s, avail);
    let title_w = text::measure(&f.bold, &scene.title, title_px);
    let mut y = 120.0 * s + text::ascent(&f.bold, title_px);
    text::draw_text(
        &mut buf,
        width,
        height,
        (column - title_w) / 2.0,
        y,
        &scene.title,
        title_px,
        pal.title,
        &f.bold,
    );

    // Tagline (centered in the column).
    let tag_px = fit_px(&f.regular, &scene.tagline, 30.0 * s, avail);
    let tag_w = text::measure(&f.regular, &scene.tagline, tag_px);
    y += 46.0 * s + text::ascent(&f.regular, tag_px);
    text::draw_text(
        &mut buf,
        width,
        height,
        (column - tag_w) / 2.0,
        y,
        &scene.tagline,
        tag_px,
        pal.tagline,
        &f.regular,
    );

    // Rows.
    let row_px = 34.0 * s;
    let row_gap = 62.0 * s;
    y += 90.0 * s;
    let label_x = margin + 42.0 * s;
    let detail_x = margin + 340.0 * s;
    for row in &scene.rows {
        let baseline = y + text::ascent(&f.regular, row_px) * 0.8;
        let sq = 22.0 * s;
        text::fill_rect(
            &mut buf,
            width,
            height,
            margin,
            baseline - sq,
            sq,
            sq,
            row.accent,
        );
        text::draw_text(
            &mut buf, width, height, label_x, baseline, &row.label, row_px, pal.label, &f.bold,
        );
        text::draw_text(
            &mut buf,
            width,
            height,
            detail_x,
            baseline,
            &row.detail,
            row_px,
            pal.detail,
            &f.regular,
        );
        y += row_gap;
    }

    // Footer.
    let foot_px = 24.0 * s;
    let foot_baseline = height as f32 - 50.0 * s;
    text::draw_text(
        &mut buf,
        width,
        height,
        margin,
        foot_baseline,
        &scene.footer,
        foot_px,
        pal.footer,
        &f.regular,
    );

    Ok(buf)
}

/// Encode an RGBA image as PNG bytes (for previews / captures).
///
/// # Errors
/// [`PipelineError`] on encode failure.
pub fn to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| PipelineError::InvalidFrame(png_err(&e)))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| PipelineError::InvalidFrame(png_err(&e)))?;
    }
    Ok(out)
}

fn png_err(_e: &png::EncodingError) -> &'static str {
    "png encode failed"
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn renders_non_blank_scene_with_text() {
        let scene = AttractScene::demo();
        let (w, h) = (1280, 720);
        let img = render(&scene, w, h).unwrap();
        assert_eq!(img.len(), (w * h * 4) as usize);
        let bright = img
            .chunks_exact(4)
            .any(|p| u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]) > 600);
        assert!(
            bright,
            "expected bright text pixels over the dark background"
        );
        let png = to_png(w, h, &img).unwrap();
        assert!(png.starts_with(b"\x89PNG"), "valid PNG signature");
    }

    #[test]
    fn scales_to_4k_without_panicking() {
        let img = render(&AttractScene::demo(), 3840, 2160).unwrap();
        assert_eq!(img.len(), 3840 * 2160 * 4);
    }

    #[test]
    fn widget_card_is_a_16_9_rect_in_the_top_right() {
        let (w, h) = (3840, 2160);
        let card = WidgetSlot::RightCard.rect(w, h).unwrap();
        let aspect = f64::from(card.width) / f64::from(card.height);
        assert!((aspect - 16.0 / 9.0).abs() < 0.02, "aspect {aspect}");
        // Inside the surface, in the top-right quadrant.
        assert!(card.x + card.width < w && card.y + card.height < h);
        assert!(card.x > w / 2 && card.y < h / 4);
        assert_eq!(WidgetSlot::None.rect(w, h), None);
    }

    /// The card's transform is what the compositor places the browser layer with, so its
    /// texels must land on whole device pixels — same reason as the OSD banner.
    #[test]
    fn widget_transform_maps_texels_onto_whole_device_pixels() {
        for (w, h) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            let card = WidgetSlot::RightCard.rect(w, h).unwrap();
            let t = card.transform(w, h);
            // Sub-pixel tolerance, not f32::EPSILON: normalizing and re-multiplying by a
            // 4K dimension loses far more than one ulp, and what matters is that the quad
            // lands on the pixel grid.
            assert!((t.scale_x * w as f32 - card.width as f32).abs() < 0.01);
            assert!((t.scale_y * h as f32 - card.height as f32).abs() < 0.01);
            assert!((t.offset_x * w as f32 - card.x as f32).abs() < 0.01);
        }
    }

    #[test]
    fn degenerate_surface_still_yields_a_usable_card() {
        let card = WidgetSlot::RightCard.rect(0, 0).unwrap();
        assert!(card.width >= 1 && card.height >= 1);
        let t = card.transform(0, 0);
        assert!(t.scale_x.is_finite() && t.offset_y.is_finite());
    }

    /// The reserved card is empty background, and the text must not be drawn through it —
    /// so the pixels inside it stay the card fill even with a very long title.
    #[test]
    fn text_does_not_bleed_into_the_reserved_card() {
        let (w, h) = (1280, 720);
        let scene = AttractScene {
            title: "a-very-long-receiver-name.example.invalid".into(),
            widget: WidgetSlot::RightCard,
            ..AttractScene::demo()
        };
        let img = render(&scene, w, h).unwrap();
        let card = WidgetSlot::RightCard.rect(w, h).unwrap();
        for y in card.y..card.y + card.height {
            for x in card.x..card.x + card.width {
                let p = ((y * w + x) * 4) as usize;
                let bright = u16::from(img[p]) + u16::from(img[p + 1]) + u16::from(img[p + 2]);
                assert!(bright < 60, "text bled into the card at {x},{y}");
            }
        }
    }
}
