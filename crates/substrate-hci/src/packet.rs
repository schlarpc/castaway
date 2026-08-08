//! HCI transport framing: the four packet types and their headers.
//!
//! This is the *framing* layer only — it turns bytes into "a command with this opcode and
//! these parameter bytes" without knowing what any command means. Semantics live in
//! [`crate::command`] and [`crate::event`]. The split is what lets the transport backends
//! be dumb byte pipes (ground rule 3).

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::HciError;
use crate::opcode::OpCode;

/// The leading byte that tags a packet on a byte-stream transport (UART, and the
/// USB bulk/interrupt endpoints once demultiplexed).
///
/// USB technically identifies packet type by *endpoint* rather than by this byte, but
/// carrying it uniformly means both backends hand the same [`HciPacket`] upward and the
/// stack above cannot tell them apart (ground rule 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketType {
    /// Host → controller command.
    Command,
    /// Bidirectional ACL data — everything L2CAP rides on.
    AclData,
    /// Synchronous (voice) data. We never send it; SCO is HFP's problem, not A2DP's.
    ScoData,
    /// Controller → host event.
    Event,
}

impl PacketType {
    /// The wire indicator byte.
    #[must_use]
    pub const fn indicator(self) -> u8 {
        match self {
            Self::Command => 0x01,
            Self::AclData => 0x02,
            Self::ScoData => 0x03,
            Self::Event => 0x04,
        }
    }

    /// Parse an indicator byte.
    ///
    /// # Errors
    /// [`HciError::UnknownPacketType`] for anything outside `0x01..=0x04`.
    pub const fn from_indicator(byte: u8) -> Result<Self, HciError> {
        match byte {
            0x01 => Ok(Self::Command),
            0x02 => Ok(Self::AclData),
            0x03 => Ok(Self::ScoData),
            0x04 => Ok(Self::Event),
            other => Err(HciError::UnknownPacketType(other)),
        }
    }
}

/// A controller-assigned connection handle. Twelve bits — the upper four carry the
/// boundary/broadcast flags in the ACL header, so a handle that doesn't fit would
/// silently corrupt them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionHandle(u16);

impl ConnectionHandle {
    /// The largest representable handle.
    pub const MAX: u16 = 0x0EFF;

    /// Build a handle, rejecting anything wider than 12 bits.
    ///
    /// # Errors
    /// [`HciError::InvalidField`] if `raw` exceeds [`ConnectionHandle::MAX`].
    pub const fn new(raw: u16) -> Result<Self, HciError> {
        if raw > Self::MAX {
            return Err(HciError::InvalidField {
                field: "connection handle",
                value: raw,
            });
        }
        Ok(Self(raw))
    }

    /// The raw 12-bit value.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }
}

impl std::fmt::Display for ConnectionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#05x}", self.0)
    }
}

/// Where an ACL fragment sits in its L2CAP PDU.
///
/// This is the seam L2CAP fragmentation rides on: a PDU larger than the controller's ACL
/// buffer goes out as one [`PacketBoundary::FirstFlushable`] followed by
/// [`PacketBoundary::Continuing`] fragments, and reassembly upstream keys on exactly this
/// field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketBoundary {
    /// First fragment of a PDU, not automatically flushable.
    FirstNonFlushable,
    /// A continuation of the PDU already in progress on this handle.
    Continuing,
    /// First fragment of a PDU, automatically flushable. What a host normally sends.
    FirstFlushable,
    /// A complete PDU in one packet (LE only in practice).
    Complete,
}

impl PacketBoundary {
    const fn bits(self) -> u16 {
        match self {
            Self::FirstNonFlushable => 0b00,
            Self::Continuing => 0b01,
            Self::FirstFlushable => 0b10,
            Self::Complete => 0b11,
        }
    }

    const fn from_bits(bits: u16) -> Self {
        match bits & 0b11 {
            0b00 => Self::FirstNonFlushable,
            0b01 => Self::Continuing,
            0b10 => Self::FirstFlushable,
            _ => Self::Complete,
        }
    }

    /// Whether this fragment starts a new PDU (as opposed to continuing one).
    ///
    /// Reassembly asks only this question, and asking it here rather than at each call
    /// site is what stops `Complete` from being forgotten in a `== FirstFlushable` test.
    #[must_use]
    pub const fn starts_pdu(self) -> bool {
        matches!(
            self,
            Self::FirstNonFlushable | Self::FirstFlushable | Self::Complete
        )
    }
}

/// Broadcast flag. Point-to-point is the only one an A2DP sink ever uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Broadcast {
    /// Directed at one peer.
    PointToPoint,
    /// Active broadcast to all connected peripherals.
    ActivePeripheral,
    /// Broadcast including parked peripherals.
    ParkedPeripheral,
}

impl Broadcast {
    const fn bits(self) -> u16 {
        match self {
            Self::PointToPoint => 0b00,
            Self::ActivePeripheral => 0b01,
            Self::ParkedPeripheral => 0b10,
        }
    }

    const fn from_bits(bits: u16) -> Self {
        match bits & 0b11 {
            0b01 => Self::ActivePeripheral,
            0b10 => Self::ParkedPeripheral,
            _ => Self::PointToPoint,
        }
    }
}

/// One ACL data packet: a fragment of an L2CAP PDU on a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclPacket {
    /// Which connection this fragment belongs to.
    pub handle: ConnectionHandle,
    /// Whether it starts or continues a PDU.
    pub boundary: PacketBoundary,
    /// Broadcast flag.
    pub broadcast: Broadcast,
    /// The fragment payload.
    pub data: Bytes,
}

impl AclPacket {
    /// The largest payload the 16-bit length field can describe. The *controller's*
    /// buffer size is smaller and is what fragmentation actually respects.
    pub const MAX_DATA: usize = u16::MAX as usize;

    /// Build a point-to-point fragment.
    #[must_use]
    pub const fn new(handle: ConnectionHandle, boundary: PacketBoundary, data: Bytes) -> Self {
        Self {
            handle,
            boundary,
            broadcast: Broadcast::PointToPoint,
            data,
        }
    }
}

/// A framed HCI packet — the unit an [`crate::transport::HciTransport`] moves.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HciPacket {
    /// Host → controller command, still as raw parameter bytes.
    Command {
        /// Which command.
        opcode: OpCode,
        /// Its parameters.
        params: Bytes,
    },
    /// Controller → host event, still as raw parameter bytes.
    Event {
        /// Event code.
        code: u8,
        /// Its parameters.
        params: Bytes,
    },
    /// ACL data in either direction.
    Acl(AclPacket),
    /// Synchronous data. Parsed so it can be counted and dropped, never produced.
    Sco {
        /// Connection handle.
        handle: ConnectionHandle,
        /// Payload.
        data: Bytes,
    },
}

impl HciPacket {
    /// Which transport packet type this is.
    #[must_use]
    pub const fn packet_type(&self) -> PacketType {
        match self {
            Self::Command { .. } => PacketType::Command,
            Self::Event { .. } => PacketType::Event,
            Self::Acl(_) => PacketType::AclData,
            Self::Sco { .. } => PacketType::ScoData,
        }
    }

    /// Encode including the leading packet-type indicator.
    ///
    /// # Errors
    /// [`HciError::TooLong`] if a payload exceeds what its length field can describe.
    pub fn encode(&self) -> Result<Bytes, HciError> {
        let mut buf = BytesMut::with_capacity(64);
        buf.put_u8(self.packet_type().indicator());
        match self {
            Self::Command { opcode, params } => {
                if params.len() > u8::MAX as usize {
                    return Err(HciError::TooLong {
                        what: "command parameters",
                        len: params.len(),
                        max: u8::MAX as usize,
                    });
                }
                buf.put_u16_le(opcode.raw());
                // Command parameter length is a single byte — the one length field in
                // HCI that is not 16-bit, and an easy place to write the wrong width.
                buf.put_u8(u8::try_from(params.len()).unwrap_or(u8::MAX));
                buf.extend_from_slice(params);
            }
            Self::Event { code, params } => {
                if params.len() > u8::MAX as usize {
                    return Err(HciError::TooLong {
                        what: "event parameters",
                        len: params.len(),
                        max: u8::MAX as usize,
                    });
                }
                buf.put_u8(*code);
                buf.put_u8(u8::try_from(params.len()).unwrap_or(u8::MAX));
                buf.extend_from_slice(params);
            }
            Self::Acl(acl) => {
                if acl.data.len() > AclPacket::MAX_DATA {
                    return Err(HciError::TooLong {
                        what: "acl payload",
                        len: acl.data.len(),
                        max: AclPacket::MAX_DATA,
                    });
                }
                let header =
                    acl.handle.raw() | (acl.boundary.bits() << 12) | (acl.broadcast.bits() << 14);
                buf.put_u16_le(header);
                buf.put_u16_le(u16::try_from(acl.data.len()).unwrap_or(u16::MAX));
                buf.extend_from_slice(&acl.data);
            }
            Self::Sco { handle, data } => {
                if data.len() > u8::MAX as usize {
                    return Err(HciError::TooLong {
                        what: "sco payload",
                        len: data.len(),
                        max: u8::MAX as usize,
                    });
                }
                buf.put_u16_le(handle.raw());
                buf.put_u8(u8::try_from(data.len()).unwrap_or(u8::MAX));
                buf.extend_from_slice(data);
            }
        }
        Ok(buf.freeze())
    }

    /// Decode a packet that still carries its leading indicator byte.
    ///
    /// # Errors
    /// [`HciError::UnknownPacketType`] or [`HciError::Truncated`].
    pub fn decode(bytes: &[u8]) -> Result<Self, HciError> {
        let (&indicator, rest) = bytes.split_first().ok_or(HciError::Truncated {
            what: "hci packet",
            need: 1,
            have: 0,
        })?;
        Self::decode_body(PacketType::from_indicator(indicator)?, rest)
    }

    /// Decode a packet whose type is already known from context — the USB case, where
    /// the endpoint identifies the type and no indicator byte is present.
    ///
    /// # Errors
    /// [`HciError::Truncated`] if the body is shorter than its header claims.
    pub fn decode_body(kind: PacketType, body: &[u8]) -> Result<Self, HciError> {
        match kind {
            PacketType::Command => {
                let (head, params) = split(body, 3, "command header")?;
                let opcode = OpCode::new(u16::from_le_bytes([head[0], head[1]]));
                let len = head[2] as usize;
                Ok(Self::Command {
                    opcode,
                    params: exact(params, len, "command parameters")?,
                })
            }
            PacketType::Event => {
                let (head, params) = split(body, 2, "event header")?;
                let len = head[1] as usize;
                Ok(Self::Event {
                    code: head[0],
                    params: exact(params, len, "event parameters")?,
                })
            }
            PacketType::AclData => {
                let (head, data) = split(body, 4, "acl header")?;
                let raw = u16::from_le_bytes([head[0], head[1]]);
                let len = u16::from_le_bytes([head[2], head[3]]) as usize;
                Ok(Self::Acl(AclPacket {
                    handle: ConnectionHandle::new(raw & 0x0FFF)?,
                    boundary: PacketBoundary::from_bits(raw >> 12),
                    broadcast: Broadcast::from_bits(raw >> 14),
                    data: exact(data, len, "acl payload")?,
                }))
            }
            PacketType::ScoData => {
                let (head, data) = split(body, 3, "sco header")?;
                let raw = u16::from_le_bytes([head[0], head[1]]);
                let len = head[2] as usize;
                Ok(Self::Sco {
                    handle: ConnectionHandle::new(raw & 0x0FFF)?,
                    data: exact(data, len, "sco payload")?,
                })
            }
        }
    }
}

fn split<'a>(
    buf: &'a [u8],
    n: usize,
    what: &'static str,
) -> Result<(&'a [u8], &'a [u8]), HciError> {
    if buf.len() < n {
        return Err(HciError::Truncated {
            what,
            need: n,
            have: buf.len(),
        });
    }
    Ok(buf.split_at(n))
}

/// Take exactly `len` bytes, refusing a body that is shorter than its header promised.
///
/// A body *longer* than declared is tolerated and truncated: USB transfers are padded to
/// the endpoint's packet size, so trailing bytes are routine rather than corruption.
fn exact(buf: &[u8], len: usize, what: &'static str) -> Result<Bytes, HciError> {
    if buf.len() < len {
        return Err(HciError::Truncated {
            what,
            need: len,
            have: buf.len(),
        });
    }
    Ok(Bytes::copy_from_slice(&buf[..len]))
}

/// Reassembles HCI packets out of an H4 byte *stream*.
///
/// The raw-HCI socket and USB backends get one packet per read, so [`HciPacket::decode`]
/// alone serves them. A stream transport — UART, or TCP to a rootcanal/netsim virtual
/// controller — has no such boundary: one read may hold half a header, or three packets
/// and the first two bytes of a fourth. This holds the tail between reads and yields
/// packets exactly when their declared length is present.
///
/// Pure by design (ground rule 3): bytes in, packets out, no socket anywhere near it —
/// which is what lets the dribble tests below run every split point of every header.
#[derive(Debug, Default)]
pub struct StreamDeframer {
    buf: BytesMut,
}

impl StreamDeframer {
    /// A deframer with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes exactly as they arrived from the stream.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Take the next complete packet, or `None` while the buffer holds only part of one.
    ///
    /// Call in a loop after each [`extend`](Self::extend): one read can complete several
    /// packets.
    ///
    /// # Errors
    /// [`HciError::UnknownPacketType`] if the byte where an indicator should be is not
    /// one. On a stream there is no way to resynchronise past that — every later byte is
    /// at an unknown offset — so the only sound response is to drop the connection.
    pub fn next_packet(&mut self) -> Result<Option<HciPacket>, HciError> {
        let Some(&indicator) = self.buf.first() else {
            return Ok(None);
        };
        let kind = PacketType::from_indicator(indicator)?;
        // Header size after the indicator, and where its length field lives — the one
        // place these four shapes are written down for streaming; `decode_body` re-reads
        // them from the complete frame below.
        let header = match kind {
            PacketType::Event => 2,
            PacketType::Command | PacketType::ScoData => 3,
            PacketType::AclData => 4,
        };
        let Some(head) = self.buf.get(1..1 + header) else {
            return Ok(None);
        };
        let payload = match kind {
            PacketType::Command | PacketType::ScoData => head[2] as usize,
            PacketType::Event => head[1] as usize,
            // The one 16-bit length: an ACL frame above 255 bytes is routine, and
            // reading one byte of it would desynchronise the stream for good.
            PacketType::AclData => u16::from_le_bytes([head[2], head[3]]) as usize,
        };
        let total = 1 + header + payload;
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame = self.buf.split_to(total);
        HciPacket::decode(&frame).map(Some)
    }

    /// Bytes held waiting for the rest of a packet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    #[test]
    fn a_reset_command_encodes_to_the_known_bytes() {
        // HCI_Reset (OGF 0x03, OCF 0x003 → opcode 0x0C03), no parameters. This exact
        // byte string is the first thing every controller ever sees from us.
        let pkt = HciPacket::Command {
            opcode: OpCode::new(0x0C03),
            params: Bytes::new(),
        };
        assert_eq!(&pkt.encode().unwrap()[..], &hex!("01 03 0c 00"));
        assert_eq!(HciPacket::decode(&hex!("01 03 0c 00")).unwrap(), pkt);
    }

    #[test]
    fn acl_header_packs_handle_and_flags_without_colliding() {
        // The handle occupies the low 12 bits and the flags the top 4; a handle that
        // bled into the flag bits would turn a first-fragment into a continuation and
        // corrupt every reassembly on that connection.
        let acl = AclPacket::new(
            ConnectionHandle::new(0x0EFF).unwrap(),
            PacketBoundary::FirstFlushable,
            Bytes::from_static(&[0xde, 0xad]),
        );
        let encoded = HciPacket::Acl(acl.clone()).encode().unwrap();
        assert_eq!(&encoded[..], &hex!("02 ff 2e 02 00 de ad"));
        let HciPacket::Acl(back) = HciPacket::decode(&encoded).unwrap() else {
            panic!("expected ACL");
        };
        assert_eq!(back, acl);
        assert_eq!(back.handle.raw(), 0x0EFF);
        assert_eq!(back.boundary, PacketBoundary::FirstFlushable);
    }

    #[test]
    fn a_continuation_fragment_is_distinguishable_from_a_first() {
        let first = HciPacket::Acl(AclPacket::new(
            ConnectionHandle::new(1).unwrap(),
            PacketBoundary::FirstFlushable,
            Bytes::from_static(&[1]),
        ));
        let cont = HciPacket::Acl(AclPacket::new(
            ConnectionHandle::new(1).unwrap(),
            PacketBoundary::Continuing,
            Bytes::from_static(&[2]),
        ));
        assert_ne!(first.encode().unwrap(), cont.encode().unwrap());
        assert!(PacketBoundary::FirstFlushable.starts_pdu());
        assert!(PacketBoundary::Complete.starts_pdu());
        assert!(!PacketBoundary::Continuing.starts_pdu());
    }

    #[test]
    fn handles_wider_than_twelve_bits_are_refused() {
        assert!(ConnectionHandle::new(0x0F00).is_err());
        assert!(ConnectionHandle::new(ConnectionHandle::MAX).is_ok());
    }

    #[test]
    fn a_truncated_body_is_an_error_but_a_padded_one_is_not() {
        // USB IN transfers arrive padded to the endpoint packet size, so trailing bytes
        // past the declared length are normal and must not fail the parse. Missing
        // bytes are a genuinely broken packet.
        let short = hex!("04 0e 04 01 03 0c");
        assert!(matches!(
            HciPacket::decode(&short),
            Err(HciError::Truncated { .. })
        ));
        let padded = hex!("04 0e 04 01 03 0c 00 00 00 00");
        let HciPacket::Event { code, params } = HciPacket::decode(&padded).unwrap() else {
            panic!("expected event");
        };
        assert_eq!(code, 0x0e);
        assert_eq!(&params[..], &hex!("01 03 0c 00"));
    }

    #[test]
    fn decode_body_matches_decode_without_the_indicator() {
        // The USB backend has no indicator byte — the endpoint tells it the type — so
        // both entry points have to agree.
        let with = HciPacket::decode(&hex!("04 0e 04 01 03 0c 00")).unwrap();
        let without =
            HciPacket::decode_body(PacketType::Event, &hex!("0e 04 01 03 0c 00")).unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn unknown_indicators_are_rejected() {
        assert!(matches!(
            HciPacket::decode(&[0x09, 0, 0]),
            Err(HciError::UnknownPacketType(0x09))
        ));
    }

    /// Every packet shape, delivered one byte at a time: the packet must appear on the
    /// final byte and not one earlier. This walks every split point of every header,
    /// which is the entire failure surface of stream framing.
    #[test]
    fn a_dribbled_stream_yields_each_packet_exactly_on_its_last_byte() {
        let packets = [
            HciPacket::Command {
                opcode: OpCode::new(0x0C03),
                params: Bytes::new(),
            },
            HciPacket::Event {
                code: 0x0e,
                params: Bytes::from_static(&hex!("01 03 0c 00")),
            },
            HciPacket::Acl(AclPacket::new(
                ConnectionHandle::new(0x0b).unwrap(),
                PacketBoundary::FirstFlushable,
                Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            )),
            HciPacket::Sco {
                handle: ConnectionHandle::new(0x0c).unwrap(),
                data: Bytes::from_static(&[0x55]),
            },
        ];
        for pkt in packets {
            let wire = pkt.encode().unwrap();
            let mut deframer = StreamDeframer::new();
            for (i, byte) in wire.iter().enumerate() {
                deframer.extend(&[*byte]);
                let got = deframer.next_packet().unwrap();
                if i + 1 < wire.len() {
                    assert!(
                        got.is_none(),
                        "{pkt:?} appeared {} bytes early",
                        wire.len() - i - 1
                    );
                } else {
                    assert_eq!(got, Some(pkt.clone()));
                }
            }
            assert_eq!(deframer.pending(), 0);
        }
    }

    #[test]
    fn one_read_holding_several_packets_yields_them_all_in_order() {
        let a = HciPacket::Event {
            code: 0x13,
            params: Bytes::from_static(&hex!("01 0b 00 01 00")),
        };
        let b = HciPacket::Acl(AclPacket::new(
            ConnectionHandle::new(0x0b).unwrap(),
            PacketBoundary::Continuing,
            Bytes::from_static(&[1, 2, 3]),
        ));
        let c = HciPacket::Command {
            opcode: OpCode::new(0x0C03),
            params: Bytes::new(),
        };
        let mut wire = Vec::new();
        for pkt in [&a, &b, &c] {
            wire.extend_from_slice(&pkt.encode().unwrap());
        }
        // Plus half of a fourth, which must stay pending rather than confuse anything.
        wire.extend_from_slice(&[0x02, 0x0b]);

        let mut deframer = StreamDeframer::new();
        deframer.extend(&wire);
        assert_eq!(deframer.next_packet().unwrap(), Some(a));
        assert_eq!(deframer.next_packet().unwrap(), Some(b));
        assert_eq!(deframer.next_packet().unwrap(), Some(c));
        assert_eq!(deframer.next_packet().unwrap(), None);
        assert_eq!(deframer.pending(), 2);
    }

    /// An ACL frame longer than 255 bytes exercises the 16-bit length field — the header
    /// byte a one-byte read of the length would truncate, silently, on every A2DP media
    /// packet (they are routinely ~600 bytes).
    #[test]
    fn an_acl_frame_larger_than_a_byte_can_describe_frames_correctly() {
        let payload = vec![0xAB; 700];
        let pkt = HciPacket::Acl(AclPacket::new(
            ConnectionHandle::new(0x0b).unwrap(),
            PacketBoundary::FirstFlushable,
            Bytes::from(payload),
        ));
        let wire = pkt.encode().unwrap();

        let mut deframer = StreamDeframer::new();
        // In two arbitrary chunks, split inside the payload.
        deframer.extend(&wire[..300]);
        assert_eq!(deframer.next_packet().unwrap(), None);
        deframer.extend(&wire[300..]);
        assert_eq!(deframer.next_packet().unwrap(), Some(pkt));
    }

    #[test]
    fn a_corrupt_indicator_is_fatal_to_the_stream() {
        // There is no resynchronising an H4 stream: report it, so the transport closes.
        let mut deframer = StreamDeframer::new();
        deframer.extend(&[0x09, 0x00]);
        assert!(matches!(
            deframer.next_packet(),
            Err(HciError::UnknownPacketType(0x09))
        ));
    }
}
