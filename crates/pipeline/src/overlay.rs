//! The navigation affordance: a home pill, and the edge swipe that does the same thing
//! without it (D38).
//!
//! Two ways to the same place on purpose. The pill is for someone who has never used the
//! panel — a hackerspace screen is mostly used by guests, and a discoverable affordance
//! matters more than an elegant one. The swipe is for when the pill is in the way.
//!
//! The pill appears on any touch and fades. It cannot be permanent: it sits above a
//! fullscreen cast, so a pill that never faded would be a smudge on every video anyone
//! ever played here.
//!
//! Geometry is pure and shared between drawing and hit-testing, so the pill cannot be
//! pressed where it is not drawn (D33).

use crate::error::PipelineError;
use crate::shape::{self, Rect};
use crate::text::{self, Rgba};
use crate::theme;

/// The design height every dimension scales from, matching the shell's screens.
const DESIGN_HEIGHT: f32 = 720.0;

/// How wide the reserved left edge is, as a fraction of the panel.
///
/// Thin, because it is taken away from whatever is underneath: a page with a control on
/// its left edge loses touches that start there. Thin enough that this is a sliver of a
/// 65-inch screen, wide enough to be reachable without looking.
pub const EDGE_FRACTION: f32 = 0.035;

/// How far right a contact must travel to be a swipe, as a fraction of the panel.
///
/// Travel rather than a tap, so resting a hand on the frame does not navigate. And
/// rightward, because the gesture means "pull the panel back out".
pub const SWIPE_TRAVEL: f32 = 0.10;

/// How far a contact may wander vertically and still count.
pub const SWIPE_DRIFT: f32 = 0.18;

/// How long the pill stays before it starts fading.
pub const PILL_HOLD: std::time::Duration = std::time::Duration::from_millis(2200);

/// How long the fade itself takes.
pub const PILL_FADE: std::time::Duration = std::time::Duration::from_millis(600);

/// Where the pill sits, in device pixels.
///
/// Bottom-left, near the edge the swipe comes from, so the two affordances teach each
/// other: someone who taps the pill sees where the gesture lives.
#[must_use]
pub fn pill_rect(width: u32, height: u32) -> Rect {
    let s = height as f32 / DESIGN_HEIGHT;
    let w = 168.0 * s;
    let h = 64.0 * s;
    // Clear of the reserved edge, not merely near it. They are two affordances for one
    // action, and a swipe that starts on the pill should be a swipe rather than an
    // ambiguity the routing order happens to settle.
    let x = (40.0 * s).max(EDGE_FRACTION * width as f32 + 24.0 * s);
    clamp_into(
        Rect {
            x,
            y: height as f32 - h - 40.0 * s,
            w,
            h,
        },
        width,
        height,
    )
}

/// Nudge a rectangle fully inside a surface, if it fits at all.
fn clamp_into(rect: Rect, width: u32, height: u32) -> Rect {
    Rect {
        x: rect.x.min(width as f32 - rect.w).max(0.0),
        y: rect.y.min(height as f32 - rect.h).max(0.0),
        ..rect
    }
}

/// Whether a panel-normalized point is on the pill.
#[must_use]
pub fn hit_pill(width: u32, height: u32, x: f32, y: f32) -> bool {
    pill_rect(width, height).contains(x * width as f32, y * height as f32)
}

/// How opaque the pill should be, `age` after the touch that raised it.
///
/// Returns `0.0` once it has fully faded, which the caller takes as "drop the layer" —
/// a fully transparent layer still costs a draw call every frame.
#[must_use]
pub fn pill_opacity(age: std::time::Duration) -> f32 {
    if age <= PILL_HOLD {
        return 1.0;
    }
    let fading = age - PILL_HOLD;
    if fading >= PILL_FADE {
        return 0.0;
    }
    1.0 - fading.as_secs_f32() / PILL_FADE.as_secs_f32()
}

struct Palette {
    plate: Rgba,
    edge: Rgba,
    glyph: Rgba,
    label: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            // Dark and opaque rather than a translucent scrim: it has to stay legible
            // over an arbitrary video frame, which may be any colour at all.
            plate: [0x0d, 0x14, 0x28, 0xf2],
            // dma.space blue.
            edge: theme::BLUE,
            glyph: theme::TEXT,
            label: theme::TEXT_BODY,
        }
    }
}

/// Draw the pill into a tight RGBA8 buffer of its own size.
///
/// Tight rather than full-surface, like the OSD banner: this is a small texture placed
/// by a transform, and a 4K buffer to hold a 170-pixel pill would be 33 MB to upload
/// every time it appeared.
///
/// # Errors
/// [`PipelineError`] if the bundled fonts fail to load.
pub fn render_pill(width: u32, height: u32) -> Result<(Vec<u8>, Rect), PipelineError> {
    let f = text::fonts()?;
    let pal = Palette::default();
    let rect = pill_rect(width, height);
    let s = height as f32 / DESIGN_HEIGHT;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (w, h) = (rect.w.ceil() as u32, rect.h.ceil() as u32);
    let mut buf = vec![0u8; (w * h * 4) as usize];

    // Local coordinates: the texture is the pill.
    let local = Rect {
        x: 0.0,
        y: 0.0,
        w: w as f32,
        h: h as f32,
    };
    let radius = local.h / 2.0;
    shape::rounded_rect(&mut buf, w, h, local, radius, pal.plate);
    shape::rounded_outline(&mut buf, w, h, local, radius, (2.0 * s).max(1.0), pal.edge);

    // A left chevron, matching the one every "back" on the panel uses, so the gesture and
    // the affordance and the screens all say the same thing with the same mark.
    let cy = local.h / 2.0;
    shape::chevron(
        &mut buf,
        w,
        h,
        local.h * 0.52,
        cy,
        local.h * 0.20,
        (3.0 * s).max(1.5),
        pal.glyph,
        shape::Facing::Left,
    );

    let px = 24.0 * s;
    text::draw_text(
        &mut buf,
        w,
        h,
        local.h * 0.86,
        cy + text::ascent(&f.regular, px) * 0.36,
        "Home",
        px,
        pal.label,
        &f.regular,
    );

    Ok((buf, rect))
}

/// Tracks one touch contact well enough to recognise the edge swipe.
#[derive(Debug, Clone, Copy)]
pub struct Contact {
    /// Where it went down, panel-normalized.
    pub start: (f32, f32),
    /// Whether it started inside the reserved left edge, and is therefore ours.
    pub from_edge: bool,
    /// Whether it has already been recognised, so one swipe fires once.
    pub fired: bool,
}

impl Contact {
    /// Begin tracking a contact.
    #[must_use]
    pub fn new(x: f32, y: f32) -> Self {
        Self {
            start: (x, y),
            from_edge: x <= EDGE_FRACTION,
            fired: false,
        }
    }

    /// Whether a move to `(x, y)` completes the go-home swipe.
    ///
    /// Only ever true for a contact that started at the edge, so an ordinary drag across
    /// the middle of a page never navigates.
    #[must_use]
    pub fn is_home_swipe(&self, x: f32, y: f32) -> bool {
        self.from_edge
            && !self.fired
            && x - self.start.0 >= SWIPE_TRAVEL
            && (y - self.start.1).abs() <= SWIPE_DRIFT
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_swipe_from_the_edge_goes_home() {
        let c = Contact::new(0.01, 0.5);
        assert!(c.from_edge);
        assert!(!c.is_home_swipe(0.05, 0.5), "not far enough yet");
        assert!(c.is_home_swipe(0.2, 0.5));
    }

    #[test]
    fn a_drag_that_did_not_start_at_the_edge_never_navigates() {
        // The failure this prevents: scrolling a page sideways would throw someone out
        // of it.
        let c = Contact::new(0.5, 0.5);
        assert!(!c.from_edge);
        assert!(!c.is_home_swipe(0.9, 0.5));
    }

    #[test]
    fn a_swipe_that_wanders_too_far_vertically_is_not_one() {
        let c = Contact::new(0.01, 0.2);
        assert!(!c.is_home_swipe(0.3, 0.9));
    }

    #[test]
    fn a_swipe_fires_once() {
        let mut c = Contact::new(0.01, 0.5);
        assert!(c.is_home_swipe(0.3, 0.5));
        c.fired = true;
        assert!(!c.is_home_swipe(0.4, 0.5), "one gesture, one navigation");
    }

    #[test]
    fn the_pill_fades_to_nothing_and_stays_there() {
        use std::time::Duration;
        assert_eq!(pill_opacity(Duration::ZERO), 1.0);
        assert_eq!(pill_opacity(PILL_HOLD), 1.0);
        let mid = pill_opacity(PILL_HOLD + PILL_FADE / 2);
        assert!(mid > 0.2 && mid < 0.8, "half-faded, got {mid}");
        assert_eq!(pill_opacity(PILL_HOLD + PILL_FADE), 0.0);
        assert_eq!(
            pill_opacity(PILL_HOLD + PILL_FADE * 10),
            0.0,
            "a pill that came back would be a smudge on every video"
        );
    }

    #[test]
    fn the_pill_is_pressable_where_it_is_drawn() {
        let (w, h) = (1920, 1080);
        let r = pill_rect(w, h);
        let (cx, cy) = r.center();
        assert!(hit_pill(w, h, cx / w as f32, cy / h as f32));
        assert!(!hit_pill(w, h, 0.5, 0.5));
        // ...and it is on the panel.
        assert!(r.x >= 0.0 && r.y >= 0.0);
        assert!(r.x + r.w <= w as f32 && r.y + r.h <= h as f32);
    }

    #[test]
    fn it_renders_at_its_own_size() {
        let (w, h) = (3840, 2160);
        let (buf, rect) = render_pill(w, h).unwrap();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expect = (rect.w.ceil() as u32 * rect.h.ceil() as u32 * 4) as usize;
        assert_eq!(buf.len(), expect);
        // Something was drawn.
        assert!(buf.iter().skip(3).step_by(4).any(|a| *a > 0));
    }
}
