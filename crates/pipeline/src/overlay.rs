//! The navigation affordance: a home pill, and the edge swipe that does the same thing
//! without it (D38).
//!
//! Two ways to the same place on purpose. The pill is for someone who has never used the
//! panel — a hackerspace screen is mostly used by guests, and a discoverable affordance
//! matters more than an elegant one. The swipe is for when the pill is in the way.
//!
//! The pill exists only while a session surface holds the whole panel — the one state
//! in which the shell's own chrome (and its back button) is covered. On the shell's own
//! screens the same corner already carries a back affordance, and at Home there is
//! nothing to exit; [`pill_presence`] derives this from focus every frame, so a pill on
//! the Home screen is unrepresentable rather than merely unlikely.
//!
//! While it exists it brightens on any touch and fades back to a dim floor. It cannot
//! be full-bright forever: it sits above a fullscreen cast, and a pill that never faded
//! would be a smudge on every video anyone ever played here.
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

/// What the pill fades *to* while it exists — dim, but present.
///
/// On an app view (Spotify's card, YouTube, a cast) the pill is the one exit that is
/// structurally in the same place everywhere, and an affordance faded to nothing reads
/// as there being no way out. Dim enough not to fight the picture.
pub const PILL_SESSION_FLOOR: f32 = 0.38;

/// Where the pill sits, in device pixels.
///
/// Top-left, vertically centred on the band the shell's own back affordance occupies
/// (`picker`/`service` put theirs at `y = 60·s`, `96·s` tall): "the way out" lives in
/// one corner everywhere, and the pill only ever shows where those buttons are covered.
/// Still on the swipe's edge, so the two affordances keep teaching each other.
#[must_use]
pub fn pill_rect(width: u32, height: u32) -> Rect {
    let s = height as f32 / DESIGN_HEIGHT;
    let w = 168.0 * s;
    let h = 64.0 * s;
    // Clear of the reserved edge, not merely near it. They are two affordances for one
    // action, and a swipe that starts on the pill should be a swipe rather than an
    // ambiguity the routing order happens to settle.
    let x = (70.0 * s).max(EDGE_FRACTION * width as f32 + 24.0 * s);
    clamp_into(
        Rect {
            x,
            y: 76.0 * s,
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

/// How present the pill is, given who holds the panel and when it was last touched.
///
/// Zero — no layer at all, not a transparent one — unless a session surface holds the
/// whole panel. Everywhere else the same corner already answers "how do I get out":
/// the shell's screens carry their own back button there, and Home *is* where the pill
/// goes. This is the whole visibility policy, recomputed from focus every frame; there
/// is no flag that could be left set with the pill showing over the wrong screen.
#[must_use]
pub fn pill_presence(session_fullscreen: bool, touched: Option<std::time::Duration>) -> f32 {
    if !session_fullscreen {
        return 0.0;
    }
    touched.map_or(0.0, pill_opacity).max(PILL_SESSION_FLOOR)
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

/// Draw the close badge — a dark disc with an X — into a tight RGBA8 buffer of
/// `side`×`side`. The pill's palette, because it is the pill's sibling: both are the
/// shell's own chrome drawn over other people's pixels.
#[must_use]
pub fn render_close_badge(side: u32) -> Vec<u8> {
    let side = side.max(8);
    let pal = Palette::default();
    let mut buf = vec![0u8; (side * side * 4) as usize];
    let s = side as f32;
    let c = s / 2.0;
    // Inset so the antialiased rim survives its own bounds.
    shape::disc(&mut buf, side, side, c, c, s - 4.0, pal.plate);
    shape::rounded_outline(
        &mut buf,
        side,
        side,
        shape::Rect {
            x: 2.0,
            y: 2.0,
            w: s - 4.0,
            h: s - 4.0,
        },
        (s - 4.0) / 2.0,
        (s * 0.03).max(1.0),
        pal.edge,
    );
    // The X: two strokes, fat enough to read across a room.
    let (a, b) = (s * 0.34, s * 0.66);
    let stroke = (s * 0.055).max(2.0);
    for (ax, ay, bx, by) in [(a, a, b, b), (a, b, b, a)] {
        shape::fill_sdf(
            &mut buf,
            side,
            side,
            shape::Rect {
                x: 0.0,
                y: 0.0,
                w: s,
                h: s,
            },
            pal.glyph,
            |px, py| shape::sd_segment(px, py, ax, ay, bx, by) - stroke,
        );
    }
    buf
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

    // Local coordinates: the texture is the pill. Inset like the close badge, because the
    // outline is stroked *centred* on the plate's boundary and antialiased past that — drawn
    // at the texture's own bounds, half the rim falls outside the buffer and the pill shows
    // up with its edge shaved flat.
    let stroke = (2.0 * s).max(1.0);
    let inset = stroke / 2.0 + 1.5;
    let local = Rect {
        x: inset,
        y: inset,
        w: w as f32 - 2.0 * inset,
        h: h as f32 - 2.0 * inset,
    };
    let radius = local.h / 2.0;
    shape::rounded_rect(&mut buf, w, h, local, radius, pal.plate);
    shape::rounded_outline(&mut buf, w, h, local, radius, stroke, pal.edge);

    // A left chevron, matching the one every "back" on the panel uses, so the gesture and
    // the affordance and the screens all say the same thing with the same mark.
    let cy = h as f32 / 2.0;
    shape::chevron(
        &mut buf,
        w,
        h,
        inset + local.h * 0.52,
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
        inset + local.h * 0.86,
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

/// What the panel should do about a contact moving on the reserved edge.
///
/// The decision is here, and pure, because it lived in the kiosk as an if-chain over five
/// variables and two of its cases were simply missing: a part-way drag began nothing, so
/// nothing followed the finger, and the flag that said "a drag is in hand" was never cleared —
/// which left the completed-swipe branch (`if complete && !dragging`) permanently unreachable.
/// The home gesture stopped working after the first time anybody dragged from the edge and let
/// go early, and there was no test that could have noticed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EdgeDrag {
    /// Not ours, or nowhere to go.
    Ignore,
    /// Fire the home gesture now: the swipe completed with nothing in hand.
    Home,
    /// Begin a back-navigation and carry it with the finger.
    Begin,
    /// Carry the one already in hand. `shown` is how much of the outgoing screen still shows,
    /// so letting go part-way can put it back.
    Carry {
        /// `1.0` = untouched, `0.0` = fully dragged away.
        shown: f32,
    },
}

/// Decide what a move to `(x, y)` means for `contact`.
///
/// `dragging` is whether a driven navigation is already in hand, `depth` the shell's screen
/// depth, and `playing` whether there is a session to hand the glass back to.
///
/// The one ordering that matters: a *completed* swipe only fires when nothing is in hand,
/// because with a navigation being carried the swipe **is** the navigation and finishing it is
/// the release's job.
#[must_use]
pub fn edge_drag(
    contact: &Contact,
    x: f32,
    y: f32,
    dragging: bool,
    depth: usize,
    playing: bool,
) -> EdgeDrag {
    if !contact.from_edge {
        return EdgeDrag::Ignore;
    }
    if dragging {
        // `travel` is a fraction of the panel, so a drag all the way across is a whole
        // navigation. Clamped, because a finger can leave the edge it started on.
        let travel = (x - contact.start.0).clamp(0.0, 1.0);
        return EdgeDrag::Carry {
            shown: 1.0 - travel,
        };
    }
    if contact.is_home_swipe(x, y) {
        return EdgeDrag::Home;
    }
    // Only a screen-to-screen back is carried today: its whole animation *is* a position, so a
    // finger can be halfway through one. Handing the glass back at Home is a change of focus
    // rather than of place, so it stays a threshold gesture — which is why `playing` alone does
    // not begin a drag.
    if depth > 1 {
        EdgeDrag::Begin
    } else {
        let _ = playing;
        EdgeDrag::Ignore
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_completed_swipe_goes_home_but_only_with_nothing_in_hand() {
        let c = Contact::new(0.01, 0.5);
        assert_eq!(edge_drag(&c, 0.2, 0.5, false, 1, false), EdgeDrag::Home);
        // With a navigation being carried the swipe *is* that navigation, and finishing it
        // belongs to the release.
        assert!(matches!(
            edge_drag(&c, 0.2, 0.5, true, 1, false),
            EdgeDrag::Carry { .. }
        ));
    }

    #[test]
    fn a_drag_from_the_edge_on_a_pushed_screen_begins_a_carry() {
        let c = Contact::new(0.01, 0.5);
        // Short of the swipe threshold, so this is the part-way case that used to begin nothing.
        assert_eq!(edge_drag(&c, 0.05, 0.5, false, 2, false), EdgeDrag::Begin);
        assert_eq!(
            edge_drag(&c, 0.05, 0.5, true, 2, false),
            EdgeDrag::Carry { shown: 0.96 }
        );
    }

    #[test]
    fn a_carry_tracks_the_finger_and_cannot_leave_its_range() {
        let c = Contact::new(0.0, 0.5);
        assert_eq!(
            edge_drag(&c, 0.0, 0.5, true, 2, false),
            EdgeDrag::Carry { shown: 1.0 }
        );
        assert_eq!(
            edge_drag(&c, 0.5, 0.5, true, 2, false),
            EdgeDrag::Carry { shown: 0.5 }
        );
        assert_eq!(
            edge_drag(&c, 1.0, 0.5, true, 2, false),
            EdgeDrag::Carry { shown: 0.0 }
        );
        // Back past where it started, and off the far side: both clamp rather than inverting
        // the navigation or overshooting past its end.
        assert_eq!(
            edge_drag(&c, -0.5, 0.5, true, 2, false),
            EdgeDrag::Carry { shown: 1.0 }
        );
        assert_eq!(
            edge_drag(&c, 2.0, 0.5, true, 2, false),
            EdgeDrag::Carry { shown: 0.0 }
        );
    }

    #[test]
    fn a_contact_from_the_middle_of_the_panel_is_never_ours() {
        // The reserved edge is what keeps an ordinary drag across a page from navigating.
        let c = Contact::new(0.5, 0.5);
        for dragging in [false, true] {
            assert_eq!(edge_drag(&c, 0.9, 0.5, dragging, 3, true), EdgeDrag::Ignore);
        }
    }

    #[test]
    fn at_home_a_part_way_drag_begins_nothing_and_leaves_the_swipe_working() {
        // The bug this function exists for. At Home there is no screen to carry, so a part-way
        // drag must begin nothing *and* leave the completed-swipe branch reachable — the old
        // code set its "dragging" flag here and never cleared it, so the home gesture stopped
        // working for the rest of the process's life.
        let c = Contact::new(0.01, 0.5);
        assert_eq!(edge_drag(&c, 0.05, 0.5, false, 1, true), EdgeDrag::Ignore);
        assert_eq!(edge_drag(&c, 0.2, 0.5, false, 1, true), EdgeDrag::Home);
    }

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
    fn the_pill_only_exists_over_a_fullscreen_session() {
        use std::time::Duration;
        // Freshly touched with the shell foreground — at Home, or on a screen with its
        // own back button — is still nothing: a pill offering "Home" on the Home screen
        // has no representation.
        assert_eq!(pill_presence(false, Some(Duration::ZERO)), 0.0);
        assert_eq!(pill_presence(false, None), 0.0);
        // Over a session it is never gone, only dim.
        assert_eq!(pill_presence(true, None), PILL_SESSION_FLOOR);
        assert_eq!(pill_presence(true, Some(Duration::ZERO)), 1.0);
        assert_eq!(
            pill_presence(true, Some(PILL_HOLD + PILL_FADE * 10)),
            PILL_SESSION_FLOOR,
            "faded hands back to the floor, not to nothing"
        );
    }

    #[test]
    fn the_pill_sits_in_the_back_buttons_band() {
        // One corner means "the way out" everywhere: the pill is vertically centred on
        // the band the picker's and service screens' back affordances occupy, and it
        // only ever shows while those are covered.
        let (w, h) = (1920, 1080);
        let (_, pill_cy) = pill_rect(w, h).center();
        let (_, back_cy) = crate::service::back_rect(w, h).center();
        assert!(
            (pill_cy - back_cy).abs() < 1.0,
            "pill centre {pill_cy} vs back centre {back_cy}"
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
