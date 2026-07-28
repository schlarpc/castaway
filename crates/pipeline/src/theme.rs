//! One palette, for every surface the panel draws (#24).
//!
//! Before this, each surface carried a private `Palette` with its own hand-picked
//! near-blacks and near-whites, and they had already drifted — the idle screen's card
//! edge was `0x22_2d_44` in one place and `0x1e_2a_44` in another, which is a difference
//! nobody chose. Colour is a property of the *panel*, not of whichever module happens to
//! be drawing.
//!
//! The accents are dma.space's own, taken from the site's CSS custom properties and
//! recorded with their provenance in `assets/brand/README.md`.
//!
//! The neutrals are not from the site: it is a web page on white, and this is a 65-inch
//! emissive panel on a wall in a dim room. A background lifted from the page would be a
//! lamp. These are the dark ramp the panel already used, kept.

use crate::text::Rgba;

/// dma.space blue — the site's default brand colour and its link colour.
pub const BLUE: Rgba = [0x02, 0xab, 0xfc, 0xff];
/// dma.space green.
pub const GREEN: Rgba = [0x56, 0xba, 0x5b, 0xff];
/// dma.space coral. The one accent that means bad news.
pub const CORAL: Rgba = [0xf5, 0x61, 0x5f, 0xff];
/// dma.space gold.
///
/// Authored on the site as `oklch(0.7079 0.1638 82.58)`, which is outside sRGB; this is
/// the gamut-clamped conversion and is duller than intended. If the panel ever renders
/// wide-gamut, go back to the oklch value rather than this.
pub const GOLD: Rgba = [0xd2, 0x94, 0x00, 0xff];

/// Top of the background ramp.
pub const BG_TOP: Rgba = [0x0d, 0x14, 0x28, 0xff];
/// Bottom of the background ramp. Not pure black: the gradient is dithered, and a ramp
/// that bottoms out at zero has nowhere left to dither.
pub const BG_BOTTOM: Rgba = [0x03, 0x05, 0x0b, 0xff];

/// A raised surface — a tile, a list row, the plate behind a name.
pub const PLATE: Rgba = [0x15, 0x1e, 0x35, 0xff];
/// The edge of a framed area.
pub const EDGE: Rgba = [0x22, 0x2d, 0x44, 0xff];
/// The inside of a frame nothing has painted into yet.
pub const WELL: Rgba = [0x02, 0x03, 0x07, 0xff];

/// Primary text.
pub const TEXT: Rgba = [0xff, 0xff, 0xff, 0xff];
/// Body text on a plate.
pub const TEXT_BODY: Rgba = [0xe8, 0xec, 0xf4, 0xff];
/// Secondary text — details, subtitles, the second line of a row.
pub const TEXT_DIM: Rgba = [0x9a, 0xa4, 0xb8, 0xff];
/// Tertiary text — footers, step numbers, labels above a value.
pub const TEXT_FAINT: Rgba = [0x55, 0x5e, 0x72, 0xff];

/// The teal the tagline and subtitles use.
///
/// Kept from the panel's own palette rather than replaced with a brand colour: it is the
/// one hue that reads as "this line is telling you something" without competing with the
/// service accents, all of which are brand colours belonging to somebody else.
pub const ACCENT: Rgba = [0x4f, 0xd1, 0xc5, 0xff];

/// A seasonal accent, replacing the usual one for part of the year (#24).
///
/// Pure and date-driven so it is testable without waiting for June: the panel asks what
/// today is, and everything else follows from that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Season {
    /// What to call it, for a log line and for anyone wondering why the screen changed.
    pub name: &'static str,
    /// The hues the season is made of, top to bottom.
    ///
    /// Not drawn as-is: [`Self::gradient`] mixes them most of the way into the panel's
    /// own dark ramp first. A flag at full strength is a lightbox, and everything on
    /// these screens is white text.
    pub hues: &'static [Rgba],
}

/// How much of a season's hue survives the mix into the background. Low on purpose: the
/// point is a room that feels different, not a screen nobody can read.
const SEASON_STRENGTH: f32 = 0.22;

impl Season {
    /// The background ramp for this season: its hues, each pulled most of the way toward
    /// the panel's own dark gradient at the height it sits.
    #[must_use]
    pub fn gradient(self) -> Vec<Rgba> {
        let n = self.hues.len().max(1);
        self.hues
            .iter()
            .enumerate()
            .map(|(i, hue)| {
                // Keep the vertical fall of the normal background, so a seasonal screen
                // is still darker at the bottom rather than uniformly tinted.
                let t = i as f32 / (n.saturating_sub(1).max(1)) as f32;
                let base = [
                    mix(BG_TOP[0], BG_BOTTOM[0], t),
                    mix(BG_TOP[1], BG_BOTTOM[1], t),
                    mix(BG_TOP[2], BG_BOTTOM[2], t),
                ];
                [
                    mix(base[0], hue[0], SEASON_STRENGTH),
                    mix(base[1], hue[1], SEASON_STRENGTH),
                    mix(base[2], hue[2], SEASON_STRENGTH),
                    0xff,
                ]
            })
            .collect()
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mix(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t.clamp(0.0, 1.0)).clamp(0.0, 255.0) as u8
}

/// Pride, in the six-colour flag.
const PRIDE: [Rgba; 6] = [
    [0xe4, 0x03, 0x03, 0xff],
    [0xff, 0x8c, 0x00, 0xff],
    [0xff, 0xed, 0x00, 0xff],
    [0x00, 0x80, 0x26, 0xff],
    [0x00, 0x4d, 0xff, 0xff],
    [0x75, 0x07, 0x87, 0xff],
];

/// Christmas.
const YULE: [Rgba; 3] = [
    [0xd6, 0x1f, 0x26, 0xff],
    [0xf5, 0xf5, 0xf5, 0xff],
    [0x1a, 0x7a, 0x3c, 0xff],
];

/// Halloween.
const HALLOWEEN: [Rgba; 2] = [[0xff, 0x77, 0x18, 0xff], [0x6a, 0x2d, 0x8f, 0xff]];

/// What season `(month, day)` falls in, if any.
///
/// Deliberately a *stripe* rather than a repaint. The screens have to stay legible and
/// the service tiles already carry other people's brand colours; a seasonal palette
/// fighting those would make the panel harder to read for a joke that lands once a year.
#[must_use]
pub const fn season(month: u32, day: u32) -> Option<Season> {
    match (month, day) {
        (6, _) => Some(Season {
            name: "pride",
            hues: &PRIDE,
        }),
        // The run-up, not the day: a decoration that appears on the 25th has missed it.
        (12, 1..=26) => Some(Season {
            name: "christmas",
            hues: &YULE,
        }),
        (10, 24..=31) => Some(Season {
            name: "halloween",
            hues: &HALLOWEEN,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seasons_land_where_they_should_and_nowhere_else() {
        assert_eq!(season(6, 1).map(|s| s.name), Some("pride"));
        assert_eq!(season(6, 30).map(|s| s.name), Some("pride"));
        assert_eq!(season(12, 20).map(|s| s.name), Some("christmas"));
        assert_eq!(season(10, 31).map(|s| s.name), Some("halloween"));
        // An ordinary Tuesday gets an ordinary screen.
        assert_eq!(season(3, 14), None);
        assert_eq!(season(10, 1), None);
        assert_eq!(season(12, 31), None);
    }

    #[test]
    fn a_seasonal_background_stays_dark_enough_to_read_white_text_on() {
        // The whole risk of tinting the background: a flag at full strength is a
        // lightbox, and every screen here is white text on this ramp.
        for (m, d) in [(6, 15), (12, 10), (10, 28)] {
            let s = season(m, d).expect("a season");
            let g = s.gradient();
            assert!(!g.is_empty(), "{} has no colours", s.name);
            for stop in g {
                let luma = 0.2126f32.mul_add(
                    f32::from(stop[0]),
                    0.7152f32.mul_add(f32::from(stop[1]), 0.0722 * f32::from(stop[2])),
                );
                assert!(
                    luma < 90.0,
                    "{} stop {stop:?} is too bright to read on (luma {luma})",
                    s.name
                );
            }
        }
    }

    #[test]
    fn a_season_still_falls_darker_toward_the_bottom() {
        // Otherwise it reads as a flat wash rather than the panel's own background
        // wearing a colour.
        let g = season(6, 15).expect("pride").gradient();
        let luma = |c: Rgba| f32::from(c[0]) + f32::from(c[1]) + f32::from(c[2]);
        assert!(luma(g[0]) > luma(g[g.len() - 1]));
    }
}
