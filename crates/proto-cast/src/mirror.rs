//! Cast mirroring (a.k.a. Cast Streaming) negotiation — the control plane for
//! "Cast desktop/tab" from Chrome/Edge. The sender sends an `OFFER` on the
//! `com.google.cast.webrtc` namespace listing encrypted RTP streams; we reply with an
//! `ANSWER` selecting streams + a UDP port. The AES key/IV live *in the offer* (the
//! sender picks them), so the receiver learns how to decrypt from the negotiation.
//!
//! This module is the pure negotiator + the per-frame AES-128-CTR crypto. The UDP RTP
//! receive/reassembly loop is the actor's job and is deferred (OPEN-QUESTIONS): the
//! negotiator hands the actor a [`MirrorConfig`] to drive it.

use std::num::NonZeroU32;

use aes::cipher::{KeyIvInit, StreamCipher};
use castaway_core::{AudioCodec, VideoCodec};
use serde::Deserialize;

use crate::error::CastError;
use crate::rtp::FrameId;

/// The mirroring namespace.
pub const WEBRTC_NS: &str = "urn:x-cast:com.google.cast.webrtc";

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// A codec named in an offered stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264/AVC video.
    H264,
    /// H.265/HEVC video.
    Hevc,
    /// VP8 video.
    Vp8,
    /// Opus audio.
    Opus,
    /// AAC audio.
    Aac,
}

impl Codec {
    fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "h264" => Some(Codec::H264),
            "hevc" | "h265" => Some(Codec::Hevc),
            "vp8" => Some(Codec::Vp8),
            "opus" => Some(Codec::Opus),
            "aac" => Some(Codec::Aac),
            _ => None,
        }
    }

    const fn is_video(self) -> bool {
        matches!(self, Codec::H264 | Codec::Hevc | Codec::Vp8)
    }

    /// How this codec is named to the pipeline, which is codec-aware but Cast-agnostic.
    ///
    /// Total on purpose: a codec we can negotiate but cannot name would be a stream we
    /// accept and then fail to play, and the compiler should refuse to let that happen.
    const fn media_kind(self) -> MediaKind {
        match self {
            Codec::H264 => MediaKind::Video(VideoCodec::H264),
            Codec::Hevc => MediaKind::Video(VideoCodec::Hevc),
            Codec::Vp8 => MediaKind::Video(VideoCodec::Vp8),
            Codec::Opus => MediaKind::Audio(AudioCodec::Opus),
            Codec::Aac => MediaKind::Audio(AudioCodec::Aac),
        }
    }

    /// Which of two offered codecs to prefer, higher wins.
    ///
    /// The deploy target hardware-decodes H.264 and HEVC but not VP8, and Chrome lists
    /// VP8 first in its offer — so "take the first one we recognize" would
    /// systematically land on the software path.
    const fn preference(self) -> u8 {
        match self {
            Codec::H264 => 3,
            Codec::Hevc => 2,
            Codec::Vp8 => 1,
            Codec::Opus => 2,
            Codec::Aac => 1,
        }
    }

    /// The RTP clock rate to assume when an offered stream omits `timeBase`.
    const fn default_timebase(self) -> NonZeroU32 {
        match self {
            Codec::H264 | Codec::Hevc | Codec::Vp8 => VIDEO_TIMEBASE,
            Codec::Opus | Codec::Aac => AUDIO_TIMEBASE,
        }
    }
}

/// A codec sorted into the two halves of [`castaway_core::EncodedFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// A video codec the pipeline decodes.
    Video(VideoCodec),
    /// An audio codec the pipeline decodes.
    Audio(AudioCodec),
}

/// A literal clock rate. `NonZeroU32::new(..).unwrap()` is not const, and a library
/// crate does not get to panic (ground rule 7), so the zero case is spelled out.
macro_rules! timebase {
    ($hz:literal) => {
        match NonZeroU32::new($hz) {
            Some(rate) => rate,
            None => NonZeroU32::MIN,
        }
    };
}

/// 90 kHz — the RTP convention for video, and what Cast senders offer.
const VIDEO_TIMEBASE: NonZeroU32 = timebase!(90_000);
/// 48 kHz — Cast audio is timestamped in samples, and every sender we have seen is 48k.
const AUDIO_TIMEBASE: NonZeroU32 = timebase!(48_000);

/// The negotiated configuration for one RTP stream.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// The offer's stream index (echoed in `sendIndexes`).
    pub index: i32,
    /// The sender's SSRC (the stream we receive).
    pub sender_ssrc: u32,
    /// Our SSRC (used for RTCP feedback).
    pub receiver_ssrc: u32,
    /// RTP payload type.
    pub payload_type: u8,
    /// The codec.
    pub codec: Codec,
    /// The stream's RTP clock rate, from the offer's `timeBase`. Frame timestamps are
    /// counted in these ticks, so a wrong value is a wrong presentation time.
    pub rtp_timebase: NonZeroU32,
    /// AES-128 key for this stream (from the offer).
    pub aes_key: [u8; 16],
    /// AES IV mask for this stream (from the offer); per-frame IVs derive from it.
    pub aes_iv_mask: [u8; 16],
}

impl StreamConfig {
    /// How the pipeline should be told to decode this stream.
    #[must_use]
    pub const fn media_kind(&self) -> MediaKind {
        self.codec.media_kind()
    }
}

/// The full negotiated mirroring session.
///
/// Video is not optional. Cast Streaming can carry an audio-only session, but
/// [`castaway_core::SessionEvent::Mirror`] has nowhere to put one — so an offer without
/// video is declined at negotiation rather than accepted and then abandoned when the
/// actor discovers it has nothing to show.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// UDP port we will receive RTP on.
    pub udp_port: u16,
    /// The selected video stream.
    pub video: StreamConfig,
    /// The selected audio stream, if any.
    pub audio: Option<StreamConfig>,
}

#[derive(Debug, Deserialize)]
struct OfferEnvelope {
    #[serde(rename = "seqNum")]
    seq_num: i64,
    offer: Offer,
}

#[derive(Debug, Deserialize)]
struct Offer {
    #[serde(rename = "supportedStreams")]
    supported_streams: Vec<OfferedStream>,
}

#[derive(Debug, Deserialize)]
struct OfferedStream {
    index: i32,
    #[serde(rename = "codecName")]
    codec_name: String,
    #[serde(rename = "rtpPayloadType")]
    rtp_payload_type: Option<u8>,
    ssrc: u32,
    #[serde(rename = "timeBase")]
    time_base: Option<String>,
    #[serde(rename = "aesKey")]
    aes_key: String,
    #[serde(rename = "aesIvMask")]
    aes_iv_mask: String,
}

/// Parse an offered `timeBase` — always written as the reciprocal, `"1/90000"`.
fn parse_timebase(spec: Option<&str>, codec: Codec) -> Result<NonZeroU32, CastError> {
    let Some(spec) = spec else {
        return Ok(codec.default_timebase());
    };
    let denominator = spec
        .trim()
        .strip_prefix("1/")
        .ok_or(CastError::Mirror("timeBase is not of the form 1/N"))?;
    denominator
        .parse::<NonZeroU32>()
        .map_err(|_| CastError::Mirror("timeBase denominator is not a positive integer"))
}

/// Negotiate a mirroring OFFER: choose one video + one audio stream and build the
/// ANSWER JSON plus the [`MirrorConfig`] the actor uses to receive+decrypt.
///
/// # Errors
/// [`CastError`] if the OFFER is malformed or has no usable stream.
pub fn negotiate(offer_payload: &str, udp_port: u16) -> Result<(String, MirrorConfig), CastError> {
    let env: OfferEnvelope =
        serde_json::from_str(offer_payload).map_err(|e| CastError::Json(e.to_string()))?;

    let mut video: Option<StreamConfig> = None;
    let mut audio: Option<StreamConfig> = None;
    for s in &env.offer.supported_streams {
        let Some(codec) = Codec::parse(&s.codec_name) else {
            continue;
        };
        let cfg = StreamConfig {
            index: s.index,
            sender_ssrc: s.ssrc,
            receiver_ssrc: s.ssrc.wrapping_add(1),
            payload_type: s.rtp_payload_type.unwrap_or(96),
            codec,
            rtp_timebase: parse_timebase(s.time_base.as_deref(), codec)?,
            aes_key: parse_hex16(&s.aes_key)?,
            aes_iv_mask: parse_hex16(&s.aes_iv_mask)?,
        };
        let slot = if codec.is_video() {
            &mut video
        } else {
            &mut audio
        };
        // Offers list several codecs per medium and we take exactly one, so this is a
        // choice rather than a first match — see `Codec::preference`.
        if slot
            .as_ref()
            .is_none_or(|held| codec.preference() > held.codec.preference())
        {
            *slot = Some(cfg);
        }
    }

    let Some(video) = video else {
        return Err(CastError::Mirror("offer had no video stream we can decode"));
    };

    let mut send_indexes = Vec::new();
    let mut ssrcs = Vec::new();
    for cfg in [Some(&video), audio.as_ref()].into_iter().flatten() {
        send_indexes.push(cfg.index);
        ssrcs.push(cfg.receiver_ssrc);
    }

    let answer = serde_json::json!({
        "type": "ANSWER",
        "seqNum": env.seq_num,
        "result": "ok",
        "answer": {
            "udpPort": udp_port,
            "sendIndexes": send_indexes,
            "ssrcs": ssrcs,
            "receiverGetStatus": true,
        },
    })
    .to_string();

    Ok((
        answer,
        MirrorConfig {
            udp_port,
            video,
            audio,
        },
    ))
}

/// Compute the per-frame AES-CTR nonce from the stream's IV mask and a frame id.
///
/// Start from sixteen zero bytes, write the frame id's low 32 bits big-endian at
/// **offset 8**, then XOR the whole thing with the mask. Byte 8 and not byte 12: the
/// nonce's last four bytes are the CTR block counter, so putting the frame id there
/// would have made it march through the keystream as a frame was encrypted.
///
/// Derived from openscreen `cast/streaming/impl/frame_crypto.cc` (`FrameCrypto::Crypt`).
#[must_use]
pub fn frame_iv(iv_mask: &[u8; 16], frame_id: FrameId) -> [u8; 16] {
    let mut nonce = [0u8; 16];
    nonce[8..12].copy_from_slice(&frame_id.lower_32_bits().to_be_bytes());
    for (dst, src) in nonce.iter_mut().zip(iv_mask.iter()) {
        *dst ^= *src;
    }
    nonce
}

/// Decrypt (or, symmetrically, encrypt) one frame's payload in place-style, returning
/// the transformed bytes. AES-128-CTR is symmetric, so this both encrypts and decrypts.
///
/// # Errors
/// [`CastError::Mirror`] if the key/IV lengths are wrong (never for a [`StreamConfig`]).
pub fn crypt_frame(
    cfg: &StreamConfig,
    frame_id: FrameId,
    data: &[u8],
) -> Result<Vec<u8>, CastError> {
    let iv = frame_iv(&cfg.aes_iv_mask, frame_id);
    let mut cipher = Aes128Ctr::new_from_slices(&cfg.aes_key, &iv)
        .map_err(|_| CastError::Mirror("bad AES key/iv length"))?;
    let mut out = data.to_vec();
    cipher.apply_keystream(&mut out);
    Ok(out)
}

fn parse_hex16(s: &str) -> Result<[u8; 16], CastError> {
    let bytes = parse_hex(s)?;
    bytes
        .try_into()
        .map_err(|_| CastError::Mirror("aes key/iv not 16 bytes"))
}

fn parse_hex(s: &str) -> Result<Vec<u8>, CastError> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err(CastError::Mirror("odd-length hex"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| CastError::Mirror("bad hex")))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const OFFER: &str = r#"{
      "type":"OFFER","seqNum":42,
      "offer":{"castMode":"mirroring","supportedStreams":[
        {"index":0,"type":"video_source","codecName":"h264","rtpPayloadType":96,"ssrc":100,
         "aesKey":"000102030405060708090a0b0c0d0e0f","aesIvMask":"0f0e0d0c0b0a09080706050403020100"},
        {"index":1,"type":"audio_source","codecName":"opus","rtpPayloadType":97,"ssrc":200,
         "aesKey":"101112131415161718191a1b1c1d1e1f","aesIvMask":"1f1e1d1c1b1a19181716151413121110"}
      ]}}"#;

    #[test]
    fn negotiates_video_and_audio() {
        let (answer, cfg) = negotiate(OFFER, 51234).unwrap();
        assert!(answer.contains("\"type\":\"ANSWER\""));
        assert!(answer.contains("\"seqNum\":42"));
        assert!(answer.contains("\"udpPort\":51234"));
        assert!(answer.contains("\"sendIndexes\":[0,1]"));

        let v = cfg.video;
        assert_eq!(v.codec, Codec::H264);
        assert_eq!(v.sender_ssrc, 100);
        assert_eq!(v.receiver_ssrc, 101);
        assert_eq!(v.aes_key[0], 0x00);
        let a = cfg.audio.unwrap();
        assert_eq!(a.codec, Codec::Opus);
        assert_eq!(a.sender_ssrc, 200);
    }

    #[test]
    fn offer_without_known_codec_is_rejected() {
        let offer = r#"{"type":"OFFER","seqNum":1,"offer":{"supportedStreams":[
          {"index":0,"type":"video_source","codecName":"theora","ssrc":1,
           "aesKey":"00000000000000000000000000000000","aesIvMask":"00000000000000000000000000000000"}]}}"#;
        assert!(negotiate(offer, 5000).is_err());
    }

    #[test]
    fn frame_crypto_roundtrips() {
        let (_a, cfg) = negotiate(OFFER, 5000).unwrap();
        let v = cfg.video;
        let plaintext = b"an-encoded-h264-access-unit";
        let ciphertext = crypt_frame(&v, FrameId::new(7), plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);
        let back = crypt_frame(&v, FrameId::new(7), &ciphertext).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn different_frame_ids_give_different_ciphertext() {
        let (_a, cfg) = negotiate(OFFER, 5000).unwrap();
        let v = cfg.video;
        let c1 = crypt_frame(&v, FrameId::new(1), b"same").unwrap();
        let c2 = crypt_frame(&v, FrameId::new(2), b"same").unwrap();
        assert_ne!(c1, c2);
    }

    /// The nonce layout is the whole of Q13, and a round-trip test cannot catch a
    /// wrong offset — our encrypt and decrypt would agree with each other while
    /// disagreeing with every real sender. So assert the bytes.
    #[test]
    fn nonce_puts_the_frame_id_at_offset_8() {
        let mask = [0u8; 16];
        let iv = frame_iv(&mask, FrameId::new(0x0102_0304));
        let mut expected = [0u8; 16];
        expected[8..12].copy_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(iv, expected);

        // The last four bytes are the CTR block counter and must stay untouched.
        assert_eq!(&iv[12..16], &[0, 0, 0, 0]);

        // A non-zero mask XORs over the whole nonce, not just the frame-id window.
        let mask = [0xff; 16];
        let iv = frame_iv(&mask, FrameId::new(1));
        assert_eq!(&iv[0..8], &[0xff; 8]);
        assert_eq!(&iv[8..12], &[0xff, 0xff, 0xff, 0xfe]);
        assert_eq!(&iv[12..16], &[0xff; 4]);
    }

    /// Frame ids are 64-bit but only the low 32 reach the nonce, so ids a multiple of
    /// 2^32 apart share a keystream. Pinning this documents the wrap as intended
    /// rather than an oversight.
    #[test]
    fn only_the_low_32_bits_of_the_frame_id_reach_the_nonce() {
        let mask = [0u8; 16];
        assert_eq!(
            frame_iv(&mask, FrameId::new(5)),
            frame_iv(&mask, FrameId::new(5 + (1 << 32)))
        );
    }
}
