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

use crate::latm::LatmParser;
use crate::ldac;

/// Every SBC frame starts with this.
const SBC_SYNCWORD: u8 = 0x9C;

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
            // rides LATM inside plain RTP (unwrapped separately, see `LatmParser`); LDAC
            // prefixes a frame-count byte.
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
    /// The bitpool of the most recent SBC frame, which is not the negotiated ceiling:
    /// every SBC frame header states its own, and a congested sender lowers it without
    /// renegotiating anything (A2DP has no feedback path to renegotiate *with*).
    sbc_bitpool: Option<u8>,
    /// AAC arrives wrapped in a LATM multiplex that has to come off before a decoder can
    /// read it; the parser is stateful because a sender may send the configuration once
    /// and reuse it (RFC 3016 §4.1).
    latm: Option<LatmParser>,
    /// The RTP sequence number of the previous packet, for loss detection.
    ///
    /// A2DP numbers its media packets and we were throwing that away, which left the one
    /// question that matters when audio breaks up unanswerable: were the packets lost
    /// before they reached us, or did we mishandle packets we did receive? A decoder
    /// complaining is a *symptom* of either, so it cannot tell them apart — but a
    /// sequence gap is proof of the first, and its absence is proof of the second.
    last_sequence: Option<u16>,
    /// How many packets the sequence numbers say went missing.
    lost_packets: u64,
    /// How many discontinuities were seen, which is not the same number — one gap may
    /// swallow many packets, and a burst of small gaps means something different from a
    /// single large one.
    sequence_gaps: u64,

    /// What the most recent LDAC payload said it was, read from its first frame header.
    ///
    /// The counterpart of `sbc_bitpool`, and for the same reason: it is the number the
    /// stream states about itself, which is not necessarily the number the negotiation
    /// settled on. LDAC is the only A2DP codec that carries its sample rate in-band, so it
    /// is the only one where that comparison is possible at all — and the decoder follows
    /// the stream, so without this nothing would ever say the two had diverged.
    ldac_stream: Option<ldac::StreamConfig>,
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
            last_sequence: None,
            lost_packets: 0,
            sequence_gaps: 0,
            sbc_bitpool: None,
            latm: (codec == AudioCodec::Aac).then(LatmParser::new),
            ldac_stream: None,
        }
    }

    /// Which codec this depacketizer was configured for.
    #[must_use]
    pub const fn codec(&self) -> AudioCodec {
        self.codec
    }

    /// Record an RTP sequence number and count anything missing between it and the last.
    ///
    /// Wrapping is normal — the field is 16 bits and a 44.1 kHz stream exhausts it every
    /// few minutes — so the delta is computed with `wrapping_sub`, which makes 0xFFFF →
    /// 0x0000 a delta of one rather than a 65535-packet catastrophe.
    ///
    /// A delta of zero (a duplicate) or a large delta (reordering, or a stream that
    /// restarted) is deliberately *not* counted as loss: neither is a missing packet, and
    /// counting them would inflate exactly the number we want to trust.
    fn note_sequence(&mut self, sequence: u16) {
        let Some(previous) = self.last_sequence.replace(sequence) else {
            return;
        };
        let delta = sequence.wrapping_sub(previous);
        // 1 is the healthy case. 0 is a duplicate. Anything past half the sequence space
        // is far likelier to be a restart or reordering than a real run of losses.
        if delta > 1 && delta < u16::MAX / 2 {
            self.sequence_gaps += 1;
            self.lost_packets += u64::from(delta - 1);
        }
    }

    /// How many media packets the sequence numbers say never arrived.
    #[must_use]
    pub const fn lost_packets(&self) -> u64 {
        self.lost_packets
    }

    /// How many separate discontinuities produced [`Self::lost_packets`].
    #[must_use]
    pub const fn sequence_gaps(&self) -> u64 {
        self.sequence_gaps
    }

    /// The bitpool the most recent SBC frame was coded at, if this is an SBC stream.
    ///
    /// Read from the frame header rather than from the negotiation, because they are
    /// different numbers: the capability exchange settles a min/max *range* and the
    /// encoder moves within it per frame. A sender whose transmit queue is backing up
    /// drops this and says nothing — watching it is the only way to see that happen.
    #[must_use]
    pub const fn bitpool(&self) -> Option<u8> {
        self.sbc_bitpool
    }

    /// What the most recent LDAC payload declared about itself, if this is an LDAC stream.
    ///
    /// Read from the frame headers rather than from the negotiation, because LDAC is the
    /// one A2DP codec where they can differ without anything failing: Sony's decoder
    /// reconfigures itself from these bytes and keeps playing. Comparing this against the
    /// negotiated [`castaway_core::AudioFormat`] is the only way that divergence becomes
    /// visible — the audio stays correct, but the endpoint we advertised does not describe
    /// what is arriving.
    #[must_use]
    pub const fn ldac_stream(&self) -> Option<ldac::StreamConfig> {
        self.ldac_stream
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
                self.note_sequence(rtp.header.sequence);
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
                    if self.codec == AudioCodec::Ldac {
                        // The check that makes the byte above provably the right thing to
                        // drop. An LDAC transport frame begins with the syncword 0xAA, so a
                        // payload that does not is one we have mis-framed — and handing
                        // that to a decoder is noise, not an error. It also gives us the
                        // stream's own rate and channel configuration for free, which is
                        // the only in-band answer any of these five codecs offers.
                        self.ldac_stream = Some(ldac::StreamConfig::parse(&payload)?);
                    }
                    if self.codec == AudioCodec::Sbc {
                        // Syncword 0x9C, then one byte of stream parameters, then the
                        // bitpool this frame was coded at.
                        if let Some(&bitpool) = payload
                            .first()
                            .filter(|b| **b == SBC_SYNCWORD)
                            .and(payload.get(2))
                        {
                            self.sbc_bitpool = Some(bitpool);
                        }
                    }
                }
                if payload.is_empty() {
                    return Err(AudioError::BadMediaPacket("media packet has no payload"));
                }
                if let Some(latm) = self.latm.as_mut() {
                    // A2DP defers its AAC payload format to RFC 3016, which carries
                    // ISO/IEC 14496-3 AudioMuxElements. Handing one straight to a decoder
                    // is "invalid data" on every packet — the access unit starts after a
                    // header whose length depends on the negotiated configuration.
                    payload = latm.access_unit(&payload)?;
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

    /// The same, at an explicit sequence number.
    fn rtp_seq(sequence: u16, payload: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(12 + payload.len());
        buf.put_u8(0x80);
        buf.put_u8(96);
        buf.put_u16(sequence);
        buf.put_u32(0);
        buf.put_u32(0xDEAD_BEEF);
        buf.extend_from_slice(payload);
        buf.freeze()
    }

    #[test]
    fn a_clean_stream_reports_no_loss() {
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        for seq in 100..200 {
            d.push(rtp_seq(seq, &[0xAA; 6])).unwrap();
        }
        assert_eq!(d.lost_packets(), 0);
        assert_eq!(d.sequence_gaps(), 0);
    }

    #[test]
    fn a_gap_in_the_sequence_counts_the_packets_that_never_arrived() {
        // The measurement that decides whether a break-up is the radio's fault or ours.
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        d.push(rtp_seq(10, &[0xAA; 6])).unwrap();
        d.push(rtp_seq(11, &[0xAA; 6])).unwrap();
        // 12, 13 and 14 never arrive.
        d.push(rtp_seq(15, &[0xAA; 6])).unwrap();
        d.push(rtp_seq(16, &[0xAA; 6])).unwrap();
        assert_eq!(d.sequence_gaps(), 1, "one discontinuity");
        assert_eq!(d.lost_packets(), 3, "three packets inside it");
    }

    #[test]
    fn the_sequence_number_wrapping_is_not_sixty_five_thousand_losses() {
        // The field is 16 bits and a 44.1 kHz stream exhausts it every few minutes, so
        // this happens in every real session. Subtracting the wrong way round turns a
        // healthy stream into a catastrophic-looking one, on a timer.
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        d.push(rtp_seq(0xFFFE, &[0xAA; 6])).unwrap();
        d.push(rtp_seq(0xFFFF, &[0xAA; 6])).unwrap();
        d.push(rtp_seq(0x0000, &[0xAA; 6])).unwrap();
        d.push(rtp_seq(0x0001, &[0xAA; 6])).unwrap();
        assert_eq!(d.lost_packets(), 0);
        assert_eq!(d.sequence_gaps(), 0);
    }

    #[test]
    fn duplicates_and_reordering_are_not_counted_as_loss() {
        // Neither is a missing packet, and counting them would inflate the one number
        // this exists to make trustworthy.
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        d.push(rtp_seq(50, &[0xAA; 6])).unwrap();
        d.push(rtp_seq(50, &[0xAA; 6])).unwrap(); // duplicate
        assert_eq!(d.lost_packets(), 0, "a duplicate is not a loss");
        // A jump backwards is reordering or a restarted stream, not 65k losses.
        d.push(rtp_seq(20, &[0xAA; 6])).unwrap();
        assert_eq!(d.lost_packets(), 0, "a backwards jump is not a loss");
    }

    #[test]
    fn loss_is_counted_for_every_rtp_codec_not_just_one() {
        // AAC and SBC ride the same RTP framing, so the diagnostic has to work there
        // too — SBC is the codec every sender falls back to when the link is bad, which
        // is exactly when this number matters most.
        let mut sbc = Depacketizer::new(AudioCodec::Sbc, 44_100);
        let payload = [&[0x05][..], &sbc_frame(35)].concat();
        sbc.push(rtp_seq(1, &payload)).unwrap();
        sbc.push(rtp_seq(4, &payload)).unwrap();
        assert_eq!(sbc.lost_packets(), 2);
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
        //
        // The two codecs need different payloads because only one of them is checked: an
        // SBC frame is taken on trust, while an LDAC payload has to start with its
        // syncword or it is refused (see the test below).
        let mut sbc = Depacketizer::new(AudioCodec::Sbc, 44_100);
        let frame = sbc.push(rtp(0, &[0x05, 0xAA, 0xBB, 0xCC])).unwrap();
        assert_eq!(&frame.data[..], &[0xAA, 0xBB, 0xCC]);

        let mut ldac = Depacketizer::new(AudioCodec::Ldac, 44_100);
        // One LDAC transport frame: syncword, 44.1 kHz stereo, three payload bytes.
        let ldac_frame = [0xAAu8, 0b0001_0000, 0b0000_1000, 0x11, 0x22, 0x33];
        let mut payload = vec![0x01];
        payload.extend_from_slice(&ldac_frame);
        let frame = ldac.push(rtp(0, &payload)).unwrap();
        assert_eq!(
            &frame.data[..],
            &ldac_frame,
            "the frame count must come off"
        );
    }

    #[test]
    fn an_ldac_payload_must_start_with_a_frame_syncword() {
        // The guard that makes dropping that byte provable rather than assumed. If the
        // one-byte payload header stopped coming off, every frame would shift by a byte and
        // Sony's decoder would be handed a frame count where it expects 0xAA — which is
        // noise on the speakers, not an error in a log.
        let mut d = Depacketizer::new(AudioCodec::Ldac, 44_100);
        assert!(
            d.push(rtp(0, &[0x05, 0x9C, 0x31, 0x35, 0x00])).is_err(),
            "an SBC frame is not an LDAC frame"
        );
        assert_eq!(d.ldac_stream(), None, "and nothing was recorded from it");
    }

    /// One SBC frame header: syncword, stream parameters, bitpool, CRC.
    fn sbc_frame(bitpool: u8) -> Vec<u8> {
        vec![SBC_SYNCWORD, 0x31, bitpool, 0x00, 0xAA, 0xBB]
    }

    #[test]
    fn sbc_bitpool_is_read_from_the_frame_not_the_negotiation() {
        // The negotiated range is a ceiling; each frame states what it actually used, and
        // a congested sender lowers it without renegotiating. Reading the wrong one means
        // a stream can degrade with nothing anywhere saying so.
        let mut d = Depacketizer::new(AudioCodec::Sbc, 44_100);
        assert_eq!(d.bitpool(), None, "nothing seen yet");

        let mut payload = vec![0x01]; // SBC's one-byte frame-count header
        payload.extend_from_slice(&sbc_frame(53));
        d.push(rtp(0, &payload)).unwrap();
        assert_eq!(d.bitpool(), Some(53));

        let mut degraded = vec![0x01];
        degraded.extend_from_slice(&sbc_frame(29));
        d.push(rtp(1000, &degraded)).unwrap();
        assert_eq!(d.bitpool(), Some(29), "a drop must be visible");
    }

    #[test]
    fn a_non_sbc_stream_reports_no_bitpool() {
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        d.push(rtp(0, &[0xAA, 0xBB, 0xCC])).unwrap();
        assert_eq!(d.bitpool(), None, "bitpool is an SBC concept");
    }

    #[test]
    fn aptx_hd_keeps_its_whole_rtp_payload() {
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        let frame = d.push(rtp(0, &[0xAA, 0xBB, 0xCC])).unwrap();
        assert_eq!(&frame.data[..], &[0xAA, 0xBB, 0xCC]);
    }

    /// One real A2DP AAC payload from an iPhone, straight out of the capture.
    fn captured_aac() -> Bytes {
        let data = include_bytes!("../tests/fixtures/a2dp-aac-iphone.bin");
        let len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
        Bytes::copy_from_slice(&data[4..4 + len])
    }

    #[test]
    fn aac_has_its_latm_multiplex_taken_off() {
        // Unlike aptX HD, an AAC payload is not the codec stream: A2DP §4.5.4 defers to
        // RFC 3016, so the RTP payload is an AudioMuxElement and the access unit starts
        // after a header whose length depends on the negotiated configuration. Passing
        // the whole thing through is what made every packet "invalid data".
        let multiplex = captured_aac();
        let mut d = Depacketizer::new(AudioCodec::Aac, 44_100);
        let frame = d.push(rtp(0, &multiplex)).unwrap();
        assert!(
            frame.data.len() < multiplex.len(),
            "the multiplex header must be gone"
        );
        // This capture's header is nine bytes of StreamMuxConfig plus a one-byte length.
        assert_eq!(frame.data.len(), multiplex.len() - 10);
        assert_eq!(&frame.data[..], &multiplex[10..]);
    }

    /// Length-prefixed records: a little-endian `u32` length, then that many bytes.
    ///
    /// The framing `CASTAWAY_DUMP_AUDIO` writes, so a capture from a real phone can be
    /// dropped in beside a generated fixture and replayed by the same code.
    fn records(data: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while let Some(header) = data.get(at..at + 4) {
            let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            at += 4;
            let Some(record) = data.get(at..at + len) else {
                break;
            };
            out.push(Bytes::copy_from_slice(record));
            at += len;
        }
        out
    }

    #[test]
    fn ldac_packets_walk_to_the_frame_count_their_encoder_reported() {
        // The cross-implementation check. These fixtures were produced by Sony's own
        // encoder (`pipeline/examples/ldac_fixtures.rs`), which *reports* how many
        // `ldac_transport_frame`s it packed into each MTU — and `crate::ldac` walks the
        // same bytes with a pure Rust parser that shares no code with the library. The
        // counts agreeing is what makes the field widths in that module a finding rather
        // than a guess, and it holds in every build because nothing here links the codec.
        //
        // The numbers below are what the encoder printed when the fixtures were generated:
        // 44.1 kHz stereo, MQ, 679-byte MTU gives 14 packets of 6 frames each.
        let packets = records(include_bytes!(
            "../tests/fixtures/a2dp-ldac-44100-stereo.bin"
        ));
        assert_eq!(packets.len(), 14, "fixture has the wrong number of packets");

        let mut d = Depacketizer::new(AudioCodec::Ldac, 44_100);
        let mut frames = 0u32;
        for packet in packets {
            let frame = d.push(packet).unwrap();
            // Every payload starts at a frame boundary, which is the property the
            // one-byte header coming off is *for*.
            assert_eq!(frame.data[0], 0xAA);
            let stream = d.ldac_stream().expect("an LDAC payload declares itself");
            assert_eq!(stream.sample_rate, ldac::SampleRate::Hz44100);
            assert_eq!(stream.channels, ldac::ChannelConfig::Stereo);
            frames += u32::from(stream.frames);
        }
        assert_eq!(
            frames, 84,
            "the walk must find every frame the encoder wrote"
        );
        // 84 frames x 128 samples per channel: a hair under the 0.25 s that went in, the
        // difference being the partial frame the encoder never emitted.
        assert_eq!(frames * 128, 10_752);
    }

    #[test]
    fn the_rate_ldac_declares_is_read_from_the_stream_not_the_negotiation() {
        // 96 kHz dual channel, and the depacketizer told 44.1 kHz stereo. It must report
        // what the *frames* say, because that is what the decoder will follow: LDAC
        // reconfigures itself from the frame header and keeps playing, so a receiver that
        // trusted its own negotiation here would log 44.1 kHz for a 96 kHz stream and
        // nothing would ever contradict it (the aptX shape of Q25, in the one codec that
        // can actually be checked).
        let packets = records(include_bytes!("../tests/fixtures/a2dp-ldac-96000-dual.bin"));
        assert_eq!(packets.len(), 7);

        let mut d = Depacketizer::new(AudioCodec::Ldac, 44_100);
        let mut frames = 0u32;
        for packet in packets {
            d.push(packet).unwrap();
            let stream = d.ldac_stream().unwrap();
            assert_eq!(stream.sample_rate, ldac::SampleRate::Hz96000);
            assert_eq!(stream.channels, ldac::ChannelConfig::DualChannel);
            // Two channels, coded independently — the trap the capability field has too.
            assert_eq!(stream.channels.channel_count(), 2);
            frames += u32::from(stream.frames);
        }
        assert_eq!(frames, 42);
        // 256 samples per frame at 96 kHz, not 128: the same PCM count from six times
        // fewer seconds of audio.
        assert_eq!(frames * 256, 10_752);
    }

    #[test]
    fn a_mono_ldac_stream_says_so_in_its_frames() {
        // Mono is the configuration where the frame's channel field actually changes the
        // *size* of what a decoder produces, so it is the one that catches a reader taking
        // the channel count from the negotiation instead. Same audio, same frame count as
        // the stereo fixture, half the bitstream.
        let packets = records(include_bytes!("../tests/fixtures/a2dp-ldac-44100-mono.bin"));
        assert_eq!(packets.len(), 7);

        let mut d = Depacketizer::new(AudioCodec::Ldac, 44_100);
        let mut frames = 0u32;
        for packet in packets {
            d.push(packet).unwrap();
            let stream = d.ldac_stream().unwrap();
            assert_eq!(stream.channels, ldac::ChannelConfig::Mono);
            assert_eq!(stream.channels.channel_count(), 1);
            frames += u32::from(stream.frames);
        }
        // Twelve frames per packet rather than six: a mono frame is half the size, so twice
        // as many fit in the same MTU.
        assert_eq!(frames, 84);
    }

    #[test]
    fn an_aac_payload_that_is_not_a_valid_multiplex_is_refused() {
        // Three arbitrary bytes are not an AudioMuxElement. Accepting them would put
        // garbage in front of the decoder, which is exactly the failure this path exists
        // to prevent.
        let mut d = Depacketizer::new(AudioCodec::Aac, 44_100);
        assert!(d.push(rtp(0, &[0xAA, 0xBB, 0xCC])).is_err());
    }

    #[test]
    fn presentation_time_is_rebased_so_a_stream_starts_at_zero() {
        // Senders start their RTP clock wherever they like. Passing the raw timestamp
        // through would place the first frame hours into the presentation timeline.
        // aptX HD rather than AAC: this is about the RTP clock, and AAC payloads now have
        // to be real multiplexes, which would only obscure what is under test.
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 44_100);
        let first = d.push(rtp(1_000_000, &[1, 2, 3])).unwrap();
        assert_eq!(first.pts, Duration::ZERO);

        let later = d.push(rtp(1_000_000 + 44_100, &[4, 5, 6])).unwrap();
        assert_eq!(later.pts, Duration::from_secs(1));
    }

    #[test]
    fn a_timestamp_wrap_does_not_throw_away_the_clock() {
        // 32-bit RTP timestamps roll over in about a day at 48 kHz — well within a
        // hackerspace's uptime. Plain subtraction would jump the clock by ~27 hours.
        let mut d = Depacketizer::new(AudioCodec::AptXHd, 48_000);
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
