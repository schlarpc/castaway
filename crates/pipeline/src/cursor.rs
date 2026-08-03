//! Whether the pointer is drawn, and what it is drawn as.
//!
//! On a 65" panel on a wall a parked arrow is a defect. The native cursor sits wherever
//! the pointer was last left — after a touch, after a mouse nudge — and stays there for
//! as long as the window has focus, which on a kiosk is forever. Nothing in the tree set
//! cursor visibility at all before this (#84).
//!
//! So it is aggressively hidden: visible only while the pointer is *being used*, gone a
//! short time after it stops, and gone immediately on a touch — because a touch is
//! somebody at the glass, and the pointer they are not using should not be on it.
//!
//! And since visibility is being decided here anyway, the arrow drawn is ours. The OS
//! pointer is the last piece of Windows/Linux chrome that leaks through a surface which
//! is otherwise entirely the panel's.
//!
//! # The policy is a function, not a flag
//!
//! [`presence`] is recomputed every frame from when the pointer was last used and whether
//! a contact is live, in the same shape as [`crate::overlay::pill_presence`]. There is no
//! "cursor hidden" bit that could be left set with the pointer moving over it, and no
//! path that has to remember to un-hide.

use std::time::Duration;

/// How long the cursor stays up after the pointer stops being used.
///
/// Much shorter than the pill's 2.2 s hold, and deliberately: the pill is an affordance
/// someone has to *find*, so it lingers to be findable. The cursor is the opposite — it
/// is only useful while it is being moved, and every millisecond past that is a parked
/// arrow on a wall.
pub const HOLD: Duration = Duration::from_millis(1500);

/// Whether the pointer should be drawn.
///
/// `used` is how long ago the pointer last moved, clicked or scrolled; `None` means it
/// has not been used since the window opened, which is the state a panel that nobody has
/// touched a mouse on stays in forever.
///
/// `contact` is whether a primary-button drag is in flight. It holds the cursor up
/// regardless of the timer: `kiosk` deliberately synthesizes a held left button into a
/// touch contact so mouse users get the edge-swipe and the home pill, and a
/// held-and-dragging pointer is a live contact whose cursor should stay exactly where the
/// hand is.
#[must_use]
pub fn shown(used: Option<Duration>, contact: bool) -> bool {
    if contact {
        return true;
    }
    used.is_some_and(|since| since < HOLD)
}

/// When the cursor's visibility next changes on its own, `used` after the last use.
///
/// The trap this exists for: the kiosk event loop *sleeps* on `Demand`, and
/// `ControlFlow::Wait` sleeps until something happens. A hide deadline that is not merged
/// into the demand calculation is simply slept through, and the arrow stays up until
/// something unrelated wakes the loop — which on an idle panel is not soon.
#[must_use]
pub fn next_change(used: Option<Duration>, contact: bool) -> Option<Duration> {
    if contact {
        // Nothing is due: the cursor is up until the button lifts, and the lift is an
        // event rather than a deadline.
        return None;
    }
    // `checked_sub` alone would answer `Some(0)` at exactly the hold boundary — a
    // deadline in the past, which asks the loop for a frame forever.
    used.and_then(|since| HOLD.checked_sub(since))
        .filter(|remaining| !remaining.is_zero())
}

/// The panel's own pointer, at `side` pixels square, as straight-alpha RGBA8 with the
/// hotspot it should be placed by.
///
/// The layout `winit`'s `CustomCursor::from_rgba` takes — byte-for-byte what
/// [`crate::icon::rasterize`] already produces for the window icon, which is why the
/// themed half of this is small.
///
/// It rides the hardware cursor path, so there is no frame of lag and no new compositor
/// work. The cost is that it cannot animate or fade: swapping images is the only
/// transition available, which is why the policy above is hold-then-hide rather than the
/// pill's hold-then-fade.
#[must_use]
pub fn rasterize(side: u32) -> Option<(Vec<u8>, (u16, u16))> {
    use resvg::tiny_skia;
    let tree = resvg::usvg::Tree::from_str(POINTER_SVG, &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(side, side)?;
    let size = tree.size();
    #[allow(clippy::cast_precision_loss)]
    let scale = (side as f32 / size.width()).min(side as f32 / size.height());
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied alpha; winit wants straight, exactly as the icon
    // path does.
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|px| {
            let c = px.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    // The arrow's tip is authored at the top-left of the viewBox, so the hotspot is the
    // origin. A hotspot in the middle would put the click a dozen pixels below where the
    // point is, which is the kind of wrongness nobody can name but everybody feels.
    Some((rgba, (0, 0)))
}

/// The pointer artwork, authored here for the same reason the icon is: one source, so it
/// cannot drift from the panel's palette.
///
/// A plain arrow in the shell's plate/edge colours ([`crate::theme::PLATE`],
/// [`crate::theme::EDGE`]) with a soft outline, so it reads against both the dark shell
/// and whatever a cast is showing. Drawn at 32×32 in the viewBox with the tip at the
/// origin; [`rasterize`] scales it to whatever the panel asks for.
pub const POINTER_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
  <path d="M1 1 L1 23.5 L7.1 17.9 L11.2 27.4 L15.4 25.6 L11.4 16.3 L19.6 15.6 Z"
        fill="#e8ecf4" stroke="#03050b" stroke-width="2.2" stroke-linejoin="round"/>
  <path d="M1 1 L1 23.5 L7.1 17.9 L11.2 27.4 L15.4 25.6 L11.4 16.3 L19.6 15.6 Z"
        fill="#e8ecf4"/>
</svg>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_nobody_has_used_is_not_drawn() {
        // The state a panel on a wall with no mouse attached stays in forever, and the
        // whole reason this exists.
        assert!(!shown(None, false));
        assert_eq!(next_change(None, false), None, "and nothing is due");
    }

    #[test]
    fn a_pointer_in_use_is_drawn_and_then_is_not() {
        assert!(shown(Some(Duration::ZERO), false));
        assert!(shown(Some(HOLD - Duration::from_millis(1)), false));
        assert!(!shown(Some(HOLD), false));
        assert!(!shown(Some(HOLD * 10), false));
    }

    #[test]
    fn the_hide_is_scheduled_rather_than_waited_for() {
        // The kiosk loop sleeps on `Demand`; a deadline it is not told about is slept
        // through, and the arrow stays up until something unrelated wakes it.
        assert_eq!(next_change(Some(Duration::ZERO), false), Some(HOLD));
        assert_eq!(
            next_change(Some(HOLD - Duration::from_millis(200)), false),
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            next_change(Some(HOLD), false),
            None,
            "already hidden, nothing further to do"
        );
    }

    #[test]
    fn a_dragging_pointer_keeps_its_cursor_however_long_the_drag_lasts() {
        // `kiosk` synthesizes a held primary button into a touch contact so mouse users
        // get the edge-swipe and the home pill. Hiding the cursor out from under that
        // would take the pointer away mid-gesture.
        assert!(shown(Some(HOLD * 100), true));
        assert_eq!(
            next_change(Some(HOLD * 100), true),
            None,
            "a drag ends with an event, not a timeout"
        );
    }

    #[test]
    fn the_pointer_artwork_rasterizes_at_the_sizes_a_cursor_is_asked_for() {
        for side in [24u32, 32, 48, 64] {
            let (rgba, hotspot) = rasterize(side).expect("the pointer failed to rasterize");
            assert_eq!(rgba.len(), (side * side * 4) as usize);
            assert_eq!(hotspot, (0, 0), "the tip is the origin");
            assert!(
                rgba.chunks(4).any(|px| px[3] > 0),
                "nothing was drawn at {side}"
            );
            // …and it is a *pointer*, not a filled square: the bottom-right corner is
            // outside the arrow and must be transparent, or the hardware cursor is a
            // block covering whatever it is over.
            let last = rgba.chunks(4).next_back().expect("a non-empty pixmap");
            assert_eq!(last[3], 0, "the corner opposite the tip must be clear");
        }
    }
}
