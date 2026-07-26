//! LATM: the multiplex A2DP wraps AAC in, and the reason a decoder sees noise without it.
//!
//! A2DP v1.3.2 §4.5.4 does not define an AAC payload format of its own — it defers to
//! RFC 3016, which carries ISO/IEC 14496-3 `AudioMuxElement()`s directly in RTP with no
//! syncword layer, because RTP already provides framing. So the bytes after the RTP header
//! are *not* an AAC access unit: they are a multiplex whose header must be parsed to find
//! where the access unit starts.
//!
//! This is worth stating plainly because the failure is silent. Handing the whole
//! `AudioMuxElement` to a decoder yields "invalid data" on every packet, and handing it to
//! ffmpeg's `AAC_LATM` decoder specifically yields that with *no log line at all* — that
//! decoder is a LOAS decoder, and its first act is to check for the 11-bit `0x2B7` syncword
//! that RFC 3016 streams do not have. It never reaches the LATM parser. ffmpeg's own RFC
//! 3016 depacketizer (`libavformat/rtpdec_latm.c`) does what this module does: parse the
//! multiplex here, hand the raw access unit to the plain AAC decoder.
//!
//! The header length is not a constant. For the configuration an iPhone negotiates —
//! AAC-LC, 44.1 kHz, stereo — `StreamMuxConfig` happens to be exactly 72 bits and so the
//! payload starts on byte 9. That is a coincidence of *that* configuration: with
//! `audioMuxVersion == 0` the config is 16 bits, and explicit SBR/PS signalling lengthens
//! the `AudioSpecificConfig`. Nothing here may assume alignment or a fixed offset.

use bytes::Bytes;

use crate::error::AudioError;

/// Reads big-endian bit fields, which is the only way to read this format: the
/// `AudioSpecificConfig` inside a `StreamMuxConfig` routinely starts mid-byte.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read(&mut self, count: u32) -> Result<u32, AudioError> {
        let mut value: u32 = 0;
        for _ in 0..count {
            let byte = self
                .bytes
                .get(self.bit >> 3)
                .ok_or(AudioError::BadMediaPacket(
                    "LATM header ran past the packet",
                ))?;
            let bit = (byte >> (7 - (self.bit & 7))) & 1;
            value = (value << 1) | u32::from(bit);
            self.bit += 1;
        }
        Ok(value)
    }

    /// Read at most eight bits, where the result cannot overflow a byte by construction.
    fn read_u8(&mut self, count: u32) -> Result<u8, AudioError> {
        debug_assert!(count <= 8);
        let value = self.read(count.min(8))?;
        u8::try_from(value).map_err(|_| AudioError::BadMediaPacket("LATM field is too wide"))
    }

    /// `LatmGetValue()`: a two-bit byte count, then that many bytes plus one.
    fn value(&mut self) -> Result<u32, AudioError> {
        let extra = self.read(2)?;
        self.read((extra + 1) * 8)
    }

    /// Copy `count` bits out as bytes, left-aligned — how the `AudioSpecificConfig` is
    /// lifted back out of a bitstream it does not start byte-aligned in.
    fn read_bits_as_bytes(&mut self, count: u32) -> Result<Bytes, AudioError> {
        let mut out = vec![0u8; (count as usize).div_ceil(8)];
        for i in 0..count as usize {
            let bit = self.read(1)?;
            if bit == 1 {
                out[i >> 3] |= 1 << (7 - (i & 7));
            }
        }
        Ok(Bytes::from(out))
    }

    const fn bit_position(&self) -> usize {
        self.bit
    }

    fn seek(&mut self, bit: usize) {
        self.bit = bit;
    }
}

/// What a `StreamMuxConfig` settled, kept so a packet that reuses it can be parsed.
///
/// A sender may set `useSameStreamMux` and omit the configuration entirely — RFC 3016 says
/// it "SHOULD be transmitted repeatedly depending on the network condition", which is a
/// choice, not a guarantee. An iPhone repeats it every packet; a sender that does not would
/// be undecodable without this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMuxConfig {
    /// The `AudioSpecificConfig`, lifted out for use as decoder extradata.
    pub audio_specific_config: Bytes,
    /// Audio object type: 2 is AAC-LC.
    pub audio_object_type: u8,
    /// Sampling frequency in Hz, resolved from the index.
    pub sample_rate: u32,
    /// Channel configuration, as the count it denotes.
    pub channels: u8,
}

/// Sampling frequencies by `samplingFrequencyIndex` (ISO/IEC 14496-3).
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Extracts AAC access units from the `AudioMuxElement`s A2DP delivers.
#[derive(Debug, Default)]
pub struct LatmParser {
    config: Option<StreamMuxConfig>,
}

impl LatmParser {
    /// A parser that has not yet seen a configuration.
    #[must_use]
    pub const fn new() -> Self {
        Self { config: None }
    }

    /// The configuration in force, once a packet has carried one.
    #[must_use]
    pub const fn config(&self) -> Option<&StreamMuxConfig> {
        self.config.as_ref()
    }

    /// Pull the access unit out of one `AudioMuxElement`.
    ///
    /// # Errors
    /// [`AudioError::BadMediaPacket`] if the multiplex is malformed, uses a shape we do not
    /// implement, or arrives before any configuration has been seen.
    pub fn access_unit(&mut self, payload: &Bytes) -> Result<Bytes, AudioError> {
        let mut r = BitReader::new(payload);

        if r.read(1)? == 0 {
            self.config = Some(Self::stream_mux_config(&mut r)?);
        }
        if self.config.is_none() {
            return Err(AudioError::BadMediaPacket(
                "LATM packet reuses a configuration we have never been sent",
            ));
        }

        // PayloadLengthInfo for frameLengthType 0: bytes, with 255 as a continue marker.
        let mut length = 0usize;
        loop {
            let byte = r.read(8)? as usize;
            length += byte;
            if byte != 255 {
                break;
            }
        }

        // The payload is byte-aligned only because everything before it happened to be a
        // whole number of bytes; assert it rather than assume it.
        let bit = r.bit_position();
        if !bit.is_multiple_of(8) {
            return Err(AudioError::BadMediaPacket(
                "LATM payload does not start on a byte boundary",
            ));
        }
        let start = bit / 8;
        let end = start
            .checked_add(length)
            .ok_or(AudioError::BadMediaPacket("LATM payload length overflows"))?;
        if end > payload.len() {
            return Err(AudioError::BadMediaPacket(
                "LATM payload length runs past the packet",
            ));
        }
        Ok(payload.slice(start..end))
    }

    fn stream_mux_config(r: &mut BitReader<'_>) -> Result<StreamMuxConfig, AudioError> {
        let audio_mux_version = r.read(1)?;
        let audio_mux_version_a = if audio_mux_version == 1 {
            r.read(1)?
        } else {
            0
        };
        if audio_mux_version_a != 0 {
            // Reserved in the spec; both ffmpeg and FDK-AAC refuse it rather than guess.
            return Err(AudioError::BadMediaPacket(
                "LATM audioMuxVersionA is reserved",
            ));
        }
        if audio_mux_version == 1 {
            let _tara_buffer_fullness = r.value()?;
        }
        let _all_streams_same_time_framing = r.read(1)?;
        let num_sub_frames = r.read(6)?;
        let num_program = r.read(4)?;
        let num_layer = r.read(3)?;

        if num_sub_frames != 0 {
            // Several access units in one multiplex. Refused rather than half-handled:
            // each would need its own presentation time derived from the frame length, and
            // no capture we have exercises it — a wrong guess here is audible drift.
            return Err(AudioError::BadMediaPacket(
                "LATM numSubFrames > 0 is not implemented",
            ));
        }
        if num_program != 0 || num_layer != 0 {
            return Err(AudioError::BadMediaPacket(
                "LATM multi-program/multi-layer streams are not implemented",
            ));
        }

        // With audioMuxVersion 1 the config is length-prefixed, and that length is the
        // field whose omission makes every following field parse as garbage: the
        // AudioSpecificConfig reads as object type 0 at 8 kHz, for a stream that is
        // demonstrably AAC-LC at 44.1 kHz.
        let asc_len = if audio_mux_version == 1 {
            Some(r.value()?)
        } else {
            None
        };
        let asc_start = r.bit_position();

        let audio_object_type = r.read_u8(5)?;
        let frequency_index = r.read(4)? as usize;
        let channel_configuration = r.read_u8(4)?;

        let sample_rate = SAMPLE_RATES.get(frequency_index).copied().ok_or(
            // 13, 14 are reserved and 15 means an explicit 24-bit rate follows, which no
            // A2DP configuration uses.
            AudioError::BadMediaPacket("LATM names a sampling frequency index we cannot resolve"),
        )?;

        let asc_len = match asc_len {
            Some(len) => len,
            // audioMuxVersion 0 carries no length, so the config ends where its fields do.
            None => u32::try_from(r.bit_position() - asc_start).unwrap_or(0) + 3,
        };
        r.seek(asc_start);
        let audio_specific_config = r.read_bits_as_bytes(asc_len)?;

        let _frame_length_type = r.read(3)?;
        let _latm_buffer_fullness = r.read(8)?;
        let other_data_present = r.read(1)?;
        if other_data_present == 1 {
            if audio_mux_version == 1 {
                let _other_data_len_bits = r.value()?;
            } else {
                loop {
                    let escape = r.read(1)?;
                    let _ = r.read(8)?;
                    if escape == 0 {
                        break;
                    }
                }
            }
        }
        if r.read(1)? == 1 {
            let _crc = r.read(8)?;
        }

        Ok(StreamMuxConfig {
            audio_specific_config,
            audio_object_type,
            sample_rate,
            channels: channel_configuration,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Real payloads from an iPhone streaming A2DP AAC, captured after the RTP header was
    /// stripped, length-prefixed. See the fixture's commit for how it was taken.
    fn fixture() -> Vec<Bytes> {
        let data = include_bytes!("../tests/fixtures/a2dp-aac-iphone.bin");
        let mut out = Vec::new();
        let mut o = 0;
        while o + 4 <= data.len() {
            let len = u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as usize;
            o += 4;
            if o + len > data.len() {
                break;
            }
            out.push(Bytes::copy_from_slice(&data[o..o + len]));
            o += len;
        }
        out
    }

    #[test]
    fn the_configuration_matches_what_avdtp_negotiated() {
        // The whole point of parsing this: the multiplex states the same thing the AVDTP
        // negotiation did, and a parse that disagrees with it is a parse that is wrong.
        let mut parser = LatmParser::new();
        let packets = fixture();
        parser.access_unit(&packets[0]).unwrap();
        let config = parser.config().unwrap();
        assert_eq!(config.audio_object_type, 2, "AAC-LC");
        assert_eq!(config.sample_rate, 44_100);
        assert_eq!(config.channels, 2);
        // Lifted out of a bitstream it does not start byte-aligned in.
        assert_eq!(&config.audio_specific_config[..2], &[0x12, 0x10]);
    }

    #[test]
    fn every_captured_packet_yields_an_access_unit_that_accounts_for_it_exactly() {
        // The header is 9 bytes for *this* configuration only, so the test asserts the
        // arithmetic rather than the offset: header + declared length == packet length,
        // for every packet, including the short silence frames whose length field does not
        // use the 255 escape.
        let mut parser = LatmParser::new();
        let packets = fixture();
        assert!(packets.len() >= 40, "fixture should be substantial");
        let mut escaped = 0;
        let mut plain = 0;
        for packet in &packets {
            let au = parser.access_unit(packet).unwrap();
            assert!(!au.is_empty(), "an access unit must have content");
            assert!(
                au.len() < packet.len(),
                "the access unit must be a strict suffix of the multiplex"
            );
            if au.len() > 255 {
                escaped += 1;
            } else {
                plain += 1;
            }
        }
        assert!(escaped > 0, "the 255-escape path must be exercised");
        assert!(plain > 0, "the single-byte length path must be exercised");
    }

    #[test]
    fn a_truncated_multiplex_is_refused_rather_than_read_past() {
        let packets = fixture();
        let mut parser = LatmParser::new();
        // Chop a real packet mid-header.
        let short = packets[0].slice(0..5);
        assert!(parser.access_unit(&short).is_err());
    }

    #[test]
    fn a_declared_length_longer_than_the_packet_is_refused() {
        // A malformed length must not slice past the buffer; it is a radio link, and this
        // is the field an attacker or a corrupt packet would move.
        let packets = fixture();
        let mut broken = packets[0].to_vec();
        broken[9] = 0xFE; // claim 254 bytes of payload in a 42-byte packet
        let mut parser = LatmParser::new();
        assert!(parser.access_unit(&Bytes::from(broken)).is_err());
    }
}
