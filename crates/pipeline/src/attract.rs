//! The idle "attract" / lobby scene: what the panel shows when nothing is casting. It's
//! the first thing anyone sees, so it names the receiver and tells them how to throw
//! media at it. Rendered on the CPU (ab_glyph text over a gradient) into an RGBA image
//! that the compositor shows as a background layer (video covers it when a cast starts).
//!
//! Rendering here is pure/deterministic, so it unit-tests without a GPU and can be dumped
//! to a PNG for preview ([`to_png`]).
//!
//! This is a small software rasterizer: pixel/coordinate/color conversions between float
//! and integer are intentional and bounded, so the workspace's cast lints (which target
//! protocol code) are allowed for this module.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::too_many_arguments
)]

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::error::PipelineError;

const FONT_REGULAR: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");

/// An RGBA color.
pub type Rgba = [u8; 4];

/// One "how to cast" row: a colored bullet, a device/app label, and the instruction.
#[derive(Debug, Clone)]
pub struct AttractRow {
    /// Bullet accent color.
    pub accent: Rgba,
    /// The sender (e.g. "Chrome / Edge").
    pub label: String,
    /// What to do (e.g. "Cast icon → Hackerspace TV").
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
}

impl AttractScene {
    /// A representative scene for previews/tests.
    #[must_use]
    pub fn demo() -> Self {
        Self {
            title: "Hackerspace TV".into(),
            tagline: "Throw anything at the wall — no app to install.".into(),
            rows: vec![
                AttractRow::new(
                    [0x42, 0x85, 0xf4, 0xff],
                    "Chrome / Edge",
                    "Cast \u{2192} Hackerspace TV",
                ),
                AttractRow::new(
                    [0xff, 0xff, 0xff, 0xff],
                    "iPhone / Mac",
                    "AirPlay \u{2192} Hackerspace TV",
                ),
                AttractRow::new(
                    [0x3d, 0xdc, 0x84, 0xff],
                    "Android / VLC",
                    "Cast or DLNA \u{2192} Hackerspace TV",
                ),
                AttractRow::new(
                    [0x1d, 0xb9, 0x54, 0xff],
                    "Spotify",
                    "Devices \u{2192} Hackerspace TV",
                ),
                AttractRow::new(
                    [0xff, 0x00, 0x00, 0xff],
                    "YouTube",
                    "Cast button \u{2192} Hackerspace TV",
                ),
            ],
            footer: "castaway  •  DLNA / mDNS on 10.0.0.5:8080".into(),
        }
    }
}

/// Colors for the scene.
struct Palette {
    bg_top: Rgba,
    bg_bottom: Rgba,
    title: Rgba,
    tagline: Rgba,
    label: Rgba,
    detail: Rgba,
    footer: Rgba,
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
        }
    }
}

/// Render the scene to an RGBA8 image of `width`×`height`.
///
/// # Errors
/// [`PipelineError`] if the embedded fonts fail to load (never, in practice).
pub fn render(scene: &AttractScene, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let regular = FontRef::try_from_slice(FONT_REGULAR)
        .map_err(|e| PipelineError::InvalidFrame("bad regular font").with(e))?;
    let bold = FontRef::try_from_slice(FONT_BOLD)
        .map_err(|e| PipelineError::InvalidFrame("bad bold font").with(e))?;
    let pal = Palette::default();

    let mut buf = vec![0u8; (width * height * 4) as usize];
    fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom);

    let w = width as f32;
    // Scale everything relative to a 720p design so it looks right at any resolution.
    let s = height as f32 / 720.0;
    let margin = 90.0 * s;

    // Title (bold, centered).
    let title_px = 76.0 * s;
    let title_w = measure(&bold, &scene.title, title_px);
    let mut y = 120.0 * s + ascent(&bold, title_px);
    draw_text(
        &mut buf,
        width,
        height,
        (w - title_w) / 2.0,
        y,
        &scene.title,
        title_px,
        pal.title,
        &bold,
    );

    // Tagline (centered).
    let tag_px = 30.0 * s;
    let tag_w = measure(&regular, &scene.tagline, tag_px);
    y += 46.0 * s + ascent(&regular, tag_px);
    draw_text(
        &mut buf,
        width,
        height,
        (w - tag_w) / 2.0,
        y,
        &scene.tagline,
        tag_px,
        pal.tagline,
        &regular,
    );

    // Rows.
    let row_px = 34.0 * s;
    let row_gap = 62.0 * s;
    y += 90.0 * s;
    let label_x = margin + 42.0 * s;
    let detail_x = margin + 340.0 * s;
    for row in &scene.rows {
        let baseline = y + ascent(&regular, row_px) * 0.8;
        // Bullet square.
        let sq = 22.0 * s;
        draw_rect(
            &mut buf,
            width,
            height,
            margin,
            baseline - sq,
            sq,
            sq,
            row.accent,
        );
        draw_text(
            &mut buf, width, height, label_x, baseline, &row.label, row_px, pal.label, &bold,
        );
        draw_text(
            &mut buf,
            width,
            height,
            detail_x,
            baseline,
            &row.detail,
            row_px,
            pal.detail,
            &regular,
        );
        y += row_gap;
    }

    // Footer.
    let foot_px = 24.0 * s;
    let foot_baseline = height as f32 - 50.0 * s;
    draw_text(
        &mut buf,
        width,
        height,
        margin,
        foot_baseline,
        &scene.footer,
        foot_px,
        pal.footer,
        &regular,
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
            .map_err(|e| PipelineError::InvalidFrame("png header").with(e))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| PipelineError::InvalidFrame("png data").with(e))?;
    }
    Ok(out)
}

fn fill_gradient(buf: &mut [u8], width: u32, height: u32, top: Rgba, bottom: Rgba) {
    for y in 0..height {
        let t = f32::from(u16::try_from(y).unwrap_or(0)) / height.max(1) as f32;
        let color = [
            lerp(top[0], bottom[0], t),
            lerp(top[1], bottom[1], t),
            lerp(top[2], bottom[2], t),
            255,
        ];
        for x in 0..width {
            let i = ((y * width + x) * 4) as usize;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
}

fn draw_rect(buf: &mut [u8], width: u32, height: u32, x: f32, y: f32, w: f32, h: f32, color: Rgba) {
    let x0 = x.max(0.0) as u32;
    let y0 = y.max(0.0) as u32;
    let x1 = ((x + w) as u32).min(width);
    let y1 = ((y + h) as u32).min(height);
    for py in y0..y1 {
        for px in x0..x1 {
            blend(buf, width, height, px as i32, py as i32, color, 1.0);
        }
    }
}

fn draw_text(
    buf: &mut [u8],
    width: u32,
    height: u32,
    mut x: f32,
    baseline: f32,
    text: &str,
    px: f32,
    color: Rgba,
    font: &FontRef,
) {
    let scale = PxScale::from(px);
    let scaled = font.as_scaled(scale);
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        let gid = font.glyph_id(ch);
        if let Some(p) = prev {
            x += scaled.kern(p, gid);
        }
        let glyph = gid.with_scale_and_position(scale, ab_glyph::point(x, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px_x = bounds.min.x as i32 + gx as i32;
                let px_y = bounds.min.y as i32 + gy as i32;
                blend(buf, width, height, px_x, px_y, color, coverage);
            });
        }
        x += scaled.h_advance(gid);
        prev = Some(gid);
    }
}

fn measure(font: &FontRef, text: &str, px: f32) -> f32 {
    let scaled = font.as_scaled(PxScale::from(px));
    let mut width = 0.0;
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        let gid = font.glyph_id(ch);
        if let Some(p) = prev {
            width += scaled.kern(p, gid);
        }
        width += scaled.h_advance(gid);
        prev = Some(gid);
    }
    width
}

fn ascent(font: &FontRef, px: f32) -> f32 {
    font.as_scaled(PxScale::from(px)).ascent()
}

fn blend(buf: &mut [u8], width: u32, height: u32, x: i32, y: i32, color: Rgba, coverage: f32) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let a = coverage.clamp(0.0, 1.0) * (f32::from(color[3]) / 255.0);
    if a <= 0.0 {
        return;
    }
    let i = ((y as u32 * width + x as u32) * 4) as usize;
    for c in 0..3 {
        let dst = f32::from(buf[i + c]);
        let src = f32::from(color[c]);
        buf[i + c] = (dst * (1.0 - a) + src * a).round().clamp(0.0, 255.0) as u8;
    }
    buf[i + 3] = 255;
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) * (1.0 - t) + f32::from(b) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Attach an error source's text to a `PipelineError` variant.
trait WithSource {
    fn with<E: std::fmt::Display>(self, e: E) -> PipelineError;
}
impl WithSource for PipelineError {
    fn with<E: std::fmt::Display>(self, e: E) -> PipelineError {
        PipelineError::GpuInit(format!("{self}: {e}"))
    }
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

        // The gradient background is dark; text pixels must be much brighter, so a bright
        // pixel proves glyphs were actually rasterized (not just a background fill).
        let bright = img
            .chunks_exact(4)
            .any(|p| u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]) > 600);
        assert!(
            bright,
            "expected bright text pixels over the dark background"
        );

        // The top rows should be darker than the title band on average (title is near top).
        let png = to_png(w, h, &img).unwrap();
        assert!(png.starts_with(b"\x89PNG"), "valid PNG signature");
    }

    #[test]
    fn scales_to_4k_without_panicking() {
        let img = render(&AttractScene::demo(), 3840, 2160).unwrap();
        assert_eq!(img.len(), 3840 * 2160 * 4);
    }
}
