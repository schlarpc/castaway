//! Antialiased shape drawing, from signed distance functions.
//!
//! Lifted out of `transport.rs`, where it was private and only the transport strip could
//! use it. Nothing about it was transport-specific: the shell needs the same circles,
//! rounded boxes, segments and triangles for tiles and navigation glyphs, and duplicating
//! them is how two parts of one screen end up antialiasing differently.
//!
//! Why distance fields rather than font characters: the bundled faces have no such
//! glyphs, a fallback font on an appliance is a dependency that may or may not be
//! installed, and a distance field antialiases for free at any size. Everything here is
//! drawn once per repaint at whatever the panel's scale actually is, so there is no
//! authored size to be wrong.
//!
//! CPU, into an RGBA8 buffer, like every other surface in this crate — the GPU's job is
//! placing and blending finished textures (architecture §4).

// Geometry is unconditional: `transport`'s pure layout half is not gated on a renderer,
// and it lays out with these rectangles. Only the *drawing* needs the text rasterizer,
// so it lives in the `draw` module below and is re-exported when `render` is on.

/// An axis-aligned rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Whether `(px, py)` is inside.
    #[must_use]
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    /// The middle of the rectangle.
    #[must_use]
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// A rectangle inset by `by` on every side. Never inverts: an inset larger than the
    /// rectangle collapses it to zero rather than turning it inside out.
    #[must_use]
    pub fn inset(&self, by: f32) -> Self {
        let w = (self.w - by * 2.0).max(0.0);
        let h = (self.h - by * 2.0).max(0.0);
        Self {
            x: self.x + (self.w - w) / 2.0,
            y: self.y + (self.h - h) / 2.0,
            w,
            h,
        }
    }

    /// A square of side `size` centred on `(cx, cy)`.
    #[must_use]
    pub fn around(cx: f32, cy: f32, size: f32) -> Self {
        Self {
            x: cx - size,
            y: cy - size,
            w: size * 2.0,
            h: size * 2.0,
        }
    }
}

#[cfg(feature = "render")]
mod draw {
    use super::Rect;
    use crate::text::{self, Rgba};

    /// Rasterize a shape over its bounding box, `sd` returning signed distance in pixels
    /// (negative inside).
    pub fn fill_sdf<F: Fn(f32, f32) -> f32>(
        buf: &mut [u8],
        width: u32,
        height: u32,
        bounds: Rect,
        color: Rgba,
        sd: F,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let (x0, y0) = (bounds.x.floor() as i32, bounds.y.floor() as i32);
        #[allow(clippy::cast_possible_truncation)]
        let (x1, y1) = (
            (bounds.x + bounds.w).ceil() as i32,
            (bounds.y + bounds.h).ceil() as i32,
        );
        for py in y0..y1 {
            for px in x0..x1 {
                let d = sd(px as f32 + 0.5, py as f32 + 0.5);
                // Half-pixel band around the edge: coverage 1 well inside, 0 well outside.
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    text::blend_over(buf, width, height, px, py, color, coverage);
                }
            }
        }
    }

    /// Signed distance to a circle.
    #[must_use]
    pub fn sd_circle(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
    }

    /// Signed distance to a rounded box centred at `(cx, cy)` with half-extents `(hx, hy)`.
    #[must_use]
    pub fn sd_round_box(px: f32, py: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
        let dx = (px - cx).abs() - (hx - r);
        let dy = (py - cy).abs() - (hy - r);
        let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
        outside + dx.max(dy).min(0.0) - r
    }

    /// Signed distance to a line segment.
    #[must_use]
    pub fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
        let (pax, pay) = (px - ax, py - ay);
        let (bax, bay) = (bx - ax, by - ay);
        let denom = bax.mul_add(bax, bay * bay).max(f32::EPSILON);
        let t = (pax.mul_add(bax, pay * bay) / denom).clamp(0.0, 1.0);
        (pax - bax * t).hypot(pay - bay * t)
    }

    /// Signed distance to a triangle (the play and skip arrowheads, and the shell's chevrons).
    #[must_use]
    pub fn sd_triangle(px: f32, py: f32, p: [(f32, f32); 3]) -> f32 {
        // Distance to the nearest edge, signed by a winding test. Cheaper and clearer than
        // the exact analytic form, and identical once quantized to a pixel.
        let d = sd_segment(px, py, p[0].0, p[0].1, p[1].0, p[1].1)
            .min(sd_segment(px, py, p[1].0, p[1].1, p[2].0, p[2].1))
            .min(sd_segment(px, py, p[2].0, p[2].1, p[0].0, p[0].1));
        let inside = {
            let sign = |a: (f32, f32), b: (f32, f32)| {
                (b.0 - a.0).mul_add(py - a.1, -((b.1 - a.1) * (px - a.0)))
            };
            let (s0, s1, s2) = (sign(p[0], p[1]), sign(p[1], p[2]), sign(p[2], p[0]));
            (s0 >= 0.0 && s1 >= 0.0 && s2 >= 0.0) || (s0 <= 0.0 && s1 <= 0.0 && s2 <= 0.0)
        };
        if inside {
            -d
        } else {
            d
        }
    }

    /// A filled circle of `diameter`, centred.
    pub fn disc(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        diameter: f32,
        color: Rgba,
    ) {
        let r = diameter / 2.0;
        fill_sdf(
            buf,
            width,
            height,
            // Two pixels of slack so the antialiased edge is not clipped by its own bounds.
            Rect::around(cx, cy, r + 2.0),
            color,
            |px, py| sd_circle(px, py, cx, cy, r),
        );
    }

    /// A filled rounded rectangle. `radius` is clamped to what the rectangle can hold, so an
    /// over-large radius yields a capsule rather than an inverted shape.
    pub fn rounded_rect(
        buf: &mut [u8],
        width: u32,
        height: u32,
        rect: Rect,
        radius: f32,
        color: Rgba,
    ) {
        let (cx, cy) = rect.center();
        let (hx, hy) = (rect.w / 2.0, rect.h / 2.0);
        let r = radius.min(hx).min(hy);
        fill_sdf(
            buf,
            width,
            height,
            Rect {
                x: rect.x - 1.0,
                y: rect.y - 1.0,
                w: rect.w + 2.0,
                h: rect.h + 2.0,
            },
            color,
            |px, py| sd_round_box(px, py, cx, cy, hx, hy, r),
        );
    }

    /// A rounded-rectangle outline of `thickness`, drawn as the difference of two distance
    /// fields so the corners stay even.
    pub fn rounded_outline(
        buf: &mut [u8],
        width: u32,
        height: u32,
        rect: Rect,
        radius: f32,
        thickness: f32,
        color: Rgba,
    ) {
        let (cx, cy) = rect.center();
        let (hx, hy) = (rect.w / 2.0, rect.h / 2.0);
        let r = radius.min(hx).min(hy);
        fill_sdf(
            buf,
            width,
            height,
            Rect {
                x: rect.x - 1.0,
                y: rect.y - 1.0,
                w: rect.w + 2.0,
                h: rect.h + 2.0,
            },
            color,
            |px, py| {
                let d = sd_round_box(px, py, cx, cy, hx, hy, r);
                // Inside the ring: within `thickness` of the edge, on the inner side.
                d.abs() - thickness / 2.0
            },
        );
    }

    /// Which way a chevron points.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Facing {
        /// Points left — "back".
        Left,
        /// Points right — "this row goes somewhere".
        Right,
    }

    /// A chevron: two segments meeting at a point, `half` from the centre in each direction.
    ///
    /// A helper rather than two `sd_segment` calls at each site, because the two segments
    /// have to *share* their meeting vertex and it is easy to write two that do not — which
    /// draws two parallel slashes that look nothing like an arrow and read as a glitch.
    pub fn chevron(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        half: f32,
        thickness: f32,
        color: Rgba,
        facing: Facing,
    ) {
        // The point, and the two arms' far ends.
        let (tip_x, arm_x) = match facing {
            Facing::Left => (cx - half * 0.5, cx + half * 0.5),
            Facing::Right => (cx + half * 0.5, cx - half * 0.5),
        };
        for dy in [-half, half] {
            fill_sdf(
                buf,
                width,
                height,
                Rect::around(cx, cy, half * 2.0),
                color,
                |px, py| sd_segment(px, py, tip_x, cy, arm_x, cy + dy) - thickness / 2.0,
            );
        }
    }
}

#[cfg(feature = "render")]
pub use draw::*;

#[cfg(all(test, feature = "render"))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn canvas(w: u32, h: u32) -> Vec<u8> {
        vec![0; (w * h * 4) as usize]
    }

    fn alpha_at(buf: &[u8], w: u32, x: u32, y: u32) -> u8 {
        buf[((y * w + x) * 4 + 3) as usize]
    }

    #[test]
    fn a_disc_is_solid_in_the_middle_and_empty_in_the_corner() {
        let (w, h) = (64, 64);
        let mut buf = canvas(w, h);
        disc(&mut buf, w, h, 32.0, 32.0, 40.0, [255, 255, 255, 255]);
        assert_eq!(alpha_at(&buf, w, 32, 32), 255);
        assert_eq!(alpha_at(&buf, w, 1, 1), 0);
    }

    #[test]
    fn edges_are_antialiased_rather_than_stepped() {
        // The reason these are distance fields at all. A hard-edged rasterizer gives only
        // 0 and 255 along the boundary; on a 65-inch panel that reads as a jagged glyph.
        let (w, h) = (64, 64);
        let mut buf = canvas(w, h);
        disc(&mut buf, w, h, 32.0, 32.0, 40.0, [255, 255, 255, 255]);
        let partial = (0..w * h)
            .map(|i| buf[(i * 4 + 3) as usize])
            .filter(|a| *a > 0 && *a < 255)
            .count();
        assert!(
            partial > 20,
            "expected a soft edge, found {partial} partial pixels"
        );
    }

    #[test]
    fn an_over_large_radius_gives_a_capsule_not_an_inverted_shape() {
        let (w, h) = (64, 32);
        let mut buf = canvas(w, h);
        rounded_rect(
            &mut buf,
            w,
            h,
            Rect {
                x: 4.0,
                y: 4.0,
                w: 56.0,
                h: 24.0,
            },
            1000.0,
            [255, 255, 255, 255],
        );
        // Centre filled, corner of the bounding box empty — a clamped radius, not a
        // negative one that would have drawn the complement.
        assert_eq!(alpha_at(&buf, w, 32, 16), 255);
        assert_eq!(alpha_at(&buf, w, 5, 5), 0);
    }

    #[test]
    fn an_outline_is_hollow() {
        let (w, h) = (64, 64);
        let mut buf = canvas(w, h);
        rounded_outline(
            &mut buf,
            w,
            h,
            Rect {
                x: 8.0,
                y: 8.0,
                w: 48.0,
                h: 48.0,
            },
            8.0,
            3.0,
            [255, 255, 255, 255],
        );
        assert_eq!(alpha_at(&buf, w, 32, 32), 0, "the middle should be empty");
        assert!(alpha_at(&buf, w, 32, 9) > 0, "the top edge should be drawn");
    }

    #[test]
    fn inset_collapses_rather_than_inverting() {
        let r = Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 10.0,
        };
        let big = r.inset(50.0);
        assert!(big.w >= 0.0 && big.h >= 0.0);
    }

    #[test]
    fn a_chevron_meets_at_one_point_rather_than_drawing_two_slashes() {
        // The bug this replaced: two segments that did not share their vertex, which
        // renders as two parallel strokes and reads as a glitch rather than an arrow.
        let (w, h) = (64, 64);
        let mut buf = canvas(w, h);
        chevron(
            &mut buf,
            w,
            h,
            32.0,
            32.0,
            12.0,
            4.0,
            [255, 255, 255, 255],
            Facing::Left,
        );
        // The tip is drawn...
        assert!(alpha_at(&buf, w, 26, 32) > 0, "the tip should be inked");
        // ...and the far side of the tip is not, which is what distinguishes a chevron
        // from an X or a pair of slashes.
        assert_eq!(
            alpha_at(&buf, w, 20, 32),
            0,
            "nothing should be left of the tip"
        );
        // Both arms reach the right-hand side, above and below.
        assert!(alpha_at(&buf, w, 37, 21) > 0, "upper arm");
        assert!(alpha_at(&buf, w, 37, 43) > 0, "lower arm");
    }

    #[test]
    fn a_triangle_is_inside_out_where_it_should_be() {
        let tri = [(0.0, 0.0), (10.0, 0.0), (5.0, 10.0)];
        assert!(sd_triangle(5.0, 3.0, tri) < 0.0, "centre is inside");
        assert!(sd_triangle(-5.0, 5.0, tri) > 0.0, "left of it is outside");
    }
}
