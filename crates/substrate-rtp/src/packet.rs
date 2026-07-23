//! RFC 3550 RTP packet parsing. The fixed header is 12 bytes; CSRC list and an
//! optional header extension follow before the payload.

use bytes::Bytes;
use thiserror::Error;

/// RTP parse errors.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtpError {
    /// The buffer is smaller than the 12-byte fixed header.
    #[error("packet shorter than RTP header")]
    TooShort,

    /// The version field wasn't 2.
    #[error("unsupported RTP version {0}")]
    BadVersion(u8),

    /// The declared CSRC/extension lengths ran past the buffer.
    #[error("truncated RTP header (csrc/extension overran)")]
    Truncated,
}

/// A parsed RTP fixed header (plus the parsed-away CSRC/extension sizes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// Marker bit — payload-specific (e.g. last packet of a frame).
    pub marker: bool,
    /// Payload type (7 bits).
    pub payload_type: u8,
    /// Sequence number.
    pub sequence: u16,
    /// Timestamp (payload clock).
    pub timestamp: u32,
    /// Synchronization source id.
    pub ssrc: u32,
    /// Number of CSRC identifiers present.
    pub csrc_count: u8,
    /// Byte offset at which the payload begins.
    pub payload_offset: usize,
}

/// A parsed RTP packet: header + payload slice (zero-copy over the input [`Bytes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    /// The parsed header.
    pub header: RtpHeader,
    /// The payload bytes (after CSRC + extension).
    pub payload: Bytes,
}

impl RtpPacket {
    /// Parse an RTP packet from a datagram.
    ///
    /// # Errors
    /// [`RtpError`] if too short, wrong version, or the header lengths overrun.
    pub fn parse(buf: Bytes) -> Result<Self, RtpError> {
        if buf.len() < 12 {
            return Err(RtpError::TooShort);
        }
        let b0 = buf[0];
        let version = b0 >> 6;
        if version != 2 {
            return Err(RtpError::BadVersion(version));
        }
        let padding = (b0 & 0b0010_0000) != 0;
        let extension = (b0 & 0b0001_0000) != 0;
        let csrc_count = b0 & 0b0000_1111;

        let b1 = buf[1];
        let marker = (b1 & 0b1000_0000) != 0;
        let payload_type = b1 & 0b0111_1111;
        let sequence = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);

        // CSRC list: 4 bytes each.
        let mut offset = 12 + (csrc_count as usize) * 4;
        if buf.len() < offset {
            return Err(RtpError::Truncated);
        }

        // Optional header extension: 4-byte header (profile + length-in-words), then words.
        if extension {
            if buf.len() < offset + 4 {
                return Err(RtpError::Truncated);
            }
            let ext_words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
            offset += 4 + ext_words * 4;
            if buf.len() < offset {
                return Err(RtpError::Truncated);
            }
        }

        // Padding: the last byte says how many padding bytes to strip from the end.
        let mut end = buf.len();
        if padding {
            let pad = *buf.last().unwrap_or(&0) as usize;
            if pad == 0 || pad > end - offset {
                return Err(RtpError::Truncated);
            }
            end -= pad;
        }

        let header = RtpHeader {
            marker,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            csrc_count,
            payload_offset: offset,
        };
        let payload = buf.slice(offset..end);
        Ok(Self { header, payload })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn base_packet() -> Vec<u8> {
        let mut v = vec![
            0x80, // v2, no pad/ext, csrc=0
            0x60, // marker=0, pt=96
            0x12, 0x34, // seq
            0x00, 0x00, 0x10, 0x00, // ts
            0xDE, 0xAD, 0xBE, 0xEF, // ssrc
        ];
        v.extend_from_slice(b"payload");
        v
    }

    #[test]
    fn parses_basic_packet() {
        let p = RtpPacket::parse(Bytes::from(base_packet())).unwrap();
        assert_eq!(p.header.payload_type, 96);
        assert_eq!(p.header.sequence, 0x1234);
        assert_eq!(p.header.timestamp, 0x1000);
        assert_eq!(p.header.ssrc, 0xDEAD_BEEF);
        assert_eq!(&p.payload[..], b"payload");
    }

    #[test]
    fn marker_and_pt_decoded() {
        let mut v = base_packet();
        v[1] = 0x80 | 33; // marker set, PT 33 (MPEG-TS)
        let p = RtpPacket::parse(Bytes::from(v)).unwrap();
        assert!(p.header.marker);
        assert_eq!(p.header.payload_type, 33);
    }

    #[test]
    fn rejects_short_and_bad_version() {
        assert_eq!(
            RtpPacket::parse(Bytes::from(vec![0u8; 4])),
            Err(RtpError::TooShort)
        );
        let mut v = base_packet();
        v[0] = 0x40; // version 1
        assert_eq!(
            RtpPacket::parse(Bytes::from(v)),
            Err(RtpError::BadVersion(1))
        );
    }

    #[test]
    fn skips_csrc_and_extension() {
        let mut v = vec![
            0x90 | 0x01, // v2, ext=1, csrc=1
            0x60,
            0x00,
            0x01,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        v.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xAA]); // 1 CSRC
        v.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // ext header: 1 word follows
        v.extend_from_slice(&[0xBB, 0xBB, 0xBB, 0xBB]); // 1 ext word
        v.extend_from_slice(b"data");
        let p = RtpPacket::parse(Bytes::from(v)).unwrap();
        assert_eq!(p.header.csrc_count, 1);
        assert_eq!(&p.payload[..], b"data");
    }

    #[test]
    fn strips_padding() {
        let mut v = base_packet();
        v[0] |= 0b0010_0000; // padding bit
        v.extend_from_slice(&[0, 0, 3]); // 3 padding bytes (last = count)
        let p = RtpPacket::parse(Bytes::from(v)).unwrap();
        assert_eq!(&p.payload[..], b"payload");
    }
}
