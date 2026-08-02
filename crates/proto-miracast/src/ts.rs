//! MPEG-2 transport stream demultiplexing — the Miracast media plane.
//!
//! Miracast is the only protocol in this workspace that puts a *container* on the wire.
//! AirPlay and Cast hand over access units directly; a WFD source hands over an MPEG2-TS
//! (ISO/IEC 13818-1) carried in RTP payload type 33, seven 188-byte packets to a
//! datagram. So there is a PAT to read, a PMT to read, PES headers to strip, and a
//! 90 kHz clock to convert — none of which the other adapters need.
//!
//! Architecture §1a notes that ffmpeg's `rtp_mpegts` demuxer would eat this stream whole,
//! and that remains true. It is not what this does, for two reasons that only became
//! clear once the rest of the crate existed: handing libav a socket puts the *network* on
//! the far side of an FFI boundary that owns its own I/O (ground rule 3 says no), and the
//! sink has to see the elementary stream anyway — an IDR arriving is what answers a
//! source's `wfd_idr_request`, and a PTS that steps backwards is what says the source
//! re-keyed (see [`PtsOrigin`]; no PCR is read, and on a re-key Android does not set the
//! adaptation field's discontinuity indicator either, so the timestamps are the signal).
//! A demuxer we can unit-test against fixture bytes is worth the ~400 lines.
//!
//! Everything here is sans-I/O and synchronous: [`TsDemux::push`] takes bytes and returns
//! frames. It never fails the session — a corrupt packet is a resync, because the one
//! thing a live mirror must not do is stop.

use std::collections::HashMap;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use castaway_core::{AudioCodec, EncodedFrame, VideoCodec};

/// A transport stream packet is always exactly this long.
pub const TS_PACKET_LEN: usize = 188;

/// The byte every TS packet starts with.
const SYNC_BYTE: u8 = 0x47;

/// The PID carrying the Program Association Table. Fixed by the spec.
const PAT_PID: Pid = Pid(0x0000);

/// The elementary stream clock: 90 kHz, for both PTS and the PCR base.
const PTS_HZ: u64 = 90_000;

/// PTS is a 33-bit counter, so it wraps every ~26.5 hours — and, far more often in
/// practice, a source that re-keys mid-session restarts it. See [`PtsOrigin`].
const PTS_MODULUS: u64 = 1 << 33;

/// Half the PTS range. A backwards step larger than this is read as a wrap rather than a
/// seek, which is the standard disambiguation and the only one available in-band.
const PTS_WRAP_THRESHOLD: u64 = PTS_MODULUS / 2;

/// A backwards step larger than this, but smaller than a wrap, is the counter being
/// restarted rather than timestamps arriving out of order.
///
/// Two seconds. What has to fit underneath it is B-frame reordering, which is a frame or
/// two, and the audio/video interleave — both planes share one origin, so alternating
/// packets step backwards by whatever the mux offset is. What has to fall outside it is a
/// re-key, which restarts the counter near zero and so steps back by the whole elapsed
/// session. There is a wide gap between the two and this sits in it.
///
/// The notes recommend re-basing on any jump over a few hundred milliseconds, since
/// Android's source never sets `discontinuity_indicator` and there is nothing else to go
/// on. Only *backwards* jumps are treated this way: a forward one is as likely to be a
/// gap in a stream we are still measuring correctly.
const PTS_RESTART_THRESHOLD: u64 = 2 * PTS_HZ;

/// A 13-bit transport stream packet identifier.
///
/// A newtype rather than a `u16` because the two are not the same set — 0x1FFF is the
/// null packet and 0x0000 is always the PAT — and because a PID and a program number are
/// both "some u16 from the PMT" at the point where confusing them is easiest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Pid(u16);

impl Pid {
    /// The null-packet PID. Stuffing; carries nothing and is discarded.
    pub const NULL: Self = Self(0x1FFF);

    /// The PID held in the low 13 bits of `raw`. The upper three bits are flags in every
    /// context a PID is read from, so they are masked rather than rejected.
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw & 0x1FFF)
    }

    /// The PID's numeric value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// An elementary stream type, as the PMT's `stream_type` byte states it.
///
/// Only the ones a WFD source can legally send are named. Anything else is
/// [`StreamType::Other`] and is ignored rather than guessed at — a sink that tried to
/// decode an unknown stream type would be inventing a codec from one byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamType {
    /// H.264 / AVC video. The only video codec base Miracast requires.
    H264,
    /// H.265 / HEVC video, from the WFD 2.0 (`wfd2_video_formats`) extension.
    Hevc,
    /// AAC in ADTS frames — the WFD "AAC" audio mode.
    AacAdts,
    /// AAC in LATM/LOAS.
    AacLatm,
    /// AC-3, carried as a private stream.
    Ac3,
    /// LPCM, carried as a private stream. The WFD baseline audio mode.
    Lpcm,
    /// A stream type we do not decode.
    Other(u8),
}

impl StreamType {
    /// Read a PMT `stream_type` byte.
    #[must_use]
    pub const fn from_u8(raw: u8) -> Self {
        match raw {
            0x1B => Self::H264,
            0x24 => Self::Hevc,
            0x0F => Self::AacAdts,
            0x11 => Self::AacLatm,
            0x81 => Self::Ac3,
            0x83 => Self::Lpcm,
            other => Self::Other(other),
        }
    }

    /// The video codec this stream decodes as, if it is video.
    #[must_use]
    pub const fn video_codec(self) -> Option<VideoCodec> {
        match self {
            Self::H264 => Some(VideoCodec::H264),
            Self::Hevc => Some(VideoCodec::Hevc),
            _ => None,
        }
    }

    /// The audio codec this stream decodes as, if it is audio.
    ///
    /// AC-3 has no [`AudioCodec`] variant: nothing in the pipeline decodes it, and giving
    /// it one here would make the media path claim a capability the decoder does not
    /// have. The negotiation never offers AC-3 for that reason ([`crate::params`]), so a
    /// stream arriving in it means the source ignored our capability set.
    #[must_use]
    pub const fn audio_codec(self) -> Option<AudioCodec> {
        match self {
            Self::AacAdts | Self::AacLatm => Some(AudioCodec::Aac),
            Self::Lpcm => Some(AudioCodec::Pcm),
            _ => None,
        }
    }
}

/// One packet's worth of parsed transport header.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TsHeader {
    pid: Pid,
    payload_unit_start: bool,
    continuity_counter: u8,
    has_payload: bool,
    /// Set by the adaptation field when the source says the continuity counter is about
    /// to jump on purpose — a re-key or a format change, not a loss.
    discontinuity: bool,
    /// Where the payload begins within the 188 bytes.
    payload_offset: usize,
}

impl TsHeader {
    /// Parse the 4-byte header plus any adaptation field. `None` for a packet that is
    /// not a TS packet at all, or whose adaptation field overruns it.
    fn parse(pkt: &[u8]) -> Option<Self> {
        let bytes: &[u8; TS_PACKET_LEN] = pkt.get(..TS_PACKET_LEN)?.try_into().ok()?;
        if bytes[0] != SYNC_BYTE {
            return None;
        }
        // The error indicator says the demodulator already knows this packet is damaged.
        // Trusting it is cheaper than discovering the same thing from a broken PES.
        if bytes[1] & 0x80 != 0 {
            return None;
        }
        let pid = Pid::from_raw(u16::from(bytes[1] & 0x1F) << 8 | u16::from(bytes[2]));
        let adaptation_field_control = (bytes[3] >> 4) & 0x03;
        let has_adaptation = adaptation_field_control & 0b10 != 0;
        let has_payload = adaptation_field_control & 0b01 != 0;
        let mut payload_offset = 4usize;
        let mut discontinuity = false;
        if has_adaptation {
            let len = usize::from(bytes[4]);
            if len > 0 {
                discontinuity = bytes.get(5).is_some_and(|f| f & 0x80 != 0);
            }
            payload_offset = 5usize.checked_add(len)?;
            if payload_offset > TS_PACKET_LEN {
                return None;
            }
        }
        Some(Self {
            pid,
            payload_unit_start: bytes[1] & 0x40 != 0,
            continuity_counter: bytes[3] & 0x0F,
            has_payload,
            discontinuity,
            payload_offset,
        })
    }
}

/// Converts a source's 33-bit PTS into time since the first frame.
///
/// [`EncodedFrame::pts`] is documented as nanoseconds from the start of the stream, so
/// something has to hold the origin. Doing it here rather than in the pipeline is what
/// makes a wrap invisible downstream: the counter rolls over at 2^33 ticks, and a
/// consumer that saw the raw value would see time jump backwards by 26.5 hours.
#[derive(Debug, Default)]
pub struct PtsOrigin {
    /// The first PTS seen, in 90 kHz ticks.
    first: Option<u64>,
    /// The previous PTS, to detect the wrap.
    previous: u64,
    /// How many times the counter has rolled over.
    wraps: u64,
    /// Ticks already resolved before the most recent restart, so a re-key continues the
    /// timeline instead of returning to zero.
    carried: u64,
}

impl PtsOrigin {
    /// Convert a raw 33-bit PTS to time since the first one this origin saw.
    pub fn resolve(&mut self, pts: u64) -> Duration {
        let mut first = *self.first.get_or_insert(pts);
        if self.previous > pts {
            let back = self.previous - pts;
            if back > PTS_WRAP_THRESHOLD {
                // A big step backwards is the counter wrapping.
                self.wraps = self.wraps.saturating_add(1);
            } else if back > PTS_RESTART_THRESHOLD {
                // A middling one is a re-key restarting the counter — the case this type
                // exists to hide and used not to. Subtracting the *old* origin from a
                // restarted PTS sends resolved time backwards by the whole session and
                // then pins it at zero until the counter climbs past where it began. So
                // bank what has elapsed and start measuring again from here.
                let previous_extended = self
                    .previous
                    .saturating_add(self.wraps.saturating_mul(PTS_MODULUS));
                self.carried = self
                    .carried
                    .saturating_add(previous_extended.saturating_sub(first));
                first = pts;
                self.first = Some(pts);
                self.wraps = 0;
            }
            // Anything smaller is B-frame reordering or two streams whose PTSs simply
            // interleave, and must not move anything.
        }
        self.previous = pts;
        let extended = pts.saturating_add(self.wraps.saturating_mul(PTS_MODULUS));
        let ticks = self.carried.saturating_add(extended.saturating_sub(first));
        Duration::from_nanos(
            ticks
                .saturating_mul(1_000_000_000)
                .checked_div(PTS_HZ)
                .unwrap_or(0),
        )
    }
}

/// The PES assembly state for one elementary stream.
#[derive(Debug)]
struct StreamState {
    stream_type: StreamType,
    /// Bytes of the PES packet collected so far, header included.
    pending: BytesMut,
    /// The declared `PES_packet_length`, or `None` when the source sent 0 (legal, and
    /// what every WFD source does for video: the packet ends at the next one's start).
    declared_len: Option<usize>,
    /// The last continuity counter seen, for loss detection.
    last_cc: Option<u8>,
    /// Set when a gap was detected; the partial PES is dropped rather than handed to a
    /// decoder as a frame with a hole in it.
    damaged: bool,
}

impl StreamState {
    fn new(stream_type: StreamType) -> Self {
        Self {
            stream_type,
            pending: BytesMut::new(),
            declared_len: None,
            last_cc: None,
            damaged: false,
        }
    }
}

/// Demultiplexes an MPEG2-TS into [`EncodedFrame`]s.
///
/// Feed it bytes with [`TsDemux::push`]; it discovers the program structure from the PAT
/// and PMT as they go past and starts emitting once it has both. A source repeats those
/// tables every few hundred milliseconds precisely so a receiver can join mid-stream, so
/// there is nothing to prime and nothing to wait for.
#[derive(Debug, Default)]
pub struct TsDemux {
    /// The PMT PID the PAT named, once one has gone past.
    pmt_pid: Option<Pid>,
    /// Per-elementary-stream assembly, keyed by PID.
    streams: HashMap<Pid, StreamState>,
    /// One origin for the whole program, so the audio and video planes are measured from
    /// the same zero. Two origins would make lip-sync error equal to whichever plane's
    /// first frame happened to arrive first.
    origin: PtsOrigin,
    /// Bytes left over from a partial TS packet at the end of the last push.
    partial: BytesMut,
    /// Packets whose sync byte was not where it should have been, since the last resync.
    resyncs: u64,
}

impl TsDemux {
    /// A demux with no program knowledge yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many times the packet stream has had to be resynchronized. A rising count
    /// means the transport is losing datagrams, which on Wi-Fi Direct usually means the
    /// P2P group and the STA link are fighting over one radio (architecture §7.5).
    #[must_use]
    pub fn resync_count(&self) -> u64 {
        self.resyncs
    }

    /// The stream types the PMT declared, for logging what a session actually negotiated.
    #[must_use]
    pub fn stream_types(&self) -> Vec<(Pid, StreamType)> {
        let mut out: Vec<_> = self
            .streams
            .iter()
            .map(|(p, s)| (*p, s.stream_type))
            .collect();
        out.sort_by_key(|(p, _)| *p);
        out
    }

    /// Push transport stream bytes — one RTP payload, one file, any split. Returns the
    /// frames that completed.
    ///
    /// Never returns an error. A packet that will not parse is skipped and the stream
    /// resynchronized on the next sync byte, because the alternative on a live mirror is
    /// to end a session over one lost datagram.
    pub fn push(&mut self, data: &[u8]) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        self.partial.extend_from_slice(data);
        while self.partial.len() >= TS_PACKET_LEN {
            if self.partial[0] != SYNC_BYTE {
                // Skip to the next plausible packet boundary rather than dropping the
                // whole buffer: a datagram that lost its head still has whole packets in
                // its tail.
                let skip = self.partial[1..]
                    .iter()
                    .position(|b| *b == SYNC_BYTE)
                    .map_or(self.partial.len(), |i| i + 1);
                let _ = self.partial.split_to(skip);
                self.resyncs = self.resyncs.saturating_add(1);
                continue;
            }
            let packet = self.partial.split_to(TS_PACKET_LEN).freeze();
            self.handle_packet(&packet, &mut out);
        }
        out
    }

    /// Flush any PES still being assembled — the end of a session, where a video PES that
    /// was waiting for the next packet's start bit will otherwise never be emitted.
    pub fn flush(&mut self) -> Vec<EncodedFrame> {
        let mut out = Vec::new();
        let pids: Vec<Pid> = self.streams.keys().copied().collect();
        for pid in pids {
            self.complete_pes(pid, &mut out);
        }
        out
    }

    fn handle_packet(&mut self, packet: &Bytes, out: &mut Vec<EncodedFrame>) {
        let Some(header) = TsHeader::parse(packet) else {
            self.resyncs = self.resyncs.saturating_add(1);
            return;
        };
        if header.pid == Pid::NULL || !header.has_payload {
            return;
        }
        let payload = &packet[header.payload_offset..];
        if header.pid == PAT_PID {
            self.handle_pat(payload, header.payload_unit_start);
        } else if Some(header.pid) == self.pmt_pid {
            self.handle_pmt(payload, header.payload_unit_start);
        } else if self.streams.contains_key(&header.pid) {
            self.handle_pes(&header, payload, out);
        }
    }

    /// Strip the `pointer_field` a PSI payload carries when it starts a section.
    ///
    /// Only sections are handled, and only when they start in this packet: a table split
    /// across packets is legal but a PAT/PMT for one program never approaches 184 bytes,
    /// and reassembling one we will never see is code with no way to test it honestly.
    fn section(payload: &[u8], payload_unit_start: bool) -> Option<&[u8]> {
        if !payload_unit_start {
            return None;
        }
        let pointer = usize::from(*payload.first()?);
        payload.get(1usize.checked_add(pointer)?..)
    }

    fn handle_pat(&mut self, payload: &[u8], payload_unit_start: bool) {
        let Some(section) = Self::section(payload, payload_unit_start) else {
            return;
        };
        // table_id 0x00 is the PAT; anything else on PID 0 is not ours.
        if section.first() != Some(&0x00) {
            return;
        }
        let Some(body) = Self::section_body(section) else {
            return;
        };
        // Fixed 5 bytes (transport_stream_id, version, section_number, last) precede the
        // program loop; the trailing 4 bytes are the CRC.
        let Some(entries) = body.get(5..) else { return };
        for entry in entries.chunks_exact(4) {
            let program = u16::from(entry[0]) << 8 | u16::from(entry[1]);
            let pid = Pid::from_raw(u16::from(entry[2]) << 8 | u16::from(entry[3]));
            // Program 0 names the network information table, not a program.
            if program != 0 {
                if self.pmt_pid != Some(pid) {
                    self.pmt_pid = Some(pid);
                    self.streams.clear();
                }
                return;
            }
        }
    }

    fn handle_pmt(&mut self, payload: &[u8], payload_unit_start: bool) {
        let Some(section) = Self::section(payload, payload_unit_start) else {
            return;
        };
        if section.first() != Some(&0x02) {
            return;
        }
        let Some(body) = Self::section_body(section) else {
            return;
        };
        // program_number(2) version(1) section(1) last(1) pcr_pid(2) program_info_len(2)
        let Some(info_len_bytes) = body.get(7..9) else {
            return;
        };
        let program_info_len =
            usize::from(info_len_bytes[0] & 0x0F) << 8 | usize::from(info_len_bytes[1]);
        let Some(mut rest) = body.get(9usize.saturating_add(program_info_len)..) else {
            return;
        };
        let mut seen: Vec<(Pid, StreamType)> = Vec::new();
        while rest.len() >= 5 {
            let stream_type = StreamType::from_u8(rest[0]);
            let pid = Pid::from_raw(u16::from(rest[1] & 0x1F) << 8 | u16::from(rest[2]));
            let es_info_len = usize::from(rest[3] & 0x0F) << 8 | usize::from(rest[4]);
            let Some(next) = rest.get(5usize.saturating_add(es_info_len)..) else {
                break;
            };
            // The CRC is the last four bytes of the section and is not a stream entry.
            if next.len() < 4 && rest.len().saturating_sub(5 + es_info_len) < 4 {
                seen.push((pid, stream_type));
                break;
            }
            seen.push((pid, stream_type));
            rest = next;
        }
        for (pid, stream_type) in seen {
            // Only streams we can actually decode get assembly state. An unknown type
            // would otherwise accumulate a PES buffer forever with nothing to emit it to.
            if stream_type.video_codec().is_none() && stream_type.audio_codec().is_none() {
                continue;
            }
            self.streams
                .entry(pid)
                .or_insert_with(|| StreamState::new(stream_type));
        }
    }

    /// The bytes of a PSI section after its 3-byte header, with the trailing CRC removed.
    fn section_body(section: &[u8]) -> Option<&[u8]> {
        let len = usize::from(section.get(1)? & 0x0F) << 8 | usize::from(*section.get(2)?);
        let end = 3usize.checked_add(len)?;
        // A section whose declared length runs past what arrived is one we only half
        // have; the source will repeat it in a few hundred milliseconds.
        let body = section.get(3..end)?;
        // Drop the CRC32. Not checked: the packets came over UDP with its own checksum,
        // and a section that fails to parse is discarded anyway.
        body.get(..body.len().checked_sub(4)?)
    }

    fn handle_pes(&mut self, header: &TsHeader, payload: &[u8], out: &mut Vec<EncodedFrame>) {
        // The continuity check comes first, because it decides whether what is already
        // buffered can be trusted — not whether this packet can.
        let lost = {
            let Some(state) = self.streams.get_mut(&header.pid) else {
                return;
            };
            let expected = state.last_cc.map(|cc| (cc + 1) & 0x0F);
            state.last_cc = Some(header.continuity_counter);
            // A declared discontinuity is the source telling us the jump is deliberate.
            expected.is_some_and(|e| e != header.continuity_counter) && !header.discontinuity
        };
        if lost {
            if let Some(state) = self.streams.get_mut(&header.pid) {
                state.damaged = true;
            }
        }

        if header.payload_unit_start {
            // A new PES starts here, so whatever was pending is as complete as it will
            // ever be. This is the only boundary an unbounded video PES has.
            self.complete_pes(header.pid, out);
            let Some(state) = self.streams.get_mut(&header.pid) else {
                return;
            };
            state.damaged = lost;
            state.declared_len = pes_declared_len(payload);
            state.pending.extend_from_slice(payload);
        } else {
            let Some(state) = self.streams.get_mut(&header.pid) else {
                return;
            };
            // Bytes with no header in front of them belong to nothing; this is what a
            // mid-PES join looks like, and it lasts until the next start bit.
            if state.pending.is_empty() {
                return;
            }
            state.pending.extend_from_slice(payload);
        }

        // A PES that declared its length is complete the moment it reaches it — audio
        // frames do this, and waiting for the next start bit would add a whole frame of
        // latency to the plane that has the least slack for it.
        let complete = self
            .streams
            .get(&header.pid)
            .and_then(|s| s.declared_len.map(|len| s.pending.len() >= len))
            .unwrap_or(false);
        if complete {
            self.complete_pes(header.pid, out);
        }
    }

    fn complete_pes(&mut self, pid: Pid, out: &mut Vec<EncodedFrame>) {
        let Some(state) = self.streams.get_mut(&pid) else {
            return;
        };
        if state.pending.is_empty() {
            return;
        }
        let raw = state.pending.split().freeze();
        let stream_type = state.stream_type;
        let damaged = state.damaged;
        state.damaged = false;
        state.declared_len = None;
        if damaged {
            // Dropping the access unit is deliberate: half a frame decodes into visible
            // corruption that persists until the next IDR, where a dropped frame is one
            // missed refresh. Ground rule 4 — latency beats freshness, and a clean
            // picture beats both.
            return;
        }
        let Some(pes) = PesPacket::parse(&raw) else {
            return;
        };
        if pes.payload.is_empty() {
            return;
        }
        let pts = pes.pts.map(|p| self.origin.resolve(p)).unwrap_or_default();
        let video_codec = stream_type.video_codec();
        let keyframe = video_codec.is_some_and(|c| annex_b_has_idr(&pes.payload, c));
        out.push(EncodedFrame {
            video_codec,
            audio_codec: stream_type.audio_codec(),
            pts,
            keyframe,
            data: pes.payload,
        });
    }
}

/// The `PES_packet_length` a packet declares, or `None` for the unbounded (0) form.
///
/// The length counts the bytes *after* the 6-byte prefix, so a buffer is complete when it
/// reaches `len + 6`.
fn pes_declared_len(payload: &[u8]) -> Option<usize> {
    let bytes = payload.get(..6)?;
    if bytes[0..3] != [0x00, 0x00, 0x01] {
        return None;
    }
    let len = usize::from(bytes[4]) << 8 | usize::from(bytes[5]);
    if len == 0 {
        None
    } else {
        Some(len.checked_add(6)?)
    }
}

/// A parsed PES packet: the elementary stream bytes, plus the timestamp on them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PesPacket {
    pts: Option<u64>,
    payload: Bytes,
}

impl PesPacket {
    fn parse(raw: &Bytes) -> Option<Self> {
        let head = raw.get(..9)?;
        if head[0..3] != [0x00, 0x00, 0x01] {
            return None;
        }
        // '10' in the top two bits of byte 6 marks the optional-header form. Stream ids
        // that never carry one (padding, private_stream_2) do not appear in a WFD
        // program, so a packet without it is malformed rather than a special case.
        if head[6] & 0xC0 != 0x80 {
            return None;
        }
        let pts_dts_flags = (head[7] >> 6) & 0x03;
        let header_data_len = usize::from(head[8]);
        let optional = raw.get(9..9usize.checked_add(header_data_len)?)?;
        // 0b10 is PTS alone, 0b11 is PTS then DTS. 0b01 is forbidden by the spec.
        let pts = if pts_dts_flags & 0b10 != 0 {
            read_timestamp(optional.get(..5)?)
        } else {
            None
        };
        let payload_start = 9usize.checked_add(header_data_len)?;
        Some(Self {
            pts,
            payload: raw.slice(payload_start..),
        })
    }
}

/// Read a 33-bit PTS/DTS out of its five marker-interleaved bytes.
///
/// The layout puts four bits of tag, then the timestamp in three chunks of 3, 15 and 15
/// bits, each followed by a marker bit that must be 1. Checking the markers is the only
/// validation available and catches a misaligned header immediately.
fn read_timestamp(bytes: &[u8]) -> Option<u64> {
    let b: &[u8; 5] = bytes.try_into().ok()?;
    if b[0] & 0x01 == 0 || b[2] & 0x01 == 0 || b[4] & 0x01 == 0 {
        return None;
    }
    let hi = u64::from((b[0] >> 1) & 0x07) << 30;
    let mid = (u64::from(b[1]) << 7 | u64::from(b[2] >> 1)) << 15;
    let lo = u64::from(b[3]) << 7 | u64::from(b[4] >> 1);
    Some(hi | mid | lo)
}

/// Whether an Annex-B elementary stream contains an IDR.
///
/// The pipeline drops every frame until it sees a keyframe, so getting this wrong does
/// not corrupt a picture — it means no picture at all, forever. H.264 puts the NAL type
/// in the low five bits; HEVC in six bits shifted up by one, and its IRAP range is
/// 16..=21 (the same reading `proto-airplay` does, and for the same reason).
fn annex_b_has_idr(data: &[u8], codec: VideoCodec) -> bool {
    let mut zeros = 0usize;
    let mut iter = data.iter().copied();
    while let Some(byte) = iter.next() {
        match byte {
            0x00 => zeros += 1,
            0x01 if zeros >= 2 => {
                zeros = 0;
                let Some(nal) = iter.next() else { return false };
                let is_idr = match codec {
                    VideoCodec::H264 => nal & 0x1F == 5,
                    VideoCodec::Hevc => matches!((nal & 0x7E) >> 1, 16..=21),
                    // VP8 never appears in a transport stream; a WFD source cannot offer
                    // it, and neither can any codec added to the enum later — this is a
                    // *transport stream*, and nothing reaches here that the PMT did not
                    // map to one of the two above.
                    VideoCodec::Vp8 | _ => false,
                };
                if is_idr {
                    return true;
                }
            }
            _ => zeros = 0,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Build a TS packet with the given payload, padding with an adaptation field the way
    /// a real muxer does (stuffing bytes, not trailing 0xFF in the payload).
    fn ts_packet(pid: Pid, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut pkt = vec![0u8; TS_PACKET_LEN];
        pkt[0] = SYNC_BYTE;
        let pid_raw = pid.get();
        pkt[1] = u8::try_from(pid_raw >> 8).unwrap() | if pusi { 0x40 } else { 0 };
        pkt[2] = u8::try_from(pid_raw & 0xFF).unwrap();
        let stuffing = 184 - payload.len();
        if stuffing == 0 {
            pkt[3] = 0x10 | (cc & 0x0F);
            pkt[4..].copy_from_slice(payload);
        } else {
            pkt[3] = 0x30 | (cc & 0x0F);
            // An adaptation field of length 1 is the flags byte alone; longer fields pad
            // with 0xFF after it.
            pkt[4] = u8::try_from(stuffing - 1).unwrap();
            if stuffing >= 2 {
                pkt[5] = 0x00;
                for b in pkt.iter_mut().take(4 + stuffing).skip(6) {
                    *b = 0xFF;
                }
            }
            pkt[4 + stuffing..].copy_from_slice(payload);
        }
        pkt
    }

    fn pat(pmt_pid: Pid) -> Vec<u8> {
        let mut section = vec![0x00u8]; // table_id
        let body = {
            let mut b = vec![0x00, 0x01, 0xC1, 0x00, 0x00]; // tsid, version, section, last
            b.extend_from_slice(&[0x00, 0x01]); // program_number 1
            b.extend_from_slice(&(0xE000u16 | pmt_pid.get()).to_be_bytes());
            b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // CRC, unchecked
            b
        };
        let len = u16::try_from(body.len()).unwrap() | 0xB000;
        section.extend_from_slice(&len.to_be_bytes());
        section.extend_from_slice(&body);
        let mut payload = vec![0x00u8]; // pointer_field
        payload.extend_from_slice(&section);
        payload
    }

    fn pmt(streams: &[(Pid, u8)]) -> Vec<u8> {
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00]; // program, version, section, last
        body.extend_from_slice(&[0xE1, 0x00]); // PCR PID 0x100
        body.extend_from_slice(&[0xF0, 0x00]); // program_info_length 0
        for (pid, stream_type) in streams {
            body.push(*stream_type);
            body.extend_from_slice(&(0xE000u16 | pid.get()).to_be_bytes());
            body.extend_from_slice(&[0xF0, 0x00]); // ES_info_length 0
        }
        body.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // CRC
        let mut section = vec![0x02u8];
        let len = u16::try_from(body.len()).unwrap() | 0xB000;
        section.extend_from_slice(&len.to_be_bytes());
        section.extend_from_slice(&body);
        let mut payload = vec![0x00u8];
        payload.extend_from_slice(&section);
        payload
    }

    fn timestamp_bytes(pts: u64, tag: u8) -> [u8; 5] {
        let b0 = (tag << 4) | u8::try_from((pts >> 30) & 0x07).unwrap() << 1 | 1;
        let mid = u16::try_from((pts >> 15) & 0x7FFF).unwrap();
        let lo = u16::try_from(pts & 0x7FFF).unwrap();
        [
            b0,
            u8::try_from(mid >> 7).unwrap(),
            u8::try_from((mid & 0x7F) << 1).unwrap() | 1,
            u8::try_from(lo >> 7).unwrap(),
            u8::try_from((lo & 0x7F) << 1).unwrap() | 1,
        ]
    }

    fn pes(stream_id: u8, pts: Option<u64>, payload: &[u8], bounded: bool) -> Vec<u8> {
        let mut header = Vec::new();
        let optional = pts.map(|p| timestamp_bytes(p, 0b0010));
        let opt_len = optional.map_or(0usize, |o| o.len());
        header.extend_from_slice(&[0x00, 0x00, 0x01, stream_id]);
        let declared = if bounded {
            u16::try_from(3 + opt_len + payload.len()).unwrap()
        } else {
            0
        };
        header.extend_from_slice(&declared.to_be_bytes());
        header.push(0x80); // '10' marker, no scrambling
        header.push(if pts.is_some() { 0x80 } else { 0x00 });
        header.push(u8::try_from(opt_len).unwrap());
        if let Some(o) = optional {
            header.extend_from_slice(&o);
        }
        header.extend_from_slice(payload);
        header
    }

    /// Split a PES across as many TS packets as it needs.
    fn packetize(pid: Pid, cc_start: u8, pes_bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cc = cc_start;
        for (i, chunk) in pes_bytes.chunks(184).enumerate() {
            out.extend_from_slice(&ts_packet(pid, i == 0, cc, chunk));
            cc = (cc + 1) & 0x0F;
        }
        out
    }

    /// One frame at 30 fps: 3000 ticks of the 90 kHz clock, to the nanosecond.
    const THIRTY_FPS_FRAME: Duration = Duration::from_nanos(33_333_333);

    const VIDEO_PID: Pid = Pid(0x1011);
    const AUDIO_PID: Pid = Pid(0x1100);
    const PMT_PID: Pid = Pid(0x0100);

    fn idr_access_unit() -> Vec<u8> {
        // SPS, then an IDR slice — the shape of every keyframe a WFD source sends.
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1E];
        au.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xCE]);
        au.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84]);
        au
    }

    fn non_idr_access_unit() -> Vec<u8> {
        vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x12]
    }

    #[test]
    fn discovers_the_program_from_pat_and_pmt() {
        let mut demux = TsDemux::new();
        demux.push(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        demux.push(&ts_packet(
            PMT_PID,
            true,
            0,
            &pmt(&[(VIDEO_PID, 0x1B), (AUDIO_PID, 0x0F)]),
        ));
        // Sorted by PID: video (0x1011) precedes audio (0x1100).
        assert_eq!(
            demux.stream_types(),
            vec![
                (VIDEO_PID, StreamType::H264),
                (AUDIO_PID, StreamType::AacAdts)
            ]
        );
    }

    #[test]
    fn emits_a_video_frame_with_its_timestamp_and_keyframe_flag() {
        let mut demux = TsDemux::new();
        demux.push(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        demux.push(&ts_packet(PMT_PID, true, 0, &pmt(&[(VIDEO_PID, 0x1B)])));

        let au = idr_access_unit();
        let first = demux.push(&packetize(
            VIDEO_PID,
            0,
            &pes(0xE0, Some(90_000), &au, false),
        ));
        // An unbounded PES ends only at the next one's start, so nothing yet.
        assert!(first.is_empty());

        let second = demux.push(&packetize(
            VIDEO_PID,
            1,
            &pes(0xE0, Some(93_000), &non_idr_access_unit(), false),
        ));
        assert_eq!(second.len(), 1);
        let frame = &second[0];
        assert_eq!(frame.video_codec, Some(VideoCodec::H264));
        assert!(frame.keyframe, "an access unit with a type-5 NAL is an IDR");
        // The first PTS seen is the origin, so it lands at zero.
        assert_eq!(frame.pts, Duration::ZERO);
        assert_eq!(&frame.data[..], &au[..]);

        let rest = demux.flush();
        assert_eq!(rest.len(), 1);
        assert!(!rest[0].keyframe);
        // 3000 ticks at 90 kHz is one 30 fps frame.
        assert_eq!(rest[0].pts, THIRTY_FPS_FRAME);
    }

    #[test]
    fn a_bounded_audio_pes_completes_without_waiting_for_the_next_one() {
        let mut demux = TsDemux::new();
        demux.push(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        demux.push(&ts_packet(PMT_PID, true, 0, &pmt(&[(AUDIO_PID, 0x0F)])));
        let adts = vec![0xFF, 0xF1, 0x4C, 0x80, 0x02, 0x1F, 0xFC, 0x01, 0x02, 0x03];
        let frames = demux.push(&packetize(
            AUDIO_PID,
            0,
            &pes(0xC0, Some(90_000), &adts, true),
        ));
        assert_eq!(frames.len(), 1, "a declared length is a complete frame");
        assert_eq!(frames[0].audio_codec, Some(AudioCodec::Aac));
        assert_eq!(frames[0].video_codec, None);
        assert_eq!(&frames[0].data[..], &adts[..]);
    }

    #[test]
    fn audio_and_video_share_one_origin() {
        // Two origins would make the lip-sync error equal to the arrival gap between the
        // planes' first frames — a bug that renders perfectly and looks like a codec issue.
        let mut demux = TsDemux::new();
        demux.push(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        demux.push(&ts_packet(
            PMT_PID,
            true,
            0,
            &pmt(&[(VIDEO_PID, 0x1B), (AUDIO_PID, 0x0F)]),
        ));
        // Video first, at tick 90_000; audio a frame later, at 93_000.
        demux.push(&packetize(
            VIDEO_PID,
            0,
            &pes(0xE0, Some(90_000), &idr_access_unit(), true),
        ));
        let audio = demux.push(&packetize(
            AUDIO_PID,
            0,
            &pes(0xC0, Some(93_000), &[1, 2, 3], true),
        ));
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].pts, THIRTY_FPS_FRAME);
    }

    #[test]
    fn a_continuity_gap_drops_the_access_unit_it_broke() {
        let mut demux = TsDemux::new();
        demux.push(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        demux.push(&ts_packet(PMT_PID, true, 0, &pmt(&[(VIDEO_PID, 0x1B)])));

        let long_au = vec![0x88u8; 400];
        let bytes = pes(0xE0, Some(90_000), &long_au, false);
        let packets = packetize(VIDEO_PID, 0, &bytes);
        // Feed the first packet, then skip the second — the continuity counter jumps.
        demux.push(&packets[..TS_PACKET_LEN]);
        demux.push(&packets[TS_PACKET_LEN * 2..]);
        // The counter continues from the packet that did arrive; only the *gap* is a loss.
        let out = demux.push(&packetize(
            VIDEO_PID,
            3,
            &pes(0xE0, Some(93_000), &idr_access_unit(), false),
        ));
        assert!(out.is_empty(), "half an access unit is not a frame");
        let flushed = demux.flush();
        assert_eq!(
            flushed.len(),
            1,
            "the next, intact access unit still arrives"
        );
        assert!(flushed[0].keyframe);
    }

    #[test]
    fn a_torn_datagram_resyncs_on_the_next_sync_byte() {
        let mut demux = TsDemux::new();
        let mut stream = vec![0x11u8, 0x22, 0x33];
        stream.extend_from_slice(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        stream.extend_from_slice(&ts_packet(PMT_PID, true, 0, &pmt(&[(VIDEO_PID, 0x1B)])));
        demux.push(&stream);
        assert_eq!(demux.stream_types(), vec![(VIDEO_PID, StreamType::H264)]);
        assert!(demux.resync_count() > 0, "the junk prefix is counted");
    }

    #[test]
    fn a_packet_split_across_two_pushes_is_reassembled() {
        let mut demux = TsDemux::new();
        let pkt = ts_packet(PAT_PID, true, 0, &pat(PMT_PID));
        demux.push(&pkt[..100]);
        demux.push(&pkt[100..]);
        demux.push(&ts_packet(PMT_PID, true, 0, &pmt(&[(VIDEO_PID, 0x1B)])));
        assert_eq!(demux.stream_types(), vec![(VIDEO_PID, StreamType::H264)]);
    }

    #[test]
    fn pts_wrap_does_not_send_time_backwards() {
        let mut origin = PtsOrigin::default();
        let near_top = PTS_MODULUS - 90_000;
        assert_eq!(origin.resolve(near_top), Duration::ZERO);
        // One second later, the counter has rolled over to 0.
        assert_eq!(origin.resolve(0), Duration::from_secs(1));
        assert_eq!(origin.resolve(90_000), Duration::from_secs(2));
    }

    #[test]
    fn reordered_timestamps_are_not_read_as_a_wrap() {
        // B-frame reordering steps PTS backwards by a frame or two. Treating that as a
        // wrap would add 26.5 hours to every frame after it.
        let mut origin = PtsOrigin::default();
        assert_eq!(origin.resolve(90_000), Duration::ZERO);
        assert_eq!(origin.resolve(96_000), THIRTY_FPS_FRAME * 2);
        assert_eq!(origin.resolve(93_000), THIRTY_FPS_FRAME);
    }

    #[test]
    fn a_rekey_that_restarts_the_counter_does_not_send_time_backwards() {
        // What a source does mid-session when it re-keys: the PTS starts again near zero.
        // It is far too small a step to be a wrap, so it used to fall through to
        // `extended - first`, which saturates — an hour into a session, every frame after
        // the restart resolved to zero until the counter climbed past where it began, and
        // the time handed to A/V pacing stepped back by the whole hour.
        let mut origin = PtsOrigin::default();
        let start = 90_000;
        assert_eq!(origin.resolve(start), Duration::ZERO);
        let an_hour_in = start + 3600 * PTS_HZ;
        assert_eq!(origin.resolve(an_hour_in), Duration::from_secs(3600));

        // …and the source re-keys, restarting at a fresh small value.
        assert_eq!(
            origin.resolve(0),
            Duration::from_secs(3600),
            "the timeline continues from where it was"
        );
        assert_eq!(origin.resolve(PTS_HZ), Duration::from_secs(3601));
        assert_eq!(origin.resolve(2 * PTS_HZ), Duration::from_secs(3602));
    }

    #[test]
    fn an_interleaved_plane_is_not_mistaken_for_a_restart() {
        // Audio and video share one origin deliberately, so their timestamps step back
        // and forth past each other by the mux offset. Re-basing on that would restart the
        // timeline on every other packet.
        let mut origin = PtsOrigin::default();
        assert_eq!(origin.resolve(10 * PTS_HZ), Duration::ZERO);
        // A plane a second behind the other, alternating.
        assert_eq!(origin.resolve(9 * PTS_HZ), Duration::ZERO);
        assert_eq!(origin.resolve(11 * PTS_HZ), Duration::from_secs(1));
        assert_eq!(origin.resolve(10 * PTS_HZ), Duration::ZERO);
    }

    #[test]
    fn an_unknown_stream_type_is_ignored_rather_than_guessed() {
        let mut demux = TsDemux::new();
        demux.push(&ts_packet(PAT_PID, true, 0, &pat(PMT_PID)));
        demux.push(&ts_packet(PMT_PID, true, 0, &pmt(&[(Pid(0x1500), 0x06)])));
        assert!(demux.stream_types().is_empty());
    }

    #[test]
    fn a_null_packet_carries_nothing() {
        let mut demux = TsDemux::new();
        assert!(demux
            .push(&ts_packet(Pid::NULL, false, 0, &[0xFF; 184]))
            .is_empty());
    }

    #[test]
    fn timestamps_with_broken_marker_bits_are_rejected() {
        let mut bytes = timestamp_bytes(90_000, 0b0010);
        assert!(read_timestamp(&bytes).is_some());
        bytes[2] &= 0xFE;
        assert!(
            read_timestamp(&bytes).is_none(),
            "a zero marker means misalignment"
        );
    }
}
