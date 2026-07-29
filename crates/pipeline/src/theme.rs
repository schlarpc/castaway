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

/// A straight RGBA8 colour, the one every surface in this crate passes around.
///
/// Defined here rather than in [`crate::text`] because this module is the one that is
/// always compiled: colour is not a rendering concern, and the config file names a
/// palette whether or not this build can draw one.
pub type Rgba = [u8; 4];

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

/// The luminance band every service accent is pulled into.
///
/// Wide enough that a brand colour still looks like itself, narrow enough that a row of
/// them reads as one set.
const ACCENT_MIN_LUMA: f32 = 0.45;
/// Top of that band. See [`ACCENT_MIN_LUMA`].
const ACCENT_MAX_LUMA: f32 = 0.75;

/// Relative luminance of a colour, on the gamma-encoded values.
///
/// Not physically correct — the proper form linearises first. It is used here to compare
/// colours against each other rather than to measure any of them, and the cheap version
/// orders them the same way.
fn luma(c: Rgba) -> f32 {
    0.2126f32.mul_add(
        f32::from(c[0]),
        0.7152f32.mul_add(f32::from(c[1]), 0.0722 * f32::from(c[2])),
    ) / 255.0
}

/// An accent brought into the panel's luminance band, keeping its hue.
///
/// The service accents are other people's brand colours and they agree about nothing:
/// YouTube's `#ff0000` and AirPlay's pure white sit next to Cast blue with no shared
/// discipline, so the row reads as a sticker sheet — and the white outline was the
/// brightest object on the screen after the title. Colours below the band are lifted
/// toward white and ones above it pulled toward black, which is the smallest change that
/// puts them all on one footing. Anything already inside comes back untouched, so most
/// of them are unaffected.
#[must_use]
pub fn regulated(c: Rgba) -> Rgba {
    let l = luma(c);
    if l < ACCENT_MIN_LUMA {
        // Mixing toward white raises luminance linearly, so the amount is closed-form.
        let t = (ACCENT_MIN_LUMA - l) / (1.0 - l).max(f32::EPSILON);
        [
            mix(c[0], 0xff, t),
            mix(c[1], 0xff, t),
            mix(c[2], 0xff, t),
            c[3],
        ]
    } else if l > ACCENT_MAX_LUMA {
        let t = 1.0 - ACCENT_MAX_LUMA / l;
        [mix(c[0], 0, t), mix(c[1], 0, t), mix(c[2], 0, t), c[3]]
    } else {
        c
    }
}

/// A surface pulled `t` of the way toward an accent.
///
/// So a tile can carry its identity in its fill and not only in its outline: a plate that
/// is the same near-black for every service leaves a 3-pixel border doing all the work of
/// telling six of them apart across a room.
#[must_use]
pub fn tinted(base: Rgba, accent: Rgba, t: f32) -> Rgba {
    [
        mix(base[0], accent[0], t),
        mix(base[1], accent[1], t),
        mix(base[2], accent[2], t),
        base[3],
    ]
}

/// A seasonal palette for the background (#24).
///
/// Pure and date-driven so it is testable without waiting for June: the panel asks what
/// today is, and everything else follows from that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Season {
    /// Pride, in the six-colour flag. All of June.
    Pride,
    /// Trans Day of Visibility, 31 March.
    Trans,
    /// International Asexuality Day, 6 April.
    Ace,
    /// Lesbian Visibility Week, 22–28 April.
    Lesbian,
    /// Pansexual Pride Day, 24 May.
    Pan,
    /// Non-Binary Awareness Week, the week to 14 July.
    NonBinary,
    /// Bisexual Awareness Week, 16–23 September.
    Bi,
    /// Christmas — the twelve days, not the run-up.
    Christmas,
    /// Halloween, the last week of October.
    Halloween,
}

/// How much of a season's hue survives the mix into the background. Low on purpose: the
/// point is a room that feels different, not a screen nobody can read.
const SEASON_STRENGTH: f32 = 0.22;

impl Season {
    /// Every season, so a test can prove each one is reachable and legible.
    pub const ALL: [Self; 9] = [
        Self::Pride,
        Self::Trans,
        Self::Ace,
        Self::Lesbian,
        Self::Pan,
        Self::NonBinary,
        Self::Bi,
        Self::Christmas,
        Self::Halloween,
    ];

    /// What to call it, for a log line and for anyone wondering why the screen changed.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pride => "pride",
            Self::Trans => "trans",
            Self::Ace => "ace",
            Self::Lesbian => "lesbian",
            Self::Pan => "pan",
            Self::NonBinary => "non-binary",
            Self::Bi => "bi",
            Self::Christmas => "christmas",
            Self::Halloween => "halloween",
        }
    }

    /// The hues this season is made of, top to bottom.
    ///
    /// Not drawn as-is: [`Self::gradient`] mixes them most of the way into the panel's own
    /// dark ramp first. A flag at full strength is a lightbox, and everything on these
    /// screens is white text.
    #[must_use]
    pub const fn hues(self) -> &'static [Rgba] {
        match self {
            Self::Pride => &PRIDE,
            Self::Trans => &TRANS,
            Self::Ace => &ACE,
            Self::Lesbian => &LESBIAN,
            Self::Pan => &PAN,
            Self::NonBinary => &NONBINARY,
            Self::Bi => &BI,
            Self::Christmas => &YULE,
            Self::Halloween => &HALLOWEEN,
        }
    }

    /// The background ramp for this season: its hues, each pulled most of the way toward
    /// the panel's own dark gradient at the height it sits.
    #[must_use]
    pub fn gradient(self) -> Vec<Rgba> {
        let hues = self.hues();
        let n = hues.len().max(1);
        hues.iter()
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

/// Which palette the idle screen wears (#24).
///
/// A config option rather than only the calendar, because the calendar cannot know that
/// the space is throwing a party in April or that someone wants the screen plain for a
/// photograph. `Auto` is the default and is what the panel does unattended.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ThemeChoice {
    /// Follow the calendar.
    #[default]
    Auto,
    /// The panel's own dark ramp, whatever today is.
    Plain,
    /// Pride, all year.
    Pride,
    /// Trans, all year.
    Trans,
    /// Asexual, all year.
    Ace,
    /// Lesbian, all year.
    Lesbian,
    /// Pansexual, all year.
    Pan,
    /// Non-binary, all year.
    NonBinary,
    /// Bisexual, all year.
    Bi,
    /// Christmas, all year.
    Christmas,
    /// Halloween, all year.
    Halloween,
}

impl ThemeChoice {
    /// Every choice, so a test can prove each season can be asked for by name.
    pub const ALL: [Self; 11] = [
        Self::Auto,
        Self::Plain,
        Self::Pride,
        Self::Trans,
        Self::Ace,
        Self::Lesbian,
        Self::Pan,
        Self::NonBinary,
        Self::Bi,
        Self::Christmas,
        Self::Halloween,
    ];

    /// The season this asks for outright, if it names one.
    #[must_use]
    pub const fn forced(self) -> Option<Season> {
        match self {
            Self::Auto | Self::Plain => None,
            Self::Pride => Some(Season::Pride),
            Self::Trans => Some(Season::Trans),
            Self::Ace => Some(Season::Ace),
            Self::Lesbian => Some(Season::Lesbian),
            Self::Pan => Some(Season::Pan),
            Self::NonBinary => Some(Season::NonBinary),
            Self::Bi => Some(Season::Bi),
            Self::Christmas => Some(Season::Christmas),
            Self::Halloween => Some(Season::Halloween),
        }
    }

    /// What to wear on `(month, day)`.
    #[must_use]
    pub const fn resolve(self, month: u32, day: u32) -> Option<Season> {
        match self {
            Self::Auto => season(month, day),
            Self::Plain => None,
            other => other.forced(),
        }
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

/// The trans flag: light blue, pink, white, pink, light blue.
const TRANS: [Rgba; 5] = [
    [0x5b, 0xce, 0xfa, 0xff],
    [0xf5, 0xa9, 0xb8, 0xff],
    [0xff, 0xff, 0xff, 0xff],
    [0xf5, 0xa9, 0xb8, 0xff],
    [0x5b, 0xce, 0xfa, 0xff],
];

/// The asexual flag: black, grey, white, purple.
///
/// The one in this set that leans on the mix rather than fighting it — three of its four
/// stripes are achromatic, so what survives is a dark-to-light fall ending in purple.
const ACE: [Rgba; 4] = [
    [0x00, 0x00, 0x00, 0xff],
    [0xa3, 0xa3, 0xa3, 0xff],
    [0xff, 0xff, 0xff, 0xff],
    [0x80, 0x00, 0x80, 0xff],
];

/// The lesbian flag, five-stripe community version: orange-red through white to magenta.
const LESBIAN: [Rgba; 5] = [
    [0xd5, 0x2d, 0x00, 0xff],
    [0xff, 0x9a, 0x56, 0xff],
    [0xff, 0xff, 0xff, 0xff],
    [0xd3, 0x62, 0xa4, 0xff],
    [0xa3, 0x02, 0x62, 0xff],
];

/// The pansexual flag: magenta, yellow, cyan.
const PAN: [Rgba; 3] = [
    [0xff, 0x21, 0x8c, 0xff],
    [0xff, 0xd8, 0x00, 0xff],
    [0x21, 0xb1, 0xff, 0xff],
];

/// The non-binary flag: yellow, white, purple, black.
const NONBINARY: [Rgba; 4] = [
    [0xfc, 0xf4, 0x34, 0xff],
    [0xff, 0xff, 0xff, 0xff],
    [0x9c, 0x59, 0xd1, 0xff],
    [0x2c, 0x2c, 0x2c, 0xff],
];

/// The bisexual flag: magenta, lavender, blue.
const BI: [Rgba; 3] = [
    [0xd6, 0x02, 0x70, 0xff],
    [0x9b, 0x4f, 0x96, 0xff],
    [0x00, 0x38, 0xa8, 0xff],
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
/// Deliberately the *background* rather than a stripe across it. The screens have to stay
/// legible and the tile marks already carry other people's brand colours; a seasonal
/// palette fighting those would make the panel harder to read for a joke that lands once
/// a year, so the hue is mixed in at [`SEASON_STRENGTH`] and no further.
#[must_use]
pub const fn season(month: u32, day: u32) -> Option<Season> {
    match (month, day) {
        (6, _) => Some(Season::Pride),
        // Trans Day of Visibility. One day, so it has to actually land on it.
        (3, 31) => Some(Season::Trans),
        // International Asexuality Day.
        (4, 6) => Some(Season::Ace),
        (4, 22..=28) => Some(Season::Lesbian),
        (5, 24) => Some(Season::Pan),
        // Non-Binary Awareness Week, ending on International Non-Binary People's Day.
        (7, 8..=14) => Some(Season::NonBinary),
        // Bisexual Awareness Week, ending on Bi Visibility Day.
        (9, 16..=23) => Some(Season::Bi),
        // The twelve days: Christmas is a season and it starts *on* the 25th. An earlier
        // version ran 1–26 December on the theory that a decoration appearing on the day
        // has missed it, which had it gone by the time anyone was off work.
        (12, 25..=31) | (1, 1..=5) => Some(Season::Christmas),
        (10, 24..=31) => Some(Season::Halloween),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_accent_outside_the_band_is_pulled_into_it_and_one_inside_is_left_alone() {
        // The two that were the problem: pure red is too dark to sit next to Cast blue,
        // and pure white was the brightest thing on the screen after the title.
        for c in [[0xff, 0x00, 0x00, 0xff], [0xff, 0xff, 0xff, 0xff]] {
            let r = regulated(c);
            let l = luma(r);
            assert!(
                (ACCENT_MIN_LUMA - 0.02..=ACCENT_MAX_LUMA + 0.02).contains(&l),
                "{c:?} regulated to {r:?}, luma {l}"
            );
        }
        // Hue survives: a regulated red is still overwhelmingly red.
        let red = regulated([0xff, 0x00, 0x00, 0xff]);
        assert!(red[0] > red[1] && red[1] == red[2]);
        // The brand colours are already in the band, so they come back untouched — the
        // regulation is a correction for outliers, not a filter over the whole palette.
        for c in [BLUE, GREEN, GOLD, ACCENT] {
            assert_eq!(regulated(c), c, "{c:?} should not have been touched");
        }
    }

    #[test]
    fn seasons_land_where_they_should_and_nowhere_else() {
        assert_eq!(season(6, 1), Some(Season::Pride));
        assert_eq!(season(6, 30), Some(Season::Pride));
        assert_eq!(season(3, 31), Some(Season::Trans));
        assert_eq!(season(10, 31), Some(Season::Halloween));
        // Christmas is the twelve days, so it starts on the 25th and crosses the year.
        assert_eq!(season(12, 25), Some(Season::Christmas));
        assert_eq!(season(12, 31), Some(Season::Christmas));
        assert_eq!(season(1, 1), Some(Season::Christmas));
        assert_eq!(season(1, 5), Some(Season::Christmas));
        assert_eq!(season(12, 24), None, "the run-up is not the season");
        assert_eq!(season(1, 6), None, "twelfth night is the end of it");
        // An ordinary Tuesday gets an ordinary screen.
        assert_eq!(season(3, 14), None);
        assert_eq!(season(3, 30), None, "the day before is not the day");
        assert_eq!(season(10, 1), None);
    }

    #[test]
    fn every_season_actually_happens_at_some_point_in_the_year() {
        // `season` is a match, so an arm can silently shadow a later one — a season
        // nobody can reach is a palette that exists only in the config file.
        let days = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        for season in Season::ALL {
            let found = (1..=12u32)
                .any(|m| (1..=days[(m - 1) as usize]).any(|d| super::season(m, d) == Some(season)));
            assert!(found, "{} never comes round", season.name());
        }
    }

    #[test]
    fn every_season_keeps_enough_colour_to_be_worth_the_trouble() {
        // The other side of the legibility limit: mixed only 22% into a dark ramp, a flag
        // that is mostly white, grey and black barely registers. Ace, non-binary and
        // lesbian are the ones this is really about — they are all near the floor, and
        // the floor is Halloween, which has shipped and reads fine.
        for season in Season::ALL {
            let chroma = season
                .gradient()
                .iter()
                .map(|c| u32::from(c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])))
                .max()
                .unwrap_or(0);
            assert!(
                chroma >= 25,
                "{} washes out to nothing (max chroma {chroma})",
                season.name()
            );
        }
    }

    #[test]
    fn every_season_can_be_asked_for_by_name() {
        // The one thing the type system does not catch on its own: a season added without
        // a matching config value would be unreachable except on its own date.
        for season in Season::ALL {
            assert!(
                ThemeChoice::ALL.iter().any(|c| c.forced() == Some(season)),
                "{} cannot be chosen in the config",
                season.name()
            );
        }
    }

    #[test]
    fn a_forced_choice_ignores_the_calendar_and_auto_follows_it() {
        // 14 March is nothing in particular, which is the point.
        assert_eq!(ThemeChoice::Auto.resolve(3, 14), None);
        assert_eq!(ThemeChoice::Plain.resolve(6, 15), None, "plain means plain");
        assert_eq!(ThemeChoice::Pride.resolve(3, 14), Some(Season::Pride));
        assert_eq!(ThemeChoice::Auto.resolve(6, 15), Some(Season::Pride));
    }

    #[test]
    fn the_theme_choice_round_trips_through_a_config_file() {
        // It is written by hand into a TOML file, so the spelling is part of the contract.
        for (text, expect) in [
            ("\"auto\"", ThemeChoice::Auto),
            ("\"plain\"", ThemeChoice::Plain),
            ("\"trans\"", ThemeChoice::Trans),
            ("\"non-binary\"", ThemeChoice::NonBinary),
            ("\"christmas\"", ThemeChoice::Christmas),
        ] {
            let parsed: ThemeChoice = serde_json::from_str(text).expect(text);
            assert_eq!(parsed, expect);
            assert_eq!(serde_json::to_string(&expect).expect("write"), text);
        }
    }

    #[test]
    fn a_seasonal_background_stays_dark_enough_to_read_white_text_on() {
        // The whole risk of tinting the background: a flag at full strength is a
        // lightbox, and every screen here is white text on this ramp.
        for s in Season::ALL {
            let g = s.gradient();
            assert!(!g.is_empty(), "{} has no colours", s.name());
            for stop in g {
                let luma = 0.2126f32.mul_add(
                    f32::from(stop[0]),
                    0.7152f32.mul_add(f32::from(stop[1]), 0.0722 * f32::from(stop[2])),
                );
                assert!(
                    luma < 90.0,
                    "{} stop {stop:?} is too bright to read on (luma {luma})",
                    s.name()
                );
            }
        }
    }

    #[test]
    fn a_season_still_falls_darker_toward_the_bottom() {
        // Otherwise it reads as a flat wash rather than the panel's own background
        // wearing a colour.
        let g = Season::Pride.gradient();
        let luma = |c: Rgba| f32::from(c[0]) + f32::from(c[1]) + f32::from(c[2]);
        assert!(luma(g[0]) > luma(g[g.len() - 1]));
    }
}
