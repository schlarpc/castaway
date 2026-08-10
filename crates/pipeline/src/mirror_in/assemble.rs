//! RTP payloads back into whole encoded frames — the pure half of the mirroring
//! receiver (ground rule 3).
//!
//! What a `TrackRemote` hands over is one RTP packet at a time; what the decoder wants is
//! one access unit at a time, with a presentation time and a keyframe flag. In between
//! sit two jobs that are easy to get subtly wrong and are therefore kept here, away from
//! the socket, with fixtures: **reassembly** (a picture is spread over as many packets as
//! the MTU demands, and the last one carries the marker bit) and **timing** (the RTP
//! clock is a 32-bit counter that starts anywhere and wraps).
//!
//! Depacketization itself is the `rtc` crate's — the same crate whose DTLS-SRTP transport
//! delivered the packet, so H.264's STAP-A/FU-A rules and VP8's descriptor are read by the
//! implementation that is already parsing them for the interceptors.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use castaway_core::{AudioCodec, EncodedFrame, VideoCodec};
use rtc::rtp::codec::{h264::H264Packet, opus::OpusPacket, vp8::Vp8Packet};
use rtc::rtp::packetizer::Depacketizer;

/// A codec this receiver will answer an offer with.
///
/// Short by design: these are the three every WebRTC sender has (H.264 and VP8 are the
/// mandatory-to-implement video codecs, Opus the mandatory audio one), and a codec we do
/// not list here is one we never register in the media engine, so it is never negotiated
/// and never arrives. Adding one means adding it in both places, which the exhaustive
/// matches below enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorCodec {
    /// H.264, depacketized to Annex-B.
    H264,
    /// VP8.
    Vp8,
    /// Opus, one RTP packet per frame.
    Opus,
}

impl MirrorCodec {
    /// The codec an SDP mime type names, or `None` for one we never offered.
    #[must_use]
    pub fn from_mime(mime: &str) -> Option<Self> {
        // ASCII-caselessly: SDP writes `video/H264` and `video/VP8`, and senders are not
        // consistent about it.
        if mime.eq_ignore_ascii_case("video/H264") {
            Some(Self::H264)
        } else if mime.eq_ignore_ascii_case("video/VP8") {
            Some(Self::Vp8)
        } else if mime.eq_ignore_ascii_case("audio/opus") {
            Some(Self::Opus)
        } else {
            None
        }
    }

    /// The RTP clock this codec's timestamps are counted in.
    #[must_use]
    pub const fn clock_rate(self) -> u32 {
        match self {
            Self::H264 | Self::Vp8 => 90_000,
            Self::Opus => 48_000,
        }
    }

    /// Whether this is a video codec — which is also whether frames need reassembling
    /// across packets at all.
    #[must_use]
    pub const fn is_video(self) -> bool {
        match self {
            Self::H264 | Self::Vp8 => true,
            Self::Opus => false,
        }
    }
}

/// Reassembles one track's packets into [`EncodedFrame`]s.
pub struct TrackAssembler {
    codec: MirrorCodec,
    depacketizer: Box<dyn Depacketizer + Send>,
    /// The access unit being built, and the RTP timestamp it belongs to.
    pending: BytesMut,
    pending_ts: Option<u32>,
    /// The first timestamp seen, so presentation times start at zero.
    base_ts: Option<u32>,
    /// `pending_ts` unwrapped against `base_ts`, in RTP ticks. Kept as an `i64` because a
    /// 32-bit 90 kHz clock wraps every 13 hours and a panel is up for weeks: a session
    /// that ran past the wrap used to hand the decoder a presentation time that leapt
    /// backwards by 13 hours, which every pacing path reads as "hopelessly late".
    extended: i64,
    last_ts: Option<u32>,
}

impl std::fmt::Debug for TrackAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackAssembler")
            .field("codec", &self.codec)
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl TrackAssembler {
    /// An assembler for one negotiated codec.
    #[must_use]
    pub fn new(codec: MirrorCodec) -> Self {
        let depacketizer: Box<dyn Depacketizer + Send> = match codec {
            // `is_avc: false` — Annex-B start codes, which is what libavcodec's H.264
            // decoder is opened for on the mirror path (the Cast and AirPlay adapters
            // hand it the same shape).
            MirrorCodec::H264 => Box::new(H264Packet::default()),
            MirrorCodec::Vp8 => Box::new(Vp8Packet::default()),
            MirrorCodec::Opus => Box::new(OpusPacket),
        };
        Self {
            codec,
            depacketizer,
            pending: BytesMut::new(),
            pending_ts: None,
            base_ts: None,
            extended: 0,
            last_ts: None,
        }
    }

    /// Feed one RTP packet, returning a frame whenever one is complete.
    ///
    /// At most one frame per packet, which is the truth for both shapes here: an audio
    /// packet *is* a frame, and a video packet either continues the current access unit
    /// or (carrying the marker) finishes it. A packet that begins a new timestamp flushes
    /// the previous unit first — senders that lose the marker packet would otherwise weld
    /// two pictures into one.
    pub fn push(&mut self, timestamp: u32, marker: bool, payload: &Bytes) -> Option<EncodedFrame> {
        // Empty payloads are padding-only packets and RTX probes; there is nothing in one
        // to depacketize and the codecs' own parsers answer `ErrShortPacket` for them.
        if payload.is_empty() {
            return None;
        }
        let mut finished = None;
        if self.pending_ts.is_some_and(|pending| pending != timestamp) {
            finished = self.flush();
        }
        let Ok(chunk) = self.depacketizer.depacketize(payload) else {
            // One malformed payload is a lost packet, not a lost session: the next
            // keyframe repairs the picture, and a sender whose every packet is malformed
            // simply never produces a frame.
            return finished;
        };
        self.advance_clock(timestamp);
        self.pending_ts = Some(timestamp);
        self.pending.extend_from_slice(&chunk);

        // Audio has no marker discipline worth relying on and no fragmentation: one
        // packet, one frame.
        if marker || !self.codec.is_video() {
            // A packet that both finishes the previous unit and is a whole unit itself
            // cannot happen — the timestamp check above already flushed — so returning
            // this one over `finished` is not dropping a frame.
            return self.flush().or(finished);
        }
        finished
    }

    /// Emit whatever has accumulated, if anything.
    pub fn flush(&mut self) -> Option<EncodedFrame> {
        if self.pending.is_empty() {
            self.pending_ts = None;
            return None;
        }
        let data = std::mem::take(&mut self.pending).freeze();
        self.pending_ts = None;
        let (video_codec, audio_codec) = match self.codec {
            MirrorCodec::H264 => (Some(VideoCodec::H264), None),
            MirrorCodec::Vp8 => (Some(VideoCodec::Vp8), None),
            MirrorCodec::Opus => (None, Some(AudioCodec::Opus)),
        };
        Some(EncodedFrame {
            video_codec,
            audio_codec,
            pts: self.pts(),
            keyframe: is_keyframe(self.codec, &data),
            data,
        })
    }

    /// Extend the 32-bit RTP clock into a monotonic tick count.
    fn advance_clock(&mut self, timestamp: u32) {
        let base = *self.base_ts.get_or_insert(timestamp);
        match self.last_ts {
            None => self.extended = i64::from(timestamp.wrapping_sub(base).cast_signed()),
            Some(last) if last != timestamp => {
                // The wrapping difference read as *signed* is the step, which handles the
                // wrap and the occasional out-of-order packet with the same arithmetic.
                let step = i64::from(timestamp.wrapping_sub(last).cast_signed());
                self.extended += step;
            }
            Some(_) => {}
        }
        self.last_ts = Some(timestamp);
    }

    /// The pending unit's presentation time, from the stream's own clock.
    fn pts(&self) -> Duration {
        let ticks = self.extended.max(0);
        // u64 arithmetic: 13 hours of 90 kHz is 4.2e9 ticks, and times a nanosecond scale
        // that overflows a u64 — so scale down first. The remainder keeps the sub-tick
        // precision that matters at 48 kHz.
        let rate = u64::from(self.codec.clock_rate());
        #[allow(clippy::cast_sign_loss)]
        let ticks = ticks as u64;
        // The remainder is below `rate`, so scaling it by 1e9 and dividing back stays
        // under a nanosecond and cannot exceed `u32::MAX`. `Duration::new` would panic on
        // a nanosecond field of 1e9 or more, which is why this is arithmetic and not a
        // cast of the whole figure.
        let nanos = u32::try_from((ticks % rate) * 1_000_000_000 / rate).unwrap_or(0);
        Duration::new(ticks / rate, nanos)
    }
}

/// Whether an assembled access unit begins a decodable picture.
///
/// Load-bearing rather than cosmetic: the decoder drops until it has one, and a stream
/// whose keyframes are never flagged shows nothing at all while looking healthy.
fn is_keyframe(codec: MirrorCodec, data: &[u8]) -> bool {
    match codec {
        // An Annex-B access unit holding an IDR slice (NAL 5) is a keyframe; so is one
        // holding a parameter set (SPS 7 / PPS 8), which senders emit immediately before
        // the IDR and often in the same unit.
        MirrorCodec::H264 => annexb_nal_types(data).any(|nal| matches!(nal, 5 | 7 | 8)),
        // VP8's uncompressed data chunk starts the payload: bit 0 of byte 0 is the frame
        // type, and 0 is a key frame (RFC 6386 §9.1).
        MirrorCodec::Vp8 => data.first().is_some_and(|b| b & 0x01 == 0),
        // Every Opus packet stands alone.
        MirrorCodec::Opus => true,
    }
}

/// The NAL unit types in an Annex-B buffer, in order.
fn annexb_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        while at + 3 < data.len() {
            // Both start codes, because the depacketizer emits four-byte ones and a
            // sender's own in-band parameter sets may use either.
            let three = data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 1;
            let four = data[at] == 0 && data[at + 1] == 0 && data[at + 2] == 0 && data[at + 3] == 1;
            if three {
                at += 3;
            } else if four {
                at += 4;
            } else {
                at += 1;
                continue;
            }
            let nal = data.get(at).map(|b| b & 0x1f);
            at += 1;
            return nal;
        }
        None
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// One H.264 NAL, small enough to ride a single packet unfragmented.
    fn single_nal(nal_type: u8) -> Bytes {
        Bytes::from(vec![nal_type & 0x1f, 0xaa, 0xbb, 0xcc])
    }

    #[test]
    fn the_mime_types_a_sender_writes_map_to_codecs() {
        assert_eq!(
            MirrorCodec::from_mime("video/H264"),
            Some(MirrorCodec::H264)
        );
        assert_eq!(
            MirrorCodec::from_mime("video/h264"),
            Some(MirrorCodec::H264)
        );
        assert_eq!(MirrorCodec::from_mime("video/VP8"), Some(MirrorCodec::Vp8));
        assert_eq!(
            MirrorCodec::from_mime("audio/opus"),
            Some(MirrorCodec::Opus)
        );
        // Never registered, so never negotiated, so never seen.
        assert_eq!(MirrorCodec::from_mime("video/AV1"), None);
    }

    /// A picture spread over three packets is one frame, emitted on the marker.
    #[test]
    fn a_fragmented_picture_becomes_one_frame() {
        let mut a = TrackAssembler::new(MirrorCodec::H264);
        // FU-A: indicator (type 28), then a header with S/E bits and the real type.
        let fu = |start: bool, end: bool, byte: u8| {
            let mut header = 0x01u8; // the fragmented NAL's type (non-IDR slice)
            if start {
                header |= 0x80;
            }
            if end {
                header |= 0x40;
            }
            Bytes::from(vec![28, header, byte])
        };
        assert!(a.push(1000, false, &fu(true, false, 0x11)).is_none());
        assert!(a.push(1000, false, &fu(false, false, 0x22)).is_none());
        let frame = a
            .push(1000, true, &fu(false, true, 0x33))
            .expect("the marker completes the access unit");
        assert_eq!(frame.video_codec, Some(VideoCodec::H264));
        assert_eq!(
            frame.pts,
            Duration::ZERO,
            "the first frame anchors the clock"
        );
        // Start code, the reassembled NAL header, then the three payload bytes.
        assert_eq!(&frame.data[..], &[0, 0, 0, 1, 0x01, 0x11, 0x22, 0x33]);
    }

    /// A sender whose marker packet is lost must not weld two pictures together: the
    /// timestamp changing is itself the boundary.
    #[test]
    fn a_new_timestamp_closes_the_previous_picture() {
        let mut a = TrackAssembler::new(MirrorCodec::H264);
        assert!(a.push(9000, false, &single_nal(1)).is_none());
        let frame = a
            .push(18_000, false, &single_nal(1))
            .expect("the first picture is closed by the second's timestamp");
        assert_eq!(frame.pts, Duration::ZERO);
        // …and the second picture is still open, carrying the later time.
        let second = a.push(18_000, true, &single_nal(1)).expect("the second");
        assert_eq!(
            second.pts,
            Duration::from_millis(100),
            "9000 ticks of a 90 kHz clock"
        );
    }

    /// The presentation clock starts at the sender's first timestamp, whatever it is —
    /// RTP timestamps start at a random offset by the RFC.
    #[test]
    fn presentation_time_is_relative_to_the_first_packet() {
        let mut a = TrackAssembler::new(MirrorCodec::H264);
        let first = a.push(3_000_000_000, true, &single_nal(1)).unwrap();
        assert_eq!(first.pts, Duration::ZERO);
        let later = a.push(3_000_090_000, true, &single_nal(1)).unwrap();
        assert_eq!(later.pts, Duration::from_secs(1));
    }

    /// The 32-bit clock wraps every 13 hours at 90 kHz, and a panel is up for weeks. A
    /// wrap must read as one more second, not as a 13-hour leap backwards — which every
    /// pacing path in the pipeline reads as "hopelessly late" and drops.
    #[test]
    fn the_rtp_clock_wrapping_is_not_a_leap_backwards() {
        let mut a = TrackAssembler::new(MirrorCodec::H264);
        let base = u32::MAX - 45_000; // half a second short of the wrap
        assert_eq!(
            a.push(base, true, &single_nal(1)).unwrap().pts,
            Duration::ZERO
        );
        // …and 90 000 ticks later, which is 44 999 *after* wrapping through zero.
        let after = a
            .push(base.wrapping_add(90_000), true, &single_nal(1))
            .unwrap();
        assert_eq!(after.pts, Duration::from_secs(1));
    }

    /// The flag the decoder drops frames until it sees.
    #[test]
    fn keyframes_are_flagged_and_ordinary_slices_are_not() {
        let mut a = TrackAssembler::new(MirrorCodec::H264);
        assert!(
            !a.push(0, true, &single_nal(1)).unwrap().keyframe,
            "a P slice"
        );
        assert!(a.push(90, true, &single_nal(5)).unwrap().keyframe, "an IDR");
        assert!(
            a.push(180, true, &single_nal(7)).unwrap().keyframe,
            "an SPS, which senders emit immediately before the IDR"
        );

        // VP8 says so in bit 0 of the first byte of the frame tag, inverted.
        let mut vp8 = TrackAssembler::new(MirrorCodec::Vp8);
        // Payload descriptor (one byte, no extensions), then the frame tag.
        let key = Bytes::from(vec![0x10, 0x00, 0x00, 0x00]);
        let inter = Bytes::from(vec![0x10, 0x01, 0x00, 0x00]);
        assert!(vp8.push(0, true, &key).unwrap().keyframe);
        assert!(!vp8.push(90, true, &inter).unwrap().keyframe);
    }

    /// Audio is one packet, one frame, on its own 48 kHz clock — no marker discipline
    /// and nothing to reassemble.
    #[test]
    fn every_opus_packet_is_a_frame_on_the_48_khz_clock() {
        let mut a = TrackAssembler::new(MirrorCodec::Opus);
        let one = Bytes::from_static(&[0xfc, 0x01, 0x02]);
        let first = a.push(0, false, &one).expect("no marker needed");
        assert_eq!(first.audio_codec, Some(AudioCodec::Opus));
        assert_eq!(first.video_codec, None);
        assert!(first.keyframe, "every Opus packet stands alone");
        let next = a.push(960, false, &one).unwrap();
        assert_eq!(next.pts, Duration::from_millis(20), "960 ticks at 48 kHz");
    }

    /// A padding-only or malformed packet is a lost packet, not a lost session.
    #[test]
    fn junk_packets_are_stepped_over() {
        let mut a = TrackAssembler::new(MirrorCodec::H264);
        assert!(a.push(0, false, &Bytes::new()).is_none());
        assert!(a.push(0, false, &Bytes::from_static(&[0x00])).is_none());
        // …and the track still works afterwards.
        assert!(a.push(0, true, &single_nal(5)).is_some());
    }
}
