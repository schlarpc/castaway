//! The signaling channel: connection, configuration and disconnection commands.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::L2capError;
use crate::ertm::{FcsType, RetransmissionConfig};
use crate::pdu::{Cid, Psm};

/// Why a connection request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionResult {
    /// Connection established.
    Success,
    /// Still authorising; a second response will follow.
    Pending,
    /// Nothing is registered on that PSM.
    PsmNotSupported,
    /// Refused for security reasons.
    SecurityBlock,
    /// Out of resources.
    NoResources,
    /// A result code we don't model.
    Other(u16),
}

impl ConnectionResult {
    const fn bits(self) -> u16 {
        match self {
            Self::Success => 0x0000,
            Self::Pending => 0x0001,
            Self::PsmNotSupported => 0x0002,
            Self::SecurityBlock => 0x0003,
            Self::NoResources => 0x0004,
            Self::Other(raw) => raw,
        }
    }

    const fn from_bits(raw: u16) -> Self {
        match raw {
            0x0000 => Self::Success,
            0x0001 => Self::Pending,
            0x0002 => Self::PsmNotSupported,
            0x0003 => Self::SecurityBlock,
            0x0004 => Self::NoResources,
            other => Self::Other(other),
        }
    }

    /// A short reason string for error messages.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Pending => "pending",
            Self::PsmNotSupported => "psm not supported",
            Self::SecurityBlock => "security block",
            Self::NoResources => "no resources",
            Self::Other(_) => "unknown result",
        }
    }
}

/// Outcome of a configuration exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigResult {
    /// Parameters accepted.
    Success,
    /// Parameters unacceptable; the response carries a counter-proposal.
    Unacceptable,
    /// Configuration rejected outright.
    Rejected,
    /// The request carried options we don't understand and that weren't hints.
    UnknownOptions,
    /// A result code we don't model.
    Other(u16),
}

impl ConfigResult {
    const fn bits(self) -> u16 {
        match self {
            Self::Success => 0x0000,
            Self::Unacceptable => 0x0001,
            Self::Rejected => 0x0002,
            Self::UnknownOptions => 0x0003,
            Self::Other(raw) => raw,
        }
    }

    const fn from_bits(raw: u16) -> Self {
        match raw {
            0x0000 => Self::Success,
            0x0001 => Self::Unacceptable,
            0x0002 => Self::Rejected,
            0x0003 => Self::UnknownOptions,
            other => Self::Other(other),
        }
    }
}

/// One configuration option.
///
/// The `hint` bit on unknown options is load-bearing: an option with it set may be
/// ignored, while one without it *must* be refused with [`ConfigResult::UnknownOptions`].
/// Ignoring that distinction either breaks interop with peers that send hints (we reject
/// a perfectly good config) or silently accepts a mode we don't implement, which fails
/// later and much more confusingly.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigOption {
    /// Maximum transmission unit the *sender of this option* can receive.
    Mtu(u16),
    /// Flush timeout.
    FlushTimeout(u16),
    /// Retransmission and flow control: which mode the channel runs in, and the window,
    /// timers and frame size that go with it. The option cover art turns on (Q29).
    Retransmission(RetransmissionConfig),
    /// Whether frames carry a frame check sequence.
    Fcs(FcsType),
    /// Extended window size, which comes with the four-byte control field we don't
    /// implement. Decoded rather than left unknown so it can be refused by name with a
    /// counter-proposal of zero — "use the standard field" — instead of failing the whole
    /// configuration.
    ExtendedWindowSize(u16),
    /// An option we don't implement.
    Unknown {
        /// Option type, with the hint bit already stripped.
        kind: u8,
        /// Whether the hint bit was set (i.e. whether it may be ignored).
        hint: bool,
        /// Raw option payload.
        data: Bytes,
    },
}

impl ConfigOption {
    const MTU: u8 = 0x01;
    const FLUSH_TIMEOUT: u8 = 0x02;
    const RETRANSMISSION: u8 = 0x04;
    const FCS: u8 = 0x05;
    const EXTENDED_WINDOW_SIZE: u8 = 0x07;
    const HINT_BIT: u8 = 0x80;

    /// Whether this option may be ignored rather than refused.
    #[must_use]
    pub const fn is_ignorable(&self) -> bool {
        match self {
            Self::Mtu(_)
            | Self::FlushTimeout(_)
            | Self::Retransmission(_)
            | Self::Fcs(_)
            | Self::ExtendedWindowSize(_) => true,
            Self::Unknown { hint, .. } => *hint,
        }
    }

    fn encode(&self, buf: &mut BytesMut) {
        match self {
            Self::Mtu(mtu) => {
                buf.put_u8(Self::MTU);
                buf.put_u8(2);
                buf.put_u16_le(*mtu);
            }
            Self::FlushTimeout(t) => {
                buf.put_u8(Self::FLUSH_TIMEOUT);
                buf.put_u8(2);
                buf.put_u16_le(*t);
            }
            Self::Retransmission(config) => {
                buf.put_u8(Self::RETRANSMISSION);
                let body = config.encode();
                buf.put_u8(u8::try_from(body.len()).unwrap_or(u8::MAX));
                buf.extend_from_slice(&body);
            }
            Self::Fcs(fcs) => {
                buf.put_u8(Self::FCS);
                buf.put_u8(1);
                buf.put_u8(fcs.bits());
            }
            Self::ExtendedWindowSize(size) => {
                buf.put_u8(Self::EXTENDED_WINDOW_SIZE);
                buf.put_u8(2);
                buf.put_u16_le(*size);
            }
            Self::Unknown { kind, hint, data } => {
                buf.put_u8(kind | if *hint { Self::HINT_BIT } else { 0 });
                buf.put_u8(u8::try_from(data.len()).unwrap_or(u8::MAX));
                buf.extend_from_slice(data);
            }
        }
    }

    /// Decode a list of options from a configuration request/response tail.
    ///
    /// # Errors
    /// [`L2capError::Truncated`] if an option's declared length runs past the buffer.
    pub fn decode_all(mut buf: &[u8]) -> Result<Vec<Self>, L2capError> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            if buf.len() < 2 {
                return Err(L2capError::Truncated {
                    what: "config option header",
                    need: 2,
                    have: buf.len(),
                });
            }
            let raw_kind = buf[0];
            let len = usize::from(buf[1]);
            if buf.len() < 2 + len {
                return Err(L2capError::Truncated {
                    what: "config option body",
                    need: 2 + len,
                    have: buf.len(),
                });
            }
            let body = &buf[2..2 + len];
            let hint = raw_kind & Self::HINT_BIT != 0;
            let kind = raw_kind & !Self::HINT_BIT;
            out.push(match (kind, len) {
                (Self::MTU, 2) => Self::Mtu(u16::from_le_bytes([body[0], body[1]])),
                (Self::FLUSH_TIMEOUT, 2) => {
                    Self::FlushTimeout(u16::from_le_bytes([body[0], body[1]]))
                }
                (Self::RETRANSMISSION, RetransmissionConfig::LEN) => {
                    Self::Retransmission(RetransmissionConfig::decode(body)?)
                }
                (Self::FCS, 1) => Self::Fcs(FcsType::from_bits(body[0])),
                (Self::EXTENDED_WINDOW_SIZE, 2) => {
                    Self::ExtendedWindowSize(u16::from_le_bytes([body[0], body[1]]))
                }
                _ => Self::Unknown {
                    kind,
                    hint,
                    data: Bytes::copy_from_slice(body),
                },
            });
            buf = &buf[2 + len..];
        }
        Ok(out)
    }
}

/// Why a command was refused. The spec's `Command Reject` reason codes.
pub mod reject_reason {
    /// The command code is not one we implement, or the command did not parse.
    pub const NOT_UNDERSTOOD: u16 = 0x0000;
    /// The command was larger than our signalling MTU.
    pub const MTU_EXCEEDED: u16 = 0x0001;
    /// The command named a channel id that means nothing on this link.
    pub const INVALID_CID: u16 = 0x0002;
}

/// One command we could not act on, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandRefusal {
    /// The id of the command being refused, so the peer can match it.
    pub id: u8,
    /// One of [`reject_reason`].
    pub reason: u16,
}

/// What a C-frame decoded to: the commands we understood, and the refusals owed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Commands to act on, in the order they arrived.
    pub signals: Vec<Signal>,
    /// `Command Reject`s to send back.
    pub rejects: Vec<CommandRefusal>,
    /// The frame ended mid-command, so anything after that point was not parsed.
    pub truncated: bool,
}

/// A signaling command. `id` correlates a response with its request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Signal {
    /// The peer could not process a command we sent.
    CommandReject {
        /// Correlation id.
        id: u8,
        /// Reason code.
        reason: u16,
        /// Reason-specific data.
        data: Bytes,
    },
    /// A peer wants a channel to a PSM.
    ConnectionRequest {
        /// Correlation id.
        id: u8,
        /// Which service.
        psm: Psm,
        /// The requester's own CID for this channel.
        source_cid: Cid,
    },
    /// Answer to a connection request.
    ConnectionResponse {
        /// Correlation id.
        id: u8,
        /// The responder's CID.
        dest_cid: Cid,
        /// The requester's CID, echoed.
        source_cid: Cid,
        /// Outcome.
        result: ConnectionResult,
        /// Further status when the result is `Pending`.
        status: u16,
    },
    /// Propose channel parameters.
    ConfigurationRequest {
        /// Correlation id.
        id: u8,
        /// The channel, named by the *recipient's* CID.
        dest_cid: Cid,
        /// Continuation flag.
        flags: u16,
        /// Proposed options.
        options: Vec<ConfigOption>,
    },
    /// Answer a configuration proposal.
    ConfigurationResponse {
        /// Correlation id.
        id: u8,
        /// The channel, named by the *requester's* CID.
        source_cid: Cid,
        /// Continuation flag.
        flags: u16,
        /// Outcome.
        result: ConfigResult,
        /// Accepted or counter-proposed options.
        options: Vec<ConfigOption>,
    },
    /// Tear a channel down.
    DisconnectionRequest {
        /// Correlation id.
        id: u8,
        /// Recipient's CID.
        dest_cid: Cid,
        /// Sender's CID.
        source_cid: Cid,
    },
    /// Acknowledge a teardown.
    DisconnectionResponse {
        /// Correlation id.
        id: u8,
        /// Recipient's CID.
        dest_cid: Cid,
        /// Sender's CID.
        source_cid: Cid,
    },
    /// Keepalive/ping.
    EchoRequest {
        /// Correlation id.
        id: u8,
        /// Opaque payload to echo.
        data: Bytes,
    },
    /// Answer to a ping.
    EchoResponse {
        /// Correlation id.
        id: u8,
        /// The echoed payload.
        data: Bytes,
    },
    /// Ask what the peer supports.
    InformationRequest {
        /// Correlation id.
        id: u8,
        /// Which info type.
        info_type: u16,
    },
    /// Answer an information request.
    InformationResponse {
        /// Correlation id.
        id: u8,
        /// Which info type.
        info_type: u16,
        /// Result code.
        result: u16,
        /// Type-specific payload.
        data: Bytes,
    },
}

mod code {
    pub const COMMAND_REJECT: u8 = 0x01;
    pub const CONNECTION_REQUEST: u8 = 0x02;
    pub const CONNECTION_RESPONSE: u8 = 0x03;
    pub const CONFIGURATION_REQUEST: u8 = 0x04;
    pub const CONFIGURATION_RESPONSE: u8 = 0x05;
    pub const DISCONNECTION_REQUEST: u8 = 0x06;
    pub const DISCONNECTION_RESPONSE: u8 = 0x07;
    pub const ECHO_REQUEST: u8 = 0x08;
    pub const ECHO_RESPONSE: u8 = 0x09;
    pub const INFORMATION_REQUEST: u8 = 0x0A;
    pub const INFORMATION_RESPONSE: u8 = 0x0B;
}

impl Signal {
    /// The correlation identifier.
    #[must_use]
    pub const fn id(&self) -> u8 {
        match self {
            Self::CommandReject { id, .. }
            | Self::ConnectionRequest { id, .. }
            | Self::ConnectionResponse { id, .. }
            | Self::ConfigurationRequest { id, .. }
            | Self::ConfigurationResponse { id, .. }
            | Self::DisconnectionRequest { id, .. }
            | Self::DisconnectionResponse { id, .. }
            | Self::EchoRequest { id, .. }
            | Self::EchoResponse { id, .. }
            | Self::InformationRequest { id, .. }
            | Self::InformationResponse { id, .. } => *id,
        }
    }

    const fn code(&self) -> u8 {
        match self {
            Self::CommandReject { .. } => code::COMMAND_REJECT,
            Self::ConnectionRequest { .. } => code::CONNECTION_REQUEST,
            Self::ConnectionResponse { .. } => code::CONNECTION_RESPONSE,
            Self::ConfigurationRequest { .. } => code::CONFIGURATION_REQUEST,
            Self::ConfigurationResponse { .. } => code::CONFIGURATION_RESPONSE,
            Self::DisconnectionRequest { .. } => code::DISCONNECTION_REQUEST,
            Self::DisconnectionResponse { .. } => code::DISCONNECTION_RESPONSE,
            Self::EchoRequest { .. } => code::ECHO_REQUEST,
            Self::EchoResponse { .. } => code::ECHO_RESPONSE,
            Self::InformationRequest { .. } => code::INFORMATION_REQUEST,
            Self::InformationResponse { .. } => code::INFORMATION_RESPONSE,
        }
    }

    /// Encode as a signaling C-frame payload (code, id, length, data).
    ///
    /// # Errors
    /// [`L2capError::TooLong`] if the command body exceeds the 16-bit length field.
    pub fn encode(&self) -> Result<Bytes, L2capError> {
        let mut body = BytesMut::with_capacity(16);
        match self {
            Self::CommandReject { reason, data, .. } => {
                body.put_u16_le(*reason);
                body.extend_from_slice(data);
            }
            Self::ConnectionRequest {
                psm, source_cid, ..
            } => {
                body.put_u16_le(psm.raw());
                body.put_u16_le(source_cid.raw());
            }
            Self::ConnectionResponse {
                dest_cid,
                source_cid,
                result,
                status,
                ..
            } => {
                body.put_u16_le(dest_cid.raw());
                body.put_u16_le(source_cid.raw());
                body.put_u16_le(result.bits());
                body.put_u16_le(*status);
            }
            Self::ConfigurationRequest {
                dest_cid,
                flags,
                options,
                ..
            } => {
                body.put_u16_le(dest_cid.raw());
                body.put_u16_le(*flags);
                for opt in options {
                    opt.encode(&mut body);
                }
            }
            Self::ConfigurationResponse {
                source_cid,
                flags,
                result,
                options,
                ..
            } => {
                body.put_u16_le(source_cid.raw());
                body.put_u16_le(*flags);
                body.put_u16_le(result.bits());
                for opt in options {
                    opt.encode(&mut body);
                }
            }
            Self::DisconnectionRequest {
                dest_cid,
                source_cid,
                ..
            }
            | Self::DisconnectionResponse {
                dest_cid,
                source_cid,
                ..
            } => {
                body.put_u16_le(dest_cid.raw());
                body.put_u16_le(source_cid.raw());
            }
            Self::EchoRequest { data, .. } | Self::EchoResponse { data, .. } => {
                body.extend_from_slice(data);
            }
            Self::InformationRequest { info_type, .. } => body.put_u16_le(*info_type),
            Self::InformationResponse {
                info_type,
                result,
                data,
                ..
            } => {
                body.put_u16_le(*info_type);
                body.put_u16_le(*result);
                body.extend_from_slice(data);
            }
        }
        let len = u16::try_from(body.len()).map_err(|_| L2capError::TooLong {
            len: body.len(),
            max: u16::MAX as usize,
        })?;
        let mut out = BytesMut::with_capacity(4 + body.len());
        out.put_u8(self.code());
        out.put_u8(self.id());
        out.put_u16_le(len);
        out.extend_from_slice(&body);
        Ok(out.freeze())
    }

    /// Decode every command packed into one signaling PDU.
    ///
    /// The spec allows several commands in a single C-frame, and real stacks do it —
    /// parsing only the first quietly drops the peer's configuration request.
    ///
    /// # Errors
    /// [`L2capError::Truncated`] on a short command, or
    /// [`L2capError::UnknownSignalingCode`] for a code we don't implement.
    pub fn decode_all(mut buf: &[u8]) -> Result<Vec<Self>, L2capError> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            if buf.len() < 4 {
                return Err(L2capError::Truncated {
                    what: "signaling header",
                    need: 4,
                    have: buf.len(),
                });
            }
            let code = buf[0];
            let id = buf[1];
            let len = usize::from(u16::from_le_bytes([buf[2], buf[3]]));
            if buf.len() < 4 + len {
                return Err(L2capError::Truncated {
                    what: "signaling body",
                    need: 4 + len,
                    have: buf.len(),
                });
            }
            let body = &buf[4..4 + len];
            out.push(Self::decode_one(code, id, body)?);
            buf = &buf[4 + len..];
        }
        Ok(out)
    }

    /// Decode a whole C-frame, keeping what parses and noting what the peer is owed a
    /// refusal for.
    ///
    /// [`Self::decode_all`] fails the entire frame on one bad command, which throws away
    /// well-formed commands packed alongside it — the very packing this module went to
    /// the trouble of supporting — and leaves the peer with silence where the spec
    /// requires `Command Reject`. A phone that packs an unrecognised command (`Create
    /// Channel`, `Move Channel`, anything future) with a `ConnectionRequest` would never
    /// get its channel, and never learn why.
    ///
    /// A truncated header or body stops parsing: past that point the stream cannot be
    /// resynchronised, because command boundaries come from lengths inside it.
    #[must_use]
    pub fn decode_frame(mut buf: &[u8]) -> DecodedFrame {
        let mut frame = DecodedFrame::default();
        while !buf.is_empty() {
            if buf.len() < 4 {
                // No id to address a rejection to; nothing useful to say.
                frame.truncated = true;
                break;
            }
            let (code, id) = (buf[0], buf[1]);
            let len = usize::from(u16::from_le_bytes([buf[2], buf[3]]));
            if buf.len() < 4 + len {
                frame.truncated = true;
                frame.rejects.push(CommandRefusal {
                    id,
                    reason: reject_reason::NOT_UNDERSTOOD,
                });
                break;
            }
            match Self::decode_one(code, id, &buf[4..4 + len]) {
                Ok(sig) => frame.signals.push(sig),
                // The length was well-formed even though the command was not, so the next
                // command is still findable and the frame is worth continuing.
                Err(_) => frame.rejects.push(CommandRefusal {
                    id,
                    reason: reject_reason::NOT_UNDERSTOOD,
                }),
            }
            buf = &buf[4 + len..];
        }
        frame
    }

    fn decode_one(code: u8, id: u8, body: &[u8]) -> Result<Self, L2capError> {
        let need = |n: usize| -> Result<(), L2capError> {
            if body.len() < n {
                Err(L2capError::Truncated {
                    what: "signaling command",
                    need: n,
                    have: body.len(),
                })
            } else {
                Ok(())
            }
        };
        let u16at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
        Ok(match code {
            code::COMMAND_REJECT => {
                need(2)?;
                Self::CommandReject {
                    id,
                    reason: u16at(0),
                    data: Bytes::copy_from_slice(&body[2..]),
                }
            }
            code::CONNECTION_REQUEST => {
                need(4)?;
                Self::ConnectionRequest {
                    id,
                    psm: Psm::new(u16at(0))?,
                    source_cid: Cid::new(u16at(2)),
                }
            }
            code::CONNECTION_RESPONSE => {
                need(8)?;
                Self::ConnectionResponse {
                    id,
                    dest_cid: Cid::new(u16at(0)),
                    source_cid: Cid::new(u16at(2)),
                    result: ConnectionResult::from_bits(u16at(4)),
                    status: u16at(6),
                }
            }
            code::CONFIGURATION_REQUEST => {
                need(4)?;
                Self::ConfigurationRequest {
                    id,
                    dest_cid: Cid::new(u16at(0)),
                    flags: u16at(2),
                    options: ConfigOption::decode_all(&body[4..])?,
                }
            }
            code::CONFIGURATION_RESPONSE => {
                need(6)?;
                Self::ConfigurationResponse {
                    id,
                    source_cid: Cid::new(u16at(0)),
                    flags: u16at(2),
                    result: ConfigResult::from_bits(u16at(4)),
                    options: ConfigOption::decode_all(&body[6..])?,
                }
            }
            code::DISCONNECTION_REQUEST => {
                need(4)?;
                Self::DisconnectionRequest {
                    id,
                    dest_cid: Cid::new(u16at(0)),
                    source_cid: Cid::new(u16at(2)),
                }
            }
            code::DISCONNECTION_RESPONSE => {
                need(4)?;
                Self::DisconnectionResponse {
                    id,
                    dest_cid: Cid::new(u16at(0)),
                    source_cid: Cid::new(u16at(2)),
                }
            }
            code::ECHO_REQUEST => Self::EchoRequest {
                id,
                data: Bytes::copy_from_slice(body),
            },
            code::ECHO_RESPONSE => Self::EchoResponse {
                id,
                data: Bytes::copy_from_slice(body),
            },
            code::INFORMATION_REQUEST => {
                need(2)?;
                Self::InformationRequest {
                    id,
                    info_type: u16at(0),
                }
            }
            code::INFORMATION_RESPONSE => {
                need(4)?;
                Self::InformationResponse {
                    id,
                    info_type: u16at(0),
                    result: u16at(2),
                    data: Bytes::copy_from_slice(&body[4..]),
                }
            }
            other => return Err(L2capError::UnknownSignalingCode(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;

    #[test]
    fn a_connection_request_for_avdtp_round_trips() {
        let sig = Signal::ConnectionRequest {
            id: 1,
            psm: Psm::AVDTP,
            source_cid: Cid::new(0x0040),
        };
        let bytes = sig.encode().unwrap();
        assert_eq!(&bytes[..], &hex!("02 01 04 00 19 00 40 00"));
        assert_eq!(Signal::decode_all(&bytes).unwrap(), vec![sig]);
    }

    #[test]
    fn several_commands_in_one_frame_are_all_decoded() {
        // Real stacks pack a connection response and a configuration request into one
        // C-frame. Parsing only the first leaves the channel stuck in WaitConfig
        // forever, waiting for a request that already arrived.
        let a = Signal::ConnectionResponse {
            id: 1,
            dest_cid: Cid::new(0x0041),
            source_cid: Cid::new(0x0040),
            result: ConnectionResult::Success,
            status: 0,
        };
        let b = Signal::ConfigurationRequest {
            id: 2,
            dest_cid: Cid::new(0x0040),
            flags: 0,
            options: vec![ConfigOption::Mtu(672)],
        };
        let mut packed = Vec::new();
        packed.extend_from_slice(&a.encode().unwrap());
        packed.extend_from_slice(&b.encode().unwrap());
        assert_eq!(Signal::decode_all(&packed).unwrap(), vec![a, b]);
    }

    #[test]
    fn mtu_options_round_trip() {
        let opts = vec![
            ConfigOption::Mtu(0x02a0),
            ConfigOption::FlushTimeout(0xffff),
        ];
        let sig = Signal::ConfigurationRequest {
            id: 7,
            dest_cid: Cid::new(0x0040),
            flags: 0,
            options: opts.clone(),
        };
        let back = Signal::decode_all(&sig.encode().unwrap()).unwrap();
        let Signal::ConfigurationRequest { options, .. } = &back[0] else {
            panic!("wrong variant");
        };
        assert_eq!(options, &opts);
    }

    #[test]
    fn the_hint_bit_decides_whether_an_unknown_option_may_be_ignored() {
        // Option 0x04 (retransmission/flow control) without the hint bit must be
        // refused; the same option with it set may be ignored. Treating them alike
        // either breaks interop or silently accepts a mode we don't implement.
        let must_refuse = ConfigOption::decode_all(&hex!("04 02 00 00")).unwrap();
        assert!(!must_refuse[0].is_ignorable());
        let may_ignore = ConfigOption::decode_all(&hex!("84 02 00 00")).unwrap();
        assert!(may_ignore[0].is_ignorable());
        // …and the hint bit is stripped from the reported kind, not left in it.
        assert_eq!(
            may_ignore[0],
            ConfigOption::Unknown {
                kind: 0x04,
                hint: true,
                data: Bytes::from_static(&[0, 0]),
            }
        );
    }

    #[test]
    fn an_option_running_past_the_buffer_is_refused() {
        assert!(matches!(
            ConfigOption::decode_all(&hex!("01 08 aa bb")),
            Err(L2capError::Truncated { .. })
        ));
    }

    #[test]
    fn an_invalid_psm_in_a_request_is_rejected_at_parse() {
        // Even PSM 0x0018 — parse-don't-validate means this never reaches the router.
        assert!(matches!(
            Signal::decode_all(&hex!("02 01 04 00 18 00 40 00")),
            Err(L2capError::InvalidPsm(0x0018))
        ));
    }

    #[test]
    fn unknown_signaling_codes_are_reported_with_their_code() {
        assert!(matches!(
            Signal::decode_all(&hex!("7f 01 00 00")),
            Err(L2capError::UnknownSignalingCode(0x7f))
        ));
    }
}
