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
