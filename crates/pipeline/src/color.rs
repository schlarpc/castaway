//! YUV → RGB, derived rather than tabulated.
//!
//! Software decode hands the compositor RGBA because swscale already did this conversion.
//! A hardware-decoded surface has not: it is NV12 (or P010) luma+chroma planes, and the
//! conversion moves into the fragment shader. That makes colorimetry *our* problem —
//! [`castaway_core::ColorInfo`] rides along with the surface and lands here.
//!
//! Getting it wrong is the bad kind of wrong: BT.601 coefficients on BT.709 content, or
//! full-range maths on limited-range samples, produce a picture that is merely washed out
//! or oversaturated. Nobody files a bug for that, they just think the panel is cheap. So
//! the matrix is *computed* from the primaries' luma coefficients instead of pasted from
//! a table, and the tests below pin the endpoints (black, white, primaries) that a
//! transcription error would move.
//!
//! Pure maths over metadata — no GPU, so it is unit-tested in every build (ground rule 6).

use castaway_core::{ColorInfo, ColorRange, ColorSpace};

/// The luma coefficients (Kr, Kb) that define a colorspace's YUV↔RGB relationship.
/// Kg is implied: the three sum to 1.
const fn luma_coefficients(space: ColorSpace) -> (f32, f32) {
    match space {
        ColorSpace::Bt601 => (0.299, 0.114),
        ColorSpace::Bt709 => (0.2126, 0.0722),
        ColorSpace::Bt2020Ncl => (0.2627, 0.0593),
        // `ColorSpace` is `#[non_exhaustive]`; a new primary set must not silently render
        // with the wrong matrix, but it also must not fail to compile the whole crate.
        // BT.709 is the safest guess and the tests pin the three we actually name.
        _ => (0.2126, 0.0722),
    }
}

/// What the *stream* said about its matrix, which is very often "nothing".
///
/// Kept separate from [`ColorSpace`] on purpose: `ColorSpace` is a decision, this is
/// evidence. Collapsing them would make "unspecified" indistinguishable from "explicitly
/// BT.709", and the two want different handling when a sender changes resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalledSpace {
    /// The stream carried no matrix coefficients.
    Unspecified,
    /// BT.601 in either of its spellings (BT.470BG / SMPTE 170M).
    Bt601,
    /// BT.709.
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020Ncl,
    /// Something we have no matrix for.
    Unsupported,
}

/// What the stream said about its code range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalledRange {
    /// Not stated.
    Unspecified,
    /// Studio range.
    Limited,
    /// Full range.
    Full,
}

/// Resolve what the stream claimed into what the shader will actually use.
///
/// The resolution heuristic for an unlabelled stream is the industry-standard one: SD
/// content is BT.601, anything HD or larger is BT.709. It matters because senders label
/// inconsistently — AirPlay mirroring and Cast both routinely emit unlabelled H.264 — and
/// picking one constant for everything visibly mistints the other case.
///
/// Range defaults to limited, because video that omits the flag is video.
#[must_use]
pub fn resolve(space: SignalledSpace, range: SignalledRange, height: u32) -> ColorInfo {
    let space = match space {
        SignalledSpace::Bt601 => ColorSpace::Bt601,
        SignalledSpace::Bt709 => ColorSpace::Bt709,
        SignalledSpace::Bt2020Ncl => ColorSpace::Bt2020Ncl,
        // 720 is the conventional SD/HD boundary for this guess.
        SignalledSpace::Unspecified | SignalledSpace::Unsupported => {
            if height >= 720 {
                ColorSpace::Bt709
            } else {
                ColorSpace::Bt601
            }
        }
    };
    let range = match range {
        SignalledRange::Full => ColorRange::Full,
        SignalledRange::Limited | SignalledRange::Unspecified => ColorRange::Limited,
    };
    ColorInfo { space, range }
}

/// A YUV→RGB conversion as the shader consumes it: subtract [`Self::offset`] from the
/// sampled (Y, U, V) triple, then multiply by the 3×3 matrix in [`Self::matrix`].
///
/// Range scaling is folded into the matrix columns rather than applied separately, so the
/// shader is three dot products and nothing else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YuvMatrix {
    /// Row-major 3×3: `rgb = matrix * (yuv - offset)`.
    pub matrix: [[f32; 3]; 3],
    /// Per-channel zero point in normalized 0..=1 sample space.
    pub offset: [f32; 3],
}

impl YuvMatrix {
    /// Derive the conversion for a surface's colorimetry.
    #[must_use]
    pub fn new(info: ColorInfo) -> Self {
        let (kr, kb) = luma_coefficients(info.space);
        let kg = 1.0 - kr - kb;

        // The canonical inverse of the RGB→YUV matrix, in *normalized* space: Y in
        // 0..=1, U and V in -0.5..=0.5.
        //   R = Y             + 2(1-Kr)·V
        //   G = Y - (2Kb(1-Kb)/Kg)·U - (2Kr(1-Kr)/Kg)·V
        //   B = Y + 2(1-Kb)·U
        let base = [
            [1.0, 0.0, 2.0 * (1.0 - kr)],
            [
                1.0,
                -2.0 * kb * (1.0 - kb) / kg,
                -2.0 * kr * (1.0 - kr) / kg,
            ],
            [1.0, 2.0 * (1.0 - kb), 0.0],
        ];

        // Limited ("studio") range packs luma into 16..=235 and chroma into 16..=240 of
        // the 0..=255 code space; full range uses all of it. Expressed as a zero point
        // and a per-channel gain, then folded into the matrix.
        let (offset, gain) = match info.range {
            ColorRange::Limited => (
                [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0],
                [255.0 / 219.0, 255.0 / 224.0, 255.0 / 224.0],
            ),
            ColorRange::Full => ([0.0, 0.5, 0.5], [1.0, 1.0, 1.0]),
        };

        let mut matrix = [[0.0f32; 3]; 3];
        for (row, out) in base.iter().zip(matrix.iter_mut()) {
            for (col, (b, g)) in row.iter().zip(gain.iter()).enumerate() {
                out[col] = b * g;
            }
        }
        Self { matrix, offset }
    }

    /// Apply the conversion on the CPU. The shader does exactly this; having it here in
    /// Rust is what lets the tests assert on colors instead of on matrix entries.
    #[must_use]
    pub fn apply(&self, yuv: [f32; 3]) -> [f32; 3] {
        let centered = [
            yuv[0] - self.offset[0],
            yuv[1] - self.offset[1],
            yuv[2] - self.offset[2],
        ];
        let mut rgb = [0.0f32; 3];
        for (row, out) in self.matrix.iter().zip(rgb.iter_mut()) {
            *out = (row[0] * centered[0] + row[1] * centered[1] + row[2] * centered[2])
                .clamp(0.0, 1.0);
        }
        rgb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8-bit code value as the shader sees it (a `unorm` texture sample).
    fn code(v: f32) -> f32 {
        v / 255.0
    }

    fn assert_close(got: [f32; 3], want: [f32; 3], tol: f32, what: &str) {
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert!(
                (g - w).abs() <= tol,
                "{what}: channel {i} was {g}, expected {w} (±{tol}); full result {got:?}",
            );
        }
    }

    #[test]
    fn limited_range_endpoints_map_to_black_and_white() {
        // Y=16 is video black and Y=235 is video white in every limited-range space. If
        // the 255/219 gain or the 16/255 offset is wrong, black lifts to dark grey or
        // white clips early — the classic "washed out" symptom.
        for space in [ColorSpace::Bt601, ColorSpace::Bt709, ColorSpace::Bt2020Ncl] {
            let m = YuvMatrix::new(ColorInfo {
                space,
                range: ColorRange::Limited,
            });
            assert_close(
                m.apply([code(16.0), code(128.0), code(128.0)]),
                [0.0, 0.0, 0.0],
                0.002,
                "limited black",
            );
            assert_close(
                m.apply([code(235.0), code(128.0), code(128.0)]),
                [1.0, 1.0, 1.0],
                0.002,
                "limited white",
            );
        }
    }

    #[test]
    fn full_range_endpoints_map_to_black_and_white() {
        let m = YuvMatrix::new(ColorInfo {
            space: ColorSpace::Bt709,
            range: ColorRange::Full,
        });
        assert_close(
            m.apply([0.0, 0.5, 0.5]),
            [0.0, 0.0, 0.0],
            0.002,
            "full black",
        );
        assert_close(
            m.apply([1.0, 0.5, 0.5]),
            [1.0, 1.0, 1.0],
            0.002,
            "full white",
        );
    }

    #[test]
    fn bt709_limited_recovers_the_primaries() {
        // Round-trip the other way: encode pure red/green/blue with BT.709's own
        // coefficients, then decode. Anything but the original primary means the matrix
        // and the coefficients disagree.
        let info = ColorInfo {
            space: ColorSpace::Bt709,
            range: ColorRange::Limited,
        };
        let (kr, kb) = luma_coefficients(ColorSpace::Bt709);
        let kg = 1.0 - kr - kb;
        let encode = |rgb: [f32; 3]| {
            let y = kr * rgb[0] + kg * rgb[1] + kb * rgb[2];
            let u = (rgb[2] - y) / (2.0 * (1.0 - kb));
            let v = (rgb[0] - y) / (2.0 * (1.0 - kr));
            // Back into limited-range normalized sample space.
            [
                (16.0 + 219.0 * y) / 255.0,
                (128.0 + 224.0 * u) / 255.0,
                (128.0 + 224.0 * v) / 255.0,
            ]
        };
        let m = YuvMatrix::new(info);
        for primary in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
            assert_close(m.apply(encode(primary)), primary, 0.005, "bt709 primary");
        }
    }

    #[test]
    fn the_spaces_actually_differ() {
        // A guard against the failure where every space silently gets the same matrix:
        // decode one mid-grey-ish chroma sample three ways and require three answers.
        let sample = [code(120.0), code(90.0), code(200.0)];
        let of = |space| {
            YuvMatrix::new(ColorInfo {
                space,
                range: ColorRange::Limited,
            })
            .apply(sample)
        };
        let (a, b, c) = (
            of(ColorSpace::Bt601),
            of(ColorSpace::Bt709),
            of(ColorSpace::Bt2020Ncl),
        );
        assert!(a != b && b != c && a != c, "{a:?} {b:?} {c:?}");
    }

    #[test]
    fn an_unlabelled_stream_is_resolved_by_picture_height() {
        // Senders label inconsistently — unlabelled H.264 is the norm for both AirPlay
        // mirroring and Cast — so this guess runs on most real frames.
        let sd = resolve(
            SignalledSpace::Unspecified,
            SignalledRange::Unspecified,
            480,
        );
        assert_eq!(sd.space, ColorSpace::Bt601);
        let hd = resolve(
            SignalledSpace::Unspecified,
            SignalledRange::Unspecified,
            1080,
        );
        assert_eq!(hd.space, ColorSpace::Bt709);
        // The boundary itself counts as HD.
        let boundary = resolve(
            SignalledSpace::Unspecified,
            SignalledRange::Unspecified,
            720,
        );
        assert_eq!(boundary.space, ColorSpace::Bt709);
    }

    #[test]
    fn an_explicit_label_beats_the_heuristic() {
        // A 480-line stream that says BT.709 means it. Overriding a stated matrix with a
        // size guess would mistint exactly the content that bothered to be correct.
        let got = resolve(SignalledSpace::Bt709, SignalledRange::Full, 480);
        assert_eq!(
            got,
            ColorInfo {
                space: ColorSpace::Bt709,
                range: ColorRange::Full
            }
        );
    }

    #[test]
    fn an_unspecified_range_is_limited() {
        for height in [480, 1080, 2160] {
            let got = resolve(
                SignalledSpace::Unspecified,
                SignalledRange::Unspecified,
                height,
            );
            assert_eq!(got.range, ColorRange::Limited, "height {height}");
        }
    }

    #[test]
    fn an_unsupported_matrix_falls_back_to_the_heuristic() {
        // Better a plausible picture from the size guess than a hard failure on an exotic
        // matrix tag nobody in this building will ever send.
        let got = resolve(SignalledSpace::Unsupported, SignalledRange::Limited, 2160);
        assert_eq!(got.space, ColorSpace::Bt709);
    }

    #[test]
    fn range_changes_the_result() {
        // Same samples, different range interpretation — must not be a no-op, or the
        // limited/full distinction is being dropped on the floor somewhere.
        let sample = [code(100.0), code(140.0), code(110.0)];
        let limited = YuvMatrix::new(ColorInfo {
            space: ColorSpace::Bt709,
            range: ColorRange::Limited,
        })
        .apply(sample);
        let full = YuvMatrix::new(ColorInfo {
            space: ColorSpace::Bt709,
            range: ColorRange::Full,
        })
        .apply(sample);
        assert_ne!(limited, full);
    }
}
