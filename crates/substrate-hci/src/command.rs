//! The commands an A2DP sink actually sends, as typed values that encode themselves.
//!
//! Deliberately not "every HCI command" — this is the BR/EDR subset that gets a
//! controller from reset to discoverable, accepts an incoming connection, completes
//! Secure Simple Pairing, and tears down. Anything outside that is a command we would not
//! know how to handle the events for.

use bytes::{BufMut, Bytes, BytesMut};

use crate::addr::BdAddr;
use crate::error::HciError;
use crate::opcode::OpCode;
use crate::packet::{ConnectionHandle, HciPacket};
use crate::status::Status;

/// A 128-bit link key, stored so a paired phone reconnects without re-pairing.
///
/// Wrapped rather than passed as `[u8; 16]` so it cannot be confused with any other
/// 16-byte value, and so its `Debug` never prints the key material.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LinkKey([u8; 16]);

impl LinkKey {
    /// Wrap raw key bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes. Callers are persisting it; treat as secret.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Debug for LinkKey {
    /// Redacted on purpose: link keys end up in logs otherwise, and a leaked one lets
    /// anyone impersonate a paired phone.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkKey(<redacted>)")
    }
}

/// Which scans the controller answers — i.e. whether we are discoverable, connectable,
/// both, or invisible.
///
/// This is the whole of the receiver's "can a guest find me" policy (OPEN-QUESTIONS Q23),
/// so it is an enum rather than two booleans: "discoverable but not connectable" is a
/// real, useless state that this makes impossible to reach by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ScanEnable {
    /// Invisible and unreachable.
    #[default]
    None,
    /// Answers paging only: an already-paired phone can reconnect, nobody new can find us.
    ConnectableOnly,
    /// Answers inquiry and paging: visible in a phone's Bluetooth menu.
    DiscoverableAndConnectable,
}

impl ScanEnable {
    const fn bits(self) -> u8 {
        match self {
            Self::None => 0x00,
            Self::ConnectableOnly => 0x02,
            Self::DiscoverableAndConnectable => 0x03,
        }
    }
}

/// The device class senders use to pick an icon and decide what we are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassOfDevice(u32);

impl ClassOfDevice {
    /// Audio/Video major class, Loudspeaker minor class, with the Audio and Rendering
    /// service bits set. This is what makes a phone show a speaker icon and offer the
    /// media-audio toggle rather than treating us as a headset.
    pub const LOUDSPEAKER: Self = Self(0x24_0414);

    /// Wrap a raw 24-bit class of device.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw & 0x00FF_FFFF)
    }

    /// The raw 24-bit value.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Our input/output capability, which decides which Secure Simple Pairing model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum IoCapability {
    /// Can show a number, cannot confirm.
    DisplayOnly,
    /// Can show a number and confirm — the numeric-comparison model.
    DisplayYesNo,
    /// Can enter a number.
    KeyboardOnly,
    /// Neither. Selects the Just Works model, which is what a kiosk wants: no prompt on
    /// either side (OPEN-QUESTIONS Q23).
    #[default]
    NoInputNoOutput,
}

impl IoCapability {
    const fn bits(self) -> u8 {
        match self {
            Self::DisplayOnly => 0x00,
            Self::DisplayYesNo => 0x01,
            Self::KeyboardOnly => 0x02,
            Self::NoInputNoOutput => 0x03,
        }
    }
}

/// What we require of the pairing, in terms of bonding and man-in-the-middle protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AuthRequirements {
    /// Pair for this session only; no key stored.
    NoBondingNoMitm,
    /// Store a link key so the phone reconnects silently, no MITM protection. The kiosk
    /// default — MITM protection needs a display/keypad we deliberately don't claim.
    #[default]
    GeneralBondingNoMitm,
    /// Store a link key and require MITM protection.
    GeneralBondingMitm,
}

impl AuthRequirements {
    const fn bits(self) -> u8 {
        match self {
            Self::NoBondingNoMitm => 0x00,
            Self::GeneralBondingNoMitm => 0x04,
            Self::GeneralBondingMitm => 0x05,
        }
    }
}

/// Which side drives the link after accepting a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AcceptRole {
    /// Take over as central, forcing a role switch.
    BecomeCentral,
    /// Stay peripheral. Right for a sink: the phone paged us, and refusing the role
    /// switch avoids a renegotiation that some controllers handle badly mid-pairing.
    #[default]
    RemainPeripheral,
}

impl AcceptRole {
    const fn bits(self) -> u8 {
        match self {
            Self::BecomeCentral => 0x00,
            Self::RemainPeripheral => 0x01,
        }
    }
}

/// A command to the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Command {
    /// Return the controller to a known state. Always first.
    Reset,
    /// Read the ACL buffer size and count that flow control is built on.
    ReadBufferSize,
    /// Read the controller's own address.
    ReadBdAddr,
    /// Read HCI/LMP version information.
    ReadLocalVersion,
    /// Choose which events the controller may emit.
    SetEventMask(u64),
    /// Set the name senders see in their Bluetooth menu.
    WriteLocalName(String),
    /// Set discoverability/connectability.
    WriteScanEnable(ScanEnable),
    /// Set the device class.
    WriteClassOfDevice(ClassOfDevice),
    /// Set the extended inquiry response payload.
    WriteExtendedInquiryResponse {
        /// Whether FEC is required.
        fec_required: bool,
        /// The EIR data structures, already assembled.
        data: Bytes,
    },
    /// Enable Secure Simple Pairing (without which the controller uses legacy PIN).
    WriteSimplePairingMode(bool),
    /// Advertise host support for Secure Connections.
    WriteSecureConnectionsHostSupport(bool),
    /// Accept an incoming connection.
    AcceptConnectionRequest {
        /// The peer paging us.
        addr: BdAddr,
        /// Which role to take.
        role: AcceptRole,
    },
    /// Refuse an incoming connection.
    RejectConnectionRequest {
        /// The peer to refuse.
        addr: BdAddr,
        /// Why (a `REJECTED_*` status).
        reason: Status,
    },
    /// Supply a stored link key so a known phone reconnects without pairing.
    LinkKeyRequestReply {
        /// The peer.
        addr: BdAddr,
        /// The stored key.
        key: LinkKey,
    },
    /// Report that we have no key for this peer, forcing fresh pairing.
    LinkKeyRequestNegativeReply(BdAddr),
    /// Refuse legacy PIN pairing.
    PinCodeRequestNegativeReply(BdAddr),
    /// Answer the controller's IO-capability request during Secure Simple Pairing.
    IoCapabilityRequestReply {
        /// The peer being paired.
        addr: BdAddr,
        /// What we claim to have.
        io: IoCapability,
        /// Bonding/MITM requirements.
        auth: AuthRequirements,
    },
    /// Accept an SSP numeric comparison. With `NoInputNoOutput` on both ends this is
    /// Just Works and nobody sees a number.
    UserConfirmationRequestReply(BdAddr),
    /// Refuse an SSP numeric comparison.
    UserConfirmationRequestNegativeReply(BdAddr),
    /// Ask the peer for its friendly name (so the OSD can say who is casting).
    RemoteNameRequest(BdAddr),
    /// Start authentication on an established link.
    AuthenticationRequested(ConnectionHandle),
    /// Turn link-layer encryption on.
    SetConnectionEncryption {
        /// The link.
        handle: ConnectionHandle,
        /// Whether to enable.
        enable: bool,
    },
    /// Drop a connection.
    Disconnect {
        /// The link.
        handle: ConnectionHandle,
        /// Why.
        reason: Status,
    },
    /// A vendor command — the escape hatch firmware upload rides on (Q21).
    Vendor {
        /// Vendor opcode.
        opcode: OpCode,
        /// Raw parameters.
        params: Bytes,
    },
}

/// Longest local name the controller accepts, null-padded.
const LOCAL_NAME_LEN: usize = 248;
/// Fixed extended-inquiry-response payload length, zero-padded.
const EIR_LEN: usize = 240;

impl Command {
    /// The opcode this command carries.
    #[must_use]
    pub const fn opcode(&self) -> OpCode {
        match self {
            Self::Reset => OpCode::RESET,
            Self::ReadBufferSize => OpCode::READ_BUFFER_SIZE,
            Self::ReadBdAddr => OpCode::READ_BD_ADDR,
            Self::ReadLocalVersion => OpCode::READ_LOCAL_VERSION,
            Self::SetEventMask(_) => OpCode::SET_EVENT_MASK,
            Self::WriteLocalName(_) => OpCode::WRITE_LOCAL_NAME,
            Self::WriteScanEnable(_) => OpCode::WRITE_SCAN_ENABLE,
            Self::WriteClassOfDevice(_) => OpCode::WRITE_CLASS_OF_DEVICE,
            Self::WriteExtendedInquiryResponse { .. } => OpCode::WRITE_EXTENDED_INQUIRY_RESPONSE,
            Self::WriteSimplePairingMode(_) => OpCode::WRITE_SIMPLE_PAIRING_MODE,
            Self::WriteSecureConnectionsHostSupport(_) => {
                OpCode::WRITE_SECURE_CONNECTIONS_HOST_SUPPORT
            }
            Self::AcceptConnectionRequest { .. } => OpCode::ACCEPT_CONNECTION_REQUEST,
            Self::RejectConnectionRequest { .. } => OpCode::REJECT_CONNECTION_REQUEST,
            Self::LinkKeyRequestReply { .. } => OpCode::LINK_KEY_REQUEST_REPLY,
            Self::LinkKeyRequestNegativeReply(_) => OpCode::LINK_KEY_REQUEST_NEGATIVE_REPLY,
            Self::PinCodeRequestNegativeReply(_) => OpCode::PIN_CODE_REQUEST_NEGATIVE_REPLY,
            Self::IoCapabilityRequestReply { .. } => OpCode::IO_CAPABILITY_REQUEST_REPLY,
            Self::UserConfirmationRequestReply(_) => OpCode::USER_CONFIRMATION_REQUEST_REPLY,
            Self::UserConfirmationRequestNegativeReply(_) => {
                OpCode::USER_CONFIRMATION_REQUEST_NEGATIVE_REPLY
            }
            Self::RemoteNameRequest(_) => OpCode::REMOTE_NAME_REQUEST,
            Self::AuthenticationRequested(_) => OpCode::AUTHENTICATION_REQUESTED,
            Self::SetConnectionEncryption { .. } => OpCode::SET_CONNECTION_ENCRYPTION,
            Self::Disconnect { .. } => OpCode::DISCONNECT,
            Self::Vendor { opcode, .. } => *opcode,
        }
    }

    /// Encode into a framed packet ready for the transport.
    ///
    /// # Errors
    /// [`HciError::TooLong`] if a variable-length field exceeds its fixed field width.
    pub fn encode(&self) -> Result<HciPacket, HciError> {
        let mut p = BytesMut::with_capacity(32);
        match self {
            Self::Reset | Self::ReadBufferSize | Self::ReadBdAddr | Self::ReadLocalVersion => {}
            Self::SetEventMask(mask) => p.put_u64_le(*mask),
            Self::WriteLocalName(name) => {
                let bytes = name.as_bytes();
                if bytes.len() >= LOCAL_NAME_LEN {
                    return Err(HciError::TooLong {
                        what: "local name",
                        len: bytes.len(),
                        max: LOCAL_NAME_LEN - 1,
                    });
                }
                p.extend_from_slice(bytes);
                p.put_bytes(0, LOCAL_NAME_LEN - bytes.len());
            }
            Self::WriteScanEnable(scan) => p.put_u8(scan.bits()),
            Self::WriteClassOfDevice(cod) => {
                let raw = cod.raw();
                // 24-bit little-endian: three bytes, not a u32.
                #[allow(clippy::cast_possible_truncation)]
                p.extend_from_slice(&[raw as u8, (raw >> 8) as u8, (raw >> 16) as u8]);
            }
            Self::WriteExtendedInquiryResponse { fec_required, data } => {
                if data.len() > EIR_LEN {
                    return Err(HciError::TooLong {
                        what: "extended inquiry response",
                        len: data.len(),
                        max: EIR_LEN,
                    });
                }
                p.put_u8(u8::from(*fec_required));
                p.extend_from_slice(data);
                p.put_bytes(0, EIR_LEN - data.len());
            }
            Self::WriteSimplePairingMode(on) | Self::WriteSecureConnectionsHostSupport(on) => {
                p.put_u8(u8::from(*on));
            }
            Self::AcceptConnectionRequest { addr, role } => {
                p.extend_from_slice(&addr.to_wire());
                p.put_u8(role.bits());
            }
            Self::RejectConnectionRequest { addr, reason } => {
                p.extend_from_slice(&addr.to_wire());
                p.put_u8(reason.0);
            }
            Self::LinkKeyRequestReply { addr, key } => {
                p.extend_from_slice(&addr.to_wire());
                p.extend_from_slice(key.as_bytes());
            }
            Self::LinkKeyRequestNegativeReply(addr)
            | Self::PinCodeRequestNegativeReply(addr)
            | Self::UserConfirmationRequestReply(addr)
            | Self::UserConfirmationRequestNegativeReply(addr) => {
                p.extend_from_slice(&addr.to_wire());
            }
            Self::IoCapabilityRequestReply { addr, io, auth } => {
                p.extend_from_slice(&addr.to_wire());
                p.put_u8(io.bits());
                // OOB data present: always absent. Claiming otherwise makes the
                // controller wait for out-of-band material that will never arrive.
                p.put_u8(0x00);
                p.put_u8(auth.bits());
            }
            Self::RemoteNameRequest(addr) => {
                p.extend_from_slice(&addr.to_wire());
                // Page scan repetition mode R1, reserved byte, and "no clock offset".
                p.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
            }
            Self::AuthenticationRequested(handle) => p.put_u16_le(handle.raw()),
            Self::SetConnectionEncryption { handle, enable } => {
                p.put_u16_le(handle.raw());
                p.put_u8(u8::from(*enable));
            }
            Self::Disconnect { handle, reason } => {
                p.put_u16_le(handle.raw());
                p.put_u8(reason.0);
            }
            Self::Vendor { params, .. } => p.extend_from_slice(params),
        }
        Ok(HciPacket::Command {
            opcode: self.opcode(),
            params: p.freeze(),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    fn bytes_of(cmd: &Command) -> Bytes {
        cmd.encode().unwrap().encode().unwrap()
    }

    #[test]
    fn reset_is_the_canonical_four_bytes() {
        assert_eq!(&bytes_of(&Command::Reset)[..], &hex!("01 03 0c 00"));
    }

    #[test]
    fn addresses_go_out_in_wire_order() {
        // The endianness trap again, this time at the point it actually matters: an
        // accept aimed at the byte-reversed address silently never completes.
        let addr: BdAddr = "11:22:33:44:55:66".parse().unwrap();
        let out = bytes_of(&Command::AcceptConnectionRequest {
            addr,
            role: AcceptRole::RemainPeripheral,
        });
        assert_eq!(&out[..], &hex!("01 09 04 07 66 55 44 33 22 11 01"));
    }

    #[test]
    fn the_local_name_is_null_padded_to_the_fixed_width() {
        let out = bytes_of(&Command::WriteLocalName("Hackerspace TV".into()));
        // indicator + opcode + length byte = 4, then exactly 248 bytes of name field.
        assert_eq!(out.len(), 4 + LOCAL_NAME_LEN);
        assert_eq!(out[3] as usize, LOCAL_NAME_LEN);
        assert_eq!(&out[4..18], b"Hackerspace TV");
        assert!(out[18..].iter().all(|&b| b == 0), "padding must be zeroes");
    }

    #[test]
    fn an_overlong_name_is_refused_rather_than_truncated() {
        let long = "x".repeat(LOCAL_NAME_LEN);
        assert!(matches!(
            Command::WriteLocalName(long).encode(),
            Err(HciError::TooLong { .. })
        ));
    }

    #[test]
    fn class_of_device_is_three_bytes_little_endian() {
        // 0x240414 = Audio/Video major, Loudspeaker minor, Audio+Rendering services.
        // Sending this as a u32 would append a stray zero and be rejected.
        let out = bytes_of(&Command::WriteClassOfDevice(ClassOfDevice::LOUDSPEAKER));
        assert_eq!(&out[..], &hex!("01 24 0c 03 14 04 24"));
    }

    #[test]
    fn just_works_pairing_claims_no_io_and_general_bonding() {
        // NoInputNoOutput on our side is what selects Just Works, and general bonding is
        // what makes a repeat guest reconnect without re-pairing (Q23).
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let out = bytes_of(&Command::IoCapabilityRequestReply {
            addr,
            io: IoCapability::NoInputNoOutput,
            auth: AuthRequirements::GeneralBondingNoMitm,
        });
        assert_eq!(&out[..], &hex!("01 2b 04 09 ff ee dd cc bb aa 03 00 04"));
    }

    #[test]
    fn scan_enable_cannot_express_discoverable_but_unreachable() {
        // Two booleans would allow inquiry-scan-without-page-scan: findable, unusable.
        assert_eq!(ScanEnable::None.bits(), 0x00);
        assert_eq!(ScanEnable::ConnectableOnly.bits(), 0x02);
        assert_eq!(ScanEnable::DiscoverableAndConnectable.bits(), 0x03);
    }

    #[test]
    fn a_link_key_never_reaches_a_log() {
        let key = LinkKey::new([0xAB; 16]);
        assert_eq!(format!("{key:?}"), "LinkKey(<redacted>)");
        // …but it does reach the wire.
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        let out = bytes_of(&Command::LinkKeyRequestReply { addr, key });
        assert_eq!(&out[out.len() - 16..], &[0xAB; 16]);
    }

    #[test]
    fn eir_is_padded_to_its_fixed_length() {
        let out = bytes_of(&Command::WriteExtendedInquiryResponse {
            fec_required: false,
            data: Bytes::from_static(&[0x02, 0x0a, 0x00]),
        });
        assert_eq!(out.len(), 4 + 1 + EIR_LEN);
    }
}
