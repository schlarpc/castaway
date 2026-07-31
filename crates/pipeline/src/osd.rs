//! The render-side OSD consumer: polls the [`castaway_core::OsdReceiver`] (fed by any
//! number of [`castaway_core::OsdSink`]s), rasterizes the current message into a banner,
//! and reports what the render loop should do with the OSD layer. TTL expiry is handled
//! here — a banner with a `ttl` auto-clears.
//!
//! The banner is a **tight texture placed by a transform**, not a full-surface canvas: it
//! is rasterized at device scale and mapped one texel per device pixel, so the text is as
//! crisp as the panel allows. The surface size is a parameter of [`OsdController::poll`]
//! rather than state on the controller — that way it cannot go stale behind a window
//! resize, and there is no second code path to forget to update.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::Instant;

use castaway_core::{OsdCommand, OsdReceiver};

use crate::compositor::Transform;
use crate::error::PipelineError;
use crate::text;

/// The design resolution the banner's proportions are authored against; every dimension
/// is scaled from it, so the layout is identical at 720p and 4K but rasterized natively.
const DESIGN_HEIGHT: f32 = 720.0;

/// A rasterized banner: a tight RGBA image plus where it goes on the surface.
pub struct Banner {
    /// Image width in device pixels.
    pub width: u32,
    /// Image height in device pixels.
    pub height: u32,
    /// RGBA8 pixels, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Placement in normalized surface coords, chosen so the quad lands on exact device
    /// pixel boundaries — one texel per pixel, nothing for the sampler to interpolate.
    pub transform: Transform,
}

/// What the render loop should do with the OSD layer after a [`OsdController::poll`].
pub enum OsdUpdate {
    /// Replace the OSD layer with this banner.
    Show(Banner),
    /// Remove the OSD layer.
    Clear,
    /// Nothing changed this frame.
    Unchanged,
}

/// The banner currently on screen: enough to re-rasterize it if the surface changes size.
struct Showing {
    text: String,
    deadline: Option<Instant>,
    /// Surface size this banner was rasterized for. A mismatch means re-render.
    rendered_for: (u32, u32),
}

/// Consumes OSD commands and renders banners. Lives on the render thread.
pub struct OsdController {
    rx: OsdReceiver,
    showing: Option<Showing>,
}

impl OsdController {
    /// Build a controller reading from `rx`. Banner size comes from the surface size
    /// passed to [`Self::poll`], so there is nothing to configure here.
    #[must_use]
    pub fn new(rx: OsdReceiver) -> Self {
        Self { rx, showing: None }
    }

    /// The waker the channel's sinks share, for the kiosk loop to arm (Q48): a banner
    /// posted while the panel sleeps has to wake it to be seen.
    #[must_use]
    pub fn waker(&self) -> castaway_core::Waker {
        self.rx.waker()
    }

    /// When the banner on screen next changes by itself: its TTL expiry, if it has one.
    /// New messages are the sender's business — they arrive with a wake.
    #[must_use]
    pub fn next_change(&self) -> Option<Instant> {
        self.showing.as_ref().and_then(|s| s.deadline)
    }

    /// Drain pending commands (latest wins), apply TTL expiry, and re-rasterize if
    /// `surface` changed since the current banner was drawn.
    pub fn poll(&mut self, now: Instant, surface: (u32, u32)) -> OsdUpdate {
        let mut latest = None;
        while let Some(cmd) = self.rx.try_recv() {
            latest = Some(cmd);
        }

        match latest {
            Some(OsdCommand::Clear) => {
                self.showing = None;
                return OsdUpdate::Clear;
            }
            Some(OsdCommand::Show(msg)) => {
                self.showing = Some(Showing {
                    text: msg.text,
                    deadline: msg.ttl.map(|d| now + d),
                    rendered_for: surface,
                });
            }
            None => match &self.showing {
                None => return OsdUpdate::Unchanged,
                Some(showing) => {
                    if showing.deadline.is_some_and(|deadline| now >= deadline) {
                        self.showing = None;
                        return OsdUpdate::Clear;
                    }
                    // The banner already on the layer is still correct unless the surface
                    // resized under us, in which case its scale is now wrong.
                    if showing.rendered_for == surface {
                        return OsdUpdate::Unchanged;
                    }
                }
            },
        }

        let Some(showing) = &mut self.showing else {
            return OsdUpdate::Unchanged;
        };
        // Recorded before rendering: on failure this stops the loop retrying every frame.
        showing.rendered_for = surface;
        match render_banner(&showing.text, surface.0, surface.1) {
            Ok(banner) => OsdUpdate::Show(banner),
            Err(_) => {
                self.showing = None;
                OsdUpdate::Clear
            }
        }
    }
}

/// Rasterize a bottom-centered banner ("pill" + accent stripe + text) sized for a
/// `surface_width`×`surface_height` surface. The image is only as big as the pill; the
/// returned [`Banner::transform`] positions it. Public so previews can composite it over
/// the attract scene.
///
/// # Errors
/// [`PipelineError`] if the embedded fonts fail to load.
pub fn render_banner(
    message: &str,
    surface_width: u32,
    surface_height: u32,
) -> Result<Banner, PipelineError> {
    // A zero-sized surface would divide by zero in the transform below and put NaN in a
    // uniform buffer, which is a much worse failure than an off-screen 1px banner.
    let surface_width = surface_width.max(1);
    let surface_height = surface_height.max(1);

    let f = text::fonts()?;
    let s = surface_height as f32 / DESIGN_HEIGHT;
    let px = 34.0 * s;
    let pad_x = 34.0 * s;
    let pad_y = 16.0 * s;
    let stripe_w = 6.0 * s;
    let margin_bottom = 60.0 * s;

    // Whole device pixels, because the transform maps this texture 1:1 onto the surface:
    // a fractional size or offset would put the quad edges between pixel centers and the
    // linear sampler would resample every glyph edge — the exact blur this avoids.
    let text_w = text::measure(&f.regular, message, px);
    let width = ((text_w + pad_x * 2.0).ceil() as u32)
        .clamp(1, surface_width)
        .max(1);
    let height = ((px + pad_y * 2.0).ceil() as u32)
        .clamp(1, surface_height)
        .max(1);

    // Centered horizontally, `margin_bottom` up from the bottom edge; saturating so a
    // banner taller than the surface pins to the top instead of wrapping around.
    let x = (surface_width - width) / 2;
    let y = surface_height.saturating_sub(height.saturating_add(margin_bottom as u32));

    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    let (w, h) = (width as f32, height as f32);

    // Semi-transparent dark pill + a cyan accent stripe on its left edge.
    text::fill_rect(
        &mut rgba,
        width,
        height,
        0.0,
        0.0,
        w,
        h,
        [0x0a, 0x0e, 0x18, 0xdc],
    );
    text::fill_rect(
        &mut rgba,
        width,
        height,
        0.0,
        0.0,
        stripe_w,
        h,
        [0x4f, 0xd1, 0xc5, 0xff],
    );

    let baseline = pad_y + text::ascent(&f.regular, px) * 0.9;
    text::draw_text(
        &mut rgba,
        width,
        height,
        pad_x,
        baseline,
        message,
        px,
        [0xf2, 0xf5, 0xfa, 0xff],
        &f.regular,
    );

    Ok(Banner {
        width,
        height,
        rgba,
        transform: Transform {
            scale_x: width as f32 / surface_width as f32,
            scale_y: height as f32 / surface_height as f32,
            offset_x: x as f32 / surface_width as f32,
            offset_y: y as f32 / surface_height as f32,
        },
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use super::*;
    use castaway_core::{osd_channel, OsdMessage};

    const UHD: (u32, u32) = (3840, 2160);
    const HD: (u32, u32) = (1280, 720);

    fn banner(update: OsdUpdate) -> Banner {
        match update {
            OsdUpdate::Show(b) => b,
            _ => panic!("expected Show"),
        }
    }

    #[test]
    fn show_then_ttl_expiry_clears() {
        let (sink, rx) = osd_channel();
        let mut ctrl = OsdController::new(rx);
        let t0 = Instant::now();

        // Nothing yet.
        assert!(matches!(ctrl.poll(t0, HD), OsdUpdate::Unchanged));

        // A 2s banner → Show, with visible (opaque) pixels.
        sink.banner("Now casting from cast/abc", Duration::from_secs(2));
        let b = banner(ctrl.poll(t0, HD));
        assert!(
            b.rgba.chunks_exact(4).any(|p| p[3] > 200),
            "banner has opaque pixels"
        );

        // Before the deadline, at an unchanged size: nothing to do.
        assert!(matches!(
            ctrl.poll(t0 + Duration::from_secs(1), HD),
            OsdUpdate::Unchanged
        ));
        // After the deadline: auto-clear.
        assert!(matches!(
            ctrl.poll(t0 + Duration::from_secs(3), HD),
            OsdUpdate::Clear
        ));
    }

    #[test]
    fn explicit_clear() {
        let (sink, rx) = osd_channel();
        let mut ctrl = OsdController::new(rx);
        sink.show(OsdMessage::sticky("stuck"));
        assert!(matches!(ctrl.poll(Instant::now(), HD), OsdUpdate::Show(_)));
        sink.clear();
        assert!(matches!(ctrl.poll(Instant::now(), HD), OsdUpdate::Clear));
    }

    #[test]
    fn banner_is_a_tight_texture_not_a_full_surface_canvas() {
        let b = render_banner("Starting YouTube", UHD.0, UHD.1).unwrap();
        assert!(
            b.width < UHD.0 && b.height < UHD.1 / 4,
            "pill should be a small strip, got {}x{}",
            b.width,
            b.height
        );
        assert_eq!(b.rgba.len(), b.width as usize * b.height as usize * 4);

        // Bottom-centered, fully on screen.
        let center = b.transform.offset_x + b.transform.scale_x / 2.0;
        assert!((center - 0.5).abs() < 0.01, "not centered: {center}");
        assert!(b.transform.offset_y > 0.5, "should sit in the lower half");
        assert!(
            b.transform.offset_y + b.transform.scale_y < 1.0,
            "off-screen"
        );
    }

    /// The regression test for the blurry-OSD bug: the pill must be *rasterized* bigger on
    /// a bigger surface, not drawn once and stretched.
    #[test]
    fn rasterizes_at_device_scale_rather_than_upscaling() {
        let hd = render_banner("Starting YouTube", HD.0, HD.1).unwrap();
        let uhd = render_banner("Starting YouTube", UHD.0, UHD.1).unwrap();

        let ratio = f64::from(uhd.height) / f64::from(hd.height);
        assert!(
            (ratio - 3.0).abs() < 0.05,
            "4K banner should be ~3x the 720p one, got {ratio}"
        );
        // Same layout either way: the pill occupies the same fraction of the surface.
        assert!((uhd.transform.scale_y - hd.transform.scale_y).abs() < 0.005);
    }

    /// One texel per device pixel — otherwise the sampler interpolates and we're back to
    /// soft glyph edges even at the right resolution.
    #[test]
    fn transform_maps_texels_onto_whole_device_pixels() {
        for surface in [HD, UHD, (1920, 1080), (2560, 1440)] {
            let b = render_banner("Now casting", surface.0, surface.1).unwrap();
            let drawn_w = b.transform.scale_x * surface.0 as f32;
            let drawn_h = b.transform.scale_y * surface.1 as f32;
            // Sub-pixel tolerance, not f32::EPSILON: normalizing and re-multiplying by a
            // 4K dimension loses far more than one ulp, and what matters is that the quad
            // lands on the pixel grid.
            assert!(
                (drawn_w - b.width as f32).abs() < 0.01,
                "{surface:?}: {drawn_w} px on screen vs {} texels",
                b.width
            );
            assert!((drawn_h - b.height as f32).abs() < 0.01);
            // And the origin lands on a pixel boundary too.
            let origin_x = b.transform.offset_x * surface.0 as f32;
            assert!((origin_x - origin_x.round()).abs() < 0.01);
        }
    }

    #[test]
    fn surface_resize_rerenders_the_current_banner() {
        let (sink, rx) = osd_channel();
        let mut ctrl = OsdController::new(rx);
        let t0 = Instant::now();

        sink.show(OsdMessage::sticky("Starting YouTube"));
        let small = banner(ctrl.poll(t0, HD));
        // Same size again: no work.
        assert!(matches!(ctrl.poll(t0, HD), OsdUpdate::Unchanged));
        // Window moved to the 4K panel: re-rasterized larger, without a new command.
        let big = banner(ctrl.poll(t0, UHD));
        assert!(big.height > small.height * 2);
        // ...and then settles again.
        assert!(matches!(ctrl.poll(t0, UHD), OsdUpdate::Unchanged));
    }

    #[test]
    fn degenerate_surface_does_not_produce_a_nan_transform() {
        let b = render_banner("hi", 0, 0).unwrap();
        assert!(b.transform.scale_x.is_finite() && b.transform.scale_y.is_finite());
        assert!(b.transform.offset_x.is_finite() && b.transform.offset_y.is_finite());
        assert!(b.width >= 1 && b.height >= 1);
    }

    /// A message far too long for the surface must clip, not allocate past it or wrap the
    /// centering arithmetic.
    #[test]
    fn overlong_message_clamps_to_the_surface() {
        let b = render_banner(&"wide ".repeat(400), 1280, 720).unwrap();
        assert_eq!(b.width, 1280);
        assert!(b.transform.scale_x <= 1.0 && b.transform.offset_x >= 0.0);
    }
}
