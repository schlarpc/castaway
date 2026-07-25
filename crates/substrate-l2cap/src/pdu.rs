//! L2CAP PDU framing, channel identifiers, and protocol/service multiplexers.

use std::fmt;

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::L2capError;

/// A channel identifier: which logical channel a PDU belongs to on one ACL link.
///
/// CIDs are *local* — each side allocates its own for the same channel, and the
/// connection handshake exchanges them. Mixing up "their CID" and "our CID" is the
/// classic L2CAP bug, so [`crate::channel::Channel`] names both explicitly rather than
/// keeping one `cid` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cid(u16);

impl Cid {
    /// The null identifier, which is never a valid destination.
    pub const NULL: Self = Self(0x0000);
    /// The signaling channel every BR/EDR link has from the moment it comes up.
    pub const SIGNALING: Self = Self(0x0001);
    /// Connectionless reception.
    pub const CONNECTIONLESS: Self = Self(0x0002);
    /// First CID available for dynamically allocated channels.
    pub const DYNAMIC_START: u16 = 0x0040;

    /// Wrap a raw CID.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The raw value.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Whether this is a dynamically allocated channel (as opposed to a fixed one).
    #[must_use]
    pub const fn is_dynamic(self) -> bool {
        self.0 >= Self::DYNAMIC_START
    }
}

impl fmt::Display for Cid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#06x}", self.0)
    }
}

/// A Protocol/Service Multiplexer: which service a connection request is asking for.
///
/// PSMs are constrained by the spec — odd-valued, with the least significant bit of the
/// most significant byte clear — and a malformed one is rejected here rather than sent to
/// a peer that will reject it less informatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Psm(u16);

impl Psm {
    /// Service Discovery Protocol.
    pub const SDP: Self = Self(0x0001);
    /// RFCOMM.
    pub const RFCOMM: Self = Self(0x0003);
    /// AVCTP — AVRCP's control transport.
    pub const AVCTP: Self = Self(0x0017);
    /// AVDTP — A2DP's signaling and media transport.
    pub const AVDTP: Self = Self(0x0019);
    /// AVCTP browsing channel (AVRCP 1.4+ media browsing).
    pub const AVCTP_BROWSING: Self = Self(0x001B);

    /// Validate and wrap a PSM.
    ///
    /// # Errors
    /// [`L2capError::InvalidPsm`] if the value breaks the odd/bit-8 rule.
    pub const fn new(raw: u16) -> Result<Self, L2capError> {
        if raw & 0x0001 == 0 || raw & 0x0100 != 0 {
            return Err(L2capError::InvalidPsm(raw));
        }
        Ok(Self(raw))
    }

    /// The raw value.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// A short name for logging, when we know one.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::SDP => "sdp",
            Self::RFCOMM => "rfcomm",
            Self::AVCTP => "avctp",
            Self::AVDTP => "avdtp",
            Self::AVCTP_BROWSING => "avctp-browsing",
            _ => return None,
        })
    }
}

impl fmt::Display for Psm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name}({:#06x})", self.0),
            None => write!(f, "psm {:#06x}", self.0),
        }
    }
}

/// A basic-mode L2CAP PDU: a length-prefixed payload on a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L2capPdu {
    /// Destination channel.
    pub cid: Cid,
    /// Payload bytes.
    pub payload: Bytes,
}

impl L2capPdu {
    /// Bytes of header ahead of the payload: a 2-byte length and a 2-byte CID.
    pub const HEADER_LEN: usize = 4;

    /// Build a PDU.
    #[must_use]
    pub const fn new(cid: Cid, payload: Bytes) -> Self {
        Self { cid, payload }
    }

    /// Encode header + payload.
    ///
    /// # Errors
    /// [`L2capError::TooLong`] if the payload exceeds the 16-bit length field.
    pub fn encode(&self) -> Result<Bytes, L2capError> {
        let len = u16::try_from(self.payload.len()).map_err(|_| L2capError::TooLong {
            len: self.payload.len(),
            max: u16::MAX as usize,
        })?;
        let mut buf = BytesMut::with_capacity(Self::HEADER_LEN + self.payload.len());
        // The length counts the payload only — not itself, and not the CID. Counting the
        // header here is the bug that makes every PDU four bytes too long and desyncs
        // the peer's reassembly.
        buf.put_u16_le(len);
        buf.put_u16_le(self.cid.raw());
        buf.extend_from_slice(&self.payload);
        Ok(buf.freeze())
    }

    /// Decode a complete PDU (as produced by HCI reassembly).
    ///
    /// # Errors
    /// [`L2capError::Truncated`] if shorter than the header or than the declared length.
    pub fn decode(bytes: &[u8]) -> Result<Self, L2capError> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(L2capError::Truncated {
                what: "l2cap header",
                need: Self::HEADER_LEN,
                have: bytes.len(),
            });
        }
        let len = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        let cid = Cid::new(u16::from_le_bytes([bytes[2], bytes[3]]));
        let body = &bytes[Self::HEADER_LEN..];
        if body.len() < len {
            return Err(L2capError::Truncated {
                what: "l2cap payload",
                need: len,
                have: body.len(),
            });
        }
        Ok(Self {
            cid,
            payload: Bytes::copy_from_slice(&body[..len]),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    #[test]
    fn the_length_field_counts_payload_only() {
        // Four bytes of payload on CID 0x0040 → length 0x0004, not 0x0008.
        let pdu = L2capPdu::new(Cid::new(0x0040), Bytes::from_static(&[1, 2, 3, 4]));
        assert_eq!(&pdu.encode().unwrap()[..], &hex!("04 00 40 00 01 02 03 04"));
        assert_eq!(L2capPdu::decode(&pdu.encode().unwrap()).unwrap(), pdu);
    }

    #[test]
    fn trailing_bytes_past_the_declared_length_are_ignored() {
        // Reassembly hands us whole ACL fragments, which can carry padding.
        let pdu = L2capPdu::decode(&hex!("02 00 40 00 aa bb cc dd")).unwrap();
        assert_eq!(&pdu.payload[..], &[0xaa, 0xbb]);
    }

    #[test]
    fn a_short_payload_is_refused() {
        assert!(matches!(
            L2capPdu::decode(&hex!("08 00 40 00 aa bb")),
            Err(L2capError::Truncated { .. })
        ));
        assert!(matches!(
            L2capPdu::decode(&hex!("08 00")),
            Err(L2capError::Truncated { .. })
        ));
    }

    #[test]
    fn psms_must_be_odd_with_bit_eight_clear() {
        // The spec's rule. Even PSMs and ones with 0x0100 set are rejected here rather
        // than by a peer that answers with an unhelpful "PSM not supported".
        assert!(Psm::new(0x0019).is_ok());
        assert!(Psm::new(0x0018).is_err(), "even PSMs are invalid");
        assert!(
            Psm::new(0x0101).is_err(),
            "bit 8 of the high byte must be 0"
        );
        assert_eq!(Psm::AVDTP.to_string(), "avdtp(0x0019)");
        assert_eq!(Psm::AVCTP.raw(), 0x0017);
    }

    #[test]
    fn dynamic_cids_start_at_0x40() {
        assert!(!Cid::SIGNALING.is_dynamic());
        assert!(!Cid::new(0x003f).is_dynamic());
        assert!(Cid::new(Cid::DYNAMIC_START).is_dynamic());
    }
}
