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

use std::borrow::Cow;

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::error::PipelineError;

const FONT_REGULAR: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/DejaVuSans-Bold.ttf");
const FONT_CJK_REGULAR: &[u8] = include_bytes!("../assets/NotoSansCJK-Regular-subset.otf");
const FONT_CJK_BOLD: &[u8] = include_bytes!("../assets/NotoSansCJK-Bold-subset.otf");

/// An RGBA color.
pub use crate::theme::Rgba;

/// One weight, plus the fonts that cover what it does not.
///
/// DejaVu has never shipped CJK, so a Japanese track title used to draw as blanks — and
/// worse, silently: `glyph_id` answers `GlyphId(0)` for an uncovered character with no
/// error, so `measure` summed `.notdef` advances and the layout went out too (#88).
///
/// A chain rather than a bigger font, because a bigger font only moves the hole. Lookup is
/// per character, first font with a real glyph wins, and adding coverage later means
/// appending to `fallbacks` — no signature in this module changes and no caller is
/// touched.
pub struct Face {
    /// Tried first. Its metrics define the line box.
    primary: FontRef<'static>,
    /// Tried in order for characters `primary` has no glyph for.
    fallbacks: Vec<FontRef<'static>>,
}

impl Face {
    /// The font that actually has a glyph for `ch`, and that glyph's id *in that font*.
    ///
    /// Glyph ids are per-font, so the id and the font it came from must travel together —
    /// looking one up here and measuring it against a different font is exactly the class
    /// of bug this replaced.
    ///
    /// Falls back to the primary's `.notdef` when nothing covers the character: something
    /// has to be drawn, and a visible box is a better report than a silent gap.
    fn resolve(&self, ch: char) -> (&FontRef<'static>, GlyphId) {
        let gid = self.primary.glyph_id(ch);
        if gid.0 != 0 {
            return (&self.primary, gid);
        }
        for font in &self.fallbacks {
            let gid = font.glyph_id(ch);
            if gid.0 != 0 {
                return (font, gid);
            }
        }
        (&self.primary, gid)
    }

    /// Whether any font in the chain can draw `ch`.
    #[must_use]
    pub fn covers(&self, ch: char) -> bool {
        self.resolve(ch).1 .0 != 0
    }
}

/// The two embedded weights.
pub struct Fonts {
    /// Regular weight.
    pub regular: Face,
    /// Bold weight.
    pub bold: Face,
}

/// Load the embedded fonts: DejaVu, with the Noto Sans CJK subset behind it.
///
/// # Errors
/// [`PipelineError`] if the embedded font data fails to parse (never, in practice).
pub fn fonts() -> Result<Fonts, PipelineError> {
    let load = |bytes: &'static [u8], what: &'static str| {
        FontRef::try_from_slice(bytes)
            .map_err(move |e| PipelineError::InvalidFrame(what).context(e))
    };
    Ok(Fonts {
        regular: Face {
            primary: load(FONT_REGULAR, "bad regular font")?,
            fallbacks: vec![load(FONT_CJK_REGULAR, "bad regular CJK font")?],
        },
        bold: Face {
            primary: load(FONT_BOLD, "bad bold font")?,
            fallbacks: vec![load(FONT_CJK_BOLD, "bad bold CJK font")?],
        },
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
    face: &Face,
) -> f32 {
    let scale = PxScale::from(px);
    // The previous glyph *and* the font it came from. Kerning is a per-font table keyed on
    // a pair of that font's own glyph ids, so a pair drawn from two different fonts has no
    // kern to look up — feeding one font's id into another's table would read a real
    // number for an unrelated pair.
    let mut prev: Option<(usize, GlyphId)> = None;
    for ch in text.chars() {
        let (font, gid) = face.resolve(ch);
        let scaled = font.as_scaled(scale);
        let key = std::ptr::from_ref::<FontRef<'static>>(font) as usize;
        if let Some((prev_key, p)) = prev {
            if prev_key == key {
                x += scaled.kern(p, gid);
            }
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
        prev = Some((key, gid));
    }
    x
}

/// Measure the advance width of `text` at `px`.
///
/// Must resolve through the same chain [`draw_text`] does, or a fallback glyph is drawn at
/// one width and measured at another — which is how centring goes wrong for exactly the
/// strings that render.
#[must_use]
pub fn measure(face: &Face, text: &str, px: f32) -> f32 {
    let scale = PxScale::from(px);
    let mut width = 0.0;
    let mut prev: Option<(usize, GlyphId)> = None;
    for ch in text.chars() {
        let (font, gid) = face.resolve(ch);
        let scaled = font.as_scaled(scale);
        let key = std::ptr::from_ref::<FontRef<'static>>(font) as usize;
        if let Some((prev_key, p)) = prev {
            if prev_key == key {
                width += scaled.kern(p, gid);
            }
        }
        width += scaled.h_advance(gid);
        prev = Some((key, gid));
    }
    width
}

/// The mark that says a string was cut. One character, so it costs one advance.
const ELLIPSIS: &str = "…";

/// Cut `text` to what fits `avail` pixels at `px`, ending in an ellipsis when it does not.
///
/// Borrows when the whole string fits, which is the common case: a row's text is normally
/// short and this should cost a measure, not an allocation.
///
/// The alternative — shrinking the size until it fits, as the now-playing card does — is
/// right for one line that owns its box and wrong for a list, where it would give every
/// row a different type size according to the length of its own text.
///
/// Walks the same resolution and kerning chain [`measure`] and [`draw_text`] do, for the
/// same reason: a cut computed against different metrics from the ones the glyphs are
/// drawn with is a cut in the wrong place.
#[must_use]
pub fn ellipsize<'a>(face: &Face, text: &'a str, px: f32, avail: f32) -> Cow<'a, str> {
    if measure(face, text, px) <= avail {
        return Cow::Borrowed(text);
    }
    let mark = measure(face, ELLIPSIS, px);
    if mark > avail {
        // No room even for the mark. Nothing is the honest answer: a bare "…" here would
        // itself overrun the box this call exists to keep text inside.
        return Cow::Borrowed("");
    }
    let scale = PxScale::from(px);
    let mut width = 0.0;
    let mut prev: Option<(usize, GlyphId)> = None;
    let mut cut = 0;
    for (i, ch) in text.char_indices() {
        let (font, gid) = face.resolve(ch);
        let scaled = font.as_scaled(scale);
        let key = std::ptr::from_ref::<FontRef<'static>>(font) as usize;
        let kern = match prev {
            Some((prev_key, p)) if prev_key == key => scaled.kern(p, gid),
            _ => 0.0,
        };
        let step = kern + scaled.h_advance(gid);
        if width + step + mark > avail {
            break;
        }
        width += step;
        prev = Some((key, gid));
        // Byte index past this character: `text` is sliced there, so it must land on a
        // boundary — hence `char_indices` rather than a running count.
        cut = i + ch.len_utf8();
    }
    let mut out = String::with_capacity(cut + ELLIPSIS.len());
    out.push_str(&text[..cut]);
    out.push_str(ELLIPSIS);
    Cow::Owned(out)
}

/// The ascent (baseline-to-top) at `px`.
///
/// From the primary font only, deliberately. Taking the maximum across the chain would
/// make every line box taller the moment a CJK fallback was bundled, moving every existing
/// layout for text that never uses it; taking it per-string would make the line box depend
/// on its contents, so a card would change height when the track changed. The primary's
/// ascent is 0.928em and a CJK ideograph's ink reaches about 0.88em, so the glyphs the
/// fallback draws still sit inside the box.
#[must_use]
pub fn ascent(face: &Face, px: f32) -> f32 {
    face.primary.as_scaled(PxScale::from(px)).ascent()
}

/// The descent at `px`. Negative, as `ab_glyph` reports it: the bottom of the line box
/// sits at `baseline - descent`. Exposed so a caller centring a line in a box can do it
/// from the font's own metrics — `(box.h + ascent + descent) / 2` below the box top —
/// instead of a guessed fraction of the size.
#[must_use]
pub fn descent(face: &Face, px: f32) -> f32 {
    face.primary.as_scaled(PxScale::from(px)).descent()
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

    /// The table from #88, as an assertion.
    ///
    /// These are the exact codepoints the issue read out of DejaVu's `cmap` to establish
    /// the fault. Every one of them must now resolve to a real glyph — the CJK ones
    /// through the fallback, the others still through DejaVu.
    #[test]
    fn the_codepoints_dejavu_has_never_shipped_now_resolve() {
        let f = fonts().unwrap();
        for face in [&f.regular, &f.bold] {
            for ch in ['\u{3041}', '\u{30A2}', '\u{65E5}'] {
                assert!(face.covers(ch), "U+{:04X} still has no glyph", ch as u32);
            }
            // Unchanged, and worth pinning: the fallback must not have displaced anything.
            for ch in ['A', '\u{03A9}', '\u{042F}'] {
                assert!(face.covers(ch), "U+{:04X} regressed", ch as u32);
            }
        }
    }

    /// Latin still comes from DejaVu, not from Noto.
    ///
    /// The chain is only correct if the primary wins whenever it can: a fallback that
    /// captured characters DejaVu covers would silently restyle the whole interface.
    #[test]
    fn the_primary_font_wins_for_everything_it_covers() {
        let f = fonts().unwrap();
        for ch in "ABCdef0123 ,.!?".chars() {
            let (font, _) = f.regular.resolve(ch);
            assert!(
                std::ptr::eq(font, &f.regular.primary),
                "{ch:?} was taken by a fallback"
            );
        }
        // And the reverse: a kanji must not come from the primary.
        let (font, gid) = f.regular.resolve('\u{65E5}');
        assert!(!std::ptr::eq(font, &f.regular.primary));
        assert_ne!(gid.0, 0);
    }

    /// The half of #88 that was not about blank glyphs.
    ///
    /// `measure` summed `.notdef` advances for uncovered text, so layout and centring were
    /// wrong for the same strings that failed to draw. A Japanese title must now measure
    /// like real text: wider than nothing, and — since these are full-width glyphs —
    /// wider than the same number of Latin characters.
    #[test]
    fn uncovered_text_is_no_longer_measured_from_notdef() {
        let f = fonts().unwrap();
        let jp = measure(&f.regular, "\u{65E5}\u{672C}\u{8A9E}", 40.0);
        let latin = measure(&f.regular, "abc", 40.0);
        assert!(jp > 0.0, "japanese measured as nothing");
        assert!(
            jp > latin,
            "three full-width glyphs ({jp}) should be wider than three latin ({latin})"
        );
    }

    /// Mixed scripts have to lay out as one run.
    ///
    /// The failure this guards is an off-by-one in the chain: measuring a mixed string
    /// must equal the sum of its parts, or a card centring "supercell -
    /// \u{6771}\u{4EAC}" puts it off-centre by however much the fallback disagreed.
    #[test]
    fn a_mixed_script_string_measures_as_the_sum_of_its_runs() {
        let f = fonts().unwrap();
        let both = measure(&f.regular, "ab\u{65E5}", 32.0);
        let sum = measure(&f.regular, "ab", 32.0) + measure(&f.regular, "\u{65E5}", 32.0);
        // Not exactly equal: "ab" alone kerns its pair the same way, but splitting drops
        // no kern here, so the difference must be nil.
        assert!((both - sum).abs() < 0.01, "{both} vs {sum}");
    }

    /// Japanese must put ink on the buffer, which is the actual user-visible complaint.
    #[test]
    fn japanese_actually_rasterises() {
        let (w, h) = (256u32, 96u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let f = fonts().unwrap();
        draw_text(
            &mut buf,
            w,
            h,
            8.0,
            64.0,
            "\u{65E5}\u{672C}\u{8A9E}",
            48.0,
            [255, 255, 255, 255],
            &f.regular,
        );
        let inked = buf.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(inked > 200, "only {inked} pixels drawn; glyphs are blank");
    }

    /// A character nothing in the chain covers must still not panic or measure as zero.
    ///
    /// The chain is deliberately not all of Unicode — the issue named Thai and Devanagari
    /// as remaining holes and they still are — so this path is reachable and has to
    /// degrade to a visible box rather than to a gap.
    ///
    /// Devanagari and Thai rather than emoji: DejaVu does ship a monochrome emoticon set,
    /// so U+1F600 is covered and would have made this test assert nothing.
    #[test]
    fn a_character_no_font_covers_degrades_rather_than_failing() {
        let f = fonts().unwrap();
        for ch in ['\u{0905}', '\u{0E01}'] {
            assert!(
                !f.regular.covers(ch),
                "U+{:04X} is covered now; pick another hole for this test",
                ch as u32
            );
            let width = measure(&f.regular, &ch.to_string(), 32.0);
            assert!(width > 0.0, "notdef must still occupy space");
        }
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

    #[test]
    fn text_that_fits_is_returned_untouched_and_unallocated() {
        let f = fonts().unwrap();
        let fits = measure(&f.regular, "Build 957", 22.0) + 1.0;
        let out = ellipsize(&f.regular, "Build 957", 22.0, fits);
        assert!(matches!(out, Cow::Borrowed("Build 957")), "{out:?}");
    }

    #[test]
    fn text_that_does_not_fit_is_cut_to_the_width_it_was_given() {
        // The property the caller needs, and the one that is easy to get wrong by a
        // character: what comes back must *measure* within the box, ellipsis included.
        let f = fonts().unwrap();
        let long = "Running build 957 — automatic updates are off";
        for avail in [40.0, 90.0, 150.0, 300.0] {
            let out = ellipsize(&f.regular, long, 22.0, avail);
            assert!(
                measure(&f.regular, &out, 22.0) <= avail,
                "{out:?} is wider than {avail}"
            );
            assert!(out.ends_with('…'), "{out:?} does not say it was cut");
            assert!(long.starts_with(out.trim_end_matches('…')), "{out:?}");
        }
    }

    #[test]
    fn a_cut_lands_on_a_character_and_never_inside_one() {
        // A byte-index cut would panic on a multi-byte character. The CJK fallback makes
        // that the normal case, not the exotic one — these are the widest glyphs here, so
        // most of this string is past any sensible box.
        let f = fonts().unwrap();
        let cjk = "日本語のトラックタイトル";
        for avail in (10..200u16).step_by(7) {
            let avail = f32::from(avail);
            let out = ellipsize(&f.regular, cjk, 22.0, avail);
            assert!(measure(&f.regular, &out, 22.0) <= avail, "{out:?}");
        }
    }

    #[test]
    fn a_box_too_narrow_for_the_mark_gets_nothing_rather_than_an_overrun() {
        let f = fonts().unwrap();
        let mark = measure(&f.regular, "…", 22.0);
        let out = ellipsize(&f.regular, "anything at all", 22.0, mark - 0.5);
        assert_eq!(out, "");
    }
}
