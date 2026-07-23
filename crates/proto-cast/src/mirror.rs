//! Cast mirroring (a.k.a. Cast Streaming) negotiation — the control plane for
//! "Cast desktop/tab" from Chrome/Edge. The sender sends an `OFFER` on the
//! `com.google.cast.webrtc` namespace listing encrypted RTP streams; we reply with an
//! `ANSWER` selecting streams + a UDP port. The AES key/IV live *in the offer* (the
//! sender picks them), so the receiver learns how to decrypt from the negotiation.
//!
//! This module is the pure negotiator + the per-frame AES-128-CTR crypto. The UDP RTP
//! receive/reassembly loop is the actor's job and is deferred (OPEN-QUESTIONS): the
//! negotiator hands the actor a [`MirrorConfig`] to drive it.

use aes::cipher::{KeyIvInit, StreamCipher};
use serde::Deserialize;

use crate::error::CastError;

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
}

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
    /// AES-128 key for this stream (from the offer).
    pub aes_key: [u8; 16],
    /// AES IV mask for this stream (from the offer); per-frame IVs derive from it.
    pub aes_iv_mask: [u8; 16],
}

/// The full negotiated mirroring session.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// UDP port we will receive RTP on.
    pub udp_port: u16,
    /// The selected video stream, if any.
    pub video: Option<StreamConfig>,
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
    #[serde(rename = "aesKey")]
    aes_key: String,
    #[serde(rename = "aesIvMask")]
    aes_iv_mask: String,
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
            aes_key: parse_hex16(&s.aes_key)?,
            aes_iv_mask: parse_hex16(&s.aes_iv_mask)?,
        };
        if codec.is_video() && video.is_none() {
            video = Some(cfg);
        } else if !codec.is_video() && audio.is_none() {
            audio = Some(cfg);
        }
    }

    if video.is_none() && audio.is_none() {
        return Err(CastError::Mirror("offer had no usable streams"));
    }

    let mut send_indexes = Vec::new();
    let mut ssrcs = Vec::new();
    for cfg in [video.as_ref(), audio.as_ref()].into_iter().flatten() {
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

/// Compute the per-frame CTR IV from the stream's IV mask and a frame id.
///
/// NOTE: the exact mixing of `frame_id` into the mask needs validation against a real
/// capture (OPEN-QUESTIONS). This is self-consistent (encrypt/decrypt round-trip) and
/// follows the documented "XOR the frame counter into the mask" shape.
#[must_use]
pub fn frame_iv(iv_mask: &[u8; 16], frame_id: u32) -> [u8; 16] {
    let mut iv = *iv_mask;
    let fid = frame_id.to_be_bytes();
    for (dst, src) in iv[12..16].iter_mut().zip(fid.iter()) {
        *dst ^= *src;
    }
    iv
}

/// Decrypt (or, symmetrically, encrypt) one frame's payload in place-style, returning
/// the transformed bytes. AES-128-CTR is symmetric, so this both encrypts and decrypts.
///
/// # Errors
/// [`CastError::Mirror`] if the key/IV lengths are wrong (never for a [`StreamConfig`]).
pub fn crypt_frame(cfg: &StreamConfig, frame_id: u32, data: &[u8]) -> Result<Vec<u8>, CastError> {
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

        let v = cfg.video.unwrap();
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
        let v = cfg.video.unwrap();
        let plaintext = b"an-encoded-h264-access-unit";
        let ciphertext = crypt_frame(&v, 7, plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);
        let back = crypt_frame(&v, 7, &ciphertext).unwrap();
        assert_eq!(back, plaintext);
    }

    #[test]
    fn different_frame_ids_give_different_ciphertext() {
        let (_a, cfg) = negotiate(OFFER, 5000).unwrap();
        let v = cfg.video.unwrap();
        let c1 = crypt_frame(&v, 1, b"same").unwrap();
        let c2 = crypt_frame(&v, 2, b"same").unwrap();
        assert_ne!(c1, c2);
    }
}
