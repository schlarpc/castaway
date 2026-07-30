//! The LDAC transport-frame header — the one A2DP codec that states its own format.
//!
//! Every other codec here has to be *told* what it is playing. aptX and aptX HD carry no
//! header at all, so a stream decoded at the wrong rate plays at the wrong pitch with
//! nothing in any log (OPEN-QUESTIONS Q25); SBC states its bitpool but not its rate; AAC
//! hides its configuration in a LATM multiplex. LDAC puts the sample rate and the channel
//! configuration in the first three bytes of every frame, which makes it the only codec
//! where the negotiated configuration can be **checked** rather than trusted.
//!
//! That is the reason this parser exists in a crate that does not decode anything. Two
//! failures it is here to catch:
//!
//! - The one-byte A2DP payload header not coming off, which shifts every frame by a byte.
//!   With no syncword check that decodes to noise rather than failing — the same silent
//!   shape as treating classic aptX as RTP (see [`crate::media`]).
//! - A sender streaming at a rate other than the one AVDTP settled on. Sony's decoder
//!   re-configures itself from these bytes and carries on, so nothing downstream would
//!   notice; comparing [`FrameHeader::sample_rate`] against the negotiated
//!   [`castaway_core::AudioFormat`] is what makes that visible.
//!
//! The field widths come from the library's own packer and unpacker
//! (`pack_frame_header_ldac` / `unpack_frame_header_ldac` in open-vela/external_libldac),
//! not from a specification we cannot see: syncword 8, sample-rate index 3, channel
//! config 2, frame length 9 (stored one less than it is), frame status 2 — twenty-four
//! bits, MSB first, and no byte alignment inside them. `tests/fixtures/a2dp-ldac-*.bin`
//! are frames from that same library, so the two readings are cross-checked against each
//! other rather than against this module's own idea of the format.

use crate::error::AudioError;

/// Bytes of frame header before an LDAC frame's payload.
const HEADER_BYTES: usize = 3;

/// LDAC's syncword, and the whole first byte of every transport frame.
const SYNCWORD: u8 = 0xAA;

/// The sample rate an LDAC frame declares.
///
/// An enum rather than a `u32` because the sample-rate index is three bits wide and only
/// four of its eight values mean anything: the library's own
/// `ldaclib_assert_supported_sampling_rate_index` refuses the rest. Parsing into this
/// makes the unsupported ones unrepresentable instead of arriving downstream as a
/// plausible-looking number (ground rule 1).
///
/// Note what is *absent*: LDAC's A2DP capability can advertise 176.4 and 192 kHz, and the
/// codec's own bitstream cannot express them. That is not an omission here — the frame
/// header has no index for them, so a sink that offers them is offering something no
/// frame can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRate {
    /// 44.1 kHz, frame index 0.
    Hz44100,
    /// 48 kHz, frame index 1.
    Hz48000,
    /// 88.2 kHz, frame index 2.
    Hz88200,
    /// 96 kHz, frame index 3.
    Hz96000,
}

impl SampleRate {
    /// Parse the 3-bit sample-rate index.
    fn from_index(index: u8) -> Result<Self, AudioError> {
        Ok(match index {
            0 => Self::Hz44100,
            1 => Self::Hz48000,
            2 => Self::Hz88200,
            3 => Self::Hz96000,
            other => {
                return Err(AudioError::UnsupportedCodec {
                    what: "ldac sample rate index",
                    id: u32::from(other),
                })
            }
        })
    }

    /// The rate in Hz.
    #[must_use]
    pub const fn hz(self) -> u32 {
        match self {
            Self::Hz44100 => 44_100,
            Self::Hz48000 => 48_000,
            Self::Hz88200 => 88_200,
            Self::Hz96000 => 96_000,
        }
    }

    /// The 3-bit index that encodes this rate on the wire.
    ///
    /// Written out rather than taken from the enum's discriminant: a wire value that
    /// depends on declaration order changes silently when somebody reorders the variants,
    /// and the inverse of [`Self::from_index`] should be visible next to it.
    const fn index(self) -> u8 {
        match self {
            Self::Hz44100 => 0,
            Self::Hz48000 => 1,
            Self::Hz88200 => 2,
            Self::Hz96000 => 3,
        }
    }

    /// PCM frames one LDAC frame decodes to, per channel.
    ///
    /// 128 at the two base rates and 256 at the doubled ones — LDAC keeps the frame
    /// *duration* roughly constant rather than the sample count. Needed to turn a frame
    /// count into a duration without waiting for a decoder to say so.
    #[must_use]
    pub const fn frame_samples(self) -> u16 {
        match self {
            Self::Hz44100 | Self::Hz48000 => 128,
            Self::Hz88200 | Self::Hz96000 => 256,
        }
    }
}

/// How an LDAC frame codes its channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelConfig {
    /// One channel.
    Mono,
    /// Two channels, coded independently.
    DualChannel,
    /// Two channels, coded jointly.
    Stereo,
}

impl ChannelConfig {
    /// Parse the 2-bit channel-configuration index.
    fn from_index(index: u8) -> Result<Self, AudioError> {
        Ok(match index {
            0 => Self::Mono,
            1 => Self::DualChannel,
            2 => Self::Stereo,
            other => {
                return Err(AudioError::UnsupportedCodec {
                    what: "ldac channel config index",
                    id: u32::from(other),
                })
            }
        })
    }

    /// The 2-bit index that encodes this configuration on the wire.
    const fn index(self) -> u8 {
        match self {
            Self::Mono => 0,
            Self::DualChannel => 1,
            Self::Stereo => 2,
        }
    }

    /// How many channels the frame carries.
    ///
    /// Dual channel is two channels, not one — the same trap as the capability's
    /// channel-mode field (see [`crate::codec::CodecCapability::channel_count`]).
    #[must_use]
    pub const fn channel_count(self) -> u8 {
        match self {
            Self::Mono => 1,
            Self::DualChannel | Self::Stereo => 2,
        }
    }
}

/// The three-byte header at the front of every LDAC transport frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// The rate the frame was coded at.
    pub sample_rate: SampleRate,
    /// The channel configuration the frame was coded in.
    pub channels: ChannelConfig,
    /// Payload bytes after this header.
    ///
    /// On the wire this is stored one less than its value, because a zero-length frame is
    /// not a thing and the extra code point buys another byte of range. Reading it
    /// without the `+ 1` produces a walk that drifts one byte per frame.
    pub payload_len: u16,
    /// The frame-status field, two bits, meaningful only to the codec's own bit
    /// allocator. Carried so a round-trip is exact.
    pub status: u8,
}

impl FrameHeader {
    /// Bytes this header occupies.
    pub const BYTES: usize = HEADER_BYTES;

    /// Parse one frame header.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if fewer than three bytes are available,
    /// [`AudioError::BadMediaPacket`] if the syncword is absent, and
    /// [`AudioError::UnsupportedCodec`] for a sample rate or channel configuration the
    /// codec does not define.
    pub fn parse(buf: &[u8]) -> Result<Self, AudioError> {
        let [b0, b1, b2, ..] = *buf else {
            return Err(AudioError::Truncated {
                what: "ldac frame header",
                need: HEADER_BYTES,
                have: buf.len(),
            });
        };
        if b0 != SYNCWORD {
            // The check that earns this module. An LDAC payload with its one-byte A2DP
            // header still attached starts with a frame count, not 0xAA.
            return Err(AudioError::BadMediaPacket(
                "ldac frame does not start with its syncword",
            ));
        }
        // syncword(8) | rate(3) | channels(2) | length-1(9) | status(2), MSB first, with
        // the length straddling both remaining bytes.
        let length_field = (u16::from(b1 & 0x07) << 6) | u16::from(b2 >> 2);
        Ok(Self {
            sample_rate: SampleRate::from_index(b1 >> 5)?,
            channels: ChannelConfig::from_index((b1 >> 3) & 0x03)?,
            payload_len: length_field + 1,
            status: b2 & 0x03,
        })
    }

    /// Encode the header back to its three bytes.
    ///
    /// Here so the parser can be tested against itself over the whole field space rather
    /// than only over the handful of configurations an encoder happens to emit — a
    /// round-trip is what catches a mask or a shift that is wrong only for values the
    /// fixtures do not contain.
    #[must_use]
    pub const fn encode(&self) -> [u8; HEADER_BYTES] {
        let rate = self.sample_rate.index();
        let channels = self.channels.index();
        let length_field = self.payload_len.saturating_sub(1) & 0x01FF;
        [
            SYNCWORD,
            (rate << 5) | (channels << 3) | ((length_field >> 6) as u8 & 0x07),
            (((length_field & 0x3F) as u8) << 2) | (self.status & 0x03),
        ]
    }

    /// Total size of the transport frame this header introduces.
    #[must_use]
    pub const fn frame_len(&self) -> usize {
        HEADER_BYTES + self.payload_len as usize
    }
}

/// The stream shape an LDAC payload declares, as read from its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// The coded sample rate.
    pub sample_rate: SampleRate,
    /// The coded channel configuration.
    pub channels: ChannelConfig,
    /// How many whole transport frames the payload was found to contain.
    ///
    /// Walked, not read from the one-byte A2DP payload header. The header byte is the
    /// frame count as AOSP's encoder writes it, but the walk is self-checking — each
    /// frame states its own length and the next syncword either lands or does not — so
    /// there is no reason to depend on a byte whose layout is defined by a sender rather
    /// than by the codec.
    pub frames: u16,
}

impl StreamConfig {
    /// Read the configuration of an LDAC payload, with the A2DP header byte already off.
    ///
    /// Strict about the *first* frame and permissive after it, deliberately. A missing
    /// first syncword is the framing mistake that decodes to noise, and it must be an
    /// error. A walk that runs out mid-sequence is a different matter: Sony's decoder
    /// consumes frame by frame and reports its own errors per frame, so refusing the
    /// whole packet here would trade a diagnosable problem for a silent gap in the audio.
    ///
    /// # Errors
    /// Whatever [`FrameHeader::parse`] returns for the first frame.
    pub fn parse(payload: &[u8]) -> Result<Self, AudioError> {
        let first = FrameHeader::parse(payload)?;
        let mut frames = 0u16;
        let mut offset = 0usize;
        while let Some(rest) = payload.get(offset..) {
            let Ok(header) = FrameHeader::parse(rest) else {
                break;
            };
            if rest.len() < header.frame_len() {
                break;
            }
            frames = frames.saturating_add(1);
            offset += header.frame_len();
        }
        Ok(Self {
            sample_rate: first.sample_rate,
            channels: first.channels,
            frames,
        })
    }

    /// How many PCM frames per channel this payload should decode to.
    #[must_use]
    pub const fn pcm_frames(&self) -> u32 {
        self.sample_rate.frame_samples() as u32 * self.frames as u32
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    #[test]
    fn the_first_byte_of_every_frame_is_the_syncword() {
        // 0xAA is the whole of byte 0 — eight bits, no fields sharing it. A payload that
        // does not start with it has not had its A2DP header removed, and handing it to a
        // decoder is the "shifted by one byte" failure that produces noise rather than an
        // error.
        let header = FrameHeader {
            sample_rate: SampleRate::Hz96000,
            channels: ChannelConfig::Stereo,
            payload_len: 330,
            status: 0,
        };
        assert_eq!(header.encode()[0], 0xAA);
        assert!(matches!(
            FrameHeader::parse(&[0x02, 0xAA, 0xBB, 0xCC]),
            Err(AudioError::BadMediaPacket(_))
        ));
    }

    #[test]
    fn a_hand_built_header_reads_as_the_fields_that_went_into_it() {
        // The golden vector, assembled bit by bit from the field widths rather than from
        // this module's own encoder, so a shift that is wrong in both directions cannot
        // hide behind a round-trip.
        //
        //   syncword 10101010
        //   rate     001        -> 48 kHz
        //   channels 10         -> stereo
        //   length   101000110  -> 326, so payload_len 327
        //   status   01
        //
        // Packed MSB first: 10101010 001 10 101 000110 01
        let bytes = hex!("aa 35 19");
        let header = FrameHeader::parse(&bytes).unwrap();
        assert_eq!(header.sample_rate, SampleRate::Hz48000);
        assert_eq!(header.channels, ChannelConfig::Stereo);
        assert_eq!(
            header.payload_len, 327,
            "the length field is stored one low"
        );
        assert_eq!(header.status, 1);
        // 3 header bytes plus the payload: the stride a walk has to take.
        assert_eq!(header.frame_len(), 330);
        assert_eq!(header.encode(), bytes, "and it packs back to the same bits");
    }

    #[test]
    fn every_representable_header_round_trips() {
        // The field space, not just the values an encoder emits. A wrong mask on the
        // nine-bit length is invisible at the 327 the fixtures happen to use and obvious
        // at 1 and at 512.
        for rate in [
            SampleRate::Hz44100,
            SampleRate::Hz48000,
            SampleRate::Hz88200,
            SampleRate::Hz96000,
        ] {
            for channels in [
                ChannelConfig::Mono,
                ChannelConfig::DualChannel,
                ChannelConfig::Stereo,
            ] {
                for payload_len in [1u16, 2, 63, 64, 65, 326, 327, 511, 512] {
                    for status in 0u8..4 {
                        let header = FrameHeader {
                            sample_rate: rate,
                            channels,
                            payload_len,
                            status,
                        };
                        assert_eq!(
                            FrameHeader::parse(&header.encode()).unwrap(),
                            header,
                            "{header:?} did not survive"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_length_field_spans_two_bytes_and_is_stored_one_low() {
        // Nine bits across a byte boundary: three in the low bits of byte 1, six in the
        // high bits of byte 2. Reading it as a byte-aligned field, or forgetting the
        // `+ 1`, gives a walk that drifts and then loses the next syncword.
        let max = FrameHeader {
            sample_rate: SampleRate::Hz44100,
            channels: ChannelConfig::Mono,
            payload_len: 512,
            status: 0,
        };
        let bytes = max.encode();
        assert_eq!(
            bytes[1] & 0x07,
            0b111,
            "top three bits of length-1 = 511>>6"
        );
        assert_eq!(bytes[2] >> 2, 0b11_1111, "bottom six");
        assert_eq!(FrameHeader::parse(&bytes).unwrap().payload_len, 512);
    }

    #[test]
    fn a_rate_the_bitstream_cannot_express_is_refused() {
        // LDAC's *capability* can advertise 176.4 and 192 kHz; its frame header has no
        // index for them. Four of the eight index values are undefined and the library's
        // own assert refuses them, so a plausible-looking rate must not reach a decoder.
        for index in 4u8..8 {
            let bytes = [SYNCWORD, index << 5, 0x00];
            assert!(
                matches!(
                    FrameHeader::parse(&bytes),
                    Err(AudioError::UnsupportedCodec { .. })
                ),
                "rate index {index} should be refused"
            );
        }
        // Channel config 3 is undefined in the same way.
        let bytes = [SYNCWORD, 0b0001_1000, 0x00];
        assert!(matches!(
            FrameHeader::parse(&bytes),
            Err(AudioError::UnsupportedCodec { .. })
        ));
    }

    #[test]
    fn a_two_byte_header_is_truncated_not_guessed_at() {
        assert!(matches!(
            FrameHeader::parse(&hex!("aa 35")),
            Err(AudioError::Truncated {
                need: 3,
                have: 2,
                ..
            })
        ));
    }

    /// One transport frame of `payload_len` bytes, filled with a recognisable pattern.
    fn frame(rate: SampleRate, channels: ChannelConfig, payload_len: u16) -> Vec<u8> {
        let header = FrameHeader {
            sample_rate: rate,
            channels,
            payload_len,
            status: 0,
        };
        let mut out = header.encode().to_vec();
        out.extend(std::iter::repeat_n(0x5A, payload_len as usize));
        out
    }

    #[test]
    fn a_payload_of_several_frames_is_walked_by_their_own_lengths() {
        // What an A2DP packet actually holds: LDAC packs as many transport frames into
        // one MTU as fit, and the count is only recoverable by walking them.
        let mut payload = Vec::new();
        for _ in 0..4 {
            payload.extend(frame(SampleRate::Hz44100, ChannelConfig::Stereo, 108));
        }
        let config = StreamConfig::parse(&payload).unwrap();
        assert_eq!(config.frames, 4);
        assert_eq!(config.sample_rate, SampleRate::Hz44100);
        assert_eq!(config.channels, ChannelConfig::Stereo);
        // 128 samples per channel per frame at 44.1 kHz.
        assert_eq!(config.pcm_frames(), 512);
    }

    #[test]
    fn frames_of_different_lengths_still_tile() {
        // The lengths are per frame, not per stream: LDAC's own ABR moves them mid-stream
        // and a walk with a fixed stride would lose the sequence at the first change.
        let mut payload = frame(SampleRate::Hz96000, ChannelConfig::DualChannel, 60);
        payload.extend(frame(SampleRate::Hz96000, ChannelConfig::DualChannel, 90));
        payload.extend(frame(SampleRate::Hz96000, ChannelConfig::DualChannel, 45));
        let config = StreamConfig::parse(&payload).unwrap();
        assert_eq!(config.frames, 3);
        // 256 samples per channel per frame at 96 kHz.
        assert_eq!(config.pcm_frames(), 768);
    }

    #[test]
    fn a_truncated_tail_costs_the_tail_and_not_the_packet() {
        // Strict about the first frame, permissive after it. Sony's decoder consumes
        // frame by frame and reports per-frame errors, so refusing the whole packet here
        // would turn something diagnosable into a silent gap.
        let mut payload = frame(SampleRate::Hz48000, ChannelConfig::Stereo, 100);
        payload.extend(frame(SampleRate::Hz48000, ChannelConfig::Stereo, 100));
        payload.truncate(payload.len() - 40);
        let config = StreamConfig::parse(&payload).unwrap();
        assert_eq!(
            config.frames, 1,
            "the whole frame counts, the partial does not"
        );
        assert_eq!(config.sample_rate, SampleRate::Hz48000);
    }

    #[test]
    fn a_payload_that_is_not_ldac_at_all_is_refused() {
        assert!(StreamConfig::parse(&[]).is_err());
        assert!(
            StreamConfig::parse(&hex!("9c 31 35 00")).is_err(),
            "an SBC frame"
        );
    }
}
