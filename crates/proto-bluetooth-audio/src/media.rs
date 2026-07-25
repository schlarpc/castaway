//! Media-packet depacketization, which is where the five codecs stop agreeing.
//!
//! The trap: **A2DP's media framing is not uniform across codecs.** Four of the five ride
//! an RTP header, and classic aptX does not — its media packets are a raw codec byte
//! stream with no header at all. Stripping twelve bytes off an aptX packet removes real
//! audio and produces a stream that decodes to noise rather than failing, so the
//! depacketizer is constructed *from the negotiated configuration* and cannot be used
//! without knowing which codec it is looking at.

use std::time::Duration;

use bytes::Bytes;
use castaway_core::{AudioCodec, EncodedFrame};
use substrate_rtp::RtpPacket;

use crate::error::AudioError;

/// How a codec's media packets are framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// RTP header, then the codec payload directly.
    Rtp,
    /// RTP header, then a one-byte codec header, then the payload.
    RtpWithHeader,
    /// No header at all: the packet *is* the codec stream.
    Raw,
}

impl Framing {
    const fn for_codec(codec: AudioCodec) -> Self {
        match codec {
            // SBC's one-byte header carries fragmentation flags and a frame count; AAC
            // rides LATM inside plain RTP; LDAC prefixes a frame-count byte.
            AudioCodec::Sbc | AudioCodec::Ldac => Self::RtpWithHeader,
            AudioCodec::Aac | AudioCodec::AptXHd => Self::Rtp,
            // Classic aptX alone has no RTP header. aptX *HD* does — the two differ
            // here despite the shared name, which is exactly how this gets got wrong.
            AudioCodec::AptX => Self::Raw,
            // Anything else reaching an A2DP media channel is a bug upstream, but
            // assuming RTP is the safe reading: it is what the majority use.
            _ => Self::Rtp,
        }
    }
}

/// Turns A2DP media packets into [`EncodedFrame`]s for the pipeline.
///
/// Built from the negotiated codec so the framing question is answered once, at
/// configuration time, rather than guessed per packet.
#[derive(Debug)]
pub struct Depacketizer {
    codec: AudioCodec,
    framing: Framing,
    sample_rate: u32,
    /// RTP timestamp of the first packet, so presentation times start at zero.
    base_timestamp: Option<u32>,
    /// Monotonic fallback for the raw framing, which carries no timestamps.
    frames_seen: u64,
}

impl Depacketizer {
    /// Build a depacketizer for a negotiated configuration.
    #[must_use]
    pub fn new(codec: AudioCodec, sample_rate: u32) -> Self {
        Self {
            codec,
            framing: Framing::for_codec(codec),
            sample_rate: sample_rate.max(1),
            base_timestamp: None,
            frames_seen: 0,
        }
    }

    /// Which codec this depacketizer was configured for.
    #[must_use]
    pub const fn codec(&self) -> AudioCodec {
        self.codec
    }

    /// Whether packets for this codec carry an RTP header at all.
    #[must_use]
    pub const fn expects_rtp(&self) -> bool {
        !matches!(self.framing, Framing::Raw)
    }

    /// Depacketize one media packet.
    ///
    /// # Errors
    /// [`AudioError::BadMediaPacket`] if the packet is too short for its framing or the
    /// RTP header is malformed.
    pub fn push(&mut self, packet: Bytes) -> Result<EncodedFrame, AudioError> {
        let (payload, pts) = match self.framing {
            Framing::Raw => {
                if packet.is_empty() {
                    return Err(AudioError::BadMediaPacket("empty aptX packet"));
                }
                // No timestamps on the wire, so derive presentation time from how much
                // audio has gone past. aptX is a fixed 4:1 stream: four PCM samples per
                // four output bytes, i.e. one sample per byte per channel pair.
                let pts = self.elapsed();
                self.frames_seen += packet.len() as u64;
                (packet, pts)
            }
            Framing::Rtp | Framing::RtpWithHeader => {
                let rtp = RtpPacket::parse(packet)
                    .map_err(|_| AudioError::BadMediaPacket("malformed RTP header"))?;
                let pts = self.pts_from(rtp.header.timestamp);
                let mut payload = rtp.payload;
                if self.framing == Framing::RtpWithHeader {
                    if payload.is_empty() {
                        return Err(AudioError::BadMediaPacket(
                            "media packet has no codec header",
                        ));
                    }
                    // The byte we drop is a frame count (SBC also packs fragmentation
                    // flags into it). Neither decoder wants it, but leaving it in place
                    // shifts every frame by one byte and decodes to noise.
                    payload = payload.slice(1..);
                }
                if payload.is_empty() {
                    return Err(AudioError::BadMediaPacket("media packet has no payload"));
                }
                (payload, pts)
            }
        };

        Ok(EncodedFrame {
            video_codec: None,
            audio_codec: Some(self.codec),
            pts,
            // Meaningless for audio, but the field is shared with the video path; a
            // decoder that waits for a keyframe would stall forever on `false`.
            keyframe: true,
            data: payload,
        })
    }

    /// Presentation time from an RTP timestamp, rebased so the stream starts at zero.
    fn pts_from(&mut self, timestamp: u32) -> Duration {
        let base = *self.base_timestamp.get_or_insert(timestamp);
        // Wrapping subtraction: RTP timestamps are 32-bit and a long session will roll
        // over. Plain subtraction would produce an enormous jump at the wrap and throw
        // the whole presentation clock away.
        let elapsed = timestamp.wrapping_sub(base);
        Duration::from_nanos(
            u64::from(elapsed)
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(self.sample_rate))
                .unwrap_or(0),
        )
    }

    /// Presentation time derived from bytes seen, for framing with no timestamps.
    fn elapsed(&self) -> Duration {
        // aptX compresses 4:1 on 16-bit stereo, so one output byte is one PCM sample
        // frame. Approximate but monotonic, which is all the renderer needs.
        Duration::from_nanos(
            self.frames_seen
                .saturating_mul(1_000_000_000)
                .checked_div(u64::from(self.sample_rate))
                .unwrap_or(0),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use bytes::{BufMut, BytesMut};

    use super::*;

    /// Build an RTP packet with `payload` at `timestamp`.
    fn rtp(timestamp: u32, payload: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(12 + payload.len());
        buf.put_u8(0x80); // version 2, no padding/extension/csrc
        buf.put_u8(96); // dynamic payload type
        buf.put_u16(1); // sequence
        buf.put_u32(timestamp);
        buf.put_u32(0xDEAD_BEEF); // ssrc
        buf.extend_from_slice(payload);
        buf.freeze()
    }

    #[test]
    fn classic_aptx_has_no_rtp_header_but_aptx_hd_does() {
        // The single most consequential difference in this file. Treating aptX as RTP
        // eats 12 bytes of real audio and decodes to noise — no error anywhere.
        assert!(!Depacketizer::new(AudioCodec::AptX, 44_100).expects_rtp());
        assert!(Depacketizer::new(AudioCodec::AptXHd, 44_100).expects_rtp());

        let raw = Bytes::from_static(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        let mut d = Depacketizer::new(AudioCodec::AptX, 44_100);
        let frame = d.push(raw.clone()).unwrap();
        assert_eq!(frame.data, raw, "every byte is audio");
        assert_eq!(frame.audio_codec, Some(AudioCodec::AptX));
    }

    #[test]
    fn sbc_and_ldac_drop_their_one_byte_codec_header() {
        // Leaving the frame-count byte in shifts every frame by one and decodes to
        // noise, in the same silent way as the aptX case.
        for codec in [AudioCodec::Sbc, AudioCodec::Ldac] {
            let mut d = Depacketizer::new(codec, 44_100);
            let frame = d.push(rtp(0, &[0x05, 0xAA, 0xBB, 0xCC])).unwrap();
            assert_eq!(
                &frame.data[..],
                &[0xAA, 0xBB, 0xCC],
                "{codec:?} must drop its codec header"
            );
        }
    }

    #[test]
    fn aac_and_aptx_hd_keep_their_whole_rtp_payload() {
        for codec in [AudioCodec::Aac, AudioCodec::AptXHd] {
            let mut d = Depacketizer::new(codec, 44_100);
            let frame = d.push(rtp(0, &[0xAA, 0xBB, 0xCC])).unwrap();
            assert_eq!(&frame.data[..], &[0xAA, 0xBB, 0xCC], "{codec:?}");
        }
    }

    #[test]
    fn presentation_time_is_rebased_so_a_stream_starts_at_zero() {
        // Senders start their RTP clock wherever they like. Passing the raw timestamp
        // through would place the first frame hours into the presentation timeline.
        let mut d = Depacketizer::new(AudioCodec::Aac, 44_100);
        let first = d.push(rtp(1_000_000, &[1, 2, 3])).unwrap();
        assert_eq!(first.pts, Duration::ZERO);

        let later = d.push(rtp(1_000_000 + 44_100, &[4, 5, 6])).unwrap();
        assert_eq!(later.pts, Duration::from_secs(1));
    }

    #[test]
    fn a_timestamp_wrap_does_not_throw_away_the_clock() {
        // 32-bit RTP timestamps roll over in about a day at 48 kHz — well within a
        // hackerspace's uptime. Plain subtraction would jump the clock by ~27 hours.
        let mut d = Depacketizer::new(AudioCodec::Aac, 48_000);
        d.push(rtp(u32::MAX - 47_999, &[1])).unwrap();
        let after_wrap = d.push(rtp(0, &[2])).unwrap();
        assert_eq!(after_wrap.pts, Duration::from_secs(1));
    }

    #[test]
    fn raw_framing_advances_time_monotonically_without_timestamps() {
        let mut d = Depacketizer::new(AudioCodec::AptX, 44_100);
        let a = d.push(Bytes::from_static(&[0; 441])).unwrap();
        let b = d.push(Bytes::from_static(&[0; 441])).unwrap();
        assert_eq!(a.pts, Duration::ZERO);
        assert!(b.pts > a.pts, "time must advance");
    }

    #[test]
    fn audio_frames_are_marked_as_keyframes() {
        // Shared with the video path, where a decoder drops frames until it sees one.
        // Audio has no such concept, and `false` would stall the decoder forever.
        let mut d = Depacketizer::new(AudioCodec::Sbc, 44_100);
        assert!(d.push(rtp(0, &[1, 2, 3])).unwrap().keyframe);
    }

    #[test]
    fn empty_and_malformed_packets_are_refused() {
        let mut d = Depacketizer::new(AudioCodec::AptX, 44_100);
        assert!(d.push(Bytes::new()).is_err());

        let mut d = Depacketizer::new(AudioCodec::Sbc, 44_100);
        assert!(d.push(Bytes::from_static(&[0, 1])).is_err(), "short RTP");
        assert!(d.push(rtp(0, &[])).is_err(), "no codec header");
        assert!(d.push(rtp(0, &[0x05])).is_err(), "header but no payload");
    }
}
