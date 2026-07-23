//! The browser surface. A real backend is CEF in offscreen-rendering mode (behind the
//! `cef` feature): it renders a page to a pixel buffer (`OnPaint`) fed into the
//! compositor as the [`crate::compositor::LayerId::Browser`] layer, doubling as PiP and
//! the YouTube Lounge playback surface (architecture §5). MVP is the CPU `OnPaint` path;
//! GPU shared-texture OSR is aspirational (cef#4057/#3730).
//!
//! Here we define the trait + a [`NullBrowser`] stub used when `cef` is off (the Lounge
//! path then falls back to a headless player — see OPEN-QUESTIONS Q6).

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
