//! Command opcodes: the OGF/OCF split, and the BR/EDR subset an A2DP sink needs.

use std::fmt;

/// A 16-bit command opcode: a 6-bit opcode group field and a 10-bit opcode command field.
///
/// Kept as a newtype rather than an enum because the host must be able to *log* an opcode
/// echoed back in a Command Complete for a command it never sent — which happens after a
/// controller reset races an in-flight command, and which an enum would turn into a parse
/// failure instead of a diagnosable one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpCode(u16);

/// Opcode group field — which command family an opcode belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ogf {
    /// Link control: connect, disconnect, page, pairing replies.
    LinkControl,
    /// Link policy: sniff/hold modes.
    LinkPolicy,
    /// Controller & baseband: reset, scan enable, name, class of device.
    ControllerBaseband,
    /// Informational: version, buffer size, BD_ADDR.
    Informational,
    /// Status parameters.
    Status,
    /// Testing.
    Testing,
    /// LE controller commands. Present for completeness; A2DP is BR/EDR only.
    LeController,
    /// Vendor-specific — where firmware upload lives (Realtek `0xFC20`/`0xFC6D`).
    Vendor,
    /// Anything else.
    Other(u8),
}

impl Ogf {
    const fn bits(self) -> u16 {
        (match self {
            Self::LinkControl => 0x01,
            Self::LinkPolicy => 0x02,
            Self::ControllerBaseband => 0x03,
            Self::Informational => 0x04,
            Self::Status => 0x05,
            Self::Testing => 0x06,
            Self::LeController => 0x08,
            Self::Vendor => 0x3F,
            Self::Other(raw) => raw,
        }) as u16
    }

    const fn from_bits(bits: u8) -> Self {
        match bits {
            0x01 => Self::LinkControl,
            0x02 => Self::LinkPolicy,
            0x03 => Self::ControllerBaseband,
            0x04 => Self::Informational,
            0x05 => Self::Status,
            0x06 => Self::Testing,
            0x08 => Self::LeController,
            0x3F => Self::Vendor,
            other => Self::Other(other),
        }
    }
}

impl OpCode {
    /// Wrap a raw opcode.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// Build from an opcode group and command field.
    #[must_use]
    pub const fn from_parts(ogf: Ogf, ocf: u16) -> Self {
        Self((ogf.bits() << 10) | (ocf & 0x03FF))
    }

    /// The raw 16-bit value as it appears on the wire.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// The opcode group field.
    #[must_use]
    pub const fn ogf(self) -> Ogf {
        #[allow(clippy::cast_possible_truncation)]
        Ogf::from_bits((self.0 >> 10) as u8)
    }

    /// The opcode command field.
    #[must_use]
    pub const fn ocf(self) -> u16 {
        self.0 & 0x03FF
    }

    // --- Link control ---
    /// Terminate an existing connection.
    pub const DISCONNECT: Self = Self(0x0406);
    /// Accept an incoming connection request.
    pub const ACCEPT_CONNECTION_REQUEST: Self = Self(0x0409);
    /// Reject an incoming connection request.
    pub const REJECT_CONNECTION_REQUEST: Self = Self(0x040A);
    /// Supply a stored link key for a reconnecting peer.
    pub const LINK_KEY_REQUEST_REPLY: Self = Self(0x040B);
    /// Tell the controller we have no stored key, forcing fresh pairing.
    pub const LINK_KEY_REQUEST_NEGATIVE_REPLY: Self = Self(0x040C);
    /// Refuse legacy PIN pairing.
    pub const PIN_CODE_REQUEST_NEGATIVE_REPLY: Self = Self(0x040E);
    /// Request authentication on a link.
    pub const AUTHENTICATION_REQUESTED: Self = Self(0x0411);
    /// Turn on link-layer encryption.
    pub const SET_CONNECTION_ENCRYPTION: Self = Self(0x0413);
    /// Ask the peer for its friendly name.
    pub const REMOTE_NAME_REQUEST: Self = Self(0x0419);
    /// Answer the controller's IO-capability request (Secure Simple Pairing).
    pub const IO_CAPABILITY_REQUEST_REPLY: Self = Self(0x042B);
    /// Confirm an SSP numeric comparison — the Just Works accept.
    pub const USER_CONFIRMATION_REQUEST_REPLY: Self = Self(0x042C);
    /// Refuse an SSP numeric comparison.
    pub const USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY: Self = Self(0x042D);

    // --- Controller & baseband ---
    /// Reset the controller to a known state.
    pub const RESET: Self = Self(0x0C03);
    /// Choose which events the controller may emit.
    pub const SET_EVENT_MASK: Self = Self(0x0C01);
    /// Set the name senders see.
    pub const WRITE_LOCAL_NAME: Self = Self(0x0C13);
    /// Set inquiry-scan and page-scan enable — discoverability and connectability.
    pub const WRITE_SCAN_ENABLE: Self = Self(0x0C1A);
    /// How often and for how long the controller listens for inquiries.
    pub const WRITE_INQUIRY_SCAN_ACTIVITY: Self = Self(0x0C1E);
    /// Standard or interlaced inquiry scan.
    pub const WRITE_INQUIRY_SCAN_TYPE: Self = Self(0x0C43);
    /// Standard or interlaced page scan.
    pub const WRITE_PAGE_SCAN_TYPE: Self = Self(0x0C47);
    /// Set the class of device (we advertise audio rendering).
    pub const WRITE_CLASS_OF_DEVICE: Self = Self(0x0C24);
    /// Set the extended inquiry response (name + service UUIDs in the scan result).
    pub const WRITE_EXTENDED_INQUIRY_RESPONSE: Self = Self(0x0C52);
    /// Enable Secure Simple Pairing. Without this the controller falls back to PIN.
    pub const WRITE_SIMPLE_PAIRING_MODE: Self = Self(0x0C56);
    /// Advertise host support for Secure Connections.
    pub const WRITE_SECURE_CONNECTIONS_HOST_SUPPORT: Self = Self(0x0C7A);

    // --- Informational ---
    /// Read HCI/LMP version.
    pub const READ_LOCAL_VERSION: Self = Self(0x1001);
    /// Read the controller's ACL buffer size and count — the input to flow control.
    pub const READ_BUFFER_SIZE: Self = Self(0x1005);
    /// Read the controller's own address.
    pub const READ_BD_ADDR: Self = Self(0x1009);

    /// A short name for logging, when we know one.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match self {
            Self::DISCONNECT => "disconnect",
            Self::ACCEPT_CONNECTION_REQUEST => "accept_connection_request",
            Self::REJECT_CONNECTION_REQUEST => "reject_connection_request",
            Self::LINK_KEY_REQUEST_REPLY => "link_key_request_reply",
            Self::LINK_KEY_REQUEST_NEGATIVE_REPLY => "link_key_request_negative_reply",
            Self::PIN_CODE_REQUEST_NEGATIVE_REPLY => "pin_code_request_negative_reply",
            Self::AUTHENTICATION_REQUESTED => "authentication_requested",
            Self::SET_CONNECTION_ENCRYPTION => "set_connection_encryption",
            Self::REMOTE_NAME_REQUEST => "remote_name_request",
            Self::IO_CAPABILITY_REQUEST_REPLY => "io_capability_request_reply",
            Self::USER_CONFIRMATION_REQUEST_REPLY => "user_confirmation_request_reply",
            Self::USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY => {
                "user_confirmation_request_negative_reply"
            }
            Self::RESET => "reset",
            Self::SET_EVENT_MASK => "set_event_mask",
            Self::WRITE_LOCAL_NAME => "write_local_name",
            Self::WRITE_SCAN_ENABLE => "write_scan_enable",
            Self::WRITE_INQUIRY_SCAN_ACTIVITY => "write_inquiry_scan_activity",
            Self::WRITE_INQUIRY_SCAN_TYPE => "write_inquiry_scan_type",
            Self::WRITE_PAGE_SCAN_TYPE => "write_page_scan_type",
            Self::WRITE_CLASS_OF_DEVICE => "write_class_of_device",
            Self::WRITE_EXTENDED_INQUIRY_RESPONSE => "write_extended_inquiry_response",
            Self::WRITE_SIMPLE_PAIRING_MODE => "write_simple_pairing_mode",
            Self::WRITE_SECURE_CONNECTIONS_HOST_SUPPORT => "write_secure_connections_host_support",
            Self::READ_LOCAL_VERSION => "read_local_version",
            Self::READ_BUFFER_SIZE => "read_buffer_size",
            Self::READ_BD_ADDR => "read_bd_addr",
            _ => return None,
        })
    }
}

impl fmt::Display for OpCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name}({:#06x})", self.0),
            None => write!(f, "opcode {:#06x}", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ogf_and_ocf_round_trip_through_the_packed_form() {
        // HCI_Reset is OGF 0x03 (controller & baseband), OCF 0x003.
        assert_eq!(
            OpCode::from_parts(Ogf::ControllerBaseband, 0x003),
            OpCode::RESET
        );
        assert_eq!(OpCode::RESET.ogf(), Ogf::ControllerBaseband);
        assert_eq!(OpCode::RESET.ocf(), 0x003);
    }

    #[test]
    fn vendor_opcodes_decompose_correctly() {
        // Realtek's firmware download command, which the Windows transport has to send
        // itself because no driver is there to do it (OPEN-QUESTIONS Q21).
        let download = OpCode::new(0xFC20);
        assert_eq!(download.ogf(), Ogf::Vendor);
        assert_eq!(download.ocf(), 0x020);
        assert_eq!(OpCode::from_parts(Ogf::Vendor, 0x020), download);
    }

    #[test]
    fn an_unknown_opcode_still_renders_for_logs() {
        // A Command Complete can echo an opcode we never sent, after a reset races an
        // in-flight command. That has to be loggable, not a parse error.
        assert_eq!(OpCode::new(0x1234).to_string(), "opcode 0x1234");
        assert_eq!(OpCode::RESET.to_string(), "reset(0x0c03)");
    }
}
