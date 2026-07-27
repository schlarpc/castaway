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

use std::time::Duration;

use aes::cipher::{BlockDecryptMut as _, KeyIvInit as _};
use bytes::Bytes;
use castaway_core::{AudioCodec, EncodedFrame};

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

    /// A stream-priming packet, which carries no audio.
    ///
    /// Not really an error — it is the sender saying "not yet" — but it travels as one
    /// so the caller's existing drop-and-continue path handles it without a second
    /// branch. Logged at debug rather than warn for the same reason.
    #[error("stream-priming packet, no audio yet")]
    Priming,
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

/// The depacketiser for one RAOP audio session.
pub struct AudioStream {
    codec: RaopCodec,
    crypto: StreamCrypto,
    audio_codec: Option<AudioCodec>,
    /// The RTP timestamp of the first audio packet, so `pts` can be relative to it.
    first_timestamp: Option<u32>,
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
        }
    }

    /// The negotiated codec.
    #[must_use]
    pub const fn codec(&self) -> &RaopCodec {
        &self.codec
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
            payload_type::SYNC => parse_sync(datagram).map(AudioOutput::Sync),
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
        // A mirroring sender emits priming packets before its clock is up: a bare header
        // with no payload, or a four-byte marker. Feeding either to a decoder produces an
        // error per packet for the first second of every session.
        if payload.is_empty() || payload == AAC_ELD_PRIMING {
            return Err(AudioError::Priming);
        }
        let data = self.decrypt(payload);

        // `pts` is documented as time since stream start, and the RTP timestamp is a
        // frame counter at the sample rate — so the origin is whatever arrived first.
        // `wrapping_sub` because the counter wraps at 2^32 (about 27 hours at 44.1 kHz).
        let first = *self.first_timestamp.get_or_insert(kind.timestamp);
        let elapsed_frames = kind.timestamp.wrapping_sub(first);
        let pts = Duration::from_nanos(
            u64::from(elapsed_frames).saturating_mul(1_000_000_000)
                / u64::from(self.codec.sample_rate().max(1)),
        );

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
        use rsa::pkcs1::DecodeRsaPrivateKey as _;
        let key = *b"0123456789abcdef";
        let iv = *b"ABCDEFGHIJKLMNOP";
        let private =
            rsa::RsaPrivateKey::from_pkcs1_pem(include_str!("../../crypto-raop/src/airport.pem"))
                .unwrap();
        let wrapped = rsa::RsaPublicKey::from(&private)
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
        let (mut s, _, _) = encrypted_stream();
        let out = s.on_audio(&audio_packet(0, b"short")).unwrap();
        let AudioOutput::Frame { frame, .. } = out else {
            panic!()
        };
        assert_eq!(frame.data.as_ref(), b"short");
    }
}
