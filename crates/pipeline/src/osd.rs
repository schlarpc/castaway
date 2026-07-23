//! The render-side OSD consumer: polls the [`castaway_core::OsdReceiver`] (fed by any
//! number of [`castaway_core::OsdSink`]s), rasterizes the current message into a
//! transparent banner, and reports what the render loop should do with the OSD layer.
//! TTL expiry is handled here — a banner with a `ttl` auto-clears.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::Instant;

use castaway_core::{OsdCommand, OsdReceiver};

use crate::error::PipelineError;
use crate::text;

/// What the render loop should do with the OSD layer after a [`OsdController::poll`].
pub enum OsdUpdate {
    /// Replace the OSD layer with this banner image.
    Show {
        /// Banner width.
        width: u32,
        /// Banner height.
        height: u32,
        /// RGBA8 pixels (mostly transparent; the pill + text are opaque).
        rgba: Vec<u8>,
    },
    /// Remove the OSD layer.
    Clear,
    /// Nothing changed this frame.
    Unchanged,
}

/// Consumes OSD commands and renders banners. Lives on the render thread.
pub struct OsdController {
    rx: OsdReceiver,
    width: u32,
    height: u32,
    deadline: Option<Instant>,
}

impl OsdController {
    /// Build a controller reading from `rx`, rendering banners on a `width`×`height`
    /// canvas (scaled to fill the surface by the compositor).
    #[must_use]
    pub fn new(rx: OsdReceiver, width: u32, height: u32) -> Self {
        Self {
            rx,
            width,
            height,
            deadline: None,
        }
    }

    /// Drain pending commands (latest wins) and apply TTL expiry.
    pub fn poll(&mut self, now: Instant) -> OsdUpdate {
        let mut latest = None;
        while let Some(cmd) = self.rx.try_recv() {
            latest = Some(cmd);
        }
        if let Some(cmd) = latest {
            return match cmd {
                OsdCommand::Show(msg) => {
                    self.deadline = msg.ttl.map(|d| now + d);
                    match render_banner(&msg.text, self.width, self.height) {
                        Ok(rgba) => OsdUpdate::Show {
                            width: self.width,
                            height: self.height,
                            rgba,
                        },
                        Err(_) => OsdUpdate::Unchanged,
                    }
                }
                OsdCommand::Clear => {
                    self.deadline = None;
                    OsdUpdate::Clear
                }
            };
        }
        if let Some(deadline) = self.deadline {
            if now >= deadline {
                self.deadline = None;
                return OsdUpdate::Clear;
            }
        }
        OsdUpdate::Unchanged
    }
}

/// Render a bottom-centered banner ("pill" + accent stripe + text) onto a transparent
/// canvas. The compositor alpha-blends it over whatever's below. Public so previews can
/// composite it over the attract scene.
///
/// # Errors
/// [`PipelineError`] if the embedded fonts fail to load.
pub fn render_banner(message: &str, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let f = text::fonts()?;
    let mut buf = vec![0u8; (width * height * 4) as usize]; // transparent

    let s = height as f32 / 720.0;
    let px = 34.0 * s;
    let pad_x = 34.0 * s;
    let pad_y = 16.0 * s;

    let text_w = text::measure(&f.regular, message, px);
    let pill_w = text_w + pad_x * 2.0;
    let pill_h = px + pad_y * 2.0;
    let x = (width as f32 - pill_w) / 2.0;
    let y = height as f32 - pill_h - 60.0 * s;

    // Semi-transparent dark pill + a cyan accent stripe on its left edge.
    text::fill_rect(&mut buf, width, height, x, y, pill_w, pill_h, [0x0a, 0x0e, 0x18, 0xdc]);
    text::fill_rect(&mut buf, width, height, x, y, 6.0 * s, pill_h, [0x4f, 0xd1, 0xc5, 0xff]);

    let baseline = y + pad_y + text::ascent(&f.regular, px) * 0.9;
    text::draw_text(
        &mut buf,
        width,
        height,
        x + pad_x,
        baseline,
        message,
        px,
        [0xf2, 0xf5, 0xfa, 0xff],
        &f.regular,
    );
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use super::*;
    use castaway_core::{osd_channel, OsdMessage};

    #[test]
    fn show_then_ttl_expiry_clears() {
        let (sink, rx) = osd_channel();
        let mut ctrl = OsdController::new(rx, 640, 360);
        let t0 = Instant::now();

        // Nothing yet.
        assert!(matches!(ctrl.poll(t0), OsdUpdate::Unchanged));

        // A 2s banner → Show, with visible (opaque) pixels.
        sink.banner("Now casting from cast/abc", Duration::from_secs(2));
        match ctrl.poll(t0) {
            OsdUpdate::Show { rgba, .. } => {
                assert!(rgba.chunks_exact(4).any(|p| p[3] > 200), "banner has opaque pixels");
            }
            _ => panic!("expected Show"),
        }

        // Before the deadline: unchanged.
        assert!(matches!(
            ctrl.poll(t0 + Duration::from_secs(1)),
            OsdUpdate::Unchanged
        ));
        // After the deadline: auto-clear.
        assert!(matches!(
            ctrl.poll(t0 + Duration::from_secs(3)),
            OsdUpdate::Clear
        ));
    }

    #[test]
    fn explicit_clear() {
        let (sink, rx) = osd_channel();
        let mut ctrl = OsdController::new(rx, 320, 180);
        sink.show(OsdMessage::sticky("stuck"));
        assert!(matches!(ctrl.poll(Instant::now()), OsdUpdate::Show { .. }));
        sink.clear();
        assert!(matches!(ctrl.poll(Instant::now()), OsdUpdate::Clear));
    }
}
