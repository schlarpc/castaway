//! A2DP codec capabilities — the table that decides what a phone may send us.
//!
//! All five codecs live here, which is the whole reason the stack is ours: Windows'
//! inbox sink offers SBC and nothing else, and no OS gives us a way to add one
//! (architecture-substrate.md §11.1).
//!
//! Two of them are "non-A2DP" vendor codecs, meaning codec type `0xFF` plus a vendor
//! id and vendor codec id inside the payload — aptX and aptX HD are Qualcomm's, LDAC is
//! Sony's. Their vendor ids are **little-endian** inside a structure whose every other
//! field is big-endian, which is exactly the kind of detail that produces a capability
//! block a phone silently declines.

use bytes::{BufMut, BytesMut};
use castaway_core::AudioCodec;

use crate::error::AudioError;

/// Codec type byte in a Media Codec capability.
mod codec_type {
    pub const SBC: u8 = 0x00;
    pub const AAC: u8 = 0x02;
    /// Vendor-specific: the real identity is the vendor id inside the payload.
    pub const NON_A2DP: u8 = 0xFF;
}

/// Vendor identifiers for the non-A2DP codecs.
mod vendor {
    /// Qualcomm/APT Licensing.
    pub const QUALCOMM: u32 = 0x0000_004F;
    /// Sony.
    pub const SONY: u32 = 0x0000_012D;
    /// aptX.
    pub const APTX_CODEC: u16 = 0x0001;
    /// aptX HD.
    pub const APTX_HD_CODEC: u16 = 0x0024;
    /// LDAC.
    pub const LDAC_CODEC: u16 = 0x00AA;
}

/// Sample rates a capability block can advertise.
///
/// A bitmask rather than a single value because a *capability* is a set and a
/// *configuration* is one element of it — see [`CodecCapability::is_configuration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SampleRates(u8);

impl SampleRates {
    /// 16 kHz.
    pub const HZ_16000: Self = Self(1 << 3);
    /// 32 kHz.
    pub const HZ_32000: Self = Self(1 << 2);
    /// 44.1 kHz — the rate SBC and every phone always support.
    pub const HZ_44100: Self = Self(1 << 1);
    /// 48 kHz.
    pub const HZ_48000: Self = Self(1 << 0);

    /// Everything an A2DP sink is expected to accept.
    pub const ALL: Self = Self(0b1111);

    /// The two rates real senders use.
    pub const COMMON: Self = Self(Self::HZ_44100.0 | Self::HZ_48000.0);

    /// Wrap raw bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0x0F)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether `other` is entirely contained in this set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether exactly one rate is set — i.e. this is a configuration, not a capability.
    #[must_use]
    pub const fn is_single(self) -> bool {
        self.0.count_ones() == 1
    }

    /// The rate in Hz, if exactly one is selected.
    #[must_use]
    pub const fn hz(self) -> Option<u32> {
        Some(match self.0 {
            0b1000 => 16_000,
            0b0100 => 32_000,
            0b0010 => 44_100,
            0b0001 => 48_000,
            _ => return None,
        })
    }

    /// The highest rate in the set, as a configuration.
    #[must_use]
    pub const fn best(self) -> Self {
        if self.0 & Self::HZ_48000.0 != 0 {
            Self::HZ_48000
        } else if self.0 & Self::HZ_44100.0 != 0 {
            Self::HZ_44100
        } else if self.0 & Self::HZ_32000.0 != 0 {
            Self::HZ_32000
        } else {
            Self::HZ_16000
        }
    }
}

impl std::ops::BitOr for SampleRates {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// SBC channel modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChannelModes(u8);

impl ChannelModes {
    /// Mono.
    pub const MONO: Self = Self(1 << 3);
    /// Dual channel.
    pub const DUAL: Self = Self(1 << 2);
    /// Stereo.
    pub const STEREO: Self = Self(1 << 1);
    /// Joint stereo — best quality per bit, and what senders pick when offered.
    pub const JOINT_STEREO: Self = Self(1 << 0);
    /// Everything.
    pub const ALL: Self = Self(0b1111);

    /// Wrap raw bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0x0F)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether exactly one mode is set.
    #[must_use]
    pub const fn is_single(self) -> bool {
        self.0.count_ones() == 1
    }

    /// The preferred mode in the set, as a configuration.
    #[must_use]
    pub const fn best(self) -> Self {
        if self.0 & Self::JOINT_STEREO.0 != 0 {
            Self::JOINT_STEREO
        } else if self.0 & Self::STEREO.0 != 0 {
            Self::STEREO
        } else if self.0 & Self::DUAL.0 != 0 {
            Self::DUAL
        } else {
            Self::MONO
        }
    }

    /// How many channels this configuration carries.
    #[must_use]
    pub const fn channel_count(self) -> u8 {
        if self.0 == Self::MONO.0 {
            1
        } else {
            2
        }
    }
}

impl std::ops::BitOr for ChannelModes {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A Media Codec capability, parsed into the codec it describes.
///
/// The same type serves as both an *offer* (several bits set per field) and a
/// *configuration* (exactly one), because that is how A2DP works: the sender picks from
/// our advertised set and hands the same structure back with the choices narrowed.
/// [`CodecCapability::is_configuration`] is what distinguishes them, and accepting a
/// SET_CONFIGURATION that still has several bits set is a real interop bug.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodecCapability {
    /// SBC — mandatory, so every sender has it and it is the guaranteed fallback.
    Sbc {
        /// Advertised or selected sample rates.
        rates: SampleRates,
        /// Advertised or selected channel modes.
        channels: ChannelModes,
        /// Block length bits (16/12/8/4 from bit 7 down).
        block_lengths: u8,
        /// Subband bits (8, 4).
        subbands: u8,
        /// Allocation method bits (SNR, Loudness).
        allocations: u8,
        /// Minimum bitpool.
        min_bitpool: u8,
        /// Maximum bitpool — the quality knob senders actually vary.
        max_bitpool: u8,
    },
    /// AAC — what every iPhone offers, and the realistic quality ceiling for Apple.
    Aac {
        /// Object type bits (MPEG-2 LC, MPEG-4 LC, LTP, scalable).
        object_types: u8,
        /// Sample rates, as AAC's own 12-bit field.
        rate_bits: u16,
        /// Channel count bits.
        channel_bits: u8,
        /// Whether variable bitrate is allowed.
        vbr: bool,
        /// Peak bitrate ceiling, in bits per second.
        bitrate: u32,
    },
    /// Qualcomm aptX.
    AptX {
        /// Sample rates.
        rates: SampleRates,
        /// Channel modes.
        channels: ChannelModes,
    },
    /// Qualcomm aptX HD.
    AptXHd {
        /// Sample rates.
        rates: SampleRates,
        /// Channel modes.
        channels: ChannelModes,
    },
    /// Sony LDAC — the highest bitrate on offer, and the only one libav cannot decode
    /// (see OPEN-QUESTIONS Q22).
    Ldac {
        /// Sample rates, in LDAC's own 6-bit field.
        rate_bits: u8,
        /// Channel mode bits (stereo, dual, mono).
        channel_bits: u8,
    },
}

impl CodecCapability {
    /// Which decoder this capability needs.
    #[must_use]
    pub const fn audio_codec(&self) -> AudioCodec {
        match self {
            Self::Sbc { .. } => AudioCodec::Sbc,
            Self::Aac { .. } => AudioCodec::Aac,
            Self::AptX { .. } => AudioCodec::AptX,
            Self::AptXHd { .. } => AudioCodec::AptXHd,
            Self::Ldac { .. } => AudioCodec::Ldac,
        }
    }

    /// A short stable name for logs.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Sbc { .. } => "sbc",
            Self::Aac { .. } => "aac",
            Self::AptX { .. } => "aptx",
            Self::AptXHd { .. } => "aptx-hd",
            Self::Ldac { .. } => "ldac",
        }
    }

    /// A human-readable summary of a *configuration*, for the on-screen device card.
    ///
    /// Only meaningful once the sender has narrowed the offer — a capability describes a
    /// set, and "44.1 or 48 kHz" is not something to put on a screen. Returns just the
    /// codec name when the rate cannot be resolved, which is honest rather than wrong.
    #[must_use]
    pub fn describe(&self) -> String {
        let Some(rate) = self.sample_rate() else {
            return self.name().to_ascii_uppercase();
        };
        let khz = if rate % 1000 == 0 {
            format!("{} kHz", rate / 1000)
        } else {
            // 44100 reads as 44.1, not 44 — the difference people actually look for.
            format!("{:.1} kHz", f64::from(rate) / 1000.0)
        };
        match self.channel_summary() {
            Some(channels) => format!("{} · {khz} · {channels}", self.display_name()),
            None => format!("{} · {khz}", self.display_name()),
        }
    }

    /// The codec's name as it is normally written.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Sbc { .. } => "SBC",
            Self::Aac { .. } => "AAC",
            Self::AptX { .. } => "aptX",
            Self::AptXHd { .. } => "aptX HD",
            Self::Ldac { .. } => "LDAC",
        }
    }

    /// The channel mode of a configuration, in words.
    #[must_use]
    pub fn channel_summary(&self) -> Option<&'static str> {
        let modes = match self {
            Self::Sbc { channels, .. }
            | Self::AptX { channels, .. }
            | Self::AptXHd { channels, .. } => *channels,
            Self::Aac { channel_bits, .. } => {
                return Some(if *channel_bits == 0b10 {
                    "mono"
                } else {
                    "stereo"
                })
            }
            Self::Ldac { channel_bits, .. } => {
                return Some(match channel_bits {
                    0b100 => "mono",
                    0b010 => "dual channel",
                    _ => "stereo",
                })
            }
        };
        if !modes.is_single() {
            return None;
        }
        Some(if modes == ChannelModes::MONO {
            "mono"
        } else if modes == ChannelModes::DUAL {
            "dual channel"
        } else if modes == ChannelModes::JOINT_STEREO {
            "joint stereo"
        } else {
            "stereo"
        })
    }

    /// Preference order, highest first. Used to order the SEP table we advertise.
    ///
    /// Senders generally pick the first endpoint they also support, so the order here is
    /// effectively the quality policy.
    #[must_use]
    pub const fn preference(&self) -> u8 {
        match self {
            Self::Ldac { .. } => 0,
            Self::AptXHd { .. } => 1,
            Self::AptX { .. } => 2,
            Self::Aac { .. } => 3,
            Self::Sbc { .. } => 4,
        }
    }

    /// Whether every field selects exactly one option, as a configuration must.
    ///
    /// A SET_CONFIGURATION that still has multiple bits set is ambiguous: the decoder
    /// cannot tell which rate the stream is in, and guessing produces audio at the wrong
    /// pitch rather than an error.
    #[must_use]
    pub fn is_configuration(&self) -> bool {
        match self {
            Self::Sbc {
                rates,
                channels,
                block_lengths,
                subbands,
                allocations,
                ..
            } => {
                rates.is_single()
                    && channels.is_single()
                    && block_lengths.count_ones() == 1
                    && subbands.count_ones() == 1
                    && allocations.count_ones() == 1
            }
            Self::Aac {
                object_types,
                rate_bits,
                channel_bits,
                ..
            } => {
                object_types.count_ones() == 1
                    && rate_bits.count_ones() == 1
                    && channel_bits.count_ones() == 1
            }
            Self::AptX { rates, channels } | Self::AptXHd { rates, channels } => {
                rates.is_single() && channels.is_single()
            }
            Self::Ldac {
                rate_bits,
                channel_bits,
            } => rate_bits.count_ones() == 1 && channel_bits.count_ones() == 1,
        }
    }

    /// The sample rate of a configuration, in Hz.
    #[must_use]
    pub fn sample_rate(&self) -> Option<u32> {
        match self {
            Self::Sbc { rates, .. } | Self::AptX { rates, .. } | Self::AptXHd { rates, .. } => {
                rates.hz()
            }
            Self::Aac { rate_bits, .. } => aac_rate_hz(*rate_bits),
            Self::Ldac { rate_bits, .. } => ldac_rate_hz(*rate_bits),
        }
    }

    /// Encode the Media Codec capability payload (media type onward).
    #[must_use]
    pub fn encode(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(16);
        // Media type: audio (0) in the high nibble.
        buf.put_u8(0x00);
        match self {
            Self::Sbc {
                rates,
                channels,
                block_lengths,
                subbands,
                allocations,
                min_bitpool,
                max_bitpool,
            } => {
                buf.put_u8(codec_type::SBC);
                buf.put_u8((rates.bits() << 4) | channels.bits());
                buf.put_u8((block_lengths << 4) | (subbands << 2) | allocations);
                buf.put_u8(*min_bitpool);
                buf.put_u8(*max_bitpool);
            }
            Self::Aac {
                object_types,
                rate_bits,
                channel_bits,
                vbr,
                bitrate,
            } => {
                buf.put_u8(codec_type::AAC);
                buf.put_u8(*object_types);
                // The 12-bit rate field straddles a byte boundary: high 8 bits in one
                // byte, low 4 bits in the top nibble of the next, with channels below.
                #[allow(clippy::cast_possible_truncation)]
                buf.put_u8((rate_bits >> 4) as u8);
                #[allow(clippy::cast_possible_truncation)]
                buf.put_u8((((rate_bits & 0x000F) as u8) << 4) | (channel_bits << 2));
                let br = bitrate & 0x007F_FFFF;
                #[allow(clippy::cast_possible_truncation)]
                buf.put_u8((u8::from(*vbr) << 7) | ((br >> 16) as u8));
                #[allow(clippy::cast_possible_truncation)]
                buf.put_u8((br >> 8) as u8);
                #[allow(clippy::cast_possible_truncation)]
                buf.put_u8(br as u8);
            }
            Self::AptX { rates, channels } => {
                buf.put_u8(codec_type::NON_A2DP);
                put_vendor(&mut buf, vendor::QUALCOMM, vendor::APTX_CODEC);
                buf.put_u8((rates.bits() << 4) | channels.bits());
            }
            Self::AptXHd { rates, channels } => {
                buf.put_u8(codec_type::NON_A2DP);
                put_vendor(&mut buf, vendor::QUALCOMM, vendor::APTX_HD_CODEC);
                buf.put_u8((rates.bits() << 4) | channels.bits());
                buf.put_bytes(0, 4); // reserved
            }
            Self::Ldac {
                rate_bits,
                channel_bits,
            } => {
                buf.put_u8(codec_type::NON_A2DP);
                put_vendor(&mut buf, vendor::SONY, vendor::LDAC_CODEC);
                buf.put_u8(rate_bits & 0x3F);
                buf.put_u8(channel_bits & 0x07);
            }
        }
        buf
    }

    /// Decode a Media Codec capability payload.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if short, [`AudioError::UnsupportedCodec`] for a codec
    /// or vendor id we don't implement.
    pub fn decode(buf: &[u8]) -> Result<Self, AudioError> {
        let need = |n: usize| -> Result<(), AudioError> {
            if buf.len() < n {
                Err(AudioError::Truncated {
                    what: "media codec capability",
                    need: n,
                    have: buf.len(),
                })
            } else {
                Ok(())
            }
        };
        need(2)?;
        let media_type = buf[0] >> 4;
        if media_type != 0 {
            return Err(AudioError::UnsupportedCodec {
                what: "media type",
                id: u32::from(media_type),
            });
        }
        match buf[1] {
            codec_type::SBC => {
                need(6)?;
                Ok(Self::Sbc {
                    rates: SampleRates::from_bits(buf[2] >> 4),
                    channels: ChannelModes::from_bits(buf[2] & 0x0F),
                    block_lengths: buf[3] >> 4,
                    subbands: (buf[3] >> 2) & 0x03,
                    allocations: buf[3] & 0x03,
                    min_bitpool: buf[4],
                    max_bitpool: buf[5],
                })
            }
            codec_type::AAC => {
                need(8)?;
                Ok(Self::Aac {
                    object_types: buf[2],
                    rate_bits: (u16::from(buf[3]) << 4) | u16::from(buf[4] >> 4),
                    channel_bits: (buf[4] >> 2) & 0x03,
                    vbr: buf[5] & 0x80 != 0,
                    bitrate: (u32::from(buf[5] & 0x7F) << 16)
                        | (u32::from(buf[6]) << 8)
                        | u32::from(buf[7]),
                })
            }
            codec_type::NON_A2DP => {
                need(8)?;
                // Vendor id and codec id are **little-endian** here, unlike everything
                // else in the block. Reading them big-endian yields a vendor nobody has
                // heard of and the endpoint is quietly skipped.
                let vendor_id = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
                let codec_id = u16::from_le_bytes([buf[6], buf[7]]);
                match (vendor_id, codec_id) {
                    (vendor::QUALCOMM, vendor::APTX_CODEC) => {
                        need(9)?;
                        Ok(Self::AptX {
                            rates: SampleRates::from_bits(buf[8] >> 4),
                            channels: ChannelModes::from_bits(buf[8] & 0x0F),
                        })
                    }
                    (vendor::QUALCOMM, vendor::APTX_HD_CODEC) => {
                        need(9)?;
                        Ok(Self::AptXHd {
                            rates: SampleRates::from_bits(buf[8] >> 4),
                            channels: ChannelModes::from_bits(buf[8] & 0x0F),
                        })
                    }
                    (vendor::SONY, vendor::LDAC_CODEC) => {
                        need(10)?;
                        Ok(Self::Ldac {
                            rate_bits: buf[8] & 0x3F,
                            channel_bits: buf[9] & 0x07,
                        })
                    }
                    _ => Err(AudioError::UnsupportedCodec {
                        what: "vendor codec",
                        id: vendor_id,
                    }),
                }
            }
            other => Err(AudioError::UnsupportedCodec {
                what: "codec type",
                id: u32::from(other),
            }),
        }
    }
}

fn put_vendor(buf: &mut BytesMut, vendor_id: u32, codec_id: u16) {
    buf.put_u32_le(vendor_id);
    buf.put_u16_le(codec_id);
}

const fn aac_rate_hz(bits: u16) -> Option<u32> {
    // AAC numbers its rate bits from the *top* of a 12-bit field: bit 11 is 8 kHz and
    // bit 0 is 96 kHz, the opposite direction to SBC's four-bit field.
    Some(match bits {
        0b1000_0000_0000 => 8_000,
        0b0100_0000_0000 => 11_025,
        0b0010_0000_0000 => 12_000,
        0b0001_0000_0000 => 16_000,
        0b0000_1000_0000 => 22_050,
        0b0000_0100_0000 => 24_000,
        0b0000_0010_0000 => 32_000,
        0b0000_0001_0000 => 44_100,
        0b0000_0000_1000 => 48_000,
        0b0000_0000_0100 => 64_000,
        0b0000_0000_0010 => 88_200,
        0b0000_0000_0001 => 96_000,
        _ => return None,
    })
}

const fn ldac_rate_hz(bits: u8) -> Option<u32> {
    Some(match bits {
        0b10_0000 => 44_100,
        0b01_0000 => 48_000,
        0b00_1000 => 88_200,
        0b00_0100 => 96_000,
        0b00_0010 => 176_400,
        0b00_0001 => 192_000,
        _ => return None,
    })
}

/// The capabilities we advertise, in preference order.
///
/// `include_ldac` reflects the `ldac` build feature: without a decoder we must *not*
/// advertise the endpoint, because a sender that picks it would stream something we
/// cannot play and the session would be silence rather than a clean fallback (Q22).
#[must_use]
pub fn advertised(include_ldac: bool) -> Vec<CodecCapability> {
    let mut caps = Vec::with_capacity(5);
    if include_ldac {
        caps.push(CodecCapability::Ldac {
            // 44.1/48/88.2/96 kHz, stereo + dual + mono.
            rate_bits: 0b11_1100,
            channel_bits: 0b111,
        });
    }
    caps.push(CodecCapability::AptXHd {
        rates: SampleRates::COMMON,
        channels: ChannelModes::STEREO | ChannelModes::JOINT_STEREO,
    });
    caps.push(CodecCapability::AptX {
        rates: SampleRates::COMMON,
        channels: ChannelModes::STEREO | ChannelModes::JOINT_STEREO,
    });
    caps.push(CodecCapability::Aac {
        // MPEG-4 AAC LC only: the one every phone implements, and the one ffmpeg
        // decodes without special cases.
        object_types: 1 << 6,
        rate_bits: (1 << 4) | (1 << 3), // 44.1 and 48 kHz
        channel_bits: 0b11,             // 1 or 2 channels
        vbr: true,
        bitrate: 320_000,
    });
    caps.push(CodecCapability::Sbc {
        rates: SampleRates::ALL,
        channels: ChannelModes::ALL,
        block_lengths: 0b1111,
        subbands: 0b11,
        allocations: 0b11,
        min_bitpool: 2,
        // 53 is the ceiling for "SBC high quality" at 44.1 kHz joint stereo; senders
        // that offer XQ push higher, and accepting it costs nothing to decode.
        max_bitpool: 53,
    });
    caps
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    fn round_trip(cap: &CodecCapability) -> CodecCapability {
        CodecCapability::decode(&cap.encode()).unwrap()
    }

    #[test]
    fn every_advertised_codec_round_trips() {
        for cap in advertised(true) {
            assert_eq!(round_trip(&cap), cap, "{} failed to round-trip", cap.name());
        }
    }

    #[test]
    fn vendor_ids_are_little_endian_inside_a_big_endian_block() {
        // The trap: everything else in a capability block is big-endian, but the vendor
        // id and vendor codec id are not. Read them the other way and the endpoint is
        // attributed to a vendor nobody has heard of, then silently skipped.
        let aptx = CodecCapability::AptX {
            rates: SampleRates::HZ_44100,
            channels: ChannelModes::JOINT_STEREO,
        };
        let bytes = aptx.encode();
        assert_eq!(&bytes[2..6], &hex!("4f 00 00 00"), "vendor id, LE");
        assert_eq!(&bytes[6..8], &hex!("01 00"), "codec id, LE");
        assert_eq!(round_trip(&aptx), aptx);

        let ldac = CodecCapability::Ldac {
            rate_bits: 1 << 5,
            channel_bits: 1,
        };
        assert_eq!(
            &ldac.encode()[2..6],
            &hex!("2d 01 00 00"),
            "Sony vendor id, LE"
        );
    }

    #[test]
    fn a_capability_is_not_a_configuration() {
        // Advertising several rates is a valid offer; accepting a SET_CONFIGURATION that
        // still has several set means the decoder cannot tell what rate the stream is,
        // and a wrong guess plays at the wrong pitch instead of erroring.
        let offer = CodecCapability::AptX {
            rates: SampleRates::COMMON,
            channels: ChannelModes::STEREO | ChannelModes::JOINT_STEREO,
        };
        assert!(!offer.is_configuration());
        assert_eq!(offer.sample_rate(), None);

        let chosen = CodecCapability::AptX {
            rates: SampleRates::HZ_48000,
            channels: ChannelModes::JOINT_STEREO,
        };
        assert!(chosen.is_configuration());
        assert_eq!(chosen.sample_rate(), Some(48_000));
    }

    #[test]
    fn sbc_configuration_narrows_every_field() {
        let offer = advertised(false)
            .into_iter()
            .find(|c| matches!(c, CodecCapability::Sbc { .. }))
            .unwrap();
        assert!(!offer.is_configuration());
        let CodecCapability::Sbc {
            min_bitpool,
            max_bitpool,
            ..
        } = offer
        else {
            panic!("expected sbc");
        };
        let chosen = CodecCapability::Sbc {
            rates: SampleRates::HZ_44100,
            channels: ChannelModes::JOINT_STEREO,
            block_lengths: 0b0001,
            subbands: 0b01,
            allocations: 0b01,
            min_bitpool,
            max_bitpool,
        };
        assert!(chosen.is_configuration());
        assert_eq!(chosen.sample_rate(), Some(44_100));
    }

    #[test]
    fn aacs_twelve_bit_rate_field_straddles_a_byte_boundary() {
        // High 8 bits in one byte, low 4 in the top nibble of the next with channels
        // beneath. Packing it as a plain u16 shifts every rate one nibble.
        let aac = CodecCapability::Aac {
            object_types: 1 << 6,
            rate_bits: 1 << 3, // 48 kHz
            channel_bits: 0b10,
            vbr: true,
            bitrate: 264_630,
        };
        let back = round_trip(&aac);
        assert_eq!(back, aac);
        let CodecCapability::Aac { rate_bits, .. } = back else {
            panic!("expected aac");
        };
        assert_eq!(aac_rate_hz(rate_bits), Some(48_000));
    }

    #[test]
    fn ldac_is_absent_from_the_table_when_the_decoder_is_not_built() {
        // Advertising an endpoint we cannot decode is worse than not advertising it: the
        // sender picks it and the session is silence rather than a clean fallback (Q22).
        let with = advertised(true);
        let without = advertised(false);
        assert!(with
            .iter()
            .any(|c| c.audio_codec() == castaway_core::AudioCodec::Ldac));
        assert!(!without
            .iter()
            .any(|c| c.audio_codec() == castaway_core::AudioCodec::Ldac));
        assert_eq!(without.len(), with.len() - 1);
    }

    #[test]
    fn the_table_is_ordered_best_first_and_always_ends_in_sbc() {
        // Senders pick the first endpoint they also support, so this ordering *is* the
        // quality policy. SBC last because it is the mandatory fallback.
        let caps = advertised(true);
        let prefs: Vec<u8> = caps.iter().map(CodecCapability::preference).collect();
        let mut sorted = prefs.clone();
        sorted.sort_unstable();
        assert_eq!(prefs, sorted);
        assert_eq!(caps.last().unwrap().name(), "sbc");
        assert_eq!(caps.first().unwrap().name(), "ldac");
    }

    #[test]
    fn an_unknown_vendor_codec_is_refused_by_name() {
        // A capability block for, say, aptX Adaptive — which ffmpeg cannot decode.
        let mut buf = BytesMut::new();
        buf.put_u8(0x00);
        buf.put_u8(codec_type::NON_A2DP);
        put_vendor(&mut buf, vendor::QUALCOMM, 0x00AD);
        buf.put_u8(0);
        assert!(matches!(
            CodecCapability::decode(&buf),
            Err(AudioError::UnsupportedCodec { .. })
        ));
    }

    #[test]
    fn a_truncated_capability_is_refused() {
        assert!(matches!(
            CodecCapability::decode(&hex!("00 00 21")),
            Err(AudioError::Truncated { .. })
        ));
    }

    #[test]
    fn a_configuration_describes_itself_for_the_device_card() {
        // What ends up on screen. 44100 must read as 44.1 kHz, not 44 — the digit people
        // actually look at to tell CD rate from 48.
        let aptx_hd = CodecCapability::AptXHd {
            rates: SampleRates::HZ_48000,
            channels: ChannelModes::JOINT_STEREO,
        };
        assert_eq!(aptx_hd.describe(), "aptX HD · 48 kHz · joint stereo");

        let sbc = CodecCapability::Sbc {
            rates: SampleRates::HZ_44100,
            channels: ChannelModes::STEREO,
            block_lengths: 0b0001,
            subbands: 0b01,
            allocations: 0b01,
            min_bitpool: 2,
            max_bitpool: 53,
        };
        assert_eq!(sbc.describe(), "SBC · 44.1 kHz · stereo");
    }

    #[test]
    fn an_unnarrowed_offer_describes_only_the_codec() {
        // A capability is a set. "44.1 or 48 kHz" is not something to put on a screen,
        // so the codec name alone is the honest answer.
        let offer = CodecCapability::AptX {
            rates: SampleRates::COMMON,
            channels: ChannelModes::ALL,
        };
        assert_eq!(offer.describe(), "APTX");
    }

    #[test]
    fn every_advertised_codec_has_a_display_name() {
        for cap in advertised(true) {
            let name = cap.display_name();
            assert!(!name.is_empty(), "{} needs a display name", cap.name());
            assert!(
                !name.contains('-'),
                "{name} should read the way it is written"
            );
        }
    }

    #[test]
    fn best_narrows_a_set_to_the_highest_member() {
        assert_eq!(SampleRates::ALL.best(), SampleRates::HZ_48000);
        assert_eq!(SampleRates::COMMON.best().hz(), Some(48_000));
        assert_eq!(ChannelModes::ALL.best(), ChannelModes::JOINT_STEREO);
        assert_eq!(ChannelModes::MONO.channel_count(), 1);
        assert_eq!(ChannelModes::JOINT_STEREO.channel_count(), 2);
    }
}
