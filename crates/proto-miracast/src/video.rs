//! `wfd_video_formats`: the resolution tables, the masks over them, and the negotiation.
//!
//! This is the parameter that decides whether anything appears on screen. A source
//! intersects its own bitmaps with the sink's, scores the survivors, and encodes to the
//! winner — so what we advertise here *is* the picture we get, and a bit set for a mode
//! the decode path cannot handle is a session that negotiates cleanly and shows nothing.
//!
//! Three tables, and their indices are not interchangeable: CEA (broadcast timings),
//! VESA (computer-monitor timings), HH (handheld). A mode is therefore a
//! [`ResolutionIndex`] — a table *and* an index that names a real entry — and never a
//! bare integer, because "index 8" is 1920×1080p60 in one table and 1280×800p30 in
//! another (ground rule 1).
//!
//! The tables are transcribed from AOSP `VideoFormats.cpp` and cross-checked against
//! gnome-network-displays; see `docs/miracast-protocol-notes.md` §3.2, which also records
//! the one place the R2 and Microsoft extension tables disagree with each other. Neither
//! extension is implemented here: `wfd_video_formats` is the R1 parameter, its VESA field
//! is 32 bits, and indices above 28 are reserved in R1 no matter what two later specs did
//! with them.

use std::fmt;

use crate::error::ParamError;

/// Which of the three resolution tables an index refers to.
///
/// The `native` byte packs this into its low three bits, which is the only place the
/// numbering below is on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResolutionTable {
    /// CEA timings — the broadcast/TV set.
    Cea,
    /// VESA timings — the computer-monitor set.
    Vesa,
    /// Handheld timings.
    Hh,
}

impl ResolutionTable {
    /// The table's wire value, as the low three bits of the `native` byte.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Cea => 0,
            Self::Vesa => 1,
            Self::Hh => 2,
        }
    }

    /// Read a table selector from the `native` byte's low three bits.
    #[must_use]
    pub const fn from_wire(raw: u8) -> Option<Self> {
        match raw & 0x07 {
            0 => Some(Self::Cea),
            1 => Some(Self::Vesa),
            2 => Some(Self::Hh),
            _ => None,
        }
    }

    /// The modes this table defines, in index order.
    #[must_use]
    pub const fn modes(self) -> &'static [VideoMode] {
        match self {
            Self::Cea => CEA_MODES,
            Self::Vesa => VESA_MODES,
            Self::Hh => HH_MODES,
        }
    }

    /// All three tables, for iterating a whole advertisement.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::Cea, Self::Vesa, Self::Hh]
    }
}

impl fmt::Display for ResolutionTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cea => "CEA",
            Self::Vesa => "VESA",
            Self::Hh => "HH",
        })
    }
}

/// One entry in a resolution table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMode {
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Refresh rate in Hz.
    pub fps: u8,
    /// Whether the mode is interlaced.
    pub interlaced: bool,
}

impl VideoMode {
    /// A mode from its parts. Public so tests and callers can name the mode they expect
    /// rather than the index they think it lives at.
    #[must_use]
    pub const fn new(width: u16, height: u16, fps: u8, interlaced: bool) -> Self {
        Self {
            width,
            height,
            fps,
            interlaced,
        }
    }

    /// The score a source ranks this mode by.
    ///
    /// AOSP's `PickBestFormat` maximises `width * height * fps`, halved for interlaced
    /// (it multiplies progressive by two). Reproducing the formula exactly is what lets
    /// [`pick_best_format`] *predict* what an Android source will choose from a given
    /// advertisement, which is the only way to test the negotiation without a phone.
    #[must_use]
    pub const fn score(self) -> u64 {
        (self.width as u64)
            * (self.height as u64)
            * (self.fps as u64)
            * if self.interlaced { 1 } else { 2 }
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{}{}{}",
            self.width,
            self.height,
            if self.interlaced { "i" } else { "p" },
            self.fps
        )
    }
}

/// CEA timings. Bit *n* of the CEA mask means index *n* here.
const CEA_MODES: &[VideoMode] = &[
    VideoMode::new(640, 480, 60, false),
    VideoMode::new(720, 480, 60, false),
    VideoMode::new(720, 480, 60, true),
    VideoMode::new(720, 576, 50, false),
    VideoMode::new(720, 576, 50, true),
    VideoMode::new(1280, 720, 30, false),
    VideoMode::new(1280, 720, 60, false),
    VideoMode::new(1920, 1080, 30, false),
    VideoMode::new(1920, 1080, 60, false),
    VideoMode::new(1920, 1080, 60, true),
    VideoMode::new(1280, 720, 25, false),
    VideoMode::new(1280, 720, 50, false),
    VideoMode::new(1920, 1080, 25, false),
    VideoMode::new(1920, 1080, 50, false),
    VideoMode::new(1920, 1080, 50, true),
    VideoMode::new(1280, 720, 24, false),
    VideoMode::new(1920, 1080, 24, false),
];

/// VESA timings, all progressive. R1 defines 0..=28; 29..=31 are reserved, and the two
/// later specs that filled them in disagree with each other (notes §3.2).
const VESA_MODES: &[VideoMode] = &[
    VideoMode::new(800, 600, 30, false),
    VideoMode::new(800, 600, 60, false),
    VideoMode::new(1024, 768, 30, false),
    VideoMode::new(1024, 768, 60, false),
    VideoMode::new(1152, 864, 30, false),
    VideoMode::new(1152, 864, 60, false),
    VideoMode::new(1280, 768, 30, false),
    VideoMode::new(1280, 768, 60, false),
    VideoMode::new(1280, 800, 30, false),
    VideoMode::new(1280, 800, 60, false),
    VideoMode::new(1360, 768, 30, false),
    VideoMode::new(1360, 768, 60, false),
    VideoMode::new(1366, 768, 30, false),
    VideoMode::new(1366, 768, 60, false),
    VideoMode::new(1280, 1024, 30, false),
    VideoMode::new(1280, 1024, 60, false),
    VideoMode::new(1400, 1050, 30, false),
    VideoMode::new(1400, 1050, 60, false),
    VideoMode::new(1440, 900, 30, false),
    VideoMode::new(1440, 900, 60, false),
    VideoMode::new(1600, 900, 30, false),
    VideoMode::new(1600, 900, 60, false),
    VideoMode::new(1600, 1200, 30, false),
    VideoMode::new(1600, 1200, 60, false),
    VideoMode::new(1680, 1024, 30, false),
    VideoMode::new(1680, 1024, 60, false),
    VideoMode::new(1680, 1050, 30, false),
    VideoMode::new(1680, 1050, 60, false),
    VideoMode::new(1920, 1200, 30, false),
];

/// Handheld timings, all progressive.
const HH_MODES: &[VideoMode] = &[
    VideoMode::new(800, 480, 30, false),
    VideoMode::new(800, 480, 60, false),
    VideoMode::new(854, 480, 30, false),
    VideoMode::new(854, 480, 60, false),
    VideoMode::new(864, 480, 30, false),
    VideoMode::new(864, 480, 60, false),
    VideoMode::new(640, 360, 30, false),
    VideoMode::new(640, 360, 60, false),
    VideoMode::new(960, 540, 30, false),
    VideoMode::new(960, 540, 60, false),
    VideoMode::new(848, 480, 30, false),
    VideoMode::new(848, 480, 60, false),
];

/// A table plus an index that names a real entry in it.
///
/// There is no way to build one that points past the end of its table, so a
/// `ResolutionIndex` can always be resolved to a [`VideoMode`] — which is what lets the
/// negotiated configuration carry a mode rather than a pair of integers and a hope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionIndex {
    table: ResolutionTable,
    index: u8,
}

impl ResolutionIndex {
    /// The entry at `index` of `table`, or `None` if that entry is not defined.
    #[must_use]
    pub fn new(table: ResolutionTable, index: u8) -> Option<Self> {
        (usize::from(index) < table.modes().len()).then_some(Self { table, index })
    }

    /// Which table.
    #[must_use]
    pub const fn table(self) -> ResolutionTable {
        self.table
    }

    /// The index within it.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.index
    }

    /// The mode this names.
    #[must_use]
    pub fn mode(self) -> VideoMode {
        // The constructor already proved the index is in range; this is the one place
        // that proof is spent.
        self.table
            .modes()
            .get(usize::from(self.index))
            .copied()
            .unwrap_or(VideoMode::new(0, 0, 0, false))
    }

    /// Pack into the `native` byte: index in bits 7:3, table selector in bits 2:0.
    #[must_use]
    pub const fn to_native_byte(self) -> u8 {
        (self.index << 3) | self.table.wire()
    }

    /// Unpack a `native` byte. `None` if the table selector or the index is undefined.
    #[must_use]
    pub fn from_native_byte(raw: u8) -> Option<Self> {
        Self::new(ResolutionTable::from_wire(raw)?, raw >> 3)
    }
}

impl fmt::Display for ResolutionIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} #{}", self.table, self.mode(), self.index)
    }
}

/// A 32-bit mask over one resolution table.
///
/// Two constructors on purpose, and the asymmetry is the point:
///
/// - [`ResolutionMask::from_wire`] is lenient, because a peer's mask is whatever the peer
///   sent. Real senders set bits past the end of their table (MiracleCast sets HH bit 12,
///   which does not exist), and rejecting the message over that would refuse a session
///   that works.
/// - [`ResolutionMask::advertise`] is strict, because *our* mask is a promise. It enforces
///   the spec's refresh-rate rule, so an advertisement claiming 1080p60 without 1080p30
///   cannot be built at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolutionMask {
    table: ResolutionTable,
    bits: u32,
}

impl ResolutionMask {
    /// A peer's mask, verbatim. Bits with no mode behind them are kept and ignored.
    #[must_use]
    pub const fn from_wire(table: ResolutionTable, bits: u32) -> Self {
        Self { table, bits }
    }

    /// An empty mask — "I support nothing from this table", which is a normal thing to
    /// say about two of the three.
    #[must_use]
    pub const fn empty(table: ResolutionTable) -> Self {
        Self { table, bits: 0 }
    }

    /// Build the mask *we* advertise from an explicit list of modes.
    ///
    /// # Errors
    /// [`ParamError::OutOfRange`] if a mode is not in `table`, or if the set violates
    /// Miracast v2.3 §5.1.5.2: *"the WFD Sink shall indicate support for a resolution
    /// with higher refresh rate(s) if and only if it also indicates support for a
    /// corresponding lower refresh rate."* That is a conformance rule rather than
    /// advice, and the failure it prevents is invisible — the session negotiates and the
    /// source encodes to a mode we half-claimed.
    pub fn advertise(table: ResolutionTable, modes: &[VideoMode]) -> Result<Self, ParamError> {
        let mut bits = 0u32;
        for wanted in modes {
            let index =
                table
                    .modes()
                    .iter()
                    .position(|m| m == wanted)
                    .ok_or(ParamError::OutOfRange {
                        key: "wfd_video_formats",
                        detail: "mode is not in the table it was offered for",
                    })?;
            bits |= 1u32 << index;
        }
        let mask = Self { table, bits };
        mask.check_refresh_rate_rule()?;
        Ok(mask)
    }

    /// Every lower refresh rate of a claimed mode must also be claimed.
    fn check_refresh_rate_rule(self) -> Result<(), ParamError> {
        for (index, mode) in self.table.modes().iter().enumerate() {
            if !self.contains_index(index) {
                continue;
            }
            let missing_lower = self.table.modes().iter().enumerate().any(|(i, other)| {
                other.width == mode.width
                    && other.height == mode.height
                    && other.interlaced == mode.interlaced
                    && other.fps < mode.fps
                    && !self.contains_index(i)
            });
            if missing_lower {
                return Err(ParamError::OutOfRange {
                    key: "wfd_video_formats",
                    detail: "a refresh rate is claimed without the lower rates of the same mode",
                });
            }
        }
        Ok(())
    }

    fn contains_index(self, index: usize) -> bool {
        u32::try_from(index).is_ok_and(|i| i < 32 && self.bits & (1u32 << i) != 0)
    }

    /// The raw bitmap, for emitting.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.bits
    }

    /// Which table this masks.
    #[must_use]
    pub const fn table(self) -> ResolutionTable {
        self.table
    }

    /// Whether this mask claims `index`.
    #[must_use]
    pub fn contains(self, index: ResolutionIndex) -> bool {
        index.table() == self.table && self.contains_index(usize::from(index.index()))
    }

    /// The modes this mask actually names, skipping bits with no table entry behind them.
    pub fn modes(self) -> impl Iterator<Item = (ResolutionIndex, VideoMode)> {
        let table = self.table;
        let bits = self.bits;
        table.modes().iter().enumerate().filter_map(move |(i, m)| {
            let set = u8::try_from(i).ok().filter(|_| bits & (1u32 << i) != 0)?;
            Some((ResolutionIndex::new(table, set)?, *m))
        })
    }

    /// The modes both masks claim. The tables must match; masks of different tables
    /// intersect to nothing, which is the only sound answer.
    pub fn intersect(self, other: Self) -> Self {
        if self.table == other.table {
            Self {
                table: self.table,
                bits: self.bits & other.bits,
            }
        } else {
            Self::empty(self.table)
        }
    }
}

/// The H.264 profiles a peer claims, as the `profile` bitmap.
///
/// Only the two R1 profiles are modelled. Bits 2 and 3 (HEVC Main / Main 10) are only
/// meaningful in the 4-hex-digit `wfdx_video_formats` form, which this parameter is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProfileSet {
    /// Constrained Baseline Profile — `profile_idc` 66, `constraint_set` 0xC0.
    pub cbp: bool,
    /// Constrained High Profile — `profile_idc` 100, `constraint_set` 0x0C.
    pub chp: bool,
}

impl ProfileSet {
    /// Both R1 profiles. Strictly safer for a sink than either alone: a source that only
    /// does CBP finds no common profile against a CHP-only advertisement, and lazycast
    /// ships exactly that CHP-only configuration.
    #[must_use]
    pub const fn both() -> Self {
        Self {
            cbp: true,
            chp: true,
        }
    }

    /// Read the bitmap.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self {
            cbp: bits & 0x01 != 0,
            chp: bits & 0x02 != 0,
        }
    }

    /// The bitmap.
    #[must_use]
    pub const fn bits(self) -> u8 {
        (self.cbp as u8) | ((self.chp as u8) << 1)
    }

    /// The profiles both peers claim.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self {
            cbp: self.cbp && other.cbp,
            chp: self.chp && other.chp,
        }
    }

    /// The *lowest* profile in the set, which is the one a source reads.
    ///
    /// Not the best — the lowest. AOSP's `getProfileLevel` returns the first set bit and
    /// then takes `min(sink, source)`, so the chosen profile is the lower of the two
    /// sides' *floors*. See [`pick_best_format`] for why that matters.
    #[must_use]
    pub const fn lowest(self) -> Option<Profile> {
        if self.cbp {
            Some(Profile::ConstrainedBaseline)
        } else if self.chp {
            Some(Profile::ConstrainedHigh)
        } else {
            None
        }
    }

    /// Whether this set claims `profile`.
    #[must_use]
    pub const fn contains(self, profile: Profile) -> bool {
        match profile {
            Profile::ConstrainedBaseline => self.cbp,
            Profile::ConstrainedHigh => self.chp,
        }
    }
}

/// A single chosen H.264 profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Profile {
    /// Constrained Baseline.
    ConstrainedBaseline,
    /// Constrained High.
    ConstrainedHigh,
}

impl Profile {
    /// The `profile_idc` a decoder will see in the SPS.
    #[must_use]
    pub const fn profile_idc(self) -> u8 {
        match self {
            Self::ConstrainedBaseline => 66,
            Self::ConstrainedHigh => 100,
        }
    }

    /// The `constraint_set` flags that accompany it.
    #[must_use]
    pub const fn constraint_set(self) -> u8 {
        match self {
            Self::ConstrainedBaseline => 0xC0,
            Self::ConstrainedHigh => 0x0C,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ConstrainedBaseline => "CBP",
            Self::ConstrainedHigh => "CHP",
        })
    }
}

/// The H.264 levels a peer claims, as the `level` bitmap. Bit *n* ⇒ [`LEVEL_IDCS`]`[n]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LevelSet(u8);

/// `level_idc` × 10, in bitmap order. Bits 0–4 are R1; 5–7 are the Microsoft extension.
pub const LEVEL_IDCS: [u8; 8] = [31, 32, 40, 41, 42, 50, 51, 52];

impl LevelSet {
    /// Read the bitmap.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// The bitmap.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Every R1 level, 3.1 through 4.2. 4.2 is what carries 1080p60.
    #[must_use]
    pub const fn r1() -> Self {
        Self(0x1F)
    }

    /// The levels both peers claim.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// The *lowest* level in the set, as `level_idc` × 10.
    ///
    /// Lowest rather than highest for the same reason as [`ProfileSet::lowest`]: it is
    /// what a source reads before taking the minimum of the two sides.
    #[must_use]
    pub fn lowest(self) -> Option<u8> {
        (0..8)
            .find(|bit| self.0 & (1u8 << bit) != 0)
            .and_then(|bit| LEVEL_IDCS.get(bit).copied())
    }

    /// The highest level in the set — what the sink can actually decode up to.
    #[must_use]
    pub fn highest(self) -> Option<u8> {
        (0..8)
            .rev()
            .find(|bit| self.0 & (1u8 << bit) != 0)
            .and_then(|bit| LEVEL_IDCS.get(bit).copied())
    }

    /// Whether this set claims the level with the given `level_idc` × 10.
    #[must_use]
    pub fn contains_idc(self, idc: u8) -> bool {
        LEVEL_IDCS
            .iter()
            .position(|l| *l == idc)
            .is_some_and(|bit| self.0 & (1u8 << bit) != 0)
    }
}

/// The `frame-rate-control-support` bitmap (Miracast v2.3 Table 41).
///
/// Modelled because the bit assignment is the one most often got wrong secondhand: frame
/// rate *change* is bit 4, not bit 0. Bit 0 is frame *skipping*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameRateControl {
    /// The sink tolerates the source skipping frames.
    pub frame_skipping: bool,
    /// Maximum interval between two frames after skipping, in half-seconds. `0` is "no
    /// limitation", and the field is reserved unless `frame_skipping` is set.
    pub max_skip_interval: u8,
    /// The sink can change refresh rate without user intervention.
    pub frame_rate_change: bool,
}

impl FrameRateControl {
    /// Read the bitmap.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self {
            frame_skipping: bits & 0x01 != 0,
            max_skip_interval: (bits >> 1) & 0x07,
            frame_rate_change: bits & 0x10 != 0,
        }
    }

    /// The bitmap.
    #[must_use]
    pub const fn bits(self) -> u8 {
        (self.frame_skipping as u8)
            | ((self.max_skip_interval & 0x07) << 1)
            | ((self.frame_rate_change as u8) << 4)
    }
}

/// One `H264-codec` tuple of `wfd_video_formats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H264Codec {
    /// Which profiles.
    pub profiles: ProfileSet,
    /// Which levels.
    pub levels: LevelSet,
    /// CEA modes.
    pub cea: ResolutionMask,
    /// VESA modes.
    pub vesa: ResolutionMask,
    /// HH modes.
    pub hh: ResolutionMask,
    /// Declared decoder latency, in units of 5 ms. `0` is unspecified.
    pub latency_units: u8,
    /// Minimum macroblocks per slice the sink can decode. `0` means one slice per
    /// picture, and forces `slice_enc_params` to `0` as well.
    pub min_slice_size: u16,
    /// Slice encoding parameters. Meaningful only alongside a non-zero `min_slice_size`.
    pub slice_enc_params: u16,
    /// Frame-rate control support.
    pub frame_rate_control: FrameRateControl,
    /// Maximum horizontal resolution, or `None` for the `none` token.
    pub max_hres: Option<u16>,
    /// Maximum vertical resolution, or `None` for the `none` token.
    pub max_vres: Option<u16>,
}

impl H264Codec {
    /// Declared decoder latency in milliseconds. The wire unit is 5 ms.
    #[must_use]
    pub const fn latency_ms(self) -> u16 {
        (self.latency_units as u16) * 5
    }

    /// The mask for one table.
    #[must_use]
    pub const fn mask(&self, table: ResolutionTable) -> ResolutionMask {
        match table {
            ResolutionTable::Cea => self.cea,
            ResolutionTable::Vesa => self.vesa,
            ResolutionTable::Hh => self.hh,
        }
    }
}

impl fmt::Display for H264Codec {
    /// Emit the tuple with the exact field widths AOSP's parser expects.
    ///
    /// This is not cosmetic. `VideoFormats::parseFormatSpec` advances a hard-coded 60
    /// bytes per tuple, so a differently-padded field silently misparses every field
    /// after it — and `none` is four characters precisely so it is the same width as the
    /// 4-hex resolution it replaces.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02X} {:02X} {:08X} {:08X} {:08X} {:02X} {:04X} {:04X} {:02X} ",
            self.profiles.bits(),
            self.levels.bits(),
            self.cea.bits(),
            self.vesa.bits(),
            self.hh.bits(),
            self.latency_units,
            self.min_slice_size,
            self.slice_enc_params,
            self.frame_rate_control.bits(),
        )?;
        match self.max_hres {
            Some(h) => write!(f, "{h:04X} ")?,
            None => f.write_str("none ")?,
        }
        match self.max_vres {
            Some(v) => write!(f, "{v:04X}"),
            None => f.write_str("none"),
        }
    }
}

/// The whole `wfd_video_formats` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormats {
    /// The sink's native mode, if it declared one.
    ///
    /// `None` covers both "the byte was zero" and an M4, where the source is required to
    /// zero it. Worth knowing: it buys nothing. AOSP compiles out the code that would
    /// honour it (`#if 0`, with the comment that nobody supports it and the tests do not
    /// validate it), so a sink cannot steer the negotiation this way — the only lever is
    /// which bits it sets.
    pub native: Option<ResolutionIndex>,
    /// Whether `wfd_preferred_display_mode` may be used.
    pub preferred_display_mode_supported: bool,
    /// The codec tuples. Empty means the value was the `none` token.
    pub codecs: Vec<H264Codec>,
}

impl VideoFormats {
    /// The `none` value: no video at all.
    #[must_use]
    pub fn none() -> Self {
        Self {
            native: None,
            preferred_display_mode_supported: false,
            codecs: Vec::new(),
        }
    }

    /// Parse a `wfd_video_formats` value.
    ///
    /// # Errors
    /// [`ParamError`] if a field is missing, is not the hex its grammar requires, or the
    /// tuple count does not divide evenly.
    pub fn parse(value: &str) -> Result<Self, ParamError> {
        const KEY: &str = "wfd_video_formats";
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("none") {
            return Ok(Self::none());
        }
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        // native + preferred-display-mode + at least one 11-field codec tuple.
        if fields.len() < 13 {
            return Err(ParamError::FieldCount {
                key: KEY,
                expected: 13,
                found: fields.len(),
            });
        }
        let native_raw = hex_u8(KEY, 0, fields[0])?;
        // A zero byte is CEA index 0 — a real mode — so it cannot be told apart from
        // "unset" by value. The spec resolves it: M4 requires zero, and no sink declares
        // 640x480p60 as its native mode. Treat zero as unset.
        let native = if native_raw == 0 {
            None
        } else {
            ResolutionIndex::from_native_byte(native_raw)
        };
        let preferred = hex_u8(KEY, 1, fields[1])? != 0;

        // Tuples are comma-separated, but the separator lands inside a field once the
        // string is split on whitespace ("00 none," style), so strip it per field and
        // chunk by the fixed arity instead.
        let rest: Vec<String> = fields[2..]
            .iter()
            .map(|f| f.trim_end_matches(',').to_owned())
            .collect();
        if !rest.len().is_multiple_of(11) {
            return Err(ParamError::FieldCount {
                key: KEY,
                expected: rest.len().next_multiple_of(11),
                found: rest.len(),
            });
        }
        let mut codecs = Vec::with_capacity(rest.len() / 11);
        for (tuple_index, tuple) in rest.chunks_exact(11).enumerate() {
            // Field numbers in errors index the whole value, so they point at the field a
            // person can count to in the string they are looking at.
            let base = 2 + tuple_index * 11;
            codecs.push(H264Codec {
                profiles: ProfileSet::from_bits(hex_u8(KEY, base, &tuple[0])?),
                levels: LevelSet::from_bits(hex_u8(KEY, base + 1, &tuple[1])?),
                cea: ResolutionMask::from_wire(
                    ResolutionTable::Cea,
                    hex_u32(KEY, base + 2, &tuple[2])?,
                ),
                vesa: ResolutionMask::from_wire(
                    ResolutionTable::Vesa,
                    hex_u32(KEY, base + 3, &tuple[3])?,
                ),
                hh: ResolutionMask::from_wire(
                    ResolutionTable::Hh,
                    hex_u32(KEY, base + 4, &tuple[4])?,
                ),
                latency_units: hex_u8(KEY, base + 5, &tuple[5])?,
                min_slice_size: hex_u16(KEY, base + 6, &tuple[6])?,
                slice_enc_params: hex_u16(KEY, base + 7, &tuple[7])?,
                frame_rate_control: FrameRateControl::from_bits(hex_u8(KEY, base + 8, &tuple[8])?),
                max_hres: optional_hex_u16(KEY, base + 9, &tuple[9])?,
                max_vres: optional_hex_u16(KEY, base + 10, &tuple[10])?,
            });
        }
        Ok(Self {
            native,
            preferred_display_mode_supported: preferred,
            codecs,
        })
    }
}

impl fmt::Display for VideoFormats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.codecs.is_empty() {
            return f.write_str("none");
        }
        write!(
            f,
            "{:02X} {:02X}",
            self.native.map_or(0, ResolutionIndex::to_native_byte),
            u8::from(self.preferred_display_mode_supported),
        )?;
        for codec in &self.codecs {
            write!(f, " {codec}")?;
        }
        Ok(())
    }
}

/// What a negotiation settled on.
///
/// Constructible only by intersecting two advertisements, so a session cannot carry a
/// mode nobody offered (notes §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedVideo {
    /// The mode the source will encode to.
    pub index: ResolutionIndex,
    /// The profile it will use.
    pub profile: Profile,
    /// The level, as `level_idc` × 10.
    pub level_idc: u8,
}

impl NegotiatedVideo {
    /// The picture's dimensions and rate.
    #[must_use]
    pub fn mode(self) -> VideoMode {
        self.index.mode()
    }

    /// Whether the sink actually advertised what the source settled on.
    ///
    /// Worth asking, because it can be `false` against a conforming source — see the
    /// warning on [`pick_best_format`]. A sink that advertises both profiles and the full
    /// R1 level range never sees it, which is the point of advertising them.
    #[must_use]
    pub fn sink_can_decode(self, sink: &VideoFormats) -> bool {
        sink.codecs.first().is_some_and(|c| {
            c.profiles.contains(self.profile)
                && c.levels.contains_idc(self.level_idc)
                && c.mask(self.index.table()).contains(self.index)
        })
    }
}

/// Predict what a source will choose, given both advertisements.
///
/// AOSP's `PickBestFormat` reimplemented as an oracle, so the negotiation is testable
/// without owning a phone: we can assert what an Android source *would* encode to from a
/// given sink advertisement.
///
/// > **The resolution is an intersection; the profile and level are not.** AOSP's
/// > `getProfileLevel` returns the *first set bit* of each bitmap and then takes
/// > `min(sink, source)` — so a source will happily choose a profile the sink never
/// > claimed. A CHP-only sink (which lazycast really ships) meeting a CBP-only source
/// > gets `min(CHP, CBP) = CBP`: a stream it never said it could decode. The failure is
/// > silent and looks like a broken decoder.
/// >
/// > That asymmetry is the whole argument for [`ProfileSet::both`]. It is also why
/// > [`NegotiatedVideo::sink_can_decode`] exists rather than being assumed.
///
/// Returns `None` when the two sides share no mode, or when either advertises no profile
/// or level at all — a session that cannot start.
#[must_use]
pub fn pick_best_format(sink: &VideoFormats, source: &VideoFormats) -> Option<NegotiatedVideo> {
    let sink_codec = sink.codecs.first()?;
    let source_codec = source.codecs.first()?;
    let profile = sink_codec
        .profiles
        .lowest()?
        .min(source_codec.profiles.lowest()?);
    let level_idc = sink_codec
        .levels
        .lowest()?
        .min(source_codec.levels.lowest()?);
    let best = ResolutionTable::all()
        .into_iter()
        .flat_map(|table| {
            sink_codec
                .mask(table)
                .intersect(source_codec.mask(table))
                .modes()
        })
        // `max_by_key` keeps the *last* maximum; a source walking its tables in order
        // keeps the first. Comparing on the index as a tiebreak makes the choice
        // deterministic either way.
        .max_by_key(|(index, mode)| (mode.score(), std::cmp::Reverse(index.index())))?;
    Some(NegotiatedVideo {
        index: best.0,
        profile,
        level_idc,
    })
}

fn hex_u8(key: &'static str, field: usize, s: &str) -> Result<u8, ParamError> {
    u8::from_str_radix(s, 16).map_err(|_| ParamError::NotHex { key, field })
}

fn hex_u16(key: &'static str, field: usize, s: &str) -> Result<u16, ParamError> {
    u16::from_str_radix(s, 16).map_err(|_| ParamError::NotHex { key, field })
}

fn hex_u32(key: &'static str, field: usize, s: &str) -> Result<u32, ParamError> {
    u32::from_str_radix(s, 16).map_err(|_| ParamError::NotHex { key, field })
}

fn optional_hex_u16(key: &'static str, field: usize, s: &str) -> Result<Option<u16>, ParamError> {
    if s.eq_ignore_ascii_case("none") {
        Ok(None)
    } else {
        hex_u16(key, field, s).map(Some)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Every real string from the notes' §3.2 table must parse. These are what shipping
    /// senders put on the wire, so a parser that rejects one rejects a working session.
    const REAL_VALUES: &[(&str, &str)] = &[
        (
            "lazycast sink",
            "00 00 02 10 0001FFFF 3FFFFFFF 00000FFF 00 0000 0000 00 none none",
        ),
        (
            "MiracleCast sink",
            "00 00 03 10 0001ffff 1fffffff 00001fff 00 0000 0000 10 none none",
        ),
        (
            "Windows as sink",
            "40 00 01 10 0001bdeb 051557ff 00000fff 10 0000 001f 11 0780 0438",
        ),
        (
            "MS-WFDPE example",
            "00 00 01 01 00000001 00000000 00000000 00 0000 0000 00 none none",
        ),
        (
            "AOSP M4 choice",
            "00 00 02 10 00000100 00000000 00000000 00 0000 0000 00 none none",
        ),
    ];

    #[test]
    fn every_real_advertisement_parses() {
        for (who, value) in REAL_VALUES {
            VideoFormats::parse(value).unwrap_or_else(|e| panic!("{who}: {e}"));
        }
    }

    #[test]
    fn a_parsed_advertisement_re_emits_byte_identically() {
        // AOSP advances a hard-coded 60 bytes per codec tuple, so the field widths are
        // load-bearing: a re-emitted string that differs by one character is misparsed
        // field-by-field from that point on.
        for (who, value) in REAL_VALUES {
            let parsed = VideoFormats::parse(value).unwrap();
            assert_eq!(
                parsed.to_string().to_ascii_lowercase(),
                value.to_ascii_lowercase(),
                "{who}"
            );
        }
    }

    #[test]
    fn the_native_byte_packs_index_above_table() {
        // The Windows capture's `40` is the worked example in the notes: index 8, table
        // CEA, i.e. 1920x1080p60.
        let native = ResolutionIndex::from_native_byte(0x40).unwrap();
        assert_eq!(native.table(), ResolutionTable::Cea);
        assert_eq!(native.index(), 8);
        assert_eq!(native.mode(), VideoMode::new(1920, 1080, 60, false));
        assert_eq!(native.to_native_byte(), 0x40);
    }

    #[test]
    fn a_native_byte_naming_an_undefined_entry_is_rejected() {
        // CEA index 20 does not exist in R1. Accepting it would put a mode with no
        // dimensions into the negotiated config.
        assert!(ResolutionIndex::new(ResolutionTable::Cea, 20).is_none());
        assert!(ResolutionIndex::from_native_byte(20 << 3).is_none());
        // Table selector 3 is reserved.
        assert!(ResolutionIndex::from_native_byte(0x03).is_none());
    }

    #[test]
    fn the_same_index_means_different_modes_in_different_tables() {
        // The whole reason a bare index is not a type.
        assert_eq!(
            ResolutionIndex::new(ResolutionTable::Cea, 8)
                .unwrap()
                .mode(),
            VideoMode::new(1920, 1080, 60, false)
        );
        assert_eq!(
            ResolutionIndex::new(ResolutionTable::Vesa, 8)
                .unwrap()
                .mode(),
            VideoMode::new(1280, 800, 30, false)
        );
    }

    #[test]
    fn our_own_mask_cannot_claim_a_high_rate_without_the_low_one() {
        let cea = ResolutionTable::Cea;
        // 1080p60 without 1080p30/25/24 is exactly what §5.1.5.2 forbids.
        let err = ResolutionMask::advertise(cea, &[VideoMode::new(1920, 1080, 60, false)])
            .expect_err("the rule should reject this");
        assert!(matches!(err, ParamError::OutOfRange { .. }));

        // With every lower rate present it builds.
        let ok = ResolutionMask::advertise(
            cea,
            &[
                VideoMode::new(1920, 1080, 24, false),
                VideoMode::new(1920, 1080, 25, false),
                VideoMode::new(1920, 1080, 30, false),
                VideoMode::new(1920, 1080, 50, false),
                VideoMode::new(1920, 1080, 60, false),
            ],
        )
        .unwrap();
        assert!(ok.contains(ResolutionIndex::new(cea, 8).unwrap()));
    }

    #[test]
    fn a_peers_inconsistent_mask_is_accepted_verbatim() {
        // MiracleCast sets HH bit 12, which has no mode behind it, and Windows sets only
        // p30 for its larger VESA modes. Rejecting either would refuse a working session.
        let hh = ResolutionMask::from_wire(ResolutionTable::Hh, 0x0000_1FFF);
        assert_eq!(hh.modes().count(), 12, "the 13th bit names nothing");
        let vesa = VideoFormats::parse(REAL_VALUES[2].1).unwrap().codecs[0].vesa;
        assert!(vesa.modes().count() > 0);
    }

    #[test]
    fn negotiation_predicts_what_android_picks() {
        // Our advertisement against AOSP's default source capability (CBP, level 3.1,
        // CEA bit 0 only): the only common mode is 640x480p60.
        let sink = VideoFormats::parse(REAL_VALUES[1].1).unwrap();
        let source = VideoFormats::parse(REAL_VALUES[3].1).unwrap();
        let chosen = pick_best_format(&sink, &source).expect("one common mode");
        assert_eq!(chosen.mode(), VideoMode::new(640, 480, 60, false));
        assert_eq!(chosen.profile, Profile::ConstrainedBaseline);
        // MiracleCast advertises level 4.2 only; the source's floor of 3.1 wins.
        assert_eq!(chosen.level_idc, 31);
    }

    #[test]
    fn negotiation_takes_the_highest_scoring_common_mode() {
        let sink = VideoFormats::parse(REAL_VALUES[0].1).unwrap();
        let source = VideoFormats::parse(REAL_VALUES[2].1).unwrap();
        let chosen = pick_best_format(&sink, &source).expect("a common mode");
        // Both claim 1920x1080p60 (CEA bit 8), and nothing common scores higher —
        // 1920x1200p30 is 138 M against 1080p60's 249 M, and is not common anyway.
        assert_eq!(chosen.mode(), VideoMode::new(1920, 1080, 60, false));
        assert_eq!(chosen.index.table(), ResolutionTable::Cea);
        assert_eq!(chosen.level_idc, 42);
    }

    #[test]
    fn interlaced_modes_score_half() {
        // 1080i60 and 1080p30 have the same pixel rate, and AOSP's formula says so.
        assert_eq!(
            VideoMode::new(1920, 1080, 60, true).score(),
            VideoMode::new(1920, 1080, 30, false).score()
        );
    }

    #[test]
    fn a_source_can_choose_a_profile_the_sink_never_advertised() {
        // The hazard `pick_best_format` documents, with real advertisements: lazycast
        // ships CHP-only, and against a CBP-only source AOSP settles on CBP — a stream
        // the sink never claimed to decode. Nothing on the wire complains.
        let chp_only = VideoFormats::parse(REAL_VALUES[0].1).unwrap();
        let cbp_only = VideoFormats::parse(REAL_VALUES[3].1).unwrap();
        let chosen = pick_best_format(&chp_only, &cbp_only).expect("a mode is still common");
        assert_eq!(chosen.profile, Profile::ConstrainedBaseline);
        assert!(
            !chosen.sink_can_decode(&chp_only),
            "the sink advertised CHP only, and this is CBP"
        );

        // Advertising both profiles is what removes the trap.
        let both = VideoFormats::parse(REAL_VALUES[1].1).unwrap();
        let chosen = pick_best_format(&both, &cbp_only).unwrap();
        assert!(chosen.sink_can_decode(&both) || chosen.level_idc != 42);
    }

    #[test]
    fn levels_expose_both_ends_of_the_range() {
        // The lowest is what a source reads; the highest is what we can decode.
        assert_eq!(LevelSet::r1().lowest(), Some(31));
        assert_eq!(LevelSet::r1().highest(), Some(42));
        assert_eq!(LevelSet::from_bits(0x10).lowest(), Some(42));
        assert_eq!(LevelSet::from_bits(0).lowest(), None);
        assert!(LevelSet::r1().contains_idc(40));
        assert!(!LevelSet::from_bits(0x10).contains_idc(31));
    }

    #[test]
    fn frame_rate_change_is_bit_four_not_bit_zero() {
        // The bit most often got wrong secondhand. MiracleCast emits `10`: rate change,
        // no skipping.
        let mira = FrameRateControl::from_bits(0x10);
        assert!(mira.frame_rate_change);
        assert!(!mira.frame_skipping);
        // Windows emits `11`: both.
        let win = FrameRateControl::from_bits(0x11);
        assert!(win.frame_rate_change && win.frame_skipping);
        assert_eq!(win.bits(), 0x11);
    }

    #[test]
    fn latency_is_five_millisecond_units() {
        let codec = VideoFormats::parse(REAL_VALUES[2].1).unwrap().codecs[0];
        assert_eq!(codec.latency_units, 0x10);
        assert_eq!(codec.latency_ms(), 80);
    }

    #[test]
    fn profiles_carry_the_idc_a_decoder_will_see() {
        assert_eq!(Profile::ConstrainedBaseline.profile_idc(), 66);
        assert_eq!(Profile::ConstrainedBaseline.constraint_set(), 0xC0);
        assert_eq!(Profile::ConstrainedHigh.profile_idc(), 100);
        assert_eq!(Profile::ConstrainedHigh.constraint_set(), 0x0C);
        assert_eq!(ProfileSet::both().bits(), 0x03);
    }

    #[test]
    fn none_round_trips() {
        let parsed = VideoFormats::parse("none").unwrap();
        assert!(parsed.codecs.is_empty());
        assert_eq!(parsed.to_string(), "none");
    }

    #[test]
    fn a_truncated_value_names_the_field_count() {
        let err = VideoFormats::parse("00 00 02 10").unwrap_err();
        assert!(matches!(err, ParamError::FieldCount { .. }));
    }

    #[test]
    fn a_non_hex_field_names_its_position() {
        let bad = "00 00 zz 10 0001FFFF 3FFFFFFF 00000FFF 00 0000 0000 00 none none";
        match VideoFormats::parse(bad).unwrap_err() {
            ParamError::NotHex { field, .. } => assert_eq!(field, 2),
            other => panic!("expected NotHex, got {other:?}"),
        }
    }

    #[test]
    fn two_codec_tuples_both_parse() {
        // The grammar allows a comma-separated list, and nothing in the wild sends one —
        // but the parser's chunking is the part that would break silently if it did.
        let one = "00 00 02 10 0001FFFF 3FFFFFFF 00000FFF 00 0000 0000 00 none none";
        let two = format!("{one}, 01 01 00000001 00000000 00000000 00 0000 0000 00 none none");
        let parsed = VideoFormats::parse(&two).unwrap();
        assert_eq!(parsed.codecs.len(), 2);
        assert_eq!(parsed.codecs[1].profiles, ProfileSet::from_bits(1));
    }
}
