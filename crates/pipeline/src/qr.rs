//! A reusable QR display component for the panel (#248).
//!
//! Two layers, so callers can take either: [`QrMatrix`] is the pure module (a
//! string in, a square grid of dark/light modules out — the QR generator wrapped
//! so nothing else in the tree names the crate), and [`draw`] renders one onto an
//! RGBA buffer with the mandatory quiet zone, snapped to whole pixels so the
//! modules stay crisp.
//!
//! Built for the FCast `fcast://r/…` connection URL first; the Matter
//! commissioning code (`MT:…`) and the remote-control page URL are the reuse this
//! was abstracted for.

use crate::error::PipelineError;
use crate::theme::Rgba;

/// The quiet zone every QR spec mandates, in modules on each side. Four is the
/// minimum a conformant scanner may assume.
pub const QUIET_ZONE: u32 = 4;

/// A rendered QR code as a square grid of modules: `size × size`, row-major,
/// `true` where a module is dark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    size: u32,
    dark: Vec<bool>,
}

impl QrMatrix {
    /// Encode `text` at the lowest error-correction level that still fits it in
    /// one symbol. Low ECC keeps the module count down, which matters on a panel
    /// where a phone reads the code off the glass from across a room — fewer,
    /// larger modules scan from farther away, and the display *is* the tamper-
    /// resistant channel, so the redundancy a damaged printed code needs is
    /// wasted here.
    ///
    /// # Errors
    /// [`PipelineError::InvalidFrame`] if `text` is too long for any QR version
    /// at the low ECC level.
    pub fn encode(text: &str) -> Result<Self, PipelineError> {
        use qrcodegen::{QrCode, QrCodeEcc};
        let code = QrCode::encode_text(text, QrCodeEcc::Low)
            .map_err(|_| PipelineError::InvalidFrame("QR payload too long to encode"))?;
        // `qrcodegen` reports size as i32 in `1..=177`; it is always positive.
        #[allow(clippy::cast_sign_loss)]
        let size = code.size() as u32;
        let mut dark = Vec::with_capacity((size * size) as usize);
        for y in 0..code.size() {
            for x in 0..code.size() {
                dark.push(code.get_module(x, y));
            }
        }
        Ok(Self { size, dark })
    }

    /// The module count per side, excluding the quiet zone.
    #[must_use]
    pub const fn size(&self) -> u32 {
        self.size
    }

    /// The side length in modules including the quiet zone on both sides.
    #[must_use]
    pub const fn size_with_quiet_zone(&self) -> u32 {
        self.size + 2 * QUIET_ZONE
    }

    /// Whether the module at `(x, y)` (excluding the quiet zone) is dark.
    /// Out-of-range coordinates are light, so a caller can index the quiet zone
    /// with negative-shifted coordinates without bounds juggling.
    #[must_use]
    pub fn is_dark(&self, x: u32, y: u32) -> bool {
        if x >= self.size || y >= self.size {
            return false;
        }
        self.dark[(y * self.size + x) as usize]
    }
}

/// Where and how big to draw a QR code.
#[derive(Debug, Clone, Copy)]
pub struct QrStyle {
    /// Top-left corner of the *quiet zone*, in pixels.
    pub x: f32,
    /// Top-left corner of the *quiet zone*, in pixels.
    pub y: f32,
    /// The side length the whole symbol (quiet zone included) should fill, in
    /// pixels. Rounded down to a whole number of pixels per module so modules
    /// never straddle a pixel boundary and blur.
    pub side: f32,
    /// Colour of the dark modules (RGBA).
    pub dark: Rgba,
    /// Colour of the light modules and quiet zone (RGBA). A QR code needs a
    /// light margin to scan, so this is drawn, not left transparent.
    pub light: Rgba,
}

/// The pixel geometry [`draw`] settled on, so a caller can place a caption under
/// the code or centre it in a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QrPlacement {
    /// Pixels per module (integer, ≥ 1).
    pub module_px: u32,
    /// The actual drawn side length in pixels (`module_px × modules`), which is
    /// `≤ style.side`.
    pub side_px: u32,
}

/// Draw `matrix` onto `buf` per `style`. Returns the geometry actually used.
///
/// The whole symbol is snapped to an integer pixels-per-module so the modules
/// are crisp; the result is centred within the requested `side` so the small
/// rounding slack does not bias it to one corner.
///
/// # Errors
/// [`PipelineError::InvalidFrame`] if `side` is too small to give even one pixel
/// per module (the code would be unscannable, so refusing beats drawing mush).
pub fn draw(
    buf: &mut [u8],
    width: u32,
    height: u32,
    matrix: &QrMatrix,
    style: QrStyle,
) -> Result<QrPlacement, PipelineError> {
    let modules = matrix.size_with_quiet_zone();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let module_px = (style.side.max(0.0) as u32) / modules;
    if module_px == 0 {
        return Err(PipelineError::InvalidFrame(
            "QR draw area too small for one pixel per module",
        ));
    }
    let side_px = module_px * modules;
    // Centre the snapped symbol within the requested box.
    let slack = (style.side - side_px as f32) / 2.0;
    let origin_x = style.x + slack.max(0.0);
    let origin_y = style.y + slack.max(0.0);

    // The light background (quiet zone included) as one rectangle, then the dark
    // modules over it — far fewer blends than painting every light module.
    crate::text::fill_rect(
        buf,
        width,
        height,
        origin_x,
        origin_y,
        side_px as f32,
        side_px as f32,
        style.light,
    );
    for my in 0..matrix.size() {
        for mx in 0..matrix.size() {
            if !matrix.is_dark(mx, my) {
                continue;
            }
            let px = origin_x + ((mx + QUIET_ZONE) * module_px) as f32;
            let py = origin_y + ((my + QUIET_ZONE) * module_px) as f32;
            crate::text::fill_rect(
                buf,
                width,
                height,
                px,
                py,
                module_px as f32,
                module_px as f32,
                style.dark,
            );
        }
    }
    Ok(QrPlacement { module_px, side_px })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A known short payload encodes to the smallest QR version (21 modules).
    #[test]
    fn a_short_payload_is_version_1() {
        let matrix = QrMatrix::encode("HELLO").unwrap();
        assert_eq!(matrix.size(), 21);
        assert_eq!(matrix.size_with_quiet_zone(), 29);
        // The three finder patterns' centres are dark (the 3x3 core of each
        // 7x7 finder), a cheap structural sanity check on orientation.
        assert!(matrix.is_dark(3, 3));
        assert!(matrix.is_dark(matrix.size() - 4, 3));
        assert!(matrix.is_dark(3, matrix.size() - 4));
    }

    /// An `fcast://r/…` URL — the payload this was built for — encodes and its
    /// module count is stable enough to size a card around.
    #[test]
    fn an_fcast_connection_url_encodes() {
        let url = "fcast://r/eyJuYW1lIjoiTGl2aW5nIFJvb20iLCJhZGRyZXNzZXMiOlsiMTkyLjE2OC4xLjQyIl0sInNlcnZpY2VzIjpbeyJwb3J0Ijo0Njg5OSwidHlwZSI6MH1dfQ";
        let matrix = QrMatrix::encode(url).unwrap();
        assert!(matrix.size() >= 21 && matrix.size() <= 177);
    }

    /// Out-of-range module coordinates read light, so quiet-zone indexing needs
    /// no bounds juggling.
    #[test]
    fn out_of_range_modules_are_light() {
        let matrix = QrMatrix::encode("x").unwrap();
        assert!(!matrix.is_dark(matrix.size(), 0));
        assert!(!matrix.is_dark(0, matrix.size()));
    }

    /// Drawing snaps to whole pixels per module and centres the slack; a box too
    /// small for one pixel per module is refused rather than blurred.
    #[test]
    fn drawing_snaps_and_centres() {
        let matrix = QrMatrix::encode("HELLO").unwrap();
        let (w, h) = (200u32, 200u32);
        let mut buf = vec![0u8; (w * h * 4) as usize];

        // 200px / 29 modules = 6px per module, 174px drawn, 26px slack → 13 each
        // side.
        let placement = draw(
            &mut buf,
            w,
            h,
            &matrix,
            QrStyle {
                x: 0.0,
                y: 0.0,
                side: 200.0,
                dark: [0, 0, 0, 0xff],
                light: [0xff, 0xff, 0xff, 0xff],
            },
        )
        .unwrap();
        assert_eq!(placement.module_px, 6);
        assert_eq!(placement.side_px, 174);

        // The quiet zone is light: the very first pixel is background, not a
        // dark module.
        let px0 = &buf[0..4];
        assert_eq!(
            px0,
            [0, 0, 0, 0],
            "outside the centred symbol stays untouched"
        );
        // The top-left finder's dark core lands where the geometry predicts:
        // quiet zone (4 modules) + into the finder, at 6px per module, offset by
        // the 13px slack.
        let finder_x = 13 + (QUIET_ZONE + 3) * placement.module_px;
        let finder_y = 13 + (QUIET_ZONE + 3) * placement.module_px;
        let idx = ((finder_y * w + finder_x) * 4) as usize;
        assert_eq!(&buf[idx..idx + 4], [0, 0, 0, 0xff], "finder core is dark");
    }

    #[test]
    fn a_box_too_small_is_refused() {
        let matrix = QrMatrix::encode("HELLO").unwrap();
        let mut buf = vec![0u8; 4];
        assert!(matches!(
            draw(
                &mut buf,
                1,
                1,
                &matrix,
                QrStyle {
                    x: 0.0,
                    y: 0.0,
                    side: 20.0, // 20 / 29 < 1 px per module
                    dark: [0, 0, 0, 0xff],
                    light: [0xff, 0xff, 0xff, 0xff],
                },
            ),
            Err(PipelineError::InvalidFrame(_))
        ));
    }
}
