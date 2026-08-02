//! Cast Streaming's own RTP framing, frame reassembly, and the NACK set a receiver
//! owes its sender.
//!
//! Cast does not use a standard RTP payload format. On top of the RFC 3550 fixed
//! header it adds six bytes of its own — key-frame bit, truncated frame id, packet id,
//! max packet id — plus optional fields, and it expects RTCP feedback naming the
//! packets that never arrived. The layout and field semantics here were derived from
//! openscreen's `cast/streaming/impl/rtp_defines.h` and `rtp_packet_parser.cc`; the
//! tests run against packet fixtures copied verbatim from openscreen's parser fuzzer
//! seed corpus (`tests/fixtures/rtp/`).
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+ ^
//! |V=2|P|X| CC=0  |M|      PT     |      sequence number          | |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+RTP
//! |                         RTP timestamp                         |Spec
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+ |
//! |              synchronization source (SSRC)                    | v
//! +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
//! |K|R| EXT count |  FID          |              PID              | ^
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+Cast
//! |             Max PID           |  optional RFID, extensions,    Spec
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+  then payload...               v
//! ```
//!
//! Pure and sans-I/O (ground rule 3): [`CastRtpStream::parse`] folds bytes into a
//! typed packet and [`FrameCollector`] accumulates packets into one frame. The UDP
//! socket lives in the actor.

use bytes::Bytes;
use thiserror::Error;

/// Ethernet MTU minus IPv4 and UDP headers — the largest *UDP payload* that crosses a
/// typical LAN unfragmented, and so the budget anything we build to send must fit.
///
/// AOSP names the same number `kMaxUDPPacketSize` and glosses it "Really UDP _payload_
/// size". Budgeting at the 1500 MTU instead sends a 1528-byte IP packet, which fragments
/// — and for RTCP feedback that only happens under heavy loss, when fragmentation is
/// exactly what one does not want.
pub const MAX_PACKET_SIZE_IPV4: usize = 1500 - 20 - 8;

/// The smallest packet that can carry a complete Cast header: 12 RTP + 6 Cast.
const MIN_VALID_SIZE: usize = 18;

/// Cast requires version 2, no padding, no RTP extension, and zero CSRCs — so the
/// whole first byte is fixed, not just the version field.
const REQUIRED_FIRST_BYTE: u8 = 0b1000_0000;

const PAYLOAD_TYPE_MASK: u8 = 0b0111_1111;
const KEY_FRAME_BIT: u8 = 0b1000_0000;
const HAS_REFERENCE_FRAME_ID_BIT: u8 = 0b0100_0000;
const EXTENSION_COUNT_MASK: u8 = 0b0011_1111;

/// The reserved packet id meaning "none of this frame's packets arrived". It is only
/// ever *sent* in RTCP feedback; a data packet carrying it is malformed, which is why
/// [`PacketId`] refuses to hold it.
const ALL_PACKETS_LOST: u16 = 0xffff;

/// The one Cast RTP extension we understand; all others are skipped by length.
const ADAPTIVE_LATENCY_EXT_TYPE: u16 = 1;
/// The extension header packs a 6-bit type and a 10-bit size into one `u16`.
const EXT_SIZE_BITS: u32 = 10;
const EXT_SIZE_MASK: u16 = (1 << EXT_SIZE_BITS) - 1;

/// Why a datagram was not a Cast RTP packet.
///
/// Every variant is a reason to drop the datagram and keep the stream running — a
/// receiver on a real network sees these constantly and must not treat them as fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum RtpError {
    /// Shorter than the 18-byte minimum Cast header.
    #[error("packet shorter than the minimum Cast RTP header")]
    TooShort,

    /// The first byte was not exactly `0x80`. Cast pins padding, extension and CSRC
    /// count to zero, so anything else is a different protocol or a corrupt packet.
    #[error("first byte {0:#04x} is not a Cast RTP header")]
    NotCastRtp(u8),

    /// The payload type was outside the ranges Cast assigns.
    #[error("unassigned RTP payload type {0}")]
    BadPayloadType(u8),

    /// The packet's SSRC belongs to a different stream than this parser tracks.
    #[error("SSRC {got:#010x} does not belong to this stream ({want:#010x})")]
    WrongSsrc {
        /// The SSRC in the packet.
        got: u32,
        /// The SSRC this parser was constructed for.
        want: u32,
    },

    /// `max_packet_id` held the reserved all-packets-lost sentinel.
    #[error("max packet id is the reserved all-packets-lost value")]
    ReservedMaxPacketId,

    /// `packet_id > max_packet_id`, so the packet claims to be past the end of
    /// its own frame.
    #[error("packet id {packet} is past this frame's last packet {max}")]
    PacketIdOutOfRange {
        /// The offending packet id.
        packet: u16,
        /// The frame's declared last packet id.
        max: u16,
    },

    /// An optional field or extension ran off the end of the buffer.
    #[error("truncated Cast RTP header (optional field or extension overran)")]
    Truncated,

    /// The adaptive-latency extension must carry exactly two bytes.
    #[error("adaptive-latency extension has {0} bytes, expected 2")]
    BadLatencyExtension(usize),
}

/// Re-expand a wire field that was truncated to its low bits.
///
/// Cast truncates frame ids to 8 bits and RTP timestamps to 32, then relies on the
/// peer knowing roughly where the sequence is to reconstruct the full value. This is
/// openscreen's `ExpandedValueBase::Expand`: take the largest value that could
/// plausibly be meant (`reference` plus half the truncated field's range), graft the
/// received low bits onto its high bits, and step down one full period if that
/// overshot.
///
/// `bits` is the width of the truncated field, so `modulus` is its range. `truncated`
/// is a `u32` because that is the widest field Cast truncates, and it widens to `i64`
/// losslessly — no cast can wrap here.
fn expand(reference: i64, truncated: u32, bits: u32) -> i64 {
    let modulus = 1i64 << bits;
    // Half the range in either direction is the design limit: values further apart
    // than this cannot be told from their neighbours, which is why Cast bounds how
    // many frames may be in flight.
    let max_possible = reference + (modulus / 2 - 1);
    let low_mask = modulus - 1;
    let candidate = (max_possible & !low_mask) | (i64::from(truncated) & low_mask);
    if candidate > max_possible {
        candidate - modulus
    } else {
        candidate
    }
}

/// A frame's identifier, expanded from the 8 bits the wire carries.
///
/// Signed and 64-bit because the sequence must be orderable across wraparound and
/// because [`Self::leader`] — "before the first frame" — has to be representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(i64);

impl FrameId {
    /// The first frame of a stream.
    #[must_use]
    pub const fn first() -> Self {
        Self(0)
    }

    /// A frame that never exists, standing for "nothing received yet". Named after
    /// the blank leader tape before a recording starts, as openscreen does.
    #[must_use]
    pub const fn leader() -> Self {
        Self(-1)
    }

    /// Construct from a full-width value.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// The full-width value.
    #[must_use]
    pub const fn value(self) -> i64 {
        self.0
    }

    /// The low 8 bits, as they appear in an RTP or RTCP header.
    #[must_use]
    pub const fn lower_8_bits(self) -> u8 {
        // `to_le_bytes()[0]` rather than `as u8`: same bits, but it reads as
        // "take the low byte" instead of tripping the truncation lint.
        self.0.to_le_bytes()[0]
    }

    /// The low 32 bits, which is what the AES nonce mixes in.
    #[must_use]
    pub const fn lower_32_bits(self) -> u32 {
        let [a, b, c, d, ..] = self.0.to_le_bytes();
        u32::from_le_bytes([a, b, c, d])
    }

    /// The next frame in sequence.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// The previous frame in sequence.
    #[must_use]
    pub const fn previous(self) -> Self {
        Self(self.0 - 1)
    }

    /// Expand a truncated 8-bit frame id against this value as the reference point.
    #[must_use]
    pub fn expand(self, truncated: u8) -> Self {
        Self(expand(self.0, u32::from(truncated), 8))
    }
}

/// A media timestamp, expanded from the 32 bits the wire carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RtpTimestamp(i64);

impl RtpTimestamp {
    /// The zero point of a stream's timeline.
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// The full-width tick count.
    #[must_use]
    pub const fn value(self) -> i64 {
        self.0
    }

    /// Expand a truncated 32-bit timestamp against this value as the reference point.
    #[must_use]
    pub fn expand(self, truncated: u32) -> Self {
        Self(expand(self.0, truncated, 32))
    }
}

/// A packet's index within its frame.
///
/// Cannot hold `ALL_PACKETS_LOST`: that value means "the whole frame is missing" in
/// RTCP feedback, so letting it into a packet id would make [`NackTarget`] ambiguous.
/// The sentinel is expressed by [`NackTarget::AllPackets`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketId(u16);

impl PacketId {
    /// The first packet of a frame — the only one carrying complete frame metadata.
    pub const ZERO: Self = Self(0);

    /// Construct a packet id, rejecting the reserved all-packets-lost value.
    #[must_use]
    pub const fn new(value: u16) -> Option<Self> {
        if value == ALL_PACKETS_LOST {
            None
        } else {
            Some(Self(value))
        }
    }

    /// The wire value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// How a frame depends on earlier frames — the receiver needs this to decide whether
/// it can start decoding here or must wait for a key frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dependency {
    /// Decodable on its own; a valid entry point into the stream.
    KeyFrame,
    /// Not flagged as a key frame, but references only itself.
    Independent,
    /// Needs [`FrameHeader::referenced_frame_id`] to have been decoded first.
    Dependent,
}

/// One parsed Cast RTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastRtpPacket {
    /// The Cast payload type byte (informative — the codec comes from the OFFER).
    pub payload_type: u8,
    /// The RTP sequence number, which counts packets rather than frames.
    pub sequence_number: u16,
    /// The media timestamp, re-expanded to full width.
    pub rtp_timestamp: RtpTimestamp,
    /// Whether the sender flagged this frame as a key frame.
    pub is_key_frame: bool,
    /// The frame this packet belongs to, re-expanded to full width.
    pub frame_id: FrameId,
    /// This packet's index within the frame.
    pub packet_id: PacketId,
    /// The frame's last packet index, so the receiver knows the expected count from
    /// any single packet.
    pub max_packet_id: PacketId,
    /// The frame that must be decoded before this one.
    pub referenced_frame_id: FrameId,
    /// A requested change to the end-to-end playout delay, in milliseconds, if the
    /// adaptive-latency extension was present.
    pub new_playout_delay_ms: Option<u16>,
    /// This packet's slice of the frame payload, still encrypted.
    pub payload: Bytes,
}

impl CastRtpPacket {
    /// The number of packets this frame was split into.
    #[must_use]
    pub const fn frame_packet_count(&self) -> u32 {
        self.max_packet_id.get() as u32 + 1
    }
}

/// The parser for one stream (one SSRC).
///
/// Stateful on purpose: frame ids and timestamps arrive truncated, and re-expanding
/// them needs the highest values seen so far. One instance per SSRC — feeding it
/// another stream's packets would corrupt those reference points, which is why the
/// SSRC is checked on every packet.
#[derive(Debug, Clone)]
pub struct CastRtpStream {
    sender_ssrc: u32,
    highest_frame_id: FrameId,
    last_timestamp: RtpTimestamp,
}

impl CastRtpStream {
    /// Start parsing the stream with the given sender SSRC.
    #[must_use]
    pub const fn new(sender_ssrc: u32) -> Self {
        Self {
            sender_ssrc,
            highest_frame_id: FrameId::first(),
            last_timestamp: RtpTimestamp::zero(),
        }
    }

    /// The SSRC this parser accepts.
    #[must_use]
    pub const fn sender_ssrc(&self) -> u32 {
        self.sender_ssrc
    }

    /// Parse one datagram.
    ///
    /// The reference points for re-expansion only advance on a fully valid packet, so
    /// a corrupt datagram cannot drag the stream's idea of "now" forward.
    ///
    /// # Errors
    /// [`RtpError`] if the datagram is not a well-formed packet of this stream. Every
    /// variant means "drop this datagram"; none means "tear down the session".
    pub fn parse(&mut self, buf: &Bytes) -> Result<CastRtpPacket, RtpError> {
        if buf.len() < MIN_VALID_SIZE {
            return Err(RtpError::TooShort);
        }
        if buf[0] != REQUIRED_FIRST_BYTE {
            return Err(RtpError::NotCastRtp(buf[0]));
        }

        // The marker bit is deliberately ignored. The spec says it marks the last
        // packet of a frame, but openscreen notes senders that don't set it, and
        // `max_packet_id` already carries that information.
        let payload_type = buf[1] & PAYLOAD_TYPE_MASK;
        if !is_assigned_payload_type(payload_type) {
            return Err(RtpError::BadPayloadType(payload_type));
        }
        let sequence_number = u16::from_be_bytes([buf[2], buf[3]]);
        let raw_timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let rtp_timestamp = self.last_timestamp.expand(raw_timestamp);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        if ssrc != self.sender_ssrc {
            return Err(RtpError::WrongSsrc {
                got: ssrc,
                want: self.sender_ssrc,
            });
        }

        let byte12 = buf[12];
        let is_key_frame = (byte12 & KEY_FRAME_BIT) != 0;
        let has_reference_frame_id = (byte12 & HAS_REFERENCE_FRAME_ID_BIT) != 0;
        let extension_count = usize::from(byte12 & EXTENSION_COUNT_MASK);

        let frame_id = self.highest_frame_id.expand(buf[13]);
        let raw_packet_id = u16::from_be_bytes([buf[14], buf[15]]);
        let raw_max_packet_id = u16::from_be_bytes([buf[16], buf[17]]);

        // Check the max first: if it is the sentinel the comparison below would be
        // meaningless, and a frame cannot legitimately claim 65536 packets.
        let Some(max_packet_id) = PacketId::new(raw_max_packet_id) else {
            return Err(RtpError::ReservedMaxPacketId);
        };
        if raw_packet_id > raw_max_packet_id {
            return Err(RtpError::PacketIdOutOfRange {
                packet: raw_packet_id,
                max: raw_max_packet_id,
            });
        }
        // Unreachable given the check above, but expressed as a real branch rather
        // than an unwrap so the invariant survives future edits (ground rule 7).
        let Some(packet_id) = PacketId::new(raw_packet_id) else {
            return Err(RtpError::ReservedMaxPacketId);
        };

        let mut offset = MIN_VALID_SIZE;
        let referenced_frame_id = if has_reference_frame_id {
            let byte = *buf.get(offset).ok_or(RtpError::Truncated)?;
            offset += 1;
            frame_id.expand(byte)
        } else if is_key_frame {
            // Absent an explicit reference, a key frame stands alone and any other
            // frame is assumed to depend on its immediate predecessor.
            frame_id
        } else {
            frame_id.previous()
        };

        let mut new_playout_delay_ms = None;
        for _ in 0..extension_count {
            let header_end = offset.checked_add(2).ok_or(RtpError::Truncated)?;
            if buf.len() < header_end {
                return Err(RtpError::Truncated);
            }
            let type_and_size = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
            let ext_type = type_and_size >> EXT_SIZE_BITS;
            let ext_size = usize::from(type_and_size & EXT_SIZE_MASK);
            offset = header_end;
            let data_end = offset.checked_add(ext_size).ok_or(RtpError::Truncated)?;
            if buf.len() < data_end {
                return Err(RtpError::Truncated);
            }
            if ext_type == ADAPTIVE_LATENCY_EXT_TYPE {
                if ext_size != 2 {
                    return Err(RtpError::BadLatencyExtension(ext_size));
                }
                new_playout_delay_ms = Some(u16::from_be_bytes([buf[offset], buf[offset + 1]]));
            }
            // Unknown extensions are skipped by their declared size, which is how a
            // receiver stays forward-compatible with senders that add new ones.
            offset = data_end;
        }

        let payload = buf.slice(offset..);

        // Only now that the packet is known good may the expansion reference points
        // move.
        self.last_timestamp = rtp_timestamp;
        self.highest_frame_id = self.highest_frame_id.max(frame_id);

        Ok(CastRtpPacket {
            payload_type,
            sequence_number,
            rtp_timestamp,
            is_key_frame,
            frame_id,
            packet_id,
            max_packet_id,
            referenced_frame_id,
            new_playout_delay_ms,
            payload,
        })
    }
}

/// Cast assigns payload types out of RTP's dynamic range: 96-99 audio, 100-104 video.
///
/// 127 and 96 are also accepted because some AndroidTV receivers demanded them
/// regardless of codec, and senders still emit that pairing for compatibility. The
/// value is informative only — the codec comes from the OFFER — so this is just a
/// corruption check.
const fn is_assigned_payload_type(pt: u8) -> bool {
    matches!(pt, 96..=104 | 127)
}

/// What a NACK asks the sender to retransmit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NackTarget {
    /// Not one packet of the frame has arrived. Serialized as the reserved
    /// all-packets-lost id.
    AllPackets,
    /// One specific packet is missing.
    Packet(PacketId),
}

impl NackTarget {
    /// The 16-bit value this target serializes to in RTCP feedback.
    #[must_use]
    pub const fn wire_value(self) -> u16 {
        match self {
            Self::AllPackets => ALL_PACKETS_LOST,
            Self::Packet(id) => id.get(),
        }
    }
}

/// A request to retransmit part of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketNack {
    /// The frame that is incomplete.
    pub frame_id: FrameId,
    /// What is missing from it.
    pub target: NackTarget,
}

/// Frame metadata, taken from packet 0 — the only packet required to carry a complete
/// set of values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Which frame this is.
    pub frame_id: FrameId,
    /// Whether it can be decoded standalone.
    pub dependency: Dependency,
    /// The frame it depends on.
    pub referenced_frame_id: FrameId,
    /// Its media timestamp.
    pub rtp_timestamp: RtpTimestamp,
    /// A requested playout-delay change, if any.
    pub new_playout_delay_ms: Option<u16>,
}

/// A complete frame: metadata plus the concatenated, still-encrypted payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedFrame {
    /// The frame's metadata.
    pub header: FrameHeader,
    /// Payload chunks in packet order, ready to decrypt.
    pub payload: Bytes,
}

/// Whether a packet added anything to a frame.
///
/// Retransmission means the same packet can arrive twice, so a duplicate is ordinary
/// traffic and not a [`CollectError`] — but the caller still needs to tell the two
/// apart, because only a new packet can be what completes a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// The packet filled a hole.
    New,
    /// We already had this packet.
    Duplicate,
}

/// Accumulates the packets of a single frame.
///
/// A frame arrives split across packets that may be reordered, duplicated, or lost.
/// The collector is told which frame it is collecting up front, so a packet for any
/// other frame is a routing bug and is refused rather than silently accepted.
#[derive(Debug, Clone)]
pub struct FrameCollector {
    frame_id: FrameId,
    header: Option<FrameHeader>,
    /// `None` until the first packet reveals the count.
    chunks: Option<Vec<Option<Bytes>>>,
    missing: usize,
}

impl FrameCollector {
    /// Begin collecting the given frame.
    #[must_use]
    pub const fn new(frame_id: FrameId) -> Self {
        Self {
            frame_id,
            header: None,
            chunks: None,
            missing: 0,
        }
    }

    /// The frame being collected.
    #[must_use]
    pub const fn frame_id(&self) -> FrameId {
        self.frame_id
    }

    /// Whether every packet has arrived.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.chunks.is_some() && self.missing == 0
    }

    /// The frame's metadata, once packet 0 has arrived.
    ///
    /// Lets a caller decide whether a frame is worth delivering — is it independent,
    /// does it change the playout delay — without consuming the collector.
    #[must_use]
    pub const fn header(&self) -> Option<&FrameHeader> {
        self.header.as_ref()
    }

    /// Add a packet.
    ///
    /// # Errors
    /// [`CollectError`] if the packet belongs to a different frame, or if its packet
    /// count disagrees with the count already established for this frame.
    pub fn collect(&mut self, packet: &CastRtpPacket) -> Result<Accepted, CollectError> {
        if packet.frame_id != self.frame_id {
            return Err(CollectError::WrongFrame {
                want: self.frame_id,
                got: packet.frame_id,
            });
        }

        let count = packet.frame_packet_count() as usize;
        let chunks = match &mut self.chunks {
            Some(chunks) => {
                if chunks.len() != count {
                    // Two packets of one frame disagreeing on its length means one of
                    // them is corrupt; trust the established value and drop this one.
                    return Err(CollectError::PacketCountMismatch {
                        established: chunks.len(),
                        got: count,
                    });
                }
                chunks
            }
            slot => {
                self.missing = count;
                slot.insert(vec![None; count])
            }
        };

        let index = usize::from(packet.packet_id.get());
        let Some(chunk) = chunks.get_mut(index) else {
            // Unreachable: the parser already rejected packet_id > max_packet_id, and
            // count derives from max_packet_id. Kept as a branch, not an index panic.
            return Err(CollectError::PacketCountMismatch {
                established: chunks.len(),
                got: index + 1,
            });
        };
        if chunk.is_some() {
            return Ok(Accepted::Duplicate);
        }
        *chunk = Some(packet.payload.clone());
        self.missing -= 1;

        if packet.packet_id == PacketId::ZERO {
            self.header = Some(FrameHeader {
                frame_id: packet.frame_id,
                dependency: if packet.is_key_frame {
                    Dependency::KeyFrame
                } else if packet.frame_id == packet.referenced_frame_id {
                    Dependency::Independent
                } else {
                    Dependency::Dependent
                },
                referenced_frame_id: packet.referenced_frame_id,
                rtp_timestamp: packet.rtp_timestamp,
                new_playout_delay_ms: packet.new_playout_delay_ms,
            });
        }

        Ok(Accepted::New)
    }

    /// The NACKs describing what is still missing.
    ///
    /// Empty once complete. If nothing at all has arrived, this is a single
    /// [`NackTarget::AllPackets`] rather than one NACK per packet — the whole point of
    /// the sentinel is that a receiver that has seen nothing does not yet know how
    /// many packets to ask for.
    #[must_use]
    pub fn missing_packets(&self) -> Vec<PacketNack> {
        let Some(chunks) = &self.chunks else {
            return vec![PacketNack {
                frame_id: self.frame_id,
                target: NackTarget::AllPackets,
            }];
        };
        if self.missing == 0 {
            return Vec::new();
        }
        if self.missing >= chunks.len() {
            return vec![PacketNack {
                frame_id: self.frame_id,
                target: NackTarget::AllPackets,
            }];
        }
        chunks
            .iter()
            .enumerate()
            .filter(|(_, chunk)| chunk.is_none())
            .filter_map(|(index, _)| {
                // `index` is bounded by chunks.len() <= 65535, so the conversion and
                // the PacketId both hold; a failure here would mean a corrupt count.
                let raw = u16::try_from(index).ok()?;
                Some(PacketNack {
                    frame_id: self.frame_id,
                    target: NackTarget::Packet(PacketId::new(raw)?),
                })
            })
            .collect()
    }

    /// Take the assembled frame, if every packet has arrived.
    ///
    /// Consumes the collector: a frame is delivered once.
    #[must_use]
    pub fn take_frame(self) -> Option<EncryptedFrame> {
        if !self.is_complete() {
            return None;
        }
        let chunks = self.chunks?;
        let header = self.header?;
        let total: usize = chunks.iter().flatten().map(bytes::Bytes::len).sum();
        let mut payload = bytes::BytesMut::with_capacity(total);
        for chunk in chunks.iter().flatten() {
            payload.extend_from_slice(chunk);
        }
        Some(EncryptedFrame {
            header,
            payload: payload.freeze(),
        })
    }
}

/// Why a packet could not be added to a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum CollectError {
    /// The packet belongs to a different frame than this collector.
    #[error("packet is for frame {got:?}, collector is assembling {want:?}")]
    WrongFrame {
        /// The frame being collected.
        want: FrameId,
        /// The frame the packet claimed.
        got: FrameId,
    },

    /// Packets of the same frame disagreed about how many packets it has.
    #[error("frame was established as {established} packets, this packet says {got}")]
    PacketCountMismatch {
        /// The count taken from the first packet seen.
        established: usize,
        /// The count this packet implies.
        got: usize,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// openscreen's fuzzer seeds all use this SSRC.
    const SEED_SSRC: u32 = 0x0102_0304;

    fn fixture(name: &str) -> Bytes {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rtp/");
        Bytes::from(std::fs::read(format!("{path}{name}")).unwrap())
    }

    #[test]
    fn parses_openscreen_key_frame_seed() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let p = stream
            .parse(&fixture("rtp_packet_for_key_frame.bin"))
            .unwrap();
        assert_eq!(p.payload_type, 96);
        assert_eq!(p.sequence_number, 0xbeef);
        assert_eq!(p.rtp_timestamp.value(), 0x0908_0706);
        assert!(p.is_key_frame);
        assert_eq!(p.frame_id, FrameId::new(5));
        assert_eq!(p.packet_id.get(), 0x0a0b);
        assert_eq!(p.max_packet_id.get(), 0x0a0c);
        // No RFID bit and a key frame, so it references itself.
        assert_eq!(p.referenced_frame_id, FrameId::new(5));
        assert_eq!(
            &p.payload[..],
            &[0x0f, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a, 0x09, 0x08]
        );
    }

    #[test]
    fn rejects_packet_id_past_end_of_frame() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let err = stream
            .parse(&fixture("rtp_packet_for_key_frame_with_bad_packet_id.bin"))
            .unwrap_err();
        assert_eq!(
            err,
            RtpError::PacketIdOutOfRange {
                packet: 0x0a0b,
                max: 1
            }
        );
    }

    #[test]
    fn non_key_frame_without_rfid_references_its_predecessor() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let p = stream
            .parse(&fixture("rtp_packet_for_non_key_frame_without_rfid.bin"))
            .unwrap();
        assert!(!p.is_key_frame);
        assert_eq!(p.frame_id, FrameId::new(0x2a));
        assert_eq!(p.referenced_frame_id, FrameId::new(0x2a - 1));
        assert_eq!(p.new_playout_delay_ms, None);
    }

    #[test]
    fn explicit_reference_frame_id_is_honored() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let p = stream
            .parse(&fixture("rtp_packet_for_non_key_frame_with_rfid.bin"))
            .unwrap();
        assert_eq!(p.frame_id, FrameId::new(0x2a));
        assert_eq!(p.referenced_frame_id, FrameId::new(0x27));
    }

    #[test]
    fn adaptive_latency_extension_is_read() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let p = stream
            .parse(&fixture("rtp_packet_for_key_frame_with_latency_ext.bin"))
            .unwrap();
        assert_eq!(p.new_playout_delay_ms, Some(0x010e));
        // The payload must start after the extension, not inside it.
        assert_eq!(p.payload[0], 0x01);
        assert_eq!(p.payload.len(), 15);
    }

    #[test]
    fn unknown_extensions_are_skipped_by_length() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let p = stream
            .parse(&fixture("rtp_packet_for_key_frame_with_multiple_ext.bin"))
            .unwrap();
        // Two extensions: an unknown one, then adaptive latency.
        assert_eq!(p.new_playout_delay_ms, Some(0x010e));
        assert_eq!(p.payload[0], 0x01);
    }

    #[test]
    fn truncated_packets_are_rejected_not_panicked_on() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        for (name, want) in [
            ("rtp_packet_trunc_to_1_byte.bin", RtpError::TooShort),
            // Exactly 18 bytes, but byte 12 promises a reference frame id that isn't
            // there — the minimum size is not the same as a complete header.
            ("rtp_packet_trunc_to_18_bytes.bin", RtpError::Truncated),
            ("rtp_packet_trunc_to_22_bytes.bin", RtpError::Truncated),
            ("rtp_packet_trunc_to_33_bytes.bin", RtpError::Truncated),
        ] {
            assert_eq!(stream.parse(&fixture(name)).unwrap_err(), want, "{name}");
        }
    }

    /// 34 bytes is the boundary: the third extension's last byte lands on the final
    /// byte of the buffer, so the packet is complete and the payload is empty. A
    /// parser that confused "no payload left" with "ran off the end" would reject it,
    /// and a sender emitting header-only packets would stall the stream.
    #[test]
    fn a_packet_whose_extensions_end_exactly_at_the_buffer_end_is_valid() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let p = stream
            .parse(&fixture("rtp_packet_trunc_to_34_bytes.bin"))
            .unwrap();
        assert_eq!(p.new_playout_delay_ms, Some(0x010e));
        assert!(p.payload.is_empty());
    }

    #[test]
    fn packets_from_another_stream_are_refused() {
        let mut stream = CastRtpStream::new(0xDEAD_BEEF);
        let err = stream
            .parse(&fixture("rtp_packet_for_key_frame.bin"))
            .unwrap_err();
        assert_eq!(
            err,
            RtpError::WrongSsrc {
                got: SEED_SSRC,
                want: 0xDEAD_BEEF
            }
        );
    }

    #[test]
    fn packet_id_cannot_hold_the_all_packets_lost_sentinel() {
        assert!(PacketId::new(0xffff).is_none());
        assert_eq!(PacketId::new(0xfffe).unwrap().get(), 0xfffe);
        assert_eq!(NackTarget::AllPackets.wire_value(), 0xffff);
    }

    #[test]
    fn frame_ids_expand_across_the_8_bit_wraparound() {
        // 0xfe truncated, with the stream already at 510, is 510 — not 254 and not 766.
        assert_eq!(FrameId::new(514).expand(0xfe), FrameId::new(510));
        // Stepping forward past the wrap: at 255, a truncated 0 means 256.
        assert_eq!(FrameId::new(255).expand(0x00), FrameId::new(256));
        assert_eq!(FrameId::first().expand(0x00), FrameId::first());
    }

    /// Build a synthetic packet; the fixtures cover the wire format, this covers
    /// reassembly over sequences the seed corpus does not contain.
    fn packet(frame_id: u8, packet_id: u16, max_packet_id: u16, key: bool, body: &[u8]) -> Bytes {
        let mut v = vec![0x80, 96];
        v.extend_from_slice(&0x0001u16.to_be_bytes());
        v.extend_from_slice(&0x0000_1000u32.to_be_bytes());
        v.extend_from_slice(&SEED_SSRC.to_be_bytes());
        v.push(if key { KEY_FRAME_BIT } else { 0 });
        v.push(frame_id);
        v.extend_from_slice(&packet_id.to_be_bytes());
        v.extend_from_slice(&max_packet_id.to_be_bytes());
        v.extend_from_slice(body);
        Bytes::from(v)
    }

    #[test]
    fn reassembles_a_frame_from_out_of_order_packets() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let mut collector = FrameCollector::new(FrameId::new(7));

        // Arrive 2, 0, 1 — the collector must restore order by packet id.
        for (pid, body) in [(2u16, &b"ccc"[..]), (0, b"aaa"), (1, b"bbb")] {
            let parsed = stream.parse(&packet(7, pid, 2, pid == 0, body)).unwrap();
            collector.collect(&parsed).unwrap();
        }

        assert!(collector.is_complete());
        let frame = collector.take_frame().unwrap();
        assert_eq!(&frame.payload[..], b"aaabbbccc");
        assert_eq!(frame.header.dependency, Dependency::KeyFrame);
        assert_eq!(frame.header.frame_id, FrameId::new(7));
    }

    #[test]
    fn duplicate_packets_are_absorbed() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let mut collector = FrameCollector::new(FrameId::new(7));
        let parsed = stream.parse(&packet(7, 0, 1, true, b"aaa")).unwrap();
        collector.collect(&parsed).unwrap();
        collector.collect(&parsed).unwrap();
        // The duplicate must not have counted toward completion.
        assert!(!collector.is_complete());
        let second = stream.parse(&packet(7, 1, 1, false, b"bbb")).unwrap();
        collector.collect(&second).unwrap();
        assert!(collector.is_complete());
    }

    #[test]
    fn nacks_name_exactly_the_missing_packets() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let mut collector = FrameCollector::new(FrameId::new(3));

        // Nothing yet: the receiver does not know the packet count, so it must ask
        // for the whole frame rather than guess.
        assert_eq!(
            collector.missing_packets(),
            vec![PacketNack {
                frame_id: FrameId::new(3),
                target: NackTarget::AllPackets
            }]
        );

        let parsed = stream.parse(&packet(3, 1, 3, false, b"b")).unwrap();
        collector.collect(&parsed).unwrap();
        let nacks = collector.missing_packets();
        assert_eq!(nacks.len(), 3);
        let targets: Vec<u16> = nacks.iter().map(|n| n.target.wire_value()).collect();
        assert_eq!(targets, vec![0, 2, 3]);

        for pid in [0u16, 2, 3] {
            let parsed = stream.parse(&packet(3, pid, 3, pid == 0, b"x")).unwrap();
            collector.collect(&parsed).unwrap();
        }
        assert!(collector.missing_packets().is_empty());
    }

    #[test]
    fn a_packet_for_another_frame_is_refused() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let mut collector = FrameCollector::new(FrameId::new(3));
        let parsed = stream.parse(&packet(4, 0, 0, true, b"x")).unwrap();
        assert_eq!(
            collector.collect(&parsed).unwrap_err(),
            CollectError::WrongFrame {
                want: FrameId::new(3),
                got: FrameId::new(4)
            }
        );
    }

    #[test]
    fn disagreeing_packet_counts_are_refused() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let mut collector = FrameCollector::new(FrameId::new(3));
        let first = stream.parse(&packet(3, 0, 2, true, b"x")).unwrap();
        collector.collect(&first).unwrap();
        let second = stream.parse(&packet(3, 1, 5, false, b"y")).unwrap();
        assert_eq!(
            collector.collect(&second).unwrap_err(),
            CollectError::PacketCountMismatch {
                established: 3,
                got: 6
            }
        );
    }

    #[test]
    fn an_incomplete_frame_is_never_delivered() {
        let mut stream = CastRtpStream::new(SEED_SSRC);
        let mut collector = FrameCollector::new(FrameId::new(3));
        let parsed = stream.parse(&packet(3, 0, 1, true, b"a")).unwrap();
        collector.collect(&parsed).unwrap();
        assert!(collector.take_frame().is_none());
    }
}
