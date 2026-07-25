//! Controller events, parsed into typed values.
//!
//! Unrecognised events decode to [`Event::Unhandled`] rather than failing. Controllers
//! emit plenty we neither asked for nor care about (mode changes, page-scan repetition
//! changes, vendor debug), and a stack that errors on the first one is a stack that dies
//! on a controller it has never met.

use bytes::Bytes;

use crate::addr::BdAddr;
use crate::command::{AuthRequirements, IoCapability, LinkKey};
use crate::error::HciError;
use crate::opcode::OpCode;
use crate::packet::ConnectionHandle;
use crate::status::Status;

/// Event codes we model.
pub mod code {
    /// Connection Complete.
    pub const CONNECTION_COMPLETE: u8 = 0x03;
    /// Connection Request.
    pub const CONNECTION_REQUEST: u8 = 0x04;
    /// Disconnection Complete.
    pub const DISCONNECTION_COMPLETE: u8 = 0x05;
    /// Authentication Complete.
    pub const AUTHENTICATION_COMPLETE: u8 = 0x06;
    /// Remote Name Request Complete.
    pub const REMOTE_NAME_REQUEST_COMPLETE: u8 = 0x07;
    /// Encryption Change.
    pub const ENCRYPTION_CHANGE: u8 = 0x08;
    /// Command Complete.
    pub const COMMAND_COMPLETE: u8 = 0x0E;
    /// Command Status.
    pub const COMMAND_STATUS: u8 = 0x0F;
    /// Number Of Completed Packets.
    pub const NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
    /// PIN Code Request.
    pub const PIN_CODE_REQUEST: u8 = 0x16;
    /// Link Key Request.
    pub const LINK_KEY_REQUEST: u8 = 0x17;
    /// Link Key Notification.
    pub const LINK_KEY_NOTIFICATION: u8 = 0x18;
    /// IO Capability Request.
    pub const IO_CAPABILITY_REQUEST: u8 = 0x31;
    /// IO Capability Response.
    pub const IO_CAPABILITY_RESPONSE: u8 = 0x32;
    /// User Confirmation Request.
    pub const USER_CONFIRMATION_REQUEST: u8 = 0x33;
    /// Simple Pairing Complete.
    pub const SIMPLE_PAIRING_COMPLETE: u8 = 0x36;
}

/// What kind of link a connection carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkType {
    /// Synchronous (voice). Refused — SCO is HFP's business.
    Sco,
    /// Asynchronous connection-oriented: everything L2CAP, and so everything we do.
    Acl,
    /// Extended SCO.
    ESco,
    /// Anything else the controller invents.
    Other(u8),
}

impl LinkType {
    const fn from_bits(raw: u8) -> Self {
        match raw {
            0x00 => Self::Sco,
            0x01 => Self::Acl,
            0x02 => Self::ESco,
            other => Self::Other(other),
        }
    }
}

/// A parsed controller event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A peer is paging us and wants a connection.
    ConnectionRequest {
        /// Who.
        addr: BdAddr,
        /// Their class of device (24-bit).
        class_of_device: u32,
        /// What kind of link they want.
        link_type: LinkType,
    },
    /// A connection finished setting up — successfully or not.
    ConnectionComplete {
        /// Result.
        status: Status,
        /// The handle, valid only when `status` is success.
        handle: ConnectionHandle,
        /// The peer.
        addr: BdAddr,
        /// Link type.
        link_type: LinkType,
        /// Whether encryption is already on.
        encryption_enabled: bool,
    },
    /// A connection went away.
    DisconnectionComplete {
        /// Whether the disconnection procedure itself succeeded.
        status: Status,
        /// The handle that is now dead.
        handle: ConnectionHandle,
        /// Why it ended — this is the interesting field, not `status`.
        reason: Status,
    },
    /// Authentication finished on a link.
    AuthenticationComplete {
        /// Result.
        status: Status,
        /// The link.
        handle: ConnectionHandle,
    },
    /// Encryption was turned on or off.
    EncryptionChange {
        /// Result.
        status: Status,
        /// The link.
        handle: ConnectionHandle,
        /// Whether encryption is now on.
        enabled: bool,
    },
    /// The peer told us its friendly name.
    RemoteNameRequestComplete {
        /// Result.
        status: Status,
        /// The peer.
        addr: BdAddr,
        /// The name, with the null padding stripped.
        name: String,
    },
    /// The controller finished a command and returned parameters.
    CommandComplete {
        /// How many further commands the host may send.
        allowed_packets: u8,
        /// Which command.
        opcode: OpCode,
        /// Command-specific return parameters, still raw.
        params: Bytes,
    },
    /// The controller accepted (or refused) a command that completes later.
    CommandStatus {
        /// Whether the command was accepted for processing.
        status: Status,
        /// How many further commands the host may send.
        allowed_packets: u8,
        /// Which command.
        opcode: OpCode,
    },
    /// ACL buffers have been freed. The whole of transmit flow control.
    NumberOfCompletedPackets(Vec<(ConnectionHandle, u16)>),
    /// The controller wants a legacy PIN. We always refuse — SSP or nothing.
    PinCodeRequest(BdAddr),
    /// The controller is asking whether we have a stored link key for this peer.
    LinkKeyRequest(BdAddr),
    /// Pairing produced a link key to store.
    LinkKeyNotification {
        /// The peer it belongs to.
        addr: BdAddr,
        /// The key.
        key: LinkKey,
        /// Key type, which distinguishes debug and authenticated keys.
        key_type: u8,
    },
    /// Secure Simple Pairing wants our IO capability.
    IoCapabilityRequest(BdAddr),
    /// The peer told us its IO capability, which fixes the pairing model.
    IoCapabilityResponse {
        /// The peer.
        addr: BdAddr,
        /// What it claims.
        io: IoCapability,
        /// What it requires.
        auth: AuthRequirements,
    },
    /// SSP numeric comparison. With `NoInputNoOutput` this is Just Works and the value
    /// is not meant to be shown to anyone.
    UserConfirmationRequest {
        /// The peer.
        addr: BdAddr,
        /// The six-digit comparison value.
        numeric_value: u32,
    },
    /// Pairing finished.
    SimplePairingComplete {
        /// Result.
        status: Status,
        /// The peer.
        addr: BdAddr,
    },
    /// An event we don't model. Kept so it can be logged, never an error.
    Unhandled {
        /// Event code.
        code: u8,
        /// Raw parameters.
        params: Bytes,
    },
}

/// Cursor over event parameters that refuses to read past the end.
struct Reader<'a> {
    buf: &'a [u8],
    what: &'static str,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8], what: &'static str) -> Self {
        Self { buf, what }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], HciError> {
        if self.buf.len() < n {
            return Err(HciError::Truncated {
                what: self.what,
                need: n,
                have: self.buf.len(),
            });
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, HciError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, HciError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u24(&mut self) -> Result<u32, HciError> {
        let b = self.take(3)?;
        Ok(u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
    }

    fn status(&mut self) -> Result<Status, HciError> {
        Ok(Status(self.u8()?))
    }

    fn addr(&mut self) -> Result<BdAddr, HciError> {
        let b = self.take(6)?;
        let mut wire = [0u8; 6];
        wire.copy_from_slice(b);
        Ok(BdAddr::from_wire(wire))
    }

    fn handle(&mut self) -> Result<ConnectionHandle, HciError> {
        // Handles in *events* carry no flag bits, but controllers have been seen to set
        // the reserved top nibble; mask rather than reject so one odd controller can't
        // take the stack down.
        ConnectionHandle::new(self.u16()? & 0x0FFF)
    }

    fn rest(self) -> Bytes {
        Bytes::copy_from_slice(self.buf)
    }
}

impl Event {
    /// Parse an event from its code and raw parameters.
    ///
    /// # Errors
    /// [`HciError::Truncated`] if a modelled event is shorter than its fixed layout.
    /// Unknown codes are *not* an error — see [`Event::Unhandled`].
    pub fn parse(code: u8, params: &[u8]) -> Result<Self, HciError> {
        let mut r = Reader::new(params, "event parameters");
        Ok(match code {
            code::CONNECTION_REQUEST => Self::ConnectionRequest {
                addr: r.addr()?,
                class_of_device: r.u24()?,
                link_type: LinkType::from_bits(r.u8()?),
            },
            code::CONNECTION_COMPLETE => Self::ConnectionComplete {
                status: r.status()?,
                handle: r.handle()?,
                addr: r.addr()?,
                link_type: LinkType::from_bits(r.u8()?),
                encryption_enabled: r.u8()? != 0,
            },
            code::DISCONNECTION_COMPLETE => Self::DisconnectionComplete {
                status: r.status()?,
                handle: r.handle()?,
                reason: r.status()?,
            },
            code::AUTHENTICATION_COMPLETE => Self::AuthenticationComplete {
                status: r.status()?,
                handle: r.handle()?,
            },
            code::ENCRYPTION_CHANGE => Self::EncryptionChange {
                status: r.status()?,
                handle: r.handle()?,
                enabled: r.u8()? != 0,
            },
            code::REMOTE_NAME_REQUEST_COMPLETE => {
                let status = r.status()?;
                let addr = r.addr()?;
                let raw = r.take(248)?;
                // The name is a fixed 248-byte field, null-padded, and is *not*
                // guaranteed to be valid UTF-8 — a phone named with an emoji that got
                // truncated mid-sequence would otherwise take the stack down.
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                Self::RemoteNameRequestComplete {
                    status,
                    addr,
                    name: String::from_utf8_lossy(&raw[..end]).into_owned(),
                }
            }
            code::COMMAND_COMPLETE => Self::CommandComplete {
                allowed_packets: r.u8()?,
                opcode: OpCode::new(r.u16()?),
                params: r.rest(),
            },
            code::COMMAND_STATUS => Self::CommandStatus {
                status: r.status()?,
                allowed_packets: r.u8()?,
                opcode: OpCode::new(r.u16()?),
            },
            code::NUMBER_OF_COMPLETED_PACKETS => {
                let n = r.u8()? as usize;
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    out.push((r.handle()?, r.u16()?));
                }
                Self::NumberOfCompletedPackets(out)
            }
            code::PIN_CODE_REQUEST => Self::PinCodeRequest(r.addr()?),
            code::LINK_KEY_REQUEST => Self::LinkKeyRequest(r.addr()?),
            code::LINK_KEY_NOTIFICATION => {
                let addr = r.addr()?;
                let raw = r.take(16)?;
                let mut key = [0u8; 16];
                key.copy_from_slice(raw);
                Self::LinkKeyNotification {
                    addr,
                    key: LinkKey::new(key),
                    key_type: r.u8()?,
                }
            }
            code::IO_CAPABILITY_REQUEST => Self::IoCapabilityRequest(r.addr()?),
            code::IO_CAPABILITY_RESPONSE => {
                let addr = r.addr()?;
                let io = match r.u8()? {
                    0x00 => IoCapability::DisplayOnly,
                    0x01 => IoCapability::DisplayYesNo,
                    0x02 => IoCapability::KeyboardOnly,
                    _ => IoCapability::NoInputNoOutput,
                };
                let _oob = r.u8()?;
                let auth = match r.u8()? {
                    0x00 | 0x01 => AuthRequirements::NoBondingNoMitm,
                    0x05 => AuthRequirements::GeneralBondingMitm,
                    _ => AuthRequirements::GeneralBondingNoMitm,
                };
                Self::IoCapabilityResponse { addr, io, auth }
            }
            code::USER_CONFIRMATION_REQUEST => Self::UserConfirmationRequest {
                addr: r.addr()?,
                numeric_value: {
                    let b = r.take(4)?;
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
                },
            },
            code::SIMPLE_PAIRING_COMPLETE => Self::SimplePairingComplete {
                status: r.status()?,
                addr: r.addr()?,
            },
            other => Self::Unhandled {
                code: other,
                params: Bytes::copy_from_slice(params),
            },
        })
    }
}

/// The controller's ACL buffer geometry, from a `Read_Buffer_Size` command complete.
///
/// These two numbers *are* transmit flow control: send more than `total_packets`
/// unacknowledged ACL fragments and the controller silently drops them, which presents as
/// audio that stutters under load and nothing in any log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferSize {
    /// Largest ACL payload the controller accepts in one fragment.
    pub acl_max_len: u16,
    /// How many ACL fragments may be outstanding at once.
    pub total_packets: u16,
}

impl BufferSize {
    /// Parse from `Read_Buffer_Size` return parameters.
    ///
    /// # Errors
    /// [`HciError::Truncated`] if short, or [`HciError::CommandFailed`] on a non-success
    /// status byte.
    pub fn parse(params: &[u8]) -> Result<Self, HciError> {
        let mut r = Reader::new(params, "read_buffer_size result");
        let status = r.status()?;
        if !status.is_success() {
            return Err(HciError::CommandFailed(status));
        }
        let acl_max_len = r.u16()?;
        let _sco_max_len = r.u8()?;
        let total_packets = r.u16()?;
        Ok(Self {
            acl_max_len,
            total_packets,
        })
    }
}

/// Parse a `Read_BD_ADDR` command complete.
///
/// # Errors
/// [`HciError::Truncated`] if short, or [`HciError::CommandFailed`] on failure status.
pub fn parse_bd_addr(params: &[u8]) -> Result<BdAddr, HciError> {
    let mut r = Reader::new(params, "read_bd_addr result");
    let status = r.status()?;
    if !status.is_success() {
        return Err(HciError::CommandFailed(status));
    }
    r.addr()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    #[test]
    fn a_connection_request_from_a_phone_parses() {
        // addr AA:BB:CC:DD:EE:FF (wire order), CoD 0x5A020C (phone), link type ACL.
        let params = hex!("ff ee dd cc bb aa 0c 02 5a 01");
        let Event::ConnectionRequest {
            addr,
            class_of_device,
            link_type,
        } = Event::parse(code::CONNECTION_REQUEST, &params).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(addr.to_string(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(class_of_device, 0x005A_020C);
        assert_eq!(link_type, LinkType::Acl);
    }

    #[test]
    fn disconnection_reports_why_separately_from_whether() {
        // `status` says the disconnection procedure worked; `reason` says the phone
        // walked out of the room. Conflating them logs "success" for a dropped link.
        let params = hex!("00 0b 00 13");
        let Event::DisconnectionComplete {
            status,
            handle,
            reason,
        } = Event::parse(code::DISCONNECTION_COMPLETE, &params).unwrap()
        else {
            panic!("wrong variant");
        };
        assert!(status.is_success());
        assert_eq!(handle.raw(), 0x000b);
        assert_eq!(reason, Status::REMOTE_USER_TERMINATED);
    }

    #[test]
    fn number_of_completed_packets_carries_every_pair() {
        // Two connections acknowledged in one event. Reading only the first is how a
        // stack slowly starves the second link of credit.
        let params = hex!("02 0b 00 05 00 0c 00 03 00");
        let Event::NumberOfCompletedPackets(pairs) =
            Event::parse(code::NUMBER_OF_COMPLETED_PACKETS, &params).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0.raw(), 0x0b);
        assert_eq!(pairs[0].1, 5);
        assert_eq!(pairs[1].0.raw(), 0x0c);
        assert_eq!(pairs[1].1, 3);
    }

    #[test]
    fn a_remote_name_is_cut_at_the_null_and_survives_bad_utf8() {
        let mut params = vec![0x00];
        params.extend_from_slice(&hex!("ff ee dd cc bb aa"));
        let mut name = vec![0u8; 248];
        name[..7].copy_from_slice(b"Pixel \xff"); // truncated multi-byte sequence
        params.extend_from_slice(&name);
        let Event::RemoteNameRequestComplete { name, .. } =
            Event::parse(code::REMOTE_NAME_REQUEST_COMPLETE, &params).unwrap()
        else {
            panic!("wrong variant");
        };
        assert!(name.starts_with("Pixel "), "got {name:?}");
        assert!(!name.contains('\0'));
    }

    #[test]
    fn an_unmodelled_event_is_carried_not_rejected() {
        // Max Slots Change. We don't care, but erroring here would kill the link.
        let ev = Event::parse(0x1b, &hex!("0b 00 05")).unwrap();
        assert!(matches!(ev, Event::Unhandled { code: 0x1b, .. }));
    }

    #[test]
    fn a_truncated_modelled_event_is_an_error() {
        assert!(matches!(
            Event::parse(code::CONNECTION_COMPLETE, &hex!("00 0b")),
            Err(HciError::Truncated { .. })
        ));
    }

    #[test]
    fn buffer_size_is_the_input_to_flow_control() {
        // status, acl_max_len=0x0154 (340), sco_len=0xff, acl_total=0x0008.
        let bs = BufferSize::parse(&hex!("00 54 01 ff 08 00 08 00")).unwrap();
        assert_eq!(bs.acl_max_len, 340);
        assert_eq!(bs.total_packets, 8);
    }

    #[test]
    fn a_failed_command_complete_is_refused_rather_than_parsed() {
        assert!(matches!(
            BufferSize::parse(&hex!("01 00 00 00 00 00")),
            Err(HciError::CommandFailed(Status::UNKNOWN_COMMAND))
        ));
    }

    #[test]
    fn link_key_notification_yields_a_storable_key() {
        let mut params = hex!("ff ee dd cc bb aa").to_vec();
        params.extend_from_slice(&[0x11; 16]);
        params.push(0x04);
        let Event::LinkKeyNotification { addr, key, .. } =
            Event::parse(code::LINK_KEY_NOTIFICATION, &params).unwrap()
        else {
            panic!("wrong variant");
        };
        assert_eq!(addr.to_string(), "AA:BB:CC:DD:EE:FF");
        assert_eq!(key.as_bytes(), &[0x11; 16]);
    }
}
