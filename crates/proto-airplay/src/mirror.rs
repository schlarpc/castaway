//! The AirPlay mirroring data channel: pure, sans-I/O.
//!
//! A separate TCP connection carries the video, and it is **not RTP** — no sequence
//! numbers, no SSRC, nothing to reorder, because TCP already did that. `substrate-rtp`
//! does not apply here.
//!
//! ## Framing
//!
//! A fixed 128-byte header, little-endian throughout, then a payload:
//!
//! | Offset | Field |
//! |---|---|
//! | 0 | payload length, `u32` LE |
//! | 4 | payload type: 0 video, 1 codec config, 2 heartbeat, 5 stats |
//! | 6 | flags, `u16` LE — bit `0x40` means the sender is suspending the stream |
//! | 8 | NTP timestamp, `u64` **LE**, and with **no epoch offset** — it counts from the
//!       sender's last boot, unlike the timing channel's big-endian, 1900-based stamps |
//! | 16, 20 | source width/height, `f32` LE |
//! | 56, 60 | *encoded* width/height, `f32` LE — these are the ones the decoder wants |
//!
//! ## The keystream is continuous
//!
//! AES-128-**CTR**, and the counter runs unbroken across the whole connection rather
//! than restarting per frame. This is the opposite of Cast mirroring, which derives a
//! fresh nonce per frame and is therefore loss-tolerant — so the in-repo precedent in
//! `proto-cast` is exactly the wrong shape to copy here.
//!
//! The consequence is worth stating plainly: **a type-0 payload that is dropped or
//! reordered before decryption desynchronises the keystream permanently**, and every
//! frame after it is noise. Dropping late frames is still right, but it has to happen
//! after depacketisation, at the `EncodedFrame` level. Types 1, 2 and 5 consume no
//! keystream at all.

use std::fmt;
use std::sync::Arc;

use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use bytes::Bytes;
use castaway_core::{EncodedFrame, VideoCodec};
use sha2::{Digest as _, Sha512};

use crate::clock::StreamOrigin;
use crate::error::MirrorError;

/// AES-128-CTR, big-endian 128-bit counter — the same primitive Cast mirroring uses,
/// wired up in the opposite way (see the module docs).
type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// The fixed header on every message.
const HEADER_LEN: usize = 128;

/// Payload types.
mod payload_type {
    /// An access unit, AVCC-framed and encrypted.
    pub const VIDEO: u8 = 0;
    /// An `AVCDecoderConfigurationRecord`, in the clear.
    pub const CODEC_CONFIG: u8 = 1;
    /// A keep-alive from older senders; real iOS uses [`STATS`] instead.
    pub const HEARTBEAT: u8 = 2;
    /// A plist of sender-side frame statistics.
    pub const STATS: u8 = 5;
}

/// A video codec a mirroring sender may be offered.
///
/// Which of these we advertise is a *policy* decision — feature bit 42 — and it changes
/// what the sender encodes. Getting it wrong is not a decode error: with the bit set and
/// no HEVC path the sender emits an empty codec-config packet and simply stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MirrorCodec {
    /// H.264. Every sender can produce it, and it is what a sender falls back to.
    H264,
    /// HEVC. Offered only when feature bit 42 is set, and what a Mac sends for a
    /// desktop above 1080p.
    Hevc,
}

impl MirrorCodec {
    /// The name as it is normally written.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }
}

/// The `hvcC` marker that distinguishes an HEVC configuration record from an `avcC`.
const HVCC_MARKER: &[u8; 4] = b"hvc1";

/// Flag bit meaning "the sender is suspending this stream" (screen locked, app closed).
const FLAG_SUSPEND: u16 = 0x40;

/// The stream identity a `SETUP` names, and the seed for the key derivation.
///
/// A newtype over `u64` for one reason. It arrives in a binary plist, whose integers are
/// signed, and it is formatted into the string that gets hashed — so rendering it signed
/// produces a `-…` for any id at or above 2^63, a different hash input, and the classic
/// symptom: a correct picture for a while and then garbage. Keeping it `u64` with an
/// unsigned `Display` makes that unrepresentable rather than merely avoided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConnectionId(u64);

impl StreamConnectionId {
    /// Build one from the plist's signed integer, reinterpreting the bit pattern.
    #[must_use]
    pub const fn from_plist_signed(raw: i64) -> Self {
        #[allow(clippy::cast_sign_loss)]
        Self(raw as u64)
    }

    /// Build one from an already-unsigned value.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
}

impl fmt::Display for StreamConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The AES key and IV for one mirroring stream.
pub struct MirrorKeys {
    /// The cipher key.
    pub key: [u8; 16],
    /// The counter's starting value.
    pub iv: [u8; 16],
}

impl fmt::Debug for MirrorKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MirrorKeys(<redacted>)")
    }
}

impl MirrorKeys {
    /// Derive the stream key and IV from the FairPlay-unwrapped AES key.
    ///
    /// The hashed input is the literal string `AirPlayStreamKey<id>` followed by the AES
    /// key — **no separator and no terminating null**, because the original takes the
    /// `strlen` of a formatted buffer. Getting that wrong yields a plausible key that
    /// decrypts to noise.
    #[must_use]
    pub fn derive(aes_key: &[u8; 16], id: StreamConnectionId) -> Self {
        Self {
            key: hash_prefix(&format!("AirPlayStreamKey{id}"), aes_key),
            iv: hash_prefix(&format!("AirPlayStreamIV{id}"), aes_key),
        }
    }
}

/// SHA-512 over `label` then `key`, truncated to 16 bytes.
fn hash_prefix(label: &str, key: &[u8; 16]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label.as_bytes());
    hasher.update(key);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// The encoded geometry a type-1 packet reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// The source surface size.
    pub source: (f32, f32),
    /// The size actually encoded — what the decoder should be configured for. These
    /// differ: a real capture shows 864×648 source against 863×647 encoded.
    pub encoded: (f32, f32),
}

/// What one framed message turned out to be.
#[derive(Debug)]
#[non_exhaustive]
pub enum MirrorOutput {
    /// An access unit, decrypted and converted to Annex-B.
    Frame(Box<EncodedFrame>),
    /// The sender reported a new encode geometry (it re-sends this on every rotation).
    Geometry(Geometry),
    /// The sender is suspending the stream — screen locked, or the app went away.
    Suspend,
    /// The stream is resuming after a suspend; the decoder should be flushed.
    Resume,
}

/// The mirroring depacketiser for one connection.
pub struct MirrorStream {
    cipher: Aes128Ctr,
    /// SPS/PPS from the last type-1 packet, waiting for the access unit they describe.
    pending_parameter_sets: Option<(u64, Vec<u8>)>,
    /// The origin shared with this session's audio plane.
    origin: Arc<StreamOrigin>,
    /// Whether the sender has told us it is suspending.
    suspended: bool,
    /// Which codec the last configuration record named.
    codec: Option<MirrorCodec>,
}

impl MirrorStream {
    /// Start a stream with the derived keys, measuring against `origin`.
    ///
    /// The origin is shared with the audio plane so the two land on one timeline; the
    /// video header is already in the sender's nanosecond domain, so this plane needs no
    /// conversion to get there.
    #[must_use]
    pub fn new(keys: &MirrorKeys, origin: Arc<StreamOrigin>) -> Self {
        Self {
            // One cipher for the life of the connection. RustCrypto's `StreamCipher`
            // tracks the position within a block, so the carry-buffer dance the original
            // needs disappears — and restarting it is not expressible.
            cipher: Aes128Ctr::new(&keys.key.into(), &keys.iv.into()),
            pending_parameter_sets: None,
            origin,
            suspended: false,
            codec: None,
        }
    }

    /// Consume as many complete messages as `buf` holds, draining what it used.
    ///
    /// # Errors
    /// [`MirrorError`] if a header or payload is malformed. The connection should end:
    /// unlike a UDP audio session, a bad frame here means the byte stream has lost sync
    /// and nothing after it can be trusted.
    pub fn feed(&mut self, buf: &mut Vec<u8>) -> Result<Vec<MirrorOutput>, MirrorError> {
        let mut out = Vec::new();
        let mut consumed = 0usize;
        loop {
            let rest = &buf[consumed..];
            if rest.len() < HEADER_LEN {
                break;
            }
            let header = Header::parse(rest)?;
            let total = HEADER_LEN
                .checked_add(header.payload_len)
                .ok_or(MirrorError::PayloadTooLarge(header.payload_len))?;
            if rest.len() < total {
                break;
            }
            let payload = &rest[HEADER_LEN..total];
            self.handle(&header, payload, &mut out)?;
            consumed = consumed
                .checked_add(total)
                .ok_or(MirrorError::PayloadTooLarge(header.payload_len))?;
        }
        buf.drain(..consumed);
        Ok(out)
    }

    /// Dispatch one complete message.
    fn handle(
        &mut self,
        header: &Header,
        payload: &[u8],
        out: &mut Vec<MirrorOutput>,
    ) -> Result<(), MirrorError> {
        // A suspend flag can ride on any packet type.
        if header.suspending && !self.suspended {
            self.suspended = true;
            out.push(MirrorOutput::Suspend);
        } else if !header.suspending && self.suspended {
            self.suspended = false;
            out.push(MirrorOutput::Resume);
        }

        match header.kind {
            payload_type::VIDEO => self.video(header, payload, out),
            payload_type::CODEC_CONFIG => {
                // An empty codec-config payload is how a sender says "I want to send a
                // codec you did not advertise" — in practice HEVC, when feature bit 42
                // is clear. It is a refusal, not a frame.
                if payload.is_empty() {
                    return Err(MirrorError::CodecRefused);
                }
                // The record says which codec this is, and the sender picks based on what
                // we advertised — so this is where a mismatch between policy and
                // capability shows up, rather than as noise from the decoder.
                let codec = if payload.len() > 8 && &payload[4..8] == HVCC_MARKER {
                    MirrorCodec::Hevc
                } else {
                    MirrorCodec::H264
                };
                if self.codec.replace(codec) != Some(codec) {
                    tracing::info!(codec = codec.display_name(), "AirPlay mirroring codec");
                }
                let sets = match codec {
                    MirrorCodec::H264 => avc_parameter_sets(payload)?,
                    MirrorCodec::Hevc => hevc_parameter_sets(payload)?,
                };
                let geometry = header.geometry;
                self.pending_parameter_sets = Some((header.timestamp, sets));
                out.push(MirrorOutput::Geometry(geometry));
                Ok(())
            }
            // Heartbeats and the sender's own frame statistics need no answer. The stats
            // payload gains a large constant trailer when the sender's screen is locked,
            // which is why nothing here tries to parse it as a plist.
            payload_type::HEARTBEAT | payload_type::STATS => Ok(()),
            other => Err(MirrorError::UnknownPayloadType(other)),
        }
    }

    /// Decrypt an access unit and convert it to Annex-B.
    fn video(
        &mut self,
        header: &Header,
        payload: &[u8],
        out: &mut Vec<MirrorOutput>,
    ) -> Result<(), MirrorError> {
        // Only video payloads advance the keystream. Applying it to a type-1 or type-5
        // payload would desynchronise everything after.
        let mut data = payload.to_vec();
        self.cipher.apply_keystream(&mut data);

        // HEVC reads its NAL type from different bits, so the codec has to be known
        // before an access unit can be classified. A video packet before any
        // configuration record is a stream we cannot describe.
        let codec = self.codec.unwrap_or(MirrorCodec::H264);
        let keyframe = to_annex_b(&mut data, codec)?;

        // The type-1 packet that precedes a keyframe carries the same timestamp; its
        // SPS/PPS belong in-band at the front of this access unit, which is what the
        // decoder wants. A mismatch means the pair was broken up and the sets are stale.
        let mut framed = match self.pending_parameter_sets.take() {
            Some((ts, sets)) if ts == header.timestamp => {
                let mut v = sets;
                v.extend_from_slice(&data);
                v
            }
            _ => data,
        };
        framed.shrink_to_fit();

        // The header's clock counts nanoseconds since the sender booted, so a difference
        // is the only meaningful reading of it. `EncodedFrame::pts` is documented as time
        // since stream start, which is exactly what the shared origin produces.
        let pts = self.origin.pts(header.timestamp);

        out.push(MirrorOutput::Frame(Box::new(EncodedFrame {
            video_codec: Some(VideoCodec::H264),
            audio_codec: None,
            pts,
            keyframe,
            data: Bytes::from(framed),
        })));
        Ok(())
    }
}

/// The parsed 128-byte header.
#[derive(Debug, Clone, Copy)]
struct Header {
    payload_len: usize,
    kind: u8,
    suspending: bool,
    timestamp: u64,
    geometry: Geometry,
}

impl Header {
    fn parse(b: &[u8]) -> Result<Self, MirrorError> {
        let fixed: &[u8; HEADER_LEN] = b
            .get(..HEADER_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(MirrorError::ShortHeader(b.len()))?;
        let u32le =
            |i: usize| u32::from_le_bytes([fixed[i], fixed[i + 1], fixed[i + 2], fixed[i + 3]]);
        let f32le =
            |i: usize| f32::from_le_bytes([fixed[i], fixed[i + 1], fixed[i + 2], fixed[i + 3]]);
        let flags = u16::from_le_bytes([fixed[6], fixed[7]]);
        let mut ts = [0u8; 8];
        ts.copy_from_slice(&fixed[8..16]);
        Ok(Self {
            payload_len: usize::try_from(u32le(0)).map_err(|_| MirrorError::PayloadTooLarge(0))?,
            kind: fixed[4],
            suspending: flags & FLAG_SUSPEND != 0,
            // Little-endian, and with no 1900 epoch offset — see the module docs.
            timestamp: u64::from_le_bytes(ts),
            geometry: Geometry {
                source: (f32le(16), f32le(20)),
                encoded: (f32le(56), f32le(60)),
            },
        })
    }
}

/// Rewrite AVCC length prefixes as Annex-B start codes, in place.
///
/// Both are four bytes, so this is length-preserving and allocation-free — a property
/// worth keeping, since it runs on every frame.
///
/// Returns whether the access unit contains an IDR.
fn to_annex_b(data: &mut [u8], codec: MirrorCodec) -> Result<bool, MirrorError> {
    let mut offset = 0usize;
    let mut keyframe = false;
    while offset < data.len() {
        let end = offset
            .checked_add(4)
            .ok_or(MirrorError::MalformedAccessUnit)?;
        let header: [u8; 4] = data
            .get(offset..end)
            .and_then(|s| s.try_into().ok())
            .ok_or(MirrorError::MalformedAccessUnit)?;
        let nal_len = usize::try_from(u32::from_be_bytes(header))
            .map_err(|_| MirrorError::MalformedAccessUnit)?;
        let nal_end = end
            .checked_add(nal_len)
            .ok_or(MirrorError::MalformedAccessUnit)?;
        if nal_end > data.len() {
            return Err(MirrorError::MalformedAccessUnit);
        }
        // The cheapest decryption-failure detector there is: both codecs reserve this bit.
        if data[end] & 0x80 != 0 {
            return Err(MirrorError::MalformedAccessUnit);
        }
        // The NAL type lives in different bits in each: five bits in H.264, six shifted
        // by one in HEVC. Reading the wrong ones misses every keyframe, and the pipeline
        // waits for a keyframe before it will decode anything at all.
        keyframe |= match codec {
            MirrorCodec::H264 => data[end] & 0x1f == 5,
            MirrorCodec::Hevc => matches!((data[end] & 0x7e) >> 1, 16..=21),
        };
        data[offset..end].copy_from_slice(&[0, 0, 0, 1]);
        offset = nal_end;
    }
    // The walk has to land exactly on the end; anything else means the payload was not
    // what we decrypted it into.
    if offset == data.len() {
        Ok(keyframe)
    } else {
        Err(MirrorError::MalformedAccessUnit)
    }
}

/// Pull SPS and PPS out of an `AVCDecoderConfigurationRecord` as Annex-B.
fn avc_parameter_sets(record: &[u8]) -> Result<Vec<u8>, MirrorError> {
    // configurationVersion(1) profile(1) compat(1) level(1) lengthSize(1) numSPS(1)
    let sps_len_at = 6usize;
    let read_u16 = |at: usize| -> Result<usize, MirrorError> {
        record
            .get(at..at + 2)
            .map(|s| usize::from(u16::from_be_bytes([s[0], s[1]])))
            .ok_or(MirrorError::MalformedCodecConfig)
    };
    let sps_len = read_u16(sps_len_at)?;
    let sps_start = sps_len_at + 2;
    let sps = record
        .get(sps_start..sps_start + sps_len)
        .ok_or(MirrorError::MalformedCodecConfig)?;

    // numPPS, then the PPS itself.
    let pps_len_at = sps_start + sps_len + 1;
    let pps_len = read_u16(pps_len_at)?;
    let pps_start = pps_len_at + 2;
    let pps = record
        .get(pps_start..pps_start + pps_len)
        .ok_or(MirrorError::MalformedCodecConfig)?;

    let mut out = Vec::with_capacity(sps.len() + pps.len() + 8);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(sps);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(pps);
    Ok(out)
}

/// Pull VPS, SPS and PPS out of an `hvcC` record as Annex-B.
///
/// A different shape from `avcC`: the parameter sets are in an array of arrays, each
/// introduced by a type byte and a count, rather than a flat SPS-then-PPS pair. The
/// walk is written defensively because the offsets are the least corroborated thing in
/// this module — one reference implementation, no second source.
fn hevc_parameter_sets(record: &[u8]) -> Result<Vec<u8>, MirrorError> {
    // The array count sits at a fixed offset in the record; each entry is
    // `type(1) count(2) [ len(2) nal ]*`.
    const ARRAYS_AT: usize = 22;
    let num_arrays = usize::from(
        *record
            .get(ARRAYS_AT)
            .ok_or(MirrorError::MalformedCodecConfig)?,
    );
    let mut out = Vec::with_capacity(record.len());
    let mut at = ARRAYS_AT + 1;
    for _ in 0..num_arrays {
        let count = record
            .get(at + 1..at + 3)
            .map(|s| usize::from(u16::from_be_bytes([s[0], s[1]])))
            .ok_or(MirrorError::MalformedCodecConfig)?;
        at += 3;
        for _ in 0..count {
            let len = record
                .get(at..at + 2)
                .map(|s| usize::from(u16::from_be_bytes([s[0], s[1]])))
                .ok_or(MirrorError::MalformedCodecConfig)?;
            at += 2;
            let nal = record
                .get(at..at + len)
                .ok_or(MirrorError::MalformedCodecConfig)?;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
            at += len;
        }
    }
    if out.is_empty() {
        return Err(MirrorError::MalformedCodecConfig);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use super::*;

    fn keys() -> MirrorKeys {
        MirrorKeys::derive(b"0123456789abcdef", StreamConnectionId::new(1234))
    }

    /// Build a framed message.
    fn message(kind: u8, timestamp: u64, flags: u16, payload: &[u8]) -> Vec<u8> {
        let mut m = vec![0u8; HEADER_LEN];
        m[0..4].copy_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
        m[4] = kind;
        m[6..8].copy_from_slice(&flags.to_le_bytes());
        m[8..16].copy_from_slice(&timestamp.to_le_bytes());
        m[16..20].copy_from_slice(&864.0f32.to_le_bytes());
        m[20..24].copy_from_slice(&648.0f32.to_le_bytes());
        m[56..60].copy_from_slice(&863.0f32.to_le_bytes());
        m[60..64].copy_from_slice(&647.0f32.to_le_bytes());
        m.extend_from_slice(payload);
        m
    }

    /// An AVCC access unit with one NAL of `nal_type`.
    fn access_unit(nal_type: u8, body: &[u8]) -> Vec<u8> {
        let mut nal = vec![nal_type & 0x1f];
        nal.extend_from_slice(body);
        let mut au = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
        au.extend_from_slice(&nal);
        au
    }

    /// Encrypt the way a sender does — one continuous keystream.
    fn sender_cipher() -> Aes128Ctr {
        let k = keys();
        Aes128Ctr::new(&k.key.into(), &k.iv.into())
    }

    #[test]
    fn the_key_derivation_matches_the_documented_construction() {
        // No separator, no null: the label and the key are hashed back to back.
        let aes = *b"0123456789abcdef";
        let id = StreamConnectionId::new(4_964_383_553_955_644_435);
        let k = MirrorKeys::derive(&aes, id);
        let mut h = Sha512::new();
        h.update(b"AirPlayStreamKey4964383553955644435");
        h.update(aes);
        assert_eq!(k.key, h.finalize()[..16]);
    }

    #[test]
    fn a_stream_id_above_two_to_the_sixty_third_stays_unsigned() {
        // The trap: a plist integer is signed, and rendering this one signed gives a
        // leading '-', a different hash input, and video that turns to garbage partway.
        let id = StreamConnectionId::from_plist_signed(-1);
        assert_eq!(id.to_string(), "18446744073709551615");
        assert!(!id.to_string().starts_with('-'));
    }

    #[test]
    fn keys_are_not_printed_by_debug() {
        assert!(format!("{:?}", keys()).contains("redacted"));
    }

    #[test]
    fn a_codec_config_yields_geometry_and_arms_the_parameter_sets() {
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        // version, profile, compat, level, lengthSize, numSPS, spsLen, SPS, numPPS, ppsLen, PPS
        let record = [
            &[1u8, 100, 0xc0, 40, 0xff, 0xe1][..],
            &3u16.to_be_bytes()[..],
            &[0x67, 0x42, 0x00][..],
            &[1u8][..],
            &2u16.to_be_bytes()[..],
            &[0x68, 0xce][..],
        ]
        .concat();
        let mut buf = message(payload_type::CODEC_CONFIG, 500, 0, &record);
        let out = s.feed(&mut buf).unwrap();
        assert!(buf.is_empty(), "the message should be consumed");
        let [MirrorOutput::Geometry(g)] = out.as_slice() else {
            panic!("expected geometry, got {out:?}")
        };
        assert_eq!(g.source, (864.0, 648.0));
        // The encoded size is the one the decoder needs, and it is not the source size.
        assert_eq!(g.encoded, (863.0, 647.0));
    }

    #[test]
    fn parameter_sets_are_prepended_to_the_access_unit_that_shares_their_timestamp() {
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let record = [
            &[1u8, 100, 0xc0, 40, 0xff, 0xe1][..],
            &3u16.to_be_bytes()[..],
            &[0x67, 0x42, 0x00][..],
            &[1u8][..],
            &2u16.to_be_bytes()[..],
            &[0x68, 0xce][..],
        ]
        .concat();
        let mut buf = message(payload_type::CODEC_CONFIG, 900, 0, &record);

        // …then the IDR at the same timestamp.
        let mut au = access_unit(5, b"idr-payload");
        sender_cipher().apply_keystream(&mut au);
        buf.extend_from_slice(&message(payload_type::VIDEO, 900, 0, &au));

        let out = s.feed(&mut buf).unwrap();
        let frame = out
            .iter()
            .find_map(|o| match o {
                MirrorOutput::Frame(f) => Some(f),
                _ => None,
            })
            .expect("an access unit");
        assert!(frame.keyframe, "a type-5 NAL is an IDR");
        // SPS start code, SPS, PPS start code, PPS, then the access unit.
        assert_eq!(&frame.data[..4], &[0, 0, 0, 1]);
        assert_eq!(&frame.data[4..7], &[0x67, 0x42, 0x00]);
        assert_eq!(&frame.data[7..11], &[0, 0, 0, 1]);
        assert_eq!(&frame.data[11..13], &[0x68, 0xce]);
        assert_eq!(
            &frame.data[13..17],
            &[0, 0, 0, 1],
            "the AU's own start code"
        );
    }

    /// An `hvcC` record with one VPS, one SPS and one PPS.
    fn hvcc_record() -> Vec<u8> {
        let mut r = vec![0u8; 22];
        r[4..8].copy_from_slice(b"hvc1");
        r.push(3); // three arrays
        for (ty, nal) in [
            (32u8, &[0x40, 0x01][..]),
            (33, &[0x42, 0x01][..]),
            (34, &[0x44, 0x01][..]),
        ] {
            r.push(ty);
            r.extend_from_slice(&1u16.to_be_bytes());
            r.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
            r.extend_from_slice(nal);
        }
        r
    }

    #[test]
    fn an_hvcc_record_yields_all_three_parameter_sets() {
        // A different shape from avcC: arrays of arrays rather than a flat SPS/PPS pair,
        // and HEVC needs the VPS too.
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut buf = message(payload_type::CODEC_CONFIG, 100, 0, &hvcc_record());
        let out = s.feed(&mut buf).unwrap();
        assert!(matches!(out.as_slice(), [MirrorOutput::Geometry(_)]));

        // The sets are prepended to the access unit at the same timestamp.
        let mut nal = vec![(19 << 1) & 0x7e]; // IDR_W_RADL
        nal.extend_from_slice(b"hevc-idr");
        let mut au = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
        au.extend_from_slice(&nal);
        sender_cipher().apply_keystream(&mut au);
        let mut buf = message(payload_type::VIDEO, 100, 0, &au);
        let out = s.feed(&mut buf).unwrap();
        let [MirrorOutput::Frame(f)] = out.as_slice() else {
            panic!("expected a frame, got {out:?}")
        };
        // VPS, SPS, PPS, then the access unit — four start codes in all.
        assert!(f.data.windows(4).filter(|w| w == b"\0\0\0\x01").count() >= 4);
        assert_eq!(&f.data[..4], &[0, 0, 0, 1]);
        assert_eq!(&f.data[4..6], &[0x40, 0x01], "VPS first");
        assert!(f.keyframe, "HEVC NAL type 19 is an IDR");
    }

    #[test]
    fn hevc_keyframes_are_read_from_the_right_bits() {
        // H.264 takes five bits and HEVC six shifted by one. Reading the wrong ones
        // misses every keyframe, and the pipeline waits for one before decoding at all.
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut buf = message(payload_type::CODEC_CONFIG, 0, 0, &hvcc_record());
        s.feed(&mut buf).unwrap();

        let mut cipher = sender_cipher();
        for (nal_type, expect_key) in [(19u8, true), (1, false)] {
            let mut nal = vec![(nal_type << 1) & 0x7e];
            nal.extend_from_slice(b"payload!");
            let mut au = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
            au.extend_from_slice(&nal);
            cipher.apply_keystream(&mut au);
            let mut buf = message(payload_type::VIDEO, 0, 0, &au);
            let out = s.feed(&mut buf).unwrap();
            let frame = out.iter().find_map(|o| match o {
                MirrorOutput::Frame(f) => Some(f),
                _ => None,
            });
            assert_eq!(
                frame.expect("a frame").keyframe,
                expect_key,
                "HEVC NAL type {nal_type}"
            );
        }
    }

    #[test]
    fn avcc_becomes_annex_b_without_changing_length() {
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let plain = access_unit(1, b"not-a-keyframe");
        let mut au = plain.clone();
        sender_cipher().apply_keystream(&mut au);
        let mut buf = message(payload_type::VIDEO, 0, 0, &au);
        let out = s.feed(&mut buf).unwrap();
        let [MirrorOutput::Frame(f)] = out.as_slice() else {
            panic!("expected a frame, got {out:?}")
        };
        assert_eq!(
            f.data.len(),
            plain.len(),
            "the rewrite is length-preserving"
        );
        assert_eq!(&f.data[..4], &[0, 0, 0, 1]);
        assert!(!f.keyframe);
    }

    #[test]
    fn the_keystream_runs_unbroken_across_messages() {
        // The detail that makes this different from Cast: restarting the counter per
        // frame would decode the first frame and turn everything after it into noise.
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut cipher = sender_cipher();
        let mut buf = Vec::new();
        let mut expected = Vec::new();
        for i in 0..3u8 {
            let plain = access_unit(1, &[i; 40]);
            expected.push(plain.clone());
            let mut au = plain;
            cipher.apply_keystream(&mut au);
            buf.extend_from_slice(&message(payload_type::VIDEO, u64::from(i) * 1000, 0, &au));
        }
        let out = s.feed(&mut buf).unwrap();
        let frames: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                MirrorOutput::Frame(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(frames.len(), 3);
        for (frame, plain) in frames.iter().zip(&expected) {
            // Same bytes, only the length prefix rewritten.
            assert_eq!(&frame.data[4..], &plain[4..]);
        }
    }

    #[test]
    fn non_video_payloads_do_not_consume_keystream() {
        // A heartbeat between two frames must not shift the cipher, or every frame after
        // it decodes to noise.
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut cipher = sender_cipher();
        let plain = access_unit(1, b"aaaaaaaaaaaaaaaa");
        let mut au = plain.clone();
        cipher.apply_keystream(&mut au);

        let mut buf = message(payload_type::HEARTBEAT, 0, 0, &[]);
        buf.extend_from_slice(&message(payload_type::STATS, 0, 0, b"stats-blob"));
        buf.extend_from_slice(&message(payload_type::VIDEO, 0, 0, &au));

        let out = s.feed(&mut buf).unwrap();
        let [MirrorOutput::Frame(f)] = out.as_slice() else {
            panic!("expected exactly one frame, got {out:?}")
        };
        assert_eq!(&f.data[4..], &plain[4..]);
    }

    #[test]
    fn pts_is_measured_from_the_first_message_not_from_the_senders_uptime() {
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut cipher = sender_cipher();
        let mut buf = Vec::new();
        // The sender's clock counts from its own boot — hours in.
        for ts in [50_000_000_000u64, 50_016_000_000] {
            let mut au = access_unit(1, b"frame-data-here!");
            cipher.apply_keystream(&mut au);
            buf.extend_from_slice(&message(payload_type::VIDEO, ts, 0, &au));
        }
        let out = s.feed(&mut buf).unwrap();
        let frames: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                MirrorOutput::Frame(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(frames[0].pts, Duration::ZERO);
        assert_eq!(frames[1].pts, Duration::from_millis(16));
    }

    #[test]
    fn a_partial_message_is_left_in_the_buffer() {
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut au = access_unit(1, b"0123456789abcdef");
        sender_cipher().apply_keystream(&mut au);
        let whole = message(payload_type::VIDEO, 0, 0, &au);

        // Feed everything but the last byte: nothing should be produced or consumed.
        let mut buf = whole[..whole.len() - 1].to_vec();
        let before = buf.len();
        assert!(s.feed(&mut buf).unwrap().is_empty());
        assert_eq!(buf.len(), before, "an incomplete message stays buffered");

        buf.push(whole[whole.len() - 1]);
        assert_eq!(s.feed(&mut buf).unwrap().len(), 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn a_suspend_flag_is_reported_once_and_resumes() {
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut buf = message(payload_type::HEARTBEAT, 0, FLAG_SUSPEND, &[]);
        assert!(matches!(
            s.feed(&mut buf).unwrap().as_slice(),
            [MirrorOutput::Suspend]
        ));
        // A second suspending packet is not a second event.
        let mut buf = message(payload_type::HEARTBEAT, 0, FLAG_SUSPEND, &[]);
        assert!(s.feed(&mut buf).unwrap().is_empty());
        // …and clearing it resumes.
        let mut buf = message(payload_type::HEARTBEAT, 0, 0, &[]);
        assert!(matches!(
            s.feed(&mut buf).unwrap().as_slice(),
            [MirrorOutput::Resume]
        ));
    }

    #[test]
    fn an_empty_codec_config_is_the_senders_way_of_refusing() {
        // What a sender sends when it wants HEVC and we did not advertise bit 42. It is
        // a refusal, and reporting it as such is the difference between a diagnosable
        // failure and a stream that simply never starts.
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut buf = message(payload_type::CODEC_CONFIG, 0, 0, &[]);
        assert!(matches!(s.feed(&mut buf), Err(MirrorError::CodecRefused)));
    }

    #[test]
    fn a_payload_that_does_not_decrypt_is_refused_rather_than_decoded() {
        // Wrong key: the NAL walk will not land on the end of the buffer, and H.264's
        // forbidden-zero bit is very likely set. Either way it must not reach a decoder.
        let mut s = MirrorStream::new(&keys(), Arc::new(StreamOrigin::new()));
        let mut buf = message(payload_type::VIDEO, 0, 0, &[0xff; 64]);
        assert!(matches!(
            s.feed(&mut buf),
            Err(MirrorError::MalformedAccessUnit)
        ));
    }
}
