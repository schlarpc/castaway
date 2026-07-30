//! The browser surface. The real backend is the Electron subprocess in offscreen
//! rendering mode (behind the `electron` feature — see `electron_browser` and D36): it
//! renders a page into shared GPU textures fed into the compositor as one of the two
//! browser layers, doubling as PiP and the YouTube Lounge playback surface
//! (architecture §5).
//!
//! Here we define the trait + a [`NullBrowser`] stub used when `electron` is off (the
//! Lounge path then falls back to a headless player — see OPEN-QUESTIONS Q6).

// The browser's *geometry* is expressed in the attract scene's types, so it exists
// only where that scene does. `BrowserCommand` above does not, and must not: `app`
// drives navigation in builds with no renderer at all.
#[cfg(feature = "render")]
use crate::attract::{InsetRect, WidgetSlot};
#[cfg(feature = "render")]
use crate::compositor::Transform;

use tracing::info;

/// Controls the offscreen browser surface.
pub trait BrowserSurface: Send {
    /// Navigate to a URL (e.g. YouTube's TV surface, or a video watch page).
    fn load_url(&mut self, url: &str);
    /// Resize the offscreen surface.
    fn resize(&mut self, width: u32, height: u32);
    /// Whether a real (rendering) browser is present. `false` for the null stub.
    fn is_real(&self) -> bool;
}

/// A stub browser used when the `electron` feature is off. It records the last URL so the
/// app can fall back to a headless player for that content.
#[derive(Default)]
pub struct NullBrowser {
    last_url: Option<String>,
    size: (u32, u32),
}

impl NullBrowser {
    /// The most recently requested URL, if any (what a headless fallback would play).
    #[must_use]
    pub fn last_url(&self) -> Option<&str> {
        self.last_url.as_deref()
    }
}

impl BrowserSurface for NullBrowser {
    fn load_url(&mut self, url: &str) {
        info!(%url, "null browser: load (headless fallback would handle this)");
        self.last_url = Some(url.to_string());
    }
    fn resize(&mut self, width: u32, height: u32) {
        self.size = (width, height);
    }
    fn is_real(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Browser geometry and commands.
//
// Lifted out of the CEF host during the Electron port (D36) because none of it was ever
// CEF-specific: which role the browser is playing, what rectangle that puts it in, and
// how a panel coordinate maps into that rectangle are the same questions whatever is
// doing the rendering. Keeping them here means the port replaced a *runtime* and left
// the geometry — and its tests — untouched.
// ---------------------------------------------------------------------------

/// A command sent from the tokio side (e.g. a DIAL launch) to the main-thread browser.
pub enum BrowserCommand {
    /// Show the browser fullscreen, navigating to `url` (the offscreen browser is
    /// created on first use).
    Navigate(String),
    /// Give the panel back: return to the idle widget if one is configured, else close
    /// the browser and drop its compositor layer (e.g. DIAL stop).
    Hide,
}

#[cfg(feature = "render")]
/// A placement an offscreen browser window can occupy on the panel.
///
/// Formerly the mode flag of the *one* browser; since the two-window split
/// (`browser_proto::Surface`) it names a viewport-and-layer, not a browser. The widget
/// window always occupies [`Self::AttractWidget`]; the page window occupies
/// [`Self::Fullscreen`] normally and [`Self::AttractWidget`] while minimized into the
/// home screen's slot — where it outranks the widget, whose paints are dropped for the
/// duration (one slot, one occupant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRole {
    /// The idle widget slot (the clock's card) in the attract scene, *below* the video
    /// layer so a starting cast simply covers it.
    AttractWidget,
    /// A cast surface (YouTube leanback): fills the panel, above the video layer.
    Fullscreen,
}

#[cfg(feature = "render")]
/// Where a role's browser lives on a `surface`-sized panel: the offscreen viewport the
/// browser rasterizes into (device pixels — the page lays itself out at the size it will actually
/// be shown, instead of a small render upscaled) and the layer that viewport maps onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserView {
    /// Viewport rect in device pixels.
    pub rect: InsetRect,
    /// Layer placement: `rect` normalized onto the surface, one texel per device pixel.
    pub transform: Transform,
    /// Which compositor layer the viewport maps onto — and therefore how deep it sits.
    pub layer: crate::compositor::LayerId,
}

#[cfg(feature = "render")]
impl BrowserRole {
    /// Viewport + layer placement for this role on a `surface`-sized panel.
    #[must_use]
    pub fn view(self, surface: (u32, u32)) -> BrowserView {
        let (w, h) = (surface.0.max(1), surface.1.max(1));
        let full = InsetRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        // The widget's rect comes from the attract renderer, which draws the card frame
        // from the same call — the two cannot drift. Falling back to fullscreen keeps a
        // slot that reserves nothing from producing a zero-sized viewport.
        let rect = match self {
            Self::AttractWidget => WidgetSlot::RightCard.rect(w, h).unwrap_or(full),
            Self::Fullscreen => full,
        };
        BrowserView {
            rect,
            transform: rect.transform(w, h),
            // The two roles are two *layers*, not one layer at two depths — which is what
            // they used to be, colliding with the now-playing card (D38). The widget sits
            // between the idle background and video, so a cast simply covers it with no
            // explicit "hide the clock" step.
            layer: match self {
                Self::AttractWidget => crate::compositor::LayerId::BrowserWidget,
                Self::Fullscreen => crate::compositor::LayerId::BrowserFullscreen,
            },
        }
    }
}

#[cfg(feature = "render")]
/// Map window-normalized coordinates into a browser view-space pixel position, clamped
/// into the viewport.
///
/// Clamping is right for a contact the browser already owns: a drag that wanders off the
/// card still needs coherent moves and an end, and the alternative is a page that thinks
/// a finger is down forever.
///
/// It is *wrong* for deciding whether a contact belongs to the browser at all — clamping
/// answers "yes" for every point on the panel. Use [`hit_view_px`] for that.
///
/// Free function (not a method) so the inset mapping is testable without a live browser.
#[must_use]
pub fn to_view_px(rect: InsetRect, surface: (u32, u32), x: f32, y: f32) -> (f32, f32) {
    let vx = (x * surface.0.max(1) as f32 - rect.x as f32).clamp(0.0, rect.width as f32);
    let vy = (y * surface.1.max(1) as f32 - rect.y as f32).clamp(0.0, rect.height as f32);
    (vx, vy)
}

#[cfg(feature = "render")]
/// Map window-normalized coordinates into the viewport, or `None` if the point is
/// outside it.
///
/// This is the one to use when deciding whether an input belongs to the browser. When the
/// browser is the idle screen's clock card it occupies a corner of a 65-inch panel, and
/// clamping instead of rejecting delivered a touch *anywhere on the glass* into that
/// card. `proto-miracast`'s `map_from_panel` has always done it this way.
#[must_use]
pub fn hit_view_px(rect: InsetRect, surface: (u32, u32), x: f32, y: f32) -> Option<(f32, f32)> {
    let px = x * surface.0.max(1) as f32;
    let py = y * surface.1.max(1) as f32;
    let vx = px - rect.x as f32;
    let vy = py - rect.y as f32;
    (vx >= 0.0 && vy >= 0.0 && vx <= rect.width as f32 && vy <= rect.height as f32)
        .then_some((vx, vy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_browser_records_url_and_is_not_real() {
        let mut b = NullBrowser::default();
        b.resize(1920, 1080);
        b.load_url("https://www.youtube.com/tv");
        assert_eq!(b.last_url(), Some("https://www.youtube.com/tv"));
        assert!(!b.is_real());
    }

    #[cfg(feature = "render")]
    #[test]
    fn a_touch_outside_the_widget_card_is_rejected_not_squashed_into_it() {
        // The bug this is here for: on the idle screen the browser is a clock card in a
        // corner, and the clamping mapper answered "yes, at the nearest edge" for a touch
        // anywhere on a 65-inch panel — so tapping the far side of the room's screen
        // poked the clock.
        let surface = (3840, 2160);
        let rect = super::super::attract::WidgetSlot::RightCard
            .rect(surface.0, surface.1)
            .expect("the right card reserves a rect");

        // Bottom-left corner of the panel, nowhere near the top-right card.
        assert_eq!(hit_view_px(rect, surface, 0.05, 0.95), None);
        // The clamping mapper is what used to decide this, and still says yes — which is
        // correct for its job and wrong for this one.
        let clamped = to_view_px(rect, surface, 0.05, 0.95);
        assert!(clamped.0 >= 0.0 && clamped.1 >= 0.0);

        // A point actually inside the card maps to the same place either way.
        let inside_x = (rect.x as f32 + rect.width as f32 / 2.0) / surface.0 as f32;
        let inside_y = (rect.y as f32 + rect.height as f32 / 2.0) / surface.1 as f32;
        let hit = hit_view_px(rect, surface, inside_x, inside_y).expect("inside the card");
        let clamped = to_view_px(rect, surface, inside_x, inside_y);
        assert!((hit.0 - clamped.0).abs() < 0.5 && (hit.1 - clamped.1).abs() < 0.5);
    }

    #[cfg(feature = "render")]
    #[test]
    fn a_fullscreen_browser_accepts_the_whole_panel() {
        // The role that must not change behaviour: its rect is the surface, so every
        // point is inside it and nothing is newly dropped.
        let surface = (1920, 1080);
        let rect = BrowserRole::Fullscreen.view(surface).rect;
        for (x, y) in [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0), (0.01, 0.99)] {
            assert!(
                hit_view_px(rect, surface, x, y).is_some(),
                "fullscreen must accept ({x}, {y})"
            );
        }
    }
}
