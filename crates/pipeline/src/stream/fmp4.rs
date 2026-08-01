//! Fragmented MP4 (CMAF) boxing, and the H.264 bitstream repacking that feeds it.
//!
//! An HLS player wants two kinds of resource: one *initialisation* segment describing the
//! track, and a run of *media* segments each carrying a `moof` + `mdat` pair. Neither is
//! big, and both are pure functions of the encoder's output — so they live here, sans
//! I/O, and are fixture-tested in every build (ground rule 6) rather than only where a
//! GPU and an encoder happen to exist.
//!
//! The one subtlety is that libavcodec's H.264 encoders emit **Annex-B** — NAL units
//! separated by start codes, parameter sets in a side-channel — while MP4 wants **AVCC**:
//! four-byte length prefixes, parameter sets hoisted into an `avcC` record in the sample
//! description. ffmpeg's muxers do that conversion internally, and since we are the muxer
//! here it is [`annexb_to_avcc`] and [`AvcConfig::from_extradata`] instead. Reimplementing
//! it is what keeps the segment bytes something a test can assert on.

use crate::error::PipelineError;

/// The media timescale, in ticks per second.
///
/// 90 kHz is the conventional video timescale, and it divides evenly by every frame rate
/// this stream will ever run at — 3000 ticks at 30 fps, 1500 at 60 — so a sample duration
/// is exact and the presentation timeline never accumulates rounding.
pub const TIMESCALE: u32 = 90_000;

/// The track id. One video track, and nothing here is general enough to want a second.
const TRACK_ID: u32 = 1;

/// One coded picture, ready to go in an `mdat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The access unit in AVCC form: a run of four-byte-length-prefixed NAL units.
    pub data: Vec<u8>,
    /// How long it is shown, in [`TIMESCALE`] ticks.
    pub duration: u32,
    /// Whether a player may start decoding here. Segments begin on one of these.
    pub keyframe: bool,
}

/// The parameter sets a decoder needs before it can touch a sample.
///
/// Held apart from the samples on purpose: in fMP4 they belong to the init segment, are
/// sent once, and are what makes a media segment startable at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AvcConfig {
    /// Sequence parameter sets, without start codes or length prefixes.
    pub sps: Vec<Vec<u8>>,
    /// Picture parameter sets, likewise.
    pub pps: Vec<Vec<u8>>,
}

impl AvcConfig {
    /// Read an encoder's `extradata`.
    ///
    /// Two spellings arrive here and both are legitimate. Most of libavcodec's H.264
    /// encoders hand back Annex-B parameter sets under `AV_CODEC_FLAG_GLOBAL_HEADER`;
    /// some hardware wrappers hand back a ready-made `AVCDecoderConfigurationRecord`.
    /// The first byte tells them apart — a configuration record opens with its version,
    /// `1`, where Annex-B opens with a start-code zero — so this parses whichever it was
    /// given rather than asserting which one it should have been.
    ///
    /// # Errors
    /// [`PipelineError::Stream`] if neither form yields at least one SPS and one PPS,
    /// which is the only outcome the caller can do anything about: without both there is
    /// no init segment to write.
    pub fn from_extradata(data: &[u8]) -> Result<Self, PipelineError> {
        let parsed = if data.first() == Some(&1) && data.len() >= 7 {
            Self::from_avcc_record(data)
        } else {
            Self::from_annexb(data)
        };
        if parsed.sps.is_empty() || parsed.pps.is_empty() {
            return Err(PipelineError::Stream(format!(
                "encoder extradata carried {} SPS and {} PPS; need at least one of each",
                parsed.sps.len(),
                parsed.pps.len()
            )));
        }
        Ok(parsed)
    }

    fn from_annexb(data: &[u8]) -> Self {
        let mut out = Self::default();
        for nal in nal_units(data) {
            match NalKind::of(nal) {
                Some(NalKind::Sps) => out.sps.push(nal.to_vec()),
                Some(NalKind::Pps) => out.pps.push(nal.to_vec()),
                _ => {}
            }
        }
        out
    }

    /// Unpack an `AVCDecoderConfigurationRecord` back into its parameter sets.
    ///
    /// Deliberately total rather than fallible: a record that runs out mid-way yields the
    /// sets it did contain, and the emptiness check in [`Self::from_extradata`] is the one
    /// place a malformed record is reported. Two error paths for "there was nothing
    /// usable in here" would only differ in their wording.
    fn from_avcc_record(data: &[u8]) -> Self {
        let mut out = Self::default();
        let mut at = 5usize;
        let Some(&counts) = data.get(at) else {
            return out;
        };
        at += 1;
        let take = |at: &mut usize, count: usize, into: &mut Vec<Vec<u8>>| {
            for _ in 0..count {
                let Some(len) = data
                    .get(*at..*at + 2)
                    .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
                else {
                    return;
                };
                *at += 2;
                let Some(nal) = data.get(*at..*at + len) else {
                    return;
                };
                *at += len;
                into.push(nal.to_vec());
            }
        };
        take(&mut at, usize::from(counts & 0x1f), &mut out.sps);
        let Some(&pps_count) = data.get(at) else {
            return out;
        };
        at += 1;
        take(&mut at, usize::from(pps_count), &mut out.pps);
        out
    }

    /// The three profile bytes an `avc1.xxxxxx` codec string and the `avcC` header both
    /// need: profile, constraint flags, level. Taken from the first SPS, which is where
    /// they are defined.
    fn profile(&self) -> [u8; 3] {
        self.sps
            .first()
            .and_then(|sps| sps.get(1..4))
            .map_or([0x42, 0x00, 0x1f], |b| [b[0], b[1], b[2]])
    }

    /// The RFC 6381 codec string, as an MSE `SourceBuffer` mime type wants it.
    #[must_use]
    pub fn codec_string(&self) -> String {
        let [profile, constraints, level] = self.profile();
        format!("avc1.{profile:02X}{constraints:02X}{level:02X}")
    }

    /// The body of the `avcC` box.
    fn avcc_body(&self) -> Vec<u8> {
        let [profile, constraints, level] = self.profile();
        let mut out = vec![
            1,
            profile,
            constraints,
            level,
            // Six reserved bits set, then `lengthSizeMinusOne` = 3: the four-byte NAL
            // length prefixes `annexb_to_avcc` writes.
            0xff,
            // Three reserved bits set, then the SPS count in five.
            0xe0 | u8_count(self.sps.len(), 0x1f),
        ];
        for sps in &self.sps {
            push_len_prefixed(&mut out, sps);
        }
        out.push(u8_count(self.pps.len(), 0xff));
        for pps in &self.pps {
            push_len_prefixed(&mut out, pps);
        }
        // The profile-dependent tail. Only defined for the profiles that can carry
        // something other than 8-bit 4:2:0, and only those may read it — writing it for
        // Baseline would be four bytes a conforming parser is entitled to reject.
        //
        // The values are constants rather than parsed out of the SPS because *we* opened
        // the encoder, and it was opened for NV12: 4:2:0, eight bits, both planes.
        if matches!(profile, 100 | 110 | 122 | 144) {
            out.extend_from_slice(&[
                0xfc | 1, // chroma_format_idc = 1 (4:2:0)
                0xf8,     // bit_depth_luma_minus8 = 0
                0xf8,     // bit_depth_chroma_minus8 = 0
                0,        // no SPS extensions
            ]);
        }
        out
    }
}

/// Clamp a count into the bits the format gives it.
///
/// Both fields this feeds are narrower than a `usize`, and an encoder that somehow
/// produced more parameter sets than fit should truncate the *count* rather than wrap it
/// into a small number and desynchronise every parser downstream.
fn u8_count(n: usize, mask: u8) -> u8 {
    u8::try_from(n).unwrap_or(mask) & mask
}

fn push_len_prefixed(out: &mut Vec<u8>, nal: &[u8]) {
    let len = u16::try_from(nal.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&nal[..usize::from(len)]);
}

/// The NAL types this module has an opinion about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NalKind {
    /// An IDR slice: a picture a decoder can start on.
    IdrSlice,
    /// Sequence parameter set — hoisted into `avcC`, dropped from samples.
    Sps,
    /// Picture parameter set — likewise.
    Pps,
    /// Access unit delimiter: a framing hint Annex-B needs and MP4 does not.
    AccessUnitDelimiter,
    /// Anything else, carried through untouched.
    Other,
}

impl NalKind {
    fn of(nal: &[u8]) -> Option<Self> {
        Some(match nal.first()? & 0x1f {
            5 => Self::IdrSlice,
            7 => Self::Sps,
            8 => Self::Pps,
            9 => Self::AccessUnitDelimiter,
            _ => Self::Other,
        })
    }

    /// Whether this NAL belongs in an `mdat` sample.
    const fn belongs_in_sample(self) -> bool {
        match self {
            Self::IdrSlice | Self::Other => true,
            // The parameter sets are in the init segment and repeating them per-sample is
            // just bytes; the delimiter's whole job was to mark boundaries a length prefix
            // already marks.
            Self::Sps | Self::Pps | Self::AccessUnitDelimiter => false,
        }
    }
}

/// Split an Annex-B buffer into its NAL units, start codes removed.
///
/// Accepts both spellings of the start code (three bytes and four) because encoders mix
/// them within a single access unit — libx264 writes the long form before parameter sets
/// and the short form before slices.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Find every start-code position first, so each NAL's end is simply the next one's
    // beginning. Scanning forward for "the next start code" from inside the loop is the
    // same work and reads worse.
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                starts.push((i, i + 3));
                i += 3;
                continue;
            }
            if data.get(i + 2) == Some(&0) && data.get(i + 3) == Some(&1) {
                starts.push((i, i + 4));
                i += 4;
                continue;
            }
        }
        i += 1;
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|(code_at, _)| *code_at)
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .map(move |((_, body), end)| &data[body..end])
        .filter(|nal| !nal.is_empty())
}

/// Repack one Annex-B access unit as AVCC, dropping what the container already carries.
///
/// Returns `None` when nothing survived — an access unit of pure parameter sets, which is
/// what some encoders emit alongside a keyframe and which is not a sample.
#[must_use]
pub fn annexb_to_avcc(access_unit: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(access_unit.len());
    for nal in nal_units(access_unit) {
        if !NalKind::of(nal).is_some_and(NalKind::belongs_in_sample) {
            continue;
        }
        let len = u32::try_from(nal.len()).ok()?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    (!out.is_empty()).then_some(out)
}

// --- box plumbing -------------------------------------------------------------------

/// Start a box, leaving room for the size that is not known yet. Returns the offset to
/// hand [`close_box`] once the body is written.
fn open_box(out: &mut Vec<u8>, kind: &[u8; 4]) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    at
}

/// Backfill a box's size. A box longer than 4 GiB cannot be expressed in the 32-bit form
/// and cannot occur here: the largest thing written is one segment's `mdat`.
fn close_box(out: &mut [u8], at: usize) {
    let len = u32::try_from(out.len() - at).unwrap_or(u32::MAX);
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

/// A full box: the four version/flags bytes every `FullBox` in ISO-BMFF opens with.
fn open_full_box(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32) -> usize {
    let at = open_box(out, kind);
    out.push(version);
    out.extend_from_slice(&flags.to_be_bytes()[1..]);
    at
}

fn u32_be(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn u16_be(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

// --- init segment -------------------------------------------------------------------

/// The initialisation segment: everything a player needs before the first sample.
///
/// `width`/`height` are the coded dimensions, which for this stream are also the display
/// dimensions — the compositor's output is square-pixel, so there is no aspect ratio to
/// carry separately.
#[must_use]
pub fn init_segment(config: &AvcConfig, width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);

    let at = open_box(&mut out, b"ftyp");
    out.extend_from_slice(b"iso6");
    u32_be(&mut out, 1);
    for brand in [b"iso6", b"mp41", b"avc1", b"cmfc"] {
        out.extend_from_slice(brand);
    }
    close_box(&mut out, at);

    let moov = open_box(&mut out, b"moov");
    write_mvhd(&mut out);
    write_trak(&mut out, config, width, height);
    write_mvex(&mut out);
    close_box(&mut out, moov);

    out
}

fn write_mvhd(out: &mut Vec<u8>) {
    let at = open_full_box(out, b"mvhd", 0, 0);
    u32_be(out, 0); // creation time
    u32_be(out, 0); // modification time
    u32_be(out, TIMESCALE);
    // Duration zero: this is a live stream and it does not have one. A fragmented file
    // says so here and lets `tfdt` carry the timeline instead.
    u32_be(out, 0);
    u32_be(out, 0x0001_0000); // rate 1.0
    u16_be(out, 0x0100); // volume 1.0
    u16_be(out, 0); // reserved
    u32_be(out, 0);
    u32_be(out, 0);
    write_unity_matrix(out);
    for _ in 0..6 {
        u32_be(out, 0); // pre_defined
    }
    u32_be(out, TRACK_ID + 1); // next_track_ID
    close_box(out, at);
}

/// The identity transformation matrix, in the 16.16/2.30 fixed point ISO-BMFF uses.
fn write_unity_matrix(out: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32_be(out, v);
    }
}

fn write_trak(out: &mut Vec<u8>, config: &AvcConfig, width: u32, height: u32) {
    let trak = open_box(out, b"trak");

    // flags 0x7: enabled, in the movie, in the preview.
    let at = open_full_box(out, b"tkhd", 0, 0x7);
    u32_be(out, 0);
    u32_be(out, 0);
    u32_be(out, TRACK_ID);
    u32_be(out, 0); // reserved
    u32_be(out, 0); // duration, as in `mvhd`
    u32_be(out, 0);
    u32_be(out, 0);
    u16_be(out, 0); // layer
    u16_be(out, 0); // alternate group
    u16_be(out, 0); // volume: zero for video
    u16_be(out, 0);
    write_unity_matrix(out);
    u32_be(out, width << 16); // 16.16 fixed point
    u32_be(out, height << 16);
    close_box(out, at);

    let mdia = open_box(out, b"mdia");
    let at = open_full_box(out, b"mdhd", 0, 0);
    u32_be(out, 0);
    u32_be(out, 0);
    u32_be(out, TIMESCALE);
    u32_be(out, 0);
    // ISO 639-2/T packed five bits per letter: "und".
    u16_be(out, 0x55c4);
    u16_be(out, 0);
    close_box(out, at);

    let at = open_full_box(out, b"hdlr", 0, 0);
    u32_be(out, 0); // pre_defined
    out.extend_from_slice(b"vide");
    u32_be(out, 0);
    u32_be(out, 0);
    u32_be(out, 0);
    out.extend_from_slice(b"VideoHandler\0");
    close_box(out, at);

    let minf = open_box(out, b"minf");
    // flags 1: the graphics mode below is the only one, per spec.
    let at = open_full_box(out, b"vmhd", 0, 1);
    u16_be(out, 0); // graphicsmode: copy
    for _ in 0..3 {
        u16_be(out, 0); // opcolor
    }
    close_box(out, at);

    let dinf = open_box(out, b"dinf");
    let at = open_full_box(out, b"dref", 0, 0);
    u32_be(out, 1);
    // flags 1 on the entry: the media is in this same file, so the URL is empty.
    let url = open_full_box(out, b"url ", 0, 1);
    close_box(out, url);
    close_box(out, at);
    close_box(out, dinf);

    let stbl = open_box(out, b"stbl");
    let at = open_full_box(out, b"stsd", 0, 0);
    u32_be(out, 1); // entry_count
    write_avc1(out, config, width, height);
    close_box(out, at);
    // Four empty tables. A fragmented track carries its timing in `trun`, but the boxes
    // themselves are mandatory and a parser that does not find them gives up on the track.
    for kind in [b"stts", b"stsc", b"stco"] {
        let at = open_full_box(out, kind, 0, 0);
        u32_be(out, 0);
        close_box(out, at);
    }
    let at = open_full_box(out, b"stsz", 0, 0);
    u32_be(out, 0); // sample_size: per-sample, and there are none
    u32_be(out, 0); // sample_count
    close_box(out, at);
    close_box(out, stbl);

    close_box(out, minf);
    close_box(out, mdia);
    close_box(out, trak);
}

fn write_avc1(out: &mut Vec<u8>, config: &AvcConfig, width: u32, height: u32) {
    let at = open_box(out, b"avc1");
    for _ in 0..6 {
        out.push(0); // reserved
    }
    u16_be(out, 1); // data_reference_index
    u16_be(out, 0); // pre_defined
    u16_be(out, 0); // reserved
    for _ in 0..3 {
        u32_be(out, 0); // pre_defined
    }
    u16_be(out, u16::try_from(width).unwrap_or(u16::MAX));
    u16_be(out, u16::try_from(height).unwrap_or(u16::MAX));
    u32_be(out, 0x0048_0000); // 72 dpi horizontal
    u32_be(out, 0x0048_0000); // 72 dpi vertical
    u32_be(out, 0); // reserved
    u16_be(out, 1); // frame_count
    out.extend_from_slice(&[0u8; 32]); // compressorname
    u16_be(out, 0x0018); // depth: 24-bit colour
    out.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined

    let avcc = open_box(out, b"avcC");
    out.extend_from_slice(&config.avcc_body());
    close_box(out, avcc);

    // What the encoder was told to produce, said out loud. The NV12 conversion pass works
    // in BT.709 with limited-range samples, and a player that assumes BT.601 on a stream
    // that omits this renders it visibly off — the same class of quiet wrongness
    // `crate::color` exists to prevent on the way in.
    let colr = open_box(out, b"colr");
    out.extend_from_slice(b"nclx");
    u16_be(out, 1); // colour primaries: BT.709
    u16_be(out, 1); // transfer characteristics: BT.709
    u16_be(out, 1); // matrix coefficients: BT.709
    out.push(0); // full_range_flag: limited
    close_box(out, colr);

    close_box(out, at);
}

fn write_mvex(out: &mut Vec<u8>) {
    let mvex = open_box(out, b"mvex");
    let at = open_full_box(out, b"trex", 0, 0);
    u32_be(out, TRACK_ID);
    u32_be(out, 1); // default_sample_description_index
                    // Every other default is zero, because `trun` states duration, size and flags for
                    // every sample explicitly. Defaults that disagree with an explicit value are a class
                    // of bug that only shows up in one player.
    u32_be(out, 0);
    u32_be(out, 0);
    u32_be(out, 0);
    close_box(out, at);
    close_box(out, mvex);
}

// --- media segments -----------------------------------------------------------------

/// Sample flags marking a picture others may depend on and nothing depends on before it.
const SAMPLE_FLAGS_KEY: u32 = 0x0200_0000;
/// …and its opposite: depends on others, and is not a sync sample.
const SAMPLE_FLAGS_DELTA: u32 = 0x0101_0000;

/// One media segment: `moof` + `mdat`, self-contained and playable after the init segment.
///
/// `base_decode_time` is where this segment sits on the track timeline in [`TIMESCALE`]
/// ticks. It is the only thing tying a segment to its neighbours, which is what lets the
/// ring in [`super::hls`] forget old ones without renumbering anything.
#[must_use]
pub fn media_segment(sequence: u32, base_decode_time: u64, samples: &[Sample]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.iter().map(|s| s.data.len()).sum::<usize>() + 256);

    let moof = open_box(&mut out, b"moof");
    let at = open_full_box(&mut out, b"mfhd", 0, 0);
    u32_be(&mut out, sequence);
    close_box(&mut out, at);

    let traf = open_box(&mut out, b"traf");
    // flags 0x020000: default-base-is-moof. The alternative is a base offset into the
    // whole file, which a live stream delivered as separate resources does not have.
    let at = open_full_box(&mut out, b"tfhd", 0, 0x0002_0000);
    u32_be(&mut out, TRACK_ID);
    close_box(&mut out, at);

    let at = open_full_box(&mut out, b"tfdt", 1, 0);
    out.extend_from_slice(&base_decode_time.to_be_bytes());
    close_box(&mut out, at);

    // data-offset (0x1), sample-duration (0x100), sample-size (0x200), sample-flags
    // (0x400). No composition offsets: the encoder is opened with no B-frames, so
    // decode order is presentation order and there is nothing to shift.
    let trun = open_full_box(&mut out, b"trun", 0, 0x0000_0701);
    u32_be(&mut out, u32::try_from(samples.len()).unwrap_or(u32::MAX));
    // Backfilled once the `moof` is closed: the offset is measured from the start of the
    // `moof`, and the `moof` is not finished until the `trun` inside it is.
    let data_offset_at = out.len();
    u32_be(&mut out, 0);
    for sample in samples {
        u32_be(&mut out, sample.duration);
        u32_be(
            &mut out,
            u32::try_from(sample.data.len()).unwrap_or(u32::MAX),
        );
        u32_be(
            &mut out,
            if sample.keyframe {
                SAMPLE_FLAGS_KEY
            } else {
                SAMPLE_FLAGS_DELTA
            },
        );
    }
    close_box(&mut out, trun);
    close_box(&mut out, traf);
    close_box(&mut out, moof);

    let moof_len = out.len() - moof;
    // The `mdat` payload starts eight bytes past the box header that follows the `moof`.
    let data_offset = u32::try_from(moof_len + 8).unwrap_or(u32::MAX);
    out[data_offset_at..data_offset_at + 4].copy_from_slice(&data_offset.to_be_bytes());

    let at = open_box(&mut out, b"mdat");
    for sample in samples {
        out.extend_from_slice(&sample.data);
    }
    close_box(&mut out, at);

    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Walk a buffer's top-level boxes, so a test can assert on structure rather than on
    /// a byte offset it worked out by hand once.
    fn top_level(data: &[u8]) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            let kind = String::from_utf8_lossy(&data[at + 4..at + 8]).into_owned();
            out.push((kind, len));
            if len < 8 {
                break;
            }
            at += len;
        }
        out
    }

    /// Where a box's four-character name sits, searching past `from`.
    ///
    /// The offset is not decoration: `ftyp` lists `avc1` among its compatible brands, so
    /// a search from zero finds the brand and then reads a sample entry's fields out of
    /// the file header.
    fn find_box_from(data: &[u8], kind: &[u8; 4], from: usize) -> Option<usize> {
        data[from..]
            .windows(4)
            .position(|w| w == kind)
            .map(|at| at + from)
    }

    /// Where `moov` begins, i.e. past every brand `ftyp` advertises. Zero for a media
    /// segment, which has no file header to skip.
    fn after_ftyp(data: &[u8]) -> usize {
        match top_level(data).first() {
            Some((kind, len)) if kind == "ftyp" => *len,
            _ => 0,
        }
    }

    fn find_box(data: &[u8], kind: &[u8; 4]) -> Option<usize> {
        find_box_from(data, kind, after_ftyp(data))
    }

    /// How far into a box's body its child boxes start, for the boxes that have any.
    ///
    /// Most containers are pure — their body *is* the children. Two are not: `stsd` puts
    /// a version/flags word and an entry count first, and a visual sample entry has 78
    /// bytes of fields before `avcC`. Walking into either at offset zero would read a
    /// field as a length and wander off the end, which is why this is a table rather than
    /// a set.
    fn child_offset(kind: &[u8; 4]) -> Option<usize> {
        match kind {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"dinf" | b"stbl" | b"mvex" | b"moof"
            | b"traf" => Some(0),
            b"stsd" => Some(8),
            b"avc1" => Some(78),
            _ => None,
        }
    }

    /// Walk the box tree, checking that every declared size lands exactly on the next
    /// box's start, and hand back the names in order.
    ///
    /// A box whose size was never backfilled reads as zero, which every real parser treats
    /// as "the rest of the file" or as a hard error depending on the parser — so it is the
    /// class of mistake that produces a segment one player takes and another refuses.
    fn walk(data: &[u8], depth: usize, into: &mut Vec<String>) {
        let mut at = 0usize;
        while at + 8 <= data.len() {
            let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = data[at + 4..at + 8].try_into().unwrap();
            let name = String::from_utf8_lossy(&kind).into_owned();
            assert!(
                (8..=data.len() - at).contains(&len),
                "{name} at depth {depth} declares {len} bytes, {} remain",
                data.len() - at
            );
            into.push(name);
            if let Some(skip) = child_offset(&kind) {
                walk(&data[at + 8 + skip..at + len], depth + 1, into);
            }
            at += len;
        }
        assert_eq!(at, data.len(), "boxes at depth {depth} do not tile exactly");
    }

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40];
    const PPS: &[u8] = &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

    fn config() -> AvcConfig {
        AvcConfig {
            sps: vec![SPS.to_vec()],
            pps: vec![PPS.to_vec()],
        }
    }

    #[test]
    fn annexb_extradata_yields_the_parameter_sets() {
        let mut extradata = vec![0, 0, 0, 1];
        extradata.extend_from_slice(SPS);
        extradata.extend_from_slice(&[0, 0, 0, 1]);
        extradata.extend_from_slice(PPS);
        assert_eq!(AvcConfig::from_extradata(&extradata).unwrap(), config());
    }

    #[test]
    fn an_avcc_record_round_trips_through_extradata_parsing() {
        // Some hardware encoders hand back a finished configuration record rather than
        // Annex-B, and reading it as Annex-B finds no start codes and no parameter sets
        // at all — an init segment that names a decoder and gives it nothing to configure
        // itself with.
        let body = config().avcc_body();
        assert_eq!(AvcConfig::from_extradata(&body).unwrap(), config());
    }

    #[test]
    fn extradata_with_no_parameter_sets_is_an_error_not_an_empty_config() {
        // The failure this guards is a segment that plays as a black rectangle: an init
        // segment is structurally fine with an empty `avcC`, so nothing downstream
        // notices until a decoder refuses it.
        let err = AvcConfig::from_extradata(&[0, 0, 0, 1, 0x65, 0x88]).unwrap_err();
        assert!(matches!(err, PipelineError::Stream(_)), "{err:?}");
    }

    #[test]
    fn both_start_code_lengths_split_the_same_way() {
        // libx264 mixes them within one access unit: four bytes before parameter sets,
        // three before slices.
        let data = [
            0, 0, 0, 1, 0x67, 0xaa, // four-byte
            0, 0, 1, 0x68, 0xbb, 0xcc, // three-byte
            0, 0, 0, 1, 0x65, 0xdd,
        ];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(
            nals,
            vec![
                &[0x67u8, 0xaa][..],
                &[0x68, 0xbb, 0xcc][..],
                &[0x65, 0xdd][..]
            ]
        );
    }

    #[test]
    fn repacking_drops_what_the_container_already_carries() {
        // Parameter sets live in `avcC` and the delimiter's job is done by the length
        // prefix. Leaving them in is not fatal, but it is bytes on every keyframe and it
        // lets a stale in-band SPS disagree with the one in the init segment.
        let mut au = vec![0, 0, 0, 1, 0x09, 0x10]; // AUD
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(SPS);
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(PPS);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0xde, 0xad]);

        let avcc = annexb_to_avcc(&au).unwrap();
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 0xde, 0xad]);
    }

    #[test]
    fn an_access_unit_of_only_parameter_sets_is_not_a_sample() {
        // Encoders emit these next to a keyframe. Written as a zero-length sample it
        // becomes a frame the player must decode and cannot.
        let mut au = vec![0, 0, 0, 1];
        au.extend_from_slice(SPS);
        assert!(annexb_to_avcc(&au).is_none());
    }

    #[test]
    fn the_init_segment_has_the_boxes_a_player_looks_for() {
        let init = init_segment(&config(), 1280, 720);
        let boxes: Vec<String> = top_level(&init).into_iter().map(|(k, _)| k).collect();
        assert_eq!(boxes, vec!["ftyp", "moov"]);
        // The sizes have to add up exactly, or every parser stops at the first box whose
        // length walked it off the end.
        assert_eq!(
            top_level(&init).iter().map(|(_, n)| n).sum::<usize>(),
            init.len()
        );
        for kind in [
            b"mvhd", b"trak", b"tkhd", b"mdia", b"mdhd", b"hdlr", b"minf", b"vmhd", b"dinf",
            b"stbl", b"stsd", b"avc1", b"avcC", b"colr", b"mvex", b"trex",
        ] {
            assert!(
                find_box(&init, kind).is_some(),
                "missing {}",
                String::from_utf8_lossy(kind)
            );
        }
    }

    #[test]
    fn every_box_in_both_segments_declares_a_size_that_tiles() {
        // The failure this exists for: a container opened and never closed keeps the
        // zero its size placeholder was written with. `traf` was exactly that, and the
        // by-name assertions above all still passed — a parser reading the tree is the
        // only thing that notices.
        let mut names = Vec::new();
        walk(&init_segment(&config(), 640, 360), 0, &mut names);
        for kind in ["moov", "trak", "stbl", "avcC", "trex"] {
            assert!(names.contains(&kind.to_string()), "missing {kind}");
        }

        let mut names = Vec::new();
        walk(
            &media_segment(
                1,
                0,
                &[Sample {
                    data: vec![0, 0, 0, 1, 0x65],
                    duration: 3000,
                    keyframe: true,
                }],
            ),
            0,
            &mut names,
        );
        assert_eq!(
            names,
            ["moof", "mfhd", "traf", "tfhd", "tfdt", "trun", "mdat"]
        );
    }

    #[test]
    fn the_track_carries_the_dimensions_it_was_given() {
        let init = init_segment(&config(), 1280, 720);
        let avc1 = find_box(&init, b"avc1").unwrap();
        // Width and height sit 24 bytes past the box name in a visual sample entry.
        let w = u16::from_be_bytes(init[avc1 + 4 + 24..avc1 + 4 + 26].try_into().unwrap());
        let h = u16::from_be_bytes(init[avc1 + 4 + 26..avc1 + 4 + 28].try_into().unwrap());
        assert_eq!((w, h), (1280, 720));
    }

    #[test]
    fn the_codec_string_names_the_profile_the_sps_declares() {
        // What an MSE `SourceBuffer` is opened with. Getting it wrong is a stream that
        // never appends a byte, with no error worth reading.
        assert_eq!(config().codec_string(), "avc1.64001F");
    }

    #[test]
    fn a_media_segment_points_trun_at_its_own_mdat() {
        // The offset is from the start of the `moof`, and it is the one field here that
        // cannot be checked by eye: get it wrong and the player decodes box headers as
        // slice data.
        let samples = vec![
            Sample {
                data: vec![0, 0, 0, 2, 0x65, 0x01],
                duration: 3000,
                keyframe: true,
            },
            Sample {
                data: vec![0, 0, 0, 1, 0x41],
                duration: 3000,
                keyframe: false,
            },
        ];
        let seg = media_segment(7, 90_000, &samples);
        let boxes = top_level(&seg);
        assert_eq!(
            boxes.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["moof", "mdat"]
        );
        let moof_len = boxes[0].1;

        let trun = find_box(&seg, b"trun").unwrap();
        // trun: name(4) version+flags(4) sample_count(4) then data_offset.
        let offset_at = trun + 4 + 4 + 4;
        let data_offset =
            u32::from_be_bytes(seg[offset_at..offset_at + 4].try_into().unwrap()) as usize;
        assert_eq!(data_offset, moof_len + 8);
        assert_eq!(&seg[data_offset..data_offset + 6], &samples[0].data[..]);
    }

    #[test]
    fn the_first_sample_is_flagged_as_one_a_player_may_start_on() {
        // A segment whose keyframe is marked as a delta sample is a segment the player
        // will not seek to — the stream starts only if you happen to join at the right
        // instant, which looks like flakiness rather than a wrong constant.
        let samples = vec![
            Sample {
                data: vec![0, 0, 0, 1, 0x65],
                duration: 3000,
                keyframe: true,
            },
            Sample {
                data: vec![0, 0, 0, 1, 0x41],
                duration: 3000,
                keyframe: false,
            },
        ];
        let seg = media_segment(1, 0, &samples);
        let trun = find_box(&seg, b"trun").unwrap();
        let first = trun + 4 + 4 + 4 + 4; // …past name, version/flags, count, data offset
        let flags = u32::from_be_bytes(seg[first + 8..first + 12].try_into().unwrap());
        assert_eq!(flags, SAMPLE_FLAGS_KEY);
        let second = first + 12;
        let flags = u32::from_be_bytes(seg[second + 8..second + 12].try_into().unwrap());
        assert_eq!(flags, SAMPLE_FLAGS_DELTA);
    }

    #[test]
    fn the_segment_timeline_is_carried_in_tfdt() {
        let seg = media_segment(
            3,
            270_000,
            &[Sample {
                data: vec![0, 0, 0, 1, 0x65],
                duration: 3000,
                keyframe: true,
            }],
        );
        let tfdt = find_box(&seg, b"tfdt").unwrap();
        let base = u64::from_be_bytes(seg[tfdt + 8..tfdt + 16].try_into().unwrap());
        assert_eq!(base, 270_000);
    }
}
