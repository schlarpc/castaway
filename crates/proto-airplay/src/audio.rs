//! The RAOP audio depacketiser: pure, sans-I/O, `fn(state, datagram) -> outputs`.
//!
//! Three UDP sockets carry an AirPlay 1 audio session and this decides what every
//! datagram on any of them means. It never touches a socket — the actor feeds it bytes
//! and acts on what comes back, which is what makes the whole thing testable against
//! captured packets.
//!
//! ## The payload-type map
//!
//! The byte at offset 1 is `marker << 7 | payload_type`, so every value appears in
//! prior art both with and without the high bit. They are matched here on the masked
//! 7-bit type, which is the only reading that is right in both forms.
//!
//! | Type | Socket | Direction | Meaning |
//! |------|--------|-----------|---------|
//! | 96 | audio | in | ALAC/PCM audio |
//! | 86 | audio **and** control | in | a retransmitted packet, wrapped |
//! | 85 | control | out | our request for a resend |
//! | 84 | control | in | sync / timing anchor |
//! | 82 | timing | out | our timing request |
//! | 83 | timing | in | the sender's timing reply |
//!
//! A retransmit reply arrives on **either** socket depending on the sender —
//! shairport-sync handles both, UxPlay only the control port — so both are accepted.
//!
//! ## The decryption rule
//!
//! AES-128-CBC, and three details that prior art agrees on and reimplementations
//! routinely get wrong:
//!
//! 1. Only whole 16-byte blocks are encrypted. The trailing `len % 16` bytes are
//!    **plaintext and copied verbatim**. There is no padding to strip.
//! 2. The IV is **re-initialised from `a=aesiv` for every packet**. CBC state does not
//!    chain from one packet to the next.
//! 3. The 12-byte RTP header is not part of the cipher input.
//!
//! Get any of those wrong and audio still "plays" — as static.

use std::sync::Arc;
use std::time::Duration;

use aes::cipher::{BlockDecryptMut as _, KeyIvInit as _};
use bytes::Bytes;
use castaway_core::{AudioCodec, EncodedFrame};

use crate::clock::{StreamOrigin, SyncAnchor, SECONDS_1900_TO_1970};
use crate::sdp::{AnnounceParams, RaopCodec, StreamCrypto};

/// AES-128-CBC decryptor.
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// The RTP fixed header length. RAOP never sends CSRCs or extensions on audio.
const RTP_HEADER_LEN: usize = 12;

/// A retransmitted packet is prefixed with this many bytes before the original packet.
const RESEND_PREFIX_LEN: usize = 4;

/// AES operates on 16-byte blocks.
const AES_BLOCK: usize = 16;

/// The marker payload an AAC-ELD sender sends before it has audio to send.
const AAC_ELD_PRIMING: &[u8] = &[0x00, 0x68, 0x34, 0x00];

/// RTP payload types, as the 7-bit field (marker bit already masked off).
pub mod payload_type {
    /// Audio data.
    pub const AUDIO: u8 = 96;
    /// A retransmitted audio packet, wrapped in a 4-byte prefix.
    pub const RESEND_REPLY: u8 = 86;
    /// A resend request — we send these, we do not receive them.
    pub const RESEND_REQUEST: u8 = 85;
    /// Sync / timing anchor.
    pub const SYNC: u8 = 84;
    /// A timing request — we send these.
    pub const TIMING_REQUEST: u8 = 82;
    /// The sender's timing reply.
    pub const TIMING_REPLY: u8 = 83;
}

/// Errors the depacketiser can produce for a single datagram.
///
/// Every one of these is per-packet and recoverable: a bad datagram is dropped, not a
/// reason to end the session. Wi-Fi delivers garbage occasionally and a receiver that
/// tears down a session over one malformed packet is worse than one that skips it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioError {
    /// Shorter than an RTP header.
    #[error("datagram is {0} bytes, shorter than an RTP header")]
    TooShort(usize),

    /// The RTP version field was not 2.
    #[error("unsupported RTP version {0}")]
    BadVersion(u8),

    /// A payload type this socket should never carry.
    #[error("unexpected RTP payload type {0}")]
    UnexpectedType(u8),

    /// Audio arrived before any sync packet placed it on the sender's clock.
    ///
    /// Only reachable for a mirroring session, which is the only one that has to share a
    /// timeline with anything. The sender emits a sync packet at stream start, so this
    /// is the first packet or two — dropping them costs a few milliseconds and is the
    /// honest alternative to guessing an origin that video would then disagree with.
    #[error("audio arrived before the stream was anchored to the sender's clock")]
    AwaitingSync,

    /// Audio from before a `FLUSH`, queued for a position the sender has already left.
    #[error("audio predates the last FLUSH")]
    Stale,

    /// A copy of a frame this stream has already delivered.
    ///
    /// Not loss and not an error: an iOS mirroring sender asks for `redundantAudio` and
    /// then sends every frame three times on purpose. Exactly like [`Self::Priming`],
    /// this travels as an error so the caller's drop-and-continue path handles it.
    #[error("a redundant copy of a frame already delivered")]
    Duplicate,

    /// A stream-priming packet, which carries no audio.
    ///
    /// Not really an error — it is the sender saying "not yet" — but it travels as one
    /// so the caller's existing drop-and-continue path handles it without a second
    /// branch. Logged at debug rather than warn for the same reason.
    #[error("stream-priming packet, no audio yet")]
    Priming,
}

/// Where a `FLUSH` told us the stream restarts.
///
/// A seek or a track change: everything the sender queued before this point is stale, and
/// playing it is a moment of the *old* position before the new audio arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushPoint {
    /// The RTP timestamp the new position starts at, from `RTP-Info: rtptime=`.
    pub rtp: Option<u32>,
    /// The sequence number it starts at, from `RTP-Info: seq=`.
    ///
    /// shairport-sync reads only `rtptime` and UxPlay only `seq`; senders send both, so
    /// both are taken and each is used for what it is good for — the timestamp decides
    /// which audio is stale, and the sequence number re-seeds the resend tracker, so the
    /// gap is not read as loss and real loss right after it still is.
    pub seq: Option<u16>,
}

impl FlushPoint {
    /// Parse an `RTP-Info` header.
    #[must_use]
    pub fn parse(header: &str) -> Self {
        let mut point = Self::default();
        for part in header.split(';').map(str::trim) {
            if let Some(v) = part.strip_prefix("rtptime=") {
                point.rtp = v.trim().parse().ok();
            } else if let Some(v) = part.strip_prefix("seq=") {
                point.seq = v.trim().parse().ok();
            }
        }
        point
    }
}

/// A sync packet: the sender telling us which frame should be playing when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sync {
    /// The RTP timestamp of the frame that is *now*, less the sender's latency.
    pub rtp_now_less_latency: u32,
    /// The sender's clock at that instant, as an NTP 64-bit timestamp.
    pub sender_ntp: u64,
    /// The RTP timestamp the anchor really refers to.
    pub rtp_anchor: u32,
    /// Whether this is the first sync of a stream (the extension bit is the flag).
    pub first: bool,
}

impl Sync {
    /// The sender's declared latency, in frames.
    ///
    /// Observed values are 77175 (about 1.75 s) for ALAC and 7497 for AAC-ELD — which
    /// is why this is read from the packet rather than assumed.
    #[must_use]
    pub const fn latency_frames(&self) -> u32 {
        self.rtp_anchor.wrapping_sub(self.rtp_now_less_latency)
    }
}

/// What one datagram turned out to mean.
///
/// Not `PartialEq`: it carries an [`EncodedFrame`], which is not comparable and should
/// not become so just to make assertions terser. Tests match and compare the fields
/// they mean.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AudioOutput {
    /// Decoded-ready audio, in sender sequence order.
    Frame {
        /// The RTP sequence number, for gap detection.
        sequence: u16,
        /// The RTP timestamp (a frame counter at the stream's sample rate).
        rtp_timestamp: u32,
        /// The decrypted payload, ready for the decoder.
        frame: EncodedFrame,
    },
    /// A sync anchor.
    Sync(Sync),
    /// A timing reply, to be handed to whatever disciplines the clock.
    TimingReply(Box<[u8; 32]>),
}

/// The frames this stream has already handed on, so a copy of one is not handed on twice.
///
/// **Why a receiver needs this at all.** An iOS mirroring sender asks for
/// `redundantAudio: 2` in its `SETUP` and then transmits every audio frame *three times*,
/// in three packets, so that losing any two still delivers it. A receiver that plays what
/// it is given plays everything three times: three times the samples through a sink that
/// runs at one fixed rate, which sounds like the audio has been slowed down and shredded.
/// Measured at 275.6 frames/s where AAC-ELD at 44.1 kHz has 91.875 — exactly threefold.
///
/// **Why the obvious test is the wrong one.** "Drop anything whose timestamp does not
/// advance" also drops every *resend*, and the two arrive looking identical: an old
/// timestamp under a new sequence number. They mean opposite things — a redundancy copy
/// is audio already played, a resend is audio that never arrived — and the only thing
/// that tells them apart is whether this receiver ever delivered that frame. So that is
/// what is remembered, which makes this an anti-replay window and not a high-water mark.
///
/// Bounded by construction: the newest [`Self::WINDOW`] frames, about 0.7 s at 44.1 kHz,
/// which is far longer than any redundancy depth or resend round trip and far shorter
/// than the 27 hours the timestamp takes to wrap.
#[derive(Debug, Default)]
struct Delivered {
    recent: std::collections::VecDeque<u32>,
}

impl Delivered {
    /// How many delivered frames to remember.
    const WINDOW: usize = 64;

    /// Record `timestamp` as delivered, or report that it already was.
    fn accept(&mut self, timestamp: u32) -> bool {
        if self.recent.contains(&timestamp) {
            return false;
        }
        if self.recent.len() == Self::WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(timestamp);
        true
    }

    /// Forget everything, for a seek that may revisit these timestamps.
    fn clear(&mut self) {
        self.recent.clear();
    }
}

/// The depacketiser for one RAOP audio session.
pub struct AudioStream {
    codec: RaopCodec,
    crypto: StreamCrypto,
    audio_codec: Option<AudioCodec>,
    /// The RTP timestamp of the first audio packet, so `pts` can be relative to it.
    /// Used only when this stream stands alone; a mirroring session shares an origin.
    first_timestamp: Option<u32>,
    /// The origin shared with the video plane, when there is one.
    origin: Option<Arc<StreamOrigin>>,
    /// The latest sync packet, which is what puts audio on the sender's clock.
    anchor: Option<SyncAnchor>,
    /// Where the stream restarted, if a `FLUSH` said so.
    flush: Option<FlushPoint>,
    /// How many packets have been discarded as pre-flush.
    stale_dropped: u64,
    /// What has already been played, so a redundant copy of it is not played again.
    delivered: Delivered,
    /// How many packets have been discarded as copies of frames already delivered.
    duplicates_dropped: u64,
}

impl AudioStream {
    /// Build a depacketiser for what `ANNOUNCE` negotiated.
    #[must_use]
    pub fn new(params: &AnnounceParams) -> Self {
        Self {
            codec: params.codec,
            crypto: params.crypto.clone(),
            audio_codec: match params.codec {
                RaopCodec::Alac(_) => Some(AudioCodec::Alac),
                // PCM needs no decoder, so it carries no codec tag; the pipeline is told
                // the format instead.
                RaopCodec::Pcm { .. } => Some(AudioCodec::Pcm),
                RaopCodec::AacEld { .. } => Some(AudioCodec::Aac),
            },
            first_timestamp: None,
            origin: None,
            anchor: None,
            flush: None,
            stale_dropped: 0,
            delivered: Delivered::default(),
            duplicates_dropped: 0,
        }
    }

    /// Share a timeline with the video plane of a mirroring session.
    ///
    /// Without this the two planes each measure from their own first frame, which throws
    /// away the only thing that relates them. With it, audio waits for a sync packet
    /// before it can place itself — see [`AudioError::AwaitingSync`].
    #[must_use]
    pub fn with_origin(mut self, origin: Arc<StreamOrigin>) -> Self {
        self.origin = Some(origin);
        self
    }

    /// The negotiated codec.
    #[must_use]
    pub const fn codec(&self) -> &RaopCodec {
        &self.codec
    }

    /// Note that the sender flushed: everything before `point` is stale.
    ///
    /// The stale audio is still in flight — it was sent before the seek — so it is
    /// discarded on arrival rather than after being played. Without this a skip plays a
    /// moment of the old position first, which is the audible half of the bug; the other
    /// half is that the sequence jump looks like loss and provokes a resend request for
    /// packets that will never come.
    pub fn flush(&mut self, point: FlushPoint) {
        self.flush = Some(point);
        // A seek can legitimately revisit timestamps this stream has already played, so
        // what was delivered before it says nothing about what to expect after.
        self.delivered.clear();
    }

    /// How many packets have been dropped as copies of frames already delivered.
    #[must_use]
    pub const fn duplicates_dropped(&self) -> u64 {
        self.duplicates_dropped
    }

    /// How many packets have been dropped as stale since the last `FLUSH`.
    #[must_use]
    pub const fn stale_dropped(&self) -> u64 {
        self.stale_dropped
    }

    /// Handle a datagram from the **audio** socket.
    ///
    /// # Errors
    /// [`AudioError`] if the datagram is not a packet this socket carries. Callers drop
    /// the datagram and continue.
    pub fn on_audio(&mut self, datagram: &[u8]) -> Result<AudioOutput, AudioError> {
        let (kind, body) = split_rtp(datagram)?;
        match kind.payload_type {
            payload_type::AUDIO => self.audio_packet(kind, body),
            // Some senders put the retransmit on the audio socket instead of control.
            payload_type::RESEND_REPLY => self.resend_reply(datagram),
            other => Err(AudioError::UnexpectedType(other)),
        }
    }

    /// Handle a datagram from the **control** socket.
    ///
    /// # Errors
    /// [`AudioError`] if the datagram is not a packet this socket carries.
    pub fn on_control(&mut self, datagram: &[u8]) -> Result<AudioOutput, AudioError> {
        // The body is deliberately unused: a sync packet is not header-plus-payload
        // (see `parse_sync`), and a resend carries its own complete packet.
        let (kind, _) = split_rtp(datagram)?;
        match kind.payload_type {
            payload_type::SYNC => parse_sync(datagram).map(|sync| {
                // The anchor is what a shared timeline is built on, so it is recorded
                // here rather than left for the caller to remember.
                self.anchor = Some(SyncAnchor {
                    rtp: sync.rtp_anchor,
                    // The timing channel's stamps carry the 1900 epoch and the mirroring
                    // header's do not; stripping it is what puts them in one domain.
                    sender_ns: crate::clock::NtpTime::from_raw(sync.sender_ntp)
                        .as_nanos()
                        .saturating_sub(SECONDS_1900_TO_1970 * 1_000_000_000),
                });
                AudioOutput::Sync(sync)
            }),
            payload_type::RESEND_REPLY => self.resend_reply(datagram),
            other => Err(AudioError::UnexpectedType(other)),
        }
    }

    /// Handle a datagram from the **timing** socket.
    ///
    /// # Errors
    /// [`AudioError`] if it is not a timing reply.
    pub fn on_timing(&mut self, datagram: &[u8]) -> Result<AudioOutput, AudioError> {
        let (kind, _) = split_rtp(datagram)?;
        if kind.payload_type != payload_type::TIMING_REPLY {
            return Err(AudioError::UnexpectedType(kind.payload_type));
        }
        let fixed =
            <[u8; 32]>::try_from(datagram).map_err(|_| AudioError::TooShort(datagram.len()))?;
        Ok(AudioOutput::TimingReply(Box::new(fixed)))
    }

    /// A retransmitted packet: four bytes of wrapper, then a complete audio packet.
    fn resend_reply(&mut self, datagram: &[u8]) -> Result<AudioOutput, AudioError> {
        let inner = datagram
            .get(RESEND_PREFIX_LEN..)
            .ok_or(AudioError::TooShort(datagram.len()))?;
        let (kind, body) = split_rtp(inner)?;
        if kind.payload_type != payload_type::AUDIO {
            return Err(AudioError::UnexpectedType(kind.payload_type));
        }
        self.audio_packet(kind, body)
    }

    /// Decrypt (if needed) and package one audio payload.
    fn audio_packet(&mut self, kind: RtpKind, payload: &[u8]) -> Result<AudioOutput, AudioError> {
        // Anything from before the flush point is audio the sender queued for a position
        // it has already left. Serial arithmetic, not comparison: the timestamp wraps.
        if let Some(from) = self.flush.and_then(|f| f.rtp) {
            if kind.timestamp.wrapping_sub(from) > u32::MAX / 2 {
                self.stale_dropped = self.stale_dropped.saturating_add(1);
                return Err(AudioError::Stale);
            }
            // Caught up: stop testing every packet for the rest of the session.
            self.flush = None;
        }
        // A mirroring sender emits priming packets before its clock is up: a bare header
        // with no payload, or a four-byte marker. Feeding either to a decoder produces an
        // error per packet for the first second of every session.
        if payload.is_empty() || payload == AAC_ELD_PRIMING {
            return Err(AudioError::Priming);
        }
        // Before decrypting, because a copy of a frame already played costs nothing to
        // recognise and a block cipher pass to decode.
        if !self.delivered.accept(kind.timestamp) {
            self.duplicates_dropped = self.duplicates_dropped.saturating_add(1);
            return Err(AudioError::Duplicate);
        }
        let data = self.decrypt(payload);

        // `pts` is documented as time since stream start, and the RTP timestamp is a
        // frame counter at the sample rate — so the origin is whatever arrived first.
        // `wrapping_sub` because the counter wraps at 2^32 (about 27 hours at 44.1 kHz).
        let pts = match &self.origin {
            // Sharing a timeline with video: place this frame on the sender's clock and
            // measure from the session's origin, not this stream's first packet.
            Some(origin) => {
                let anchor = self.anchor.ok_or(AudioError::AwaitingSync)?;
                origin.pts(anchor.sender_ns_of(kind.timestamp, self.codec.sample_rate()))
            }
            // Standing alone, so there is nothing to be out of step with.
            None => {
                let first = *self.first_timestamp.get_or_insert(kind.timestamp);
                let elapsed_frames = kind.timestamp.wrapping_sub(first);
                Duration::from_nanos(
                    u64::from(elapsed_frames).saturating_mul(1_000_000_000)
                        / u64::from(self.codec.sample_rate().max(1)),
                )
            }
        };

        Ok(AudioOutput::Frame {
            sequence: kind.sequence,
            rtp_timestamp: kind.timestamp,
            frame: EncodedFrame {
                video_codec: None,
                audio_codec: self.audio_codec,
                pts,
                keyframe: false,
                data: Bytes::from(data),
            },
        })
    }

    /// Apply the AES-CBC rule from the module docs.
    fn decrypt(&self, payload: &[u8]) -> Vec<u8> {
        let StreamCrypto::Aes { key, iv } = &self.crypto else {
            return payload.to_vec();
        };
        let mut out = payload.to_vec();
        // Whole blocks only; the ragged tail stays exactly as it arrived.
        let encrypted_len = out.len() - (out.len() % AES_BLOCK);
        if encrypted_len == 0 {
            return out;
        }
        // A fresh cipher per packet *is* the per-packet IV reset. Chaining one across
        // packets is the classic bug, and it is not expressible here.
        let mut cipher = Aes128CbcDec::new(key.expose().into(), iv.into());
        cipher.decrypt_blocks_mut(bytemuck_blocks(&mut out[..encrypted_len]));
        out
    }
}

/// Reinterpret a whole-block slice as AES blocks for the block cipher API.
fn bytemuck_blocks(buf: &mut [u8]) -> &mut [aes::cipher::Block<aes::Aes128>] {
    // `chunks_exact_mut` yields 16-byte slices; `GenericArray` is `#[repr(transparent)]`
    // over `[u8; 16]`, so the conversion is a reborrow rather than a cast.
    aes::cipher::inout::InOutBuf::from(buf)
        .into_chunks::<aes::cipher::consts::U16>()
        .0
        .into_out()
}

/// The parts of an RTP header this protocol actually uses.
#[derive(Debug, Clone, Copy)]
struct RtpKind {
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
}

/// Split a datagram into its header fields and payload.
fn split_rtp(datagram: &[u8]) -> Result<(RtpKind, &[u8]), AudioError> {
    if datagram.len() < RTP_HEADER_LEN {
        return Err(AudioError::TooShort(datagram.len()));
    }
    let version = datagram[0] >> 6;
    if version != 2 {
        return Err(AudioError::BadVersion(version));
    }
    let kind = RtpKind {
        // Mask off the marker bit: every payload type in this protocol shows up both
        // with and without it, depending on the packet.
        payload_type: datagram[1] & 0x7F,
        sequence: u16::from_be_bytes([datagram[2], datagram[3]]),
        timestamp: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
    };
    Ok((kind, &datagram[RTP_HEADER_LEN..]))
}

/// A sync packet is exactly this long.
const SYNC_PACKET_LEN: usize = 20;

/// Parse a 20-byte sync packet.
///
/// This one is **not** an RTP header followed by a payload, which is why it is parsed
/// from the whole datagram rather than from the body `split_rtp` hands back: the NTP
/// timestamp occupies bytes 8..16, where an RTP header would carry its SSRC. Only the
/// first eight bytes line up with RTP at all.
fn parse_sync(datagram: &[u8]) -> Result<Sync, AudioError> {
    let fixed: &[u8; SYNC_PACKET_LEN] = datagram
        .get(..SYNC_PACKET_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or(AudioError::TooShort(datagram.len()))?;
    Ok(Sync {
        rtp_now_less_latency: u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]),
        sender_ntp: u64::from_be_bytes([
            fixed[8], fixed[9], fixed[10], fixed[11], fixed[12], fixed[13], fixed[14], fixed[15],
        ]),
        rtp_anchor: u32::from_be_bytes([fixed[16], fixed[17], fixed[18], fixed[19]]),
        // The RTP "extension" bit is reused as a first-sync-of-the-stream flag.
        first: datagram[0] & 0x10 != 0,
    })
}

/// Build the 8-byte resend request for `count` packets starting at `first`.
///
/// We are the only ones who send these, so it is a constructor rather than a parser.
#[must_use]
pub fn resend_request(our_sequence: u16, first: u16, count: u16) -> [u8; 8] {
    let mut p = [0u8; 8];
    p[0] = 0x80;
    // The marker bit is always set on the requests every implementation sends.
    p[1] = 0x80 | payload_type::RESEND_REQUEST;
    p[2..4].copy_from_slice(&our_sequence.to_be_bytes());
    p[4..6].copy_from_slice(&first.to_be_bytes());
    p[6..8].copy_from_slice(&count.to_be_bytes());
    p
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    /// The AirPort key is carved at build time rather than checked in, so a build
    /// without it cannot exercise the RSA paths. `nix flake check` always has it.
    fn skip_without_airport_key() -> bool {
        if crypto_raop::has_airport_key() {
            return false;
        }
        eprintln!("skipping: this build has no AirPort key");
        true
    }
    use super::*;
    use crate::sdp::AnnounceParams;

    const ALAC_SDP: &str = "v=0\r\nm=audio 0 RTP/AVP 96\r\n\
        a=rtpmap:96 AppleLossless\r\n\
        a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n";

    fn plain_stream() -> AudioStream {
        AudioStream::new(&AnnounceParams::parse(ALAC_SDP.as_bytes()).unwrap())
    }

    /// An audio packet with `payload`, sequence 7, timestamp `ts`.
    fn audio_packet(ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, 0x60];
        p.extend_from_slice(&7u16.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn an_unencrypted_payload_passes_straight_through() {
        let mut s = plain_stream();
        let out = s.on_audio(&audio_packet(1000, b"hello world")).unwrap();
        let AudioOutput::Frame {
            frame, sequence, ..
        } = out
        else {
            panic!("expected a frame")
        };
        assert_eq!(sequence, 7);
        assert_eq!(frame.data.as_ref(), b"hello world");
        assert_eq!(frame.audio_codec, Some(AudioCodec::Alac));
    }

    #[test]
    fn the_marker_bit_does_not_change_the_payload_type() {
        // The first packet of a stream sets it, so a match on the raw byte sees 0xE0
        // and treats real audio as an unknown type.
        let mut s = plain_stream();
        let mut p = audio_packet(0, b"abcd");
        p[1] |= 0x80;
        assert!(s.on_audio(&p).is_ok());
    }

    #[test]
    fn pts_is_measured_from_the_first_packet_not_from_zero() {
        // RTP timestamps start wherever the sender's counter happens to be. A pts taken
        // from the raw value would start the stream hours in.
        let mut s = plain_stream();
        let first = s.on_audio(&audio_packet(1_000_000, b"aaaa")).unwrap();
        let AudioOutput::Frame { frame, .. } = first else {
            panic!()
        };
        assert_eq!(frame.pts, Duration::ZERO);

        // 44100 frames later is exactly one second.
        let next = s.on_audio(&audio_packet(1_044_100, b"bbbb")).unwrap();
        let AudioOutput::Frame { frame, .. } = next else {
            panic!()
        };
        assert_eq!(frame.pts, Duration::from_secs(1));
    }

    #[test]
    fn the_rtp_timestamp_wrapping_does_not_rewind_the_stream() {
        let mut s = plain_stream();
        s.on_audio(&audio_packet(u32::MAX - 100, b"aaaa")).unwrap();
        // 101 frames later, having wrapped through zero.
        let out = s.on_audio(&audio_packet(0, b"bbbb")).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!()
        };
        assert_eq!(frame.pts, Duration::from_nanos(101 * 1_000_000_000 / 44100));
    }

    /// The same frame under a different sequence number, which is how a redundant copy
    /// and a resend both arrive.
    fn audio_packet_seq(seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = audio_packet(ts, payload);
        p[2..4].copy_from_slice(&seq.to_be_bytes());
        p
    }

    #[test]
    fn a_redundant_copy_of_a_delivered_frame_is_not_delivered_again() {
        // iOS asks for `redundantAudio: 2` on every mirroring session and then sends each
        // frame three times. Playing all three is three times the samples through a sink
        // running at one fixed rate — measured at 275.6 frames/s where 44.1 kHz AAC-ELD
        // has 91.875 — which is what "slow and garbled" was.
        let mut s = plain_stream();
        assert!(s.on_audio(&audio_packet_seq(1, 1000, b"aaaa")).is_ok());
        for seq in 2..=3 {
            assert_eq!(
                s.on_audio(&audio_packet_seq(seq, 1000, b"aaaa"))
                    .unwrap_err(),
                AudioError::Duplicate,
                "copy {seq} of one frame must not be played again"
            );
        }
        assert_eq!(s.duplicates_dropped(), 2);
        // …and the next real frame still passes.
        assert!(s.on_audio(&audio_packet_seq(4, 1480, b"bbbb")).is_ok());
    }

    #[test]
    fn a_resend_of_a_frame_that_never_arrived_is_still_delivered() {
        // The distinction the window exists for. A resend and a redundant copy arrive
        // looking identical — an old timestamp under a new sequence number — and mean
        // opposite things. A high-water mark on the timestamp cannot tell them apart and
        // would silently break retransmission, which is the whole point of asking for it.
        let mut s = plain_stream();
        assert!(s.on_audio(&audio_packet_seq(1, 1000, b"aaaa")).is_ok());
        // 1480 is lost; 1960 arrives.
        assert!(s.on_audio(&audio_packet_seq(3, 1960, b"cccc")).is_ok());
        // The sender resends the gap, out of order and older than what we have played.
        let out = s.on_audio(&audio_packet_seq(2, 1480, b"bbbb")).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!("a resent frame must reach the decoder")
        };
        assert_eq!(frame.data.as_ref(), b"bbbb");
        assert_eq!(s.duplicates_dropped(), 0);
    }

    #[test]
    fn a_seek_may_revisit_timestamps_it_has_already_played() {
        // Otherwise seeking back into audio that was played once would be silence.
        let mut s = plain_stream();
        assert!(s.on_audio(&audio_packet_seq(1, 1000, b"aaaa")).is_ok());
        s.flush(FlushPoint {
            rtp: Some(1000),
            seq: Some(1),
        });
        let out = s.on_audio(&audio_packet_seq(9, 1000, b"aaaa")).unwrap();
        assert!(matches!(out, AudioOutput::Frame { .. }));
    }

    #[test]
    fn the_delivered_window_stays_bounded() {
        // It is fed by the network for the length of a session, so it must not be a leak.
        let mut d = Delivered::default();
        for ts in 0..10_000u32 {
            assert!(d.accept(ts * 480));
        }
        assert!(d.recent.len() <= Delivered::WINDOW);
        // And what fell out of it is accepted again rather than blocked forever, which is
        // the right failure: a copy that late is indistinguishable from a fresh frame.
        assert!(d.accept(0));
    }

    #[test]
    fn a_retransmitted_packet_is_unwrapped_and_treated_as_audio() {
        let mut s = plain_stream();
        let mut wrapped = vec![0x80, 0xD6, 0x00, 0x00];
        wrapped.extend_from_slice(&audio_packet(500, b"resent!!"));
        let out = s.on_control(&wrapped).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!("a resend should yield the audio it carries")
        };
        assert_eq!(frame.data.as_ref(), b"resent!!");
    }

    #[test]
    fn a_retransmit_on_the_audio_socket_is_also_accepted() {
        // shairport-sync handles this and UxPlay does not; senders differ, so we take
        // it on either socket rather than dropping audio we were given.
        let mut s = plain_stream();
        let mut wrapped = vec![0x80, 0xD6, 0x00, 0x00];
        wrapped.extend_from_slice(&audio_packet(500, b"resent!!"));
        assert!(matches!(
            s.on_audio(&wrapped),
            Ok(AudioOutput::Frame { .. })
        ));
    }

    #[test]
    fn a_sync_packet_yields_the_senders_latency() {
        let mut s = plain_stream();
        // The real 20-byte layout: flags at 2..4, "now less latency" at 4..8, the
        // sender's NTP clock at 8..16 (where RTP would put an SSRC), anchor at 16..20.
        let mut p = vec![0x90, 0xD4]; // extension bit set: the first sync of a stream
        p.extend_from_slice(&4u16.to_be_bytes());
        p.extend_from_slice(&1000u32.to_be_bytes()); // now, less latency
        p.extend_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        p.extend_from_slice(&78175u32.to_be_bytes()); // the anchor
        assert_eq!(p.len(), 20);
        let AudioOutput::Sync(sync) = s.on_control(&p).unwrap() else {
            panic!("expected a sync")
        };
        assert!(sync.first);
        assert_eq!(sync.sender_ntp, 0x1122_3344_5566_7788);
        assert_eq!(sync.latency_frames(), 77175);
    }

    #[test]
    fn a_timing_reply_is_handed_over_whole() {
        let mut s = plain_stream();
        let mut p = vec![0x80, 0xD3];
        p.extend_from_slice(&[0u8; 30]);
        let AudioOutput::TimingReply(reply) = s.on_timing(&p).unwrap() else {
            panic!("expected a timing reply")
        };
        assert_eq!(reply.len(), 32);
    }

    #[test]
    fn a_packet_type_a_socket_never_carries_is_refused() {
        let mut s = plain_stream();
        // A sync packet on the audio socket.
        let mut p = vec![0x80, 0xD4];
        p.extend_from_slice(&[0u8; 18]);
        assert_eq!(s.on_audio(&p).unwrap_err(), AudioError::UnexpectedType(84));
    }

    #[test]
    fn mirror_audio_frames_are_tagged_aac_and_decrypt_with_the_media_key() {
        use crate::sdp::SessionKey;
        let key = *b"0123456789abcdef";
        let iv = *b"ABCDEFGHIJKLMNOP";
        let params = AnnounceParams::mirror_aac_eld(SessionKey::from_bytes(key), iv);
        let mut s = AudioStream::new(&params);

        // A real AAC-ELD access unit starts 0x8c..0x8e; the sanity check prior art uses.
        let mut plain = vec![0x8cu8];
        plain.extend_from_slice(b"an-aac-eld-access-unit-payload");
        let cipher = encrypt_like_a_sender(&key, &iv, &plain);

        let out = s.on_audio(&audio_packet(0, &cipher)).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!("expected a frame")
        };
        assert_eq!(frame.audio_codec, Some(AudioCodec::Aac));
        assert_eq!(frame.data.as_ref(), plain.as_slice());
    }

    #[test]
    fn a_shared_origin_puts_audio_on_the_same_timeline_as_video() {
        // The bug this closes: each plane used to measure from its own first frame, so
        // the offset between them was whatever the two streams happened to start at.
        use crate::clock::StreamOrigin;
        use crate::sdp::SessionKey;
        let origin = std::sync::Arc::new(StreamOrigin::new());

        // Video anchors the session at 50 s of sender uptime.
        assert_eq!(origin.pts(50_000_000_000), Duration::ZERO);

        let params = AnnounceParams::mirror_aac_eld(SessionKey::from_bytes([1u8; 16]), [2u8; 16]);
        let mut s = AudioStream::new(&params).with_origin(std::sync::Arc::clone(&origin));

        // A sync packet says RTP 1_000_000 was 50.5 s of sender uptime.
        let mut sync = vec![0x80, 0xD4, 0, 0];
        sync.extend_from_slice(&0u32.to_be_bytes());
        // `from_unix_nanos` adds the 1900 epoch, which is exactly what a sender's sync
        // packet carries and what the parser strips back off.
        let ntp = crate::clock::NtpTime::from_unix_nanos(50_500_000_000);
        sync.extend_from_slice(&ntp.raw().to_be_bytes());
        sync.extend_from_slice(&1_000_000u32.to_be_bytes());
        s.on_control(&sync).unwrap();

        // So an audio frame at that RTP presents half a second after the video did.
        let out = s
            .on_audio(&audio_packet(1_000_000, b"sixteen bytes!!!"))
            .unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!("expected a frame")
        };
        let drift = frame.pts.as_millis().abs_diff(500);
        assert!(drift < 5, "audio landed at {:?}, not 500 ms", frame.pts);
    }

    #[test]
    fn mirror_audio_before_a_sync_packet_is_dropped_rather_than_guessed() {
        // It cannot be placed on the shared timeline, and inventing an origin would put
        // it wherever it happened to start — which is exactly the bug being fixed.
        use crate::clock::StreamOrigin;
        use crate::sdp::SessionKey;
        let params = AnnounceParams::mirror_aac_eld(SessionKey::from_bytes([1u8; 16]), [2u8; 16]);
        let mut s = AudioStream::new(&params).with_origin(std::sync::Arc::new(StreamOrigin::new()));
        assert_eq!(
            s.on_audio(&audio_packet(0, b"sixteen bytes!!!"))
                .unwrap_err(),
            AudioError::AwaitingSync
        );
    }

    #[test]
    fn an_audio_only_session_needs_no_anchor_because_it_syncs_with_nothing() {
        // The AirPlay 1 flow has no second plane, so it keeps measuring from its own
        // first packet and never waits for a sync packet to start playing.
        let mut s = plain_stream();
        assert!(s.on_audio(&audio_packet(1_000, b"aaaa")).is_ok());
    }

    #[test]
    fn priming_packets_are_not_treated_as_broken_audio() {
        // A mirroring sender emits these before its clock is up. Without this they would
        // be a decoder error per packet for the first second of every session.
        use crate::sdp::SessionKey;
        let params = AnnounceParams::mirror_aac_eld(SessionKey::from_bytes([1u8; 16]), [2u8; 16]);
        let mut s = AudioStream::new(&params);
        assert_eq!(
            s.on_audio(&audio_packet(0, &[0x00, 0x68, 0x34, 0x00]))
                .unwrap_err(),
            AudioError::Priming
        );
        // A header with no payload at all is the same thing.
        assert_eq!(
            s.on_audio(&audio_packet(0, &[])).unwrap_err(),
            AudioError::Priming
        );
    }

    #[test]
    fn a_flush_discards_the_audio_the_sender_has_left_behind() {
        // What a skip on the phone looks like: packets for the old position are already
        // in flight. Playing them is a moment of the previous track before the new one.
        let mut s = plain_stream();
        s.on_audio(&audio_packet(1_000, b"old-audio-aaaa")).unwrap();
        s.flush(FlushPoint {
            rtp: Some(50_000),
            seq: Some(7),
        });
        assert_eq!(
            s.on_audio(&audio_packet(1_352, b"stale-aaaaaaaa"))
                .unwrap_err(),
            AudioError::Stale
        );
        assert_eq!(s.stale_dropped(), 1);
        // …and the audio from the new position plays.
        assert!(s.on_audio(&audio_packet(50_000, b"new-audio-aaaa")).is_ok());
    }

    #[test]
    fn a_flush_across_the_rtp_wrap_does_not_discard_the_whole_stream() {
        // Serial arithmetic again: read as a comparison, a flush point just below the
        // wrap makes every packet after it look stale for the next 27 hours.
        let mut s = plain_stream();
        s.flush(FlushPoint {
            rtp: Some(u32::MAX - 100),
            seq: None,
        });
        // A packet 200 frames on, having wrapped through zero, is *after* the point.
        assert!(s.on_audio(&audio_packet(99, b"after-the-wrap")).is_ok());
    }

    #[test]
    fn once_the_stream_catches_up_the_flush_stops_being_tested() {
        let mut s = plain_stream();
        s.flush(FlushPoint {
            rtp: Some(1_000),
            seq: None,
        });
        assert!(s.on_audio(&audio_packet(1_000, b"at-the-point!!")).is_ok());
        // A late straggler from before the point is now let through rather than dropped
        // — the sender has moved on and so have we.
        assert!(s.on_audio(&audio_packet(500, b"a-late-packet!")).is_ok());
    }

    #[test]
    fn rtp_info_is_parsed_the_way_both_reference_receivers_read_it() {
        // shairport-sync reads only rtptime and UxPlay only seq; senders send both.
        let p = FlushPoint::parse("seq=1234;rtptime=567890");
        assert_eq!(p.seq, Some(1234));
        assert_eq!(p.rtp, Some(567_890));
        // A header with only one of them is still usable.
        assert_eq!(FlushPoint::parse("rtptime=42").rtp, Some(42));
        assert_eq!(FlushPoint::parse("seq=9").seq, Some(9));
    }

    #[test]
    fn a_runt_datagram_is_an_error_not_a_panic() {
        let mut s = plain_stream();
        assert_eq!(
            s.on_audio(&[0x80, 0x60]).unwrap_err(),
            AudioError::TooShort(2)
        );
    }

    #[test]
    fn a_non_rtp_datagram_is_refused() {
        let mut s = plain_stream();
        assert_eq!(
            s.on_audio(&[0x00; 16]).unwrap_err(),
            AudioError::BadVersion(0)
        );
    }

    #[test]
    fn resend_requests_have_the_shape_every_sender_expects() {
        let p = resend_request(1, 0x1234, 3);
        assert_eq!(p[0], 0x80);
        assert_eq!(p[1], 0xD5, "marker bit set, payload type 85");
        assert_eq!(&p[4..6], &0x1234u16.to_be_bytes());
        assert_eq!(&p[6..8], &3u16.to_be_bytes());
    }

    // --- decryption ---

    /// Build an encrypted stream plus the key/iv used, so a test can encrypt for it.
    fn encrypted_stream() -> (AudioStream, [u8; 16], [u8; 16]) {
        use base64::Engine as _;
        let key = *b"0123456789abcdef";
        let iv = *b"ABCDEFGHIJKLMNOP";
        let wrapped = crypto_raop::airport_public_key()
            .unwrap()
            .encrypt(
                &mut rsa::rand_core::OsRng,
                rsa::Oaep::new::<sha1::Sha1>(),
                &key,
            )
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD_NO_PAD;
        let sdp = format!(
            "{ALAC_SDP}a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
            b64.encode(&wrapped),
            b64.encode(iv)
        );
        let params = AnnounceParams::parse(sdp.as_bytes()).unwrap();
        (AudioStream::new(&params), key, iv)
    }

    /// Encrypt the way a sender does: whole blocks only, tail left alone.
    fn encrypt_like_a_sender(key: &[u8; 16], iv: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        use aes::cipher::BlockEncryptMut as _;
        let mut out = plain.to_vec();
        let n = out.len() - (out.len() % AES_BLOCK);
        if n > 0 {
            let mut enc = cbc::Encryptor::<aes::Aes128>::new(key.into(), iv.into());
            let buf = aes::cipher::inout::InOutBuf::from(&mut out[..n])
                .into_chunks::<aes::cipher::consts::U16>()
                .0
                .into_out();
            enc.encrypt_blocks_mut(buf);
        }
        out
    }

    #[test]
    fn an_encrypted_payload_round_trips() {
        if skip_without_airport_key() {
            return;
        }
        let (mut s, key, iv) = encrypted_stream();
        let plain = b"sixteen bytes!!! and a ragged tail";
        let cipher = encrypt_like_a_sender(&key, &iv, plain);
        let out = s.on_audio(&audio_packet(0, &cipher)).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!()
        };
        assert_eq!(frame.data.as_ref(), plain.as_slice());
    }

    #[test]
    fn the_ragged_tail_is_copied_verbatim_not_decrypted() {
        if skip_without_airport_key() {
            return;
        }
        // 33 bytes: two whole blocks and one left over. If the tail went through the
        // cipher, or were dropped as padding, this fails.
        let (mut s, key, iv) = encrypted_stream();
        let plain: Vec<u8> = (0u8..33).collect();
        let cipher = encrypt_like_a_sender(&key, &iv, &plain);
        assert_eq!(cipher[32], plain[32], "the sender leaves the tail alone");
        let out = s.on_audio(&audio_packet(0, &cipher)).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!()
        };
        assert_eq!(frame.data.as_ref(), plain.as_slice());
        assert_eq!(frame.data.len(), 33, "nothing stripped as padding");
    }

    #[test]
    fn the_iv_resets_every_packet_rather_than_chaining() {
        if skip_without_airport_key() {
            return;
        }
        // The bug this exists to prevent: carrying CBC state across packets. Two
        // identical payloads must decrypt identically, and the second must not depend
        // on the first having been seen.
        let (mut s, key, iv) = encrypted_stream();
        let plain = b"exactly16bytes!!";
        let cipher = encrypt_like_a_sender(&key, &iv, plain);

        let first = s.on_audio(&audio_packet(0, &cipher)).unwrap();
        let second = s.on_audio(&audio_packet(352, &cipher)).unwrap();
        let (AudioOutput::Frame { frame: a, .. }, AudioOutput::Frame { frame: b, .. }) =
            (first, second)
        else {
            panic!()
        };
        assert_eq!(a.data, b.data);
        assert_eq!(a.data.as_ref(), plain.as_slice());
    }

    #[test]
    fn a_payload_shorter_than_one_block_is_left_alone() {
        if skip_without_airport_key() {
            return;
        }
        let (mut s, _, _) = encrypted_stream();
        let out = s.on_audio(&audio_packet(0, b"short")).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!()
        };
        assert_eq!(frame.data.as_ref(), b"short");
    }
}
