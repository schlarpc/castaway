//! The browser surface. A real backend is CEF in offscreen-rendering mode (behind the
//! `cef` feature): it renders a page to a pixel buffer (`OnPaint`) fed into the
//! compositor as the [`crate::compositor::LayerId::Browser`] layer, doubling as PiP and
//! the YouTube Lounge playback surface (architecture §5). MVP is the CPU `OnPaint` path;
//! GPU shared-texture OSR is aspirational (cef#4057/#3730).
//!
//! Here we define the trait + a [`NullBrowser`] stub used when `cef` is off (the Lounge
//! path then falls back to a headless player — see OPEN-QUESTIONS Q6).

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

/// A stub browser used when the `cef` feature is off. It records the last URL so the
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
/// What the one offscreen browser is currently for. There is exactly one browser, so its two
/// uses are mutually exclusive by construction: a cast takes the panel over, and
/// dismissing it hands the screen back to the idle widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRole {
    /// The idle web widget (the clock) painting into the attract scene's reserved card,
    /// *below* the video layer so a starting cast simply covers it.
    AttractWidget,
    /// A cast surface (YouTube leanback): fills the panel, above the video layer.
    Fullscreen,
}

#[cfg(feature = "render")]
/// Where a role's browser lives on a `surface`-sized panel: the offscreen viewport CEF
/// rasterizes into (device pixels — the page lays itself out at the size it will actually
/// be shown, instead of a small render upscaled) and the layer that viewport maps onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserView {
    /// Viewport rect in device pixels.
    pub rect: InsetRect,
    /// Layer placement: `rect` normalized onto the surface, one texel per device pixel.
    pub transform: Transform,
    /// Layer depth in the compositor stack.
    pub z: i32,
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
            // Attract is -10 and video is 0, so the idle widget sits between them: no
            // explicit "hide the clock" step, a cast just covers it.
            z: match self {
                Self::AttractWidget => -5,
                Self::Fullscreen => 5,
            },
        }
    }
}

#[cfg(feature = "render")]
/// Map window-normalized coordinates into a browser view-space pixel position. Free
/// function (not a method) so the inset mapping is testable without a live CEF instance.
pub fn to_view_px(rect: InsetRect, surface: (u32, u32), x: f32, y: f32) -> (f32, f32) {
    // Clamped into the viewport rather than dropped when outside: the browser still
    // needs the move that leaves the card to end a hover or a drag.
    let vx = (x * surface.0.max(1) as f32 - rect.x as f32).clamp(0.0, rect.width as f32);
    let vy = (y * surface.1.max(1) as f32 - rect.y as f32).clamp(0.0, rect.height as f32);
    (vx, vy)
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
}
