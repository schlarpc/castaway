//! A tiny software text rasterizer shared by the attract scene and the OSD overlay.
//! ab_glyph outlines over an RGBA buffer with source-over compositing, so it works both
//! on an opaque canvas (attract) and a transparent one (OSD banner).
//!
//! Pixel/coordinate/color conversions between float and integer are intentional and
//! bounded, so the workspace's cast lints (aimed at protocol code) are allowed here.
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

/// The two embedded weights.
pub struct Fonts {
    /// Regular weight.
    pub regular: FontRef<'static>,
    /// Bold weight.
    pub bold: FontRef<'static>,
}

/// Load the embedded DejaVu fonts.
///
/// # Errors
/// [`PipelineError`] if the embedded font data fails to parse (never, in practice).
pub fn fonts() -> Result<Fonts, PipelineError> {
    Ok(Fonts {
        regular: FontRef::try_from_slice(FONT_REGULAR)
            .map_err(|e| PipelineError::InvalidFrame("bad regular font").context(e))?,
        bold: FontRef::try_from_slice(FONT_BOLD)
            .map_err(|e| PipelineError::InvalidFrame("bad bold font").context(e))?,
    })
}

/// Fill the whole buffer with a top→bottom vertical gradient (opaque).
pub fn fill_gradient(buf: &mut [u8], width: u32, height: u32, top: Rgba, bottom: Rgba) {
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

/// Fill a rectangle with source-over compositing (honours the color's alpha).
pub fn fill_rect(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: Rgba,
) {
    let x0 = x.max(0.0) as u32;
    let y0 = y.max(0.0) as u32;
    let x1 = ((x + w) as u32).min(width);
    let y1 = ((y + h) as u32).min(height);
    for py in y0..y1 {
        for px in x0..x1 {
            blend_over(buf, width, height, px as i32, py as i32, color, 1.0);
        }
    }
}

/// Draw left-aligned text with its baseline at `baseline`, returning the pen x-advance.
pub fn draw_text(
    buf: &mut [u8],
    width: u32,
    height: u32,
    mut x: f32,
    baseline: f32,
    text: &str,
    px: f32,
    color: Rgba,
    font: &FontRef,
) -> f32 {
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
                blend_over(buf, width, height, px_x, px_y, color, coverage);
            });
        }
        x += scaled.h_advance(gid);
        prev = Some(gid);
    }
    x
}

/// Measure the advance width of `text` at `px`.
#[must_use]
pub fn measure(font: &FontRef, text: &str, px: f32) -> f32 {
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

/// The ascent (baseline-to-top) at `px`.
#[must_use]
pub fn ascent(font: &FontRef, px: f32) -> f32 {
    font.as_scaled(PxScale::from(px)).ascent()
}

/// Source-over composite one pixel: works on opaque and transparent canvases alike.
pub fn blend_over(
    buf: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: Rgba,
    coverage: f32,
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let sa = (f32::from(color[3]) / 255.0) * coverage.clamp(0.0, 1.0);
    if sa <= 0.0 {
        return;
    }
    let i = ((y as u32 * width + x as u32) * 4) as usize;
    let da = f32::from(buf[i + 3]) / 255.0;
    let out_a = sa + da * (1.0 - sa);
    if out_a <= 0.0 {
        buf[i..i + 4].copy_from_slice(&[0, 0, 0, 0]);
        return;
    }
    for c in 0..3 {
        let s = f32::from(color[c]);
        let d = f32::from(buf[i + c]);
        let out = (s * sa + d * da * (1.0 - sa)) / out_a;
        buf[i + c] = out.round().clamp(0.0, 255.0) as u8;
    }
    buf[i + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) * (1.0 - t) + f32::from(b) * t)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Attach an error source to a `PipelineError` (used by the embedded-font loaders).
trait Context {
    fn context<E: std::fmt::Display>(self, e: E) -> PipelineError;
}
impl Context for PipelineError {
    fn context<E: std::fmt::Display>(self, e: E) -> PipelineError {
        PipelineError::GpuInit(format!("{self}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn text_on_transparent_stays_mostly_transparent() {
        let (w, h) = (64u32, 32u32);
        let mut buf = vec![0u8; (w * h * 4) as usize]; // transparent
        let f = fonts().unwrap();
        draw_text(
            &mut buf,
            w,
            h,
            2.0,
            24.0,
            "Hi",
            24.0,
            [255, 255, 255, 255],
            &f.regular,
        );
        // Some pixels became opaque (the glyphs); most stayed transparent.
        let opaque = buf.chunks_exact(4).filter(|p| p[3] > 200).count();
        let transparent = buf.chunks_exact(4).filter(|p| p[3] == 0).count();
        assert!(opaque > 0, "glyphs should be drawn");
        assert!(transparent > opaque, "background should stay transparent");
    }
}
