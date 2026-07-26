//! AVCTP: the thin transport AVRCP rides on, and the AV/C frame inside it.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::AudioError;

/// The AVRCP profile identifier that tags an AVCTP message.
pub const PID_AVRCP: u16 = 0x110E;

/// Whether an AVCTP message is a command or a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandResponse {
    /// A command.
    Command,
    /// A response.
    Response,
}

/// One AVCTP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvctpMessage {
    /// Correlates a response with its command. Four bits.
    pub transaction: u8,
    /// Direction.
    pub cr: CommandResponse,
    /// Profile identifier — [`PID_AVRCP`] for everything here.
    pub pid: u16,
    /// The AV/C frame.
    pub body: Bytes,
}

impl AvctpMessage {
    /// Build a command.
    #[must_use]
    pub fn command(transaction: u8, body: Bytes) -> Self {
        Self {
            transaction,
            cr: CommandResponse::Command,
            pid: PID_AVRCP,
            body,
        }
    }

    /// Build a response to `command`.
    #[must_use]
    pub fn response(command: &Self, body: Bytes) -> Self {
        Self {
            transaction: command.transaction,
            cr: CommandResponse::Response,
            pid: command.pid,
            body,
        }
    }

    /// Encode as a single-packet message.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(3 + self.body.len());
        // transaction (4) | packet type = single (2) | c/r (1) | ipid (1)
        let cr = u8::from(self.cr == CommandResponse::Response);
        buf.put_u8((self.transaction << 4) | (cr << 1));
        buf.put_u16(self.pid);
        buf.extend_from_slice(&self.body);
        buf.freeze()
    }

    /// Decode a single-packet message.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if shorter than the header, or
    /// [`AudioError::BadMediaPacket`] for a fragmented message or one whose "invalid
    /// profile id" bit is set.
    pub fn decode(buf: &[u8]) -> Result<Self, AudioError> {
        if buf.len() < 3 {
            return Err(AudioError::Truncated {
                what: "avctp header",
                need: 3,
                have: buf.len(),
            });
        }
        if (buf[0] >> 2) & 0b11 != 0 {
            return Err(AudioError::BadMediaPacket(
                "fragmented AVCTP is not supported",
            ));
        }
        if buf[0] & 0x01 != 0 {
            // The peer is telling us it does not recognise the profile id we sent. A
            // silent parse would make this look like an ordinary empty response.
            return Err(AudioError::BadMediaPacket("peer rejected the profile id"));
        }
        Ok(Self {
            transaction: buf[0] >> 4,
            cr: if buf[0] & 0b10 == 0 {
                CommandResponse::Command
            } else {
                CommandResponse::Response
            },
            pid: u16::from_be_bytes([buf[1], buf[2]]),
            body: Bytes::copy_from_slice(&buf[3..]),
        })
    }
}

/// AV/C command types and response codes. They share the same four-bit field, which is
/// why one enum covers both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ctype {
    /// Command: do something.
    Control,
    /// Command: tell me something.
    Status,
    /// Command: tell me when something changes.
    Notify,
    /// Response: not implemented.
    NotImplemented,
    /// Response: done.
    Accepted,
    /// Response: refused.
    Rejected,
    /// Response: here is the answer, and it won't change on its own.
    Stable,
    /// Response: registered; a `Changed` will follow when it does.
    Interim,
    /// Response: the thing you registered for has changed.
    Changed,
    /// Anything else.
    Other(u8),
}

impl Ctype {
    /// The four-bit wire value.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Self::Control => 0x0,
            Self::Status => 0x1,
            Self::Notify => 0x3,
            Self::NotImplemented => 0x8,
            Self::Accepted => 0x9,
            Self::Rejected => 0xA,
            Self::Stable => 0xC,
            Self::Changed => 0xD,
            Self::Interim => 0xF,
            Self::Other(raw) => raw & 0x0F,
        }
    }

    /// Parse the four-bit wire value.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits & 0x0F {
            0x0 => Self::Control,
            0x1 => Self::Status,
            0x3 => Self::Notify,
            0x8 => Self::NotImplemented,
            0x9 => Self::Accepted,
            0xA => Self::Rejected,
            0xC => Self::Stable,
            0xD => Self::Changed,
            0xF => Self::Interim,
            other => Self::Other(other),
        }
    }

    /// Whether this response means the command failed.
    ///
    /// [`Ctype::NotImplemented`] counts: a peer that does not implement a verb is a
    /// failure for the caller even though it is not an error for the peer, and treating
    /// it as success is how a UI ends up showing a button that does nothing.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Rejected | Self::NotImplemented)
    }

    /// Whether this frame is a response rather than a command.
    ///
    /// AV/C splits the four-bit field down the middle — `0x0..=0x7` are command types,
    /// `0x8..=0xF` are response codes — which makes this the reliable discriminator even
    /// when a peer is careless with AVCTP's own command/response bit. It matters because
    /// the two directions share a PDU id: a head unit's `GetElementAttributes` *command*
    /// read as a response parses its eight-byte track identifier as an attribute count of
    /// zero, and quietly empties the now-playing card.
    #[must_use]
    pub const fn is_response(self) -> bool {
        self.bits() >= 0x8
    }
}

/// AV/C opcodes.
pub mod opcode {
    /// Vendor-dependent — everything AVRCP metadata rides on.
    pub const VENDOR_DEPENDENT: u8 = 0x00;
    /// Unit info.
    pub const UNIT_INFO: u8 = 0x30;
    /// Subunit info.
    pub const SUBUNIT_INFO: u8 = 0x31;
    /// Pass through — the transport keys.
    pub const PASS_THROUGH: u8 = 0x7C;
}

/// The PANEL subunit, which is what AVRCP addresses.
const SUBUNIT_PANEL: u8 = 0x09;

/// One AV/C frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcFrame {
    /// Command type or response code.
    pub ctype: Ctype,
    /// Opcode.
    pub opcode: u8,
    /// Opcode-specific operands.
    pub operands: Bytes,
}

impl AvcFrame {
    /// Build a frame addressed to the PANEL subunit.
    #[must_use]
    pub fn panel(ctype: Ctype, opcode: u8, operands: Bytes) -> Self {
        Self {
            ctype,
            opcode,
            operands,
        }
    }

    /// Encode.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(3 + self.operands.len());
        buf.put_u8(self.ctype.bits());
        // subunit type (5 bits) | subunit id (3 bits). PANEL/0 for all of AVRCP.
        buf.put_u8(SUBUNIT_PANEL << 3);
        buf.put_u8(self.opcode);
        buf.extend_from_slice(&self.operands);
        buf.freeze()
    }

    /// Decode.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if shorter than the three-byte header.
    pub fn decode(buf: &[u8]) -> Result<Self, AudioError> {
        if buf.len() < 3 {
            return Err(AudioError::Truncated {
                what: "av/c frame",
                need: 3,
                have: buf.len(),
            });
        }
        Ok(Self {
            ctype: Ctype::from_bits(buf[0]),
            opcode: buf[2],
            operands: Bytes::copy_from_slice(&buf[3..]),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    #[test]
    fn an_avctp_command_round_trips_with_the_avrcp_profile_id() {
        let msg = AvctpMessage::command(5, Bytes::from_static(&[0x00, 0x48, 0x7c]));
        assert_eq!(&msg.encode()[..3], &hex!("50 11 0e"));
        assert_eq!(AvctpMessage::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn a_response_carries_the_commands_transaction_and_sets_the_cr_bit() {
        let cmd = AvctpMessage::command(9, Bytes::from_static(&[1]));
        let rsp = AvctpMessage::response(&cmd, Bytes::from_static(&[2]));
        let back = AvctpMessage::decode(&rsp.encode()).unwrap();
        assert_eq!(back.transaction, 9);
        assert_eq!(back.cr, CommandResponse::Response);
    }

    #[test]
    fn a_profile_id_rejection_is_surfaced_rather_than_read_as_an_empty_response() {
        // The IPID bit means "I don't know that profile". Parsing past it produces a
        // response with no body, which is indistinguishable from a legitimate one.
        let bad = hex!("51 11 0e");
        assert!(matches!(
            AvctpMessage::decode(&bad),
            Err(AudioError::BadMediaPacket(_))
        ));
    }

    #[test]
    fn an_avc_frame_addresses_the_panel_subunit() {
        let frame = AvcFrame::panel(
            Ctype::Control,
            opcode::PASS_THROUGH,
            Bytes::from_static(&[0x44, 0x00]),
        );
        assert_eq!(&frame.encode()[..], &hex!("00 48 7c 44 00"));
        assert_eq!(AvcFrame::decode(&frame.encode()).unwrap(), frame);
    }

    #[test]
    fn not_implemented_counts_as_a_failure() {
        // A peer that doesn't implement a verb has not succeeded, even though it hasn't
        // errored either. Treating it as success leaves a dead button in the UI.
        assert!(Ctype::NotImplemented.is_failure());
        assert!(Ctype::Rejected.is_failure());
        assert!(!Ctype::Accepted.is_failure());
        assert!(!Ctype::Interim.is_failure());
    }

    #[test]
    fn ctypes_round_trip_including_ones_we_do_not_name() {
        for raw in 0u8..16 {
            assert_eq!(Ctype::from_bits(raw).bits(), raw);
        }
    }

    #[test]
    fn the_top_bit_of_the_ctype_separates_commands_from_responses() {
        // The two directions share a PDU id, so this is what tells them apart. Reading a
        // GetElementAttributes *command* as a response parses its eight-byte track
        // identifier as an attribute count of zero and empties the card.
        for command in [Ctype::Control, Ctype::Status, Ctype::Notify] {
            assert!(!command.is_response(), "{command:?}");
        }
        for response in [
            Ctype::NotImplemented,
            Ctype::Accepted,
            Ctype::Rejected,
            Ctype::Stable,
            Ctype::Changed,
            Ctype::Interim,
        ] {
            assert!(response.is_response(), "{response:?}");
        }
    }
}
