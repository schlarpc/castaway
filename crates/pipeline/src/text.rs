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
pub use crate::theme::Rgba;

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
        let row = [
            lerp_f(top[0], bottom[0], t),
            lerp_f(top[1], bottom[1], t),
            lerp_f(top[2], bottom[2], t),
        ];
        for x in 0..width {
            // Ordered dither before 8-bit quantization: a slow dark gradient spans few
            // 8-bit steps, so flat per-row rounding shows as wide bands on a big panel.
            let d = dither_offset(x, y);
            let i = ((y * width + x) * 4) as usize;
            buf[i] = quant(row[0] + d);
            buf[i + 1] = quant(row[1] + d);
            buf[i + 2] = quant(row[2] + d);
            buf[i + 3] = 255;
        }
    }
}

/// A 64×64 blue-noise ranking matrix (void-and-cluster), 4096 little-endian u16 ranks.
/// Blue noise beats a Bayer matrix here: no visible crosshatch, energy pushed to high
/// frequencies the eye ignores.
const BLUE_NOISE: &[u8] = include_bytes!("../assets/bluenoise64.bin");

/// The dither offset for a pixel, in `[-0.5, 0.5)` — one same-sign nudge for all three
/// channels (luminance dither; per-channel offsets would add color speckle).
fn dither_offset(x: u32, y: u32) -> f32 {
    let i = (((y & 63) * 64 + (x & 63)) * 2) as usize;
    let rank = u16::from_le_bytes([BLUE_NOISE[i], BLUE_NOISE[i + 1]]);
    (f32::from(rank) + 0.5) / 4096.0 - 0.5
}

fn lerp_f(a: u8, b: u8, t: f32) -> f32 {
    f32::from(a) * (1.0 - t) + f32::from(b) * t
}

fn quant(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Fill a rectangle with source-over compositing (honours the color's alpha).
/// Fill with a vertical gradient through several stops, dithered like the two-stop one.
///
/// Exists for the seasonal backgrounds (#24): a flag is more than two colours, and
/// approximating one with a stripe put a band across the screen instead of colouring it.
/// Fewer than two stops falls back to a flat fill of the first.
pub fn fill_gradient_stops(buf: &mut [u8], width: u32, height: u32, stops: &[Rgba]) {
    match stops {
        [] => return,
        [only] => {
            fill_gradient(buf, width, height, *only, *only);
            return;
        }
        _ => {}
    }
    let segments = stops.len() - 1;
    let h = height.max(1) as f32;
    for y in 0..height {
        // Where this row falls across the whole ramp, then which pair of stops that is
        // between and how far along.
        let t = (y as f32 / h) * segments as f32;
        let i = (t.floor() as usize).min(segments - 1);
        let f = t - i as f32;
        let (a, b) = (stops[i], stops[i + 1]);
        let row = [
            lerp_f(a[0], b[0], f),
            lerp_f(a[1], b[1], f),
            lerp_f(a[2], b[2], f),
        ];
        for x in 0..width {
            // The same ordered dither as the two-stop ramp, and for the same reason: a
            // seasonal background is just as dark, so it bands just as readily.
            let d = dither_offset(x, y);
            let i = ((y * width + x) * 4) as usize;
            buf[i] = quant(row[0] + d);
            buf[i + 1] = quant(row[1] + d);
            buf[i + 2] = quant(row[2] + d);
            buf[i + 3] = 255;
        }
    }
}

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

    #[test]
    fn blue_noise_matrix_is_a_complete_ranking() {
        // 64×64 u16 ranks, each of 0..4096 exactly once — a valid ordered-dither matrix.
        assert_eq!(BLUE_NOISE.len(), 64 * 64 * 2);
        let mut seen = [false; 64 * 64];
        for pair in BLUE_NOISE.chunks_exact(2) {
            let rank = u16::from_le_bytes([pair[0], pair[1]]) as usize;
            assert!(rank < 64 * 64, "rank out of range");
            assert!(!seen[rank], "duplicate rank {rank}");
            seen[rank] = true;
        }
    }

    #[test]
    fn gradient_dither_preserves_mean_and_breaks_bands() {
        // A gradient spanning ONE 8-bit step: undithered it would be two flat bands;
        // dithered, every row mixes the two levels in the right proportion.
        let (w, h) = (256u32, 256u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        fill_gradient(&mut buf, w, h, [10, 10, 10, 255], [11, 11, 11, 255]);
        // Rows near the middle should contain BOTH values (no hard band edge)...
        let row = |y: u32| {
            let start = (y * w * 4) as usize;
            &buf[start..start + (w * 4) as usize]
        };
        let mid = row(h / 2);
        let lo = mid.chunks_exact(4).filter(|p| p[0] == 10).count();
        let hi = mid.chunks_exact(4).filter(|p| p[0] == 11).count();
        assert!(lo > 0 && hi > 0, "mid row should dither between levels");
        // ...and the overall mean must track the analytic gradient mean closely.
        let sum: u64 = buf.chunks_exact(4).map(|p| u64::from(p[0])).sum();
        let mean = sum as f64 / f64::from(w * h);
        assert!((mean - 10.5).abs() < 0.05, "mean drifted: {mean}");
    }
}
