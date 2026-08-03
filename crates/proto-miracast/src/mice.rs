//! Miracast over Infrastructure — the [MS-MICE] control channel, as a pure state machine.
//!
//! Windows 10 v1703+ prefers to run Miracast over the existing WLAN rather than forming a
//! Wi-Fi Direct group, and this is the path that needs no P2P data plane at all: an mDNS
//! registration, a TCP listener on 7250, and then the *same* RTSP session this crate
//! already drives (#166).
//!
//! `docs/miracast-protocol-notes.md` §1.10 is the record this is built from, including the
//! golden fixtures and PIN vectors the tests below use.
//!
//! ## What MICE does and does not replace
//!
//! **Only the data path.** The sink must still be discoverable the ordinary way — a
//! Wi-Fi Direct peer beaconing a WSC Vendor Extension attribute ([`vendor_extension`]) is
//! how Windows learns MICE is on offer. What is avoided is group formation, not discovery.
//!
//! And once the control channel has done its work, nothing about RTSP changes: the sink
//! still dials *out* to the source's RTSP port, which is what this crate's actor already
//! does for Wi-Fi Direct ([`crate::actor`]). MICE's contribution is telling us which
//! address and port to dial.
//!
//! ## Endianness, which is mixed and is a trap
//!
//! Every size and length field is **big-endian**; the friendly name is **UTF-16LE**. Both
//! are handled at this boundary and nothing above it sees a byte order (ground rule 1).
//!
//! ## What is deliberately not implemented
//!
//! The DTLS-secured and PIN flows (entry points 2 and 3 in §1.10). Both begin with the
//! source reading our advertised capability, so the honest way to not implement them is to
//! not advertise them — [`Capability::insecure`] is the spec's own example byte `0x05`,
//! and a source that respects it never asks. A source that asks anyway is refused by name
//! rather than half-answered, because a DTLS handshake we cannot finish is a projection
//! that hangs for the full establishment timer instead of failing in one message.

use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::MiceError;

/// The control channel's TCP port, per [MS-MICE] §1.9.
///
/// Not IANA-registered despite the spec citing `[IANAPORT]` — 7236 is; 7250 is not. Named
/// here so nothing has to rediscover that.
pub const CONTROL_PORT: u16 = 7250;

/// The mDNS service a MICE sink registers, per [MS-MICE] §3.1.3.
///
/// Converges with WFA R2, which independently defines `_display._tcp` for the sink
/// (Miracast v2.3 §4.4.1) — so one responder serves both, which is what this project's
/// single shared responder is for.
pub const SERVICE_TYPE: &str = "_display._tcp";

/// The TXT key [MS-MICE] §3.1.3 requires alongside the service instance.
pub const CONTAINER_ID_KEY: &str = "container_id";

/// The source's default RTSP port when it names none.
pub const DEFAULT_RTSP_PORT: u16 = 7236;

/// How long a control channel may exist without RTSP being established.
///
/// [MS-MICE] §3.1.5.8: two minutes with PIN entry, thirty seconds without. We never offer
/// PIN entry ([`Capability::insecure`]), so thirty seconds is the only one that applies.
pub const ESTABLISHMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Message version. The only one defined.
const VERSION: u8 = 0x01;

/// The fixed part of every message: size, version, command.
const HEADER_LEN: usize = 4;

/// A source's opaque 16-byte identifier.
///
/// Newtyped rather than passed as `[u8; 16]` so it cannot be confused with a security
/// token or a PIN hash, both of which are also just bytes on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceId([u8; 16]);

impl SourceId {
    /// Wrap sixteen bytes.
    #[must_use]
    pub const fn new(raw: [u8; 16]) -> Self {
        Self(raw)
    }

    /// The bytes, as they travel.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl std::fmt::Display for SourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Why a `PIN_CHALLENGE` was answered the way it was ([MS-MICE] TLV `0x07`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PinResponseReason {
    /// The PIN matched.
    Accepted,
    /// It did not.
    WrongPin,
    /// The message could not be understood in this state.
    InvalidMessage,
}

impl PinResponseReason {
    const fn bits(self) -> u8 {
        match self {
            Self::Accepted => 0x00,
            Self::WrongPin => 0x01,
            Self::InvalidMessage => 0x02,
        }
    }

    const fn from_bits(raw: u8) -> Option<Self> {
        match raw {
            0x00 => Some(Self::Accepted),
            0x01 => Some(Self::WrongPin),
            0x02 => Some(Self::InvalidMessage),
            _ => None,
        }
    }
}

/// What a source asked for in a `SESSION_REQUEST` ([MS-MICE] TLV `0x05`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityOptions {
    /// Encrypt the RTP and UIBC streams with DTLS.
    pub dtls: bool,
    /// The sink displays a PIN the source must prove it was told.
    pub pin: bool,
}

impl SecurityOptions {
    const fn bits(self) -> u8 {
        (self.dtls as u8) | ((self.pin as u8) << 1)
    }

    /// Decode the options byte.
    ///
    /// # Errors
    /// [`MiceError::IllegalSecurityOptions`] for "PIN without encryption", which the spec
    /// forbids outright: the PIN exchange is carried *inside* the encrypted TLVs, so a
    /// source asking for one without the other is describing something that cannot happen.
    fn from_bits(raw: u8) -> Result<Self, MiceError> {
        let options = Self {
            dtls: raw & 0b1 != 0,
            pin: raw & 0b10 != 0,
        };
        if options.pin && !options.dtls {
            return Err(MiceError::IllegalSecurityOptions(raw));
        }
        Ok(options)
    }
}

/// One message on the control channel.
///
/// Parsed into named fields rather than kept as a TLV bag: a `SOURCE_READY` without an
/// RTSP port is not a message with a field missing, it is not a `SOURCE_READY`, and the
/// only place that distinction can be enforced cheaply is here (ground rule 1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MiceMessage {
    /// The source is listening for RTSP and wants the sink to connect.
    ///
    /// The friendly name is absent when a `SESSION_REQUEST` already carried it, which is
    /// why it is an `Option` rather than a `String` with an empty sentinel.
    SourceReady {
        /// The source's name, when this is the first message that carries one.
        friendly_name: Option<String>,
        /// Where to dial RTSP.
        rtsp_port: u16,
        /// Which source this is.
        source_id: SourceId,
    },
    /// Either side ending the projection.
    StopProjection {
        /// The name of whoever is stopping.
        friendly_name: Option<String>,
        /// Which source this is about.
        source_id: SourceId,
    },
    /// One leg of a DTLS handshake.
    SecurityHandshake {
        /// The opaque DTLS payload (RFC 6347).
        token: Bytes,
    },
    /// The source opening a secured session. Must be its first message when sent at all.
    SessionRequest {
        /// The source's name.
        friendly_name: String,
        /// Which source this is.
        source_id: SourceId,
        /// What it is asking for.
        security: SecurityOptions,
    },
    /// The source proving it was told the PIN.
    PinChallenge {
        /// Which source this is.
        source_id: SourceId,
        /// Salted SHA-256 of the PIN; see [`pin_hash`].
        challenge: Bytes,
    },
    /// The sink's verdict on that proof.
    PinResponse {
        /// Which source this is about.
        source_id: SourceId,
        /// The verdict.
        reason: PinResponseReason,
    },
}

impl MiceMessage {
    const SOURCE_READY: u8 = 0x01;
    const STOP_PROJECTION: u8 = 0x02;
    const SECURITY_HANDSHAKE: u8 = 0x03;
    const SESSION_REQUEST: u8 = 0x04;
    const PIN_CHALLENGE: u8 = 0x05;
    const PIN_RESPONSE: u8 = 0x06;

    /// A stable name, for logs and errors.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SourceReady { .. } => "SOURCE_READY",
            Self::StopProjection { .. } => "STOP_PROJECTION",
            Self::SecurityHandshake { .. } => "SECURITY_HANDSHAKE",
            Self::SessionRequest { .. } => "SESSION_REQUEST",
            Self::PinChallenge { .. } => "PIN_CHALLENGE",
            Self::PinResponse { .. } => "PIN_RESPONSE",
        }
    }

    const fn command(&self) -> u8 {
        match self {
            Self::SourceReady { .. } => Self::SOURCE_READY,
            Self::StopProjection { .. } => Self::STOP_PROJECTION,
            Self::SecurityHandshake { .. } => Self::SECURITY_HANDSHAKE,
            Self::SessionRequest { .. } => Self::SESSION_REQUEST,
            Self::PinChallenge { .. } => Self::PIN_CHALLENGE,
            Self::PinResponse { .. } => Self::PIN_RESPONSE,
        }
    }

    /// Encode, header included.
    ///
    /// # Errors
    /// [`MiceError::TooLong`] if the message will not fit the 16-bit size field, which a
    /// friendly name alone can do — the field caps at 520 bytes and the header at 65535.
    pub fn encode(&self) -> Result<Bytes, MiceError> {
        let mut body = BytesMut::new();
        match self {
            Self::SourceReady {
                friendly_name,
                rtsp_port,
                source_id,
            } => {
                if let Some(name) = friendly_name {
                    put_tlv(&mut body, Tlv::FRIENDLY_NAME, &encode_utf16le(name))?;
                }
                put_tlv(&mut body, Tlv::RTSP_PORT, &rtsp_port.to_be_bytes())?;
                put_tlv(&mut body, Tlv::SOURCE_ID, source_id.as_bytes())?;
            }
            Self::StopProjection {
                friendly_name,
                source_id,
            } => {
                if let Some(name) = friendly_name {
                    put_tlv(&mut body, Tlv::FRIENDLY_NAME, &encode_utf16le(name))?;
                }
                put_tlv(&mut body, Tlv::SOURCE_ID, source_id.as_bytes())?;
            }
            Self::SecurityHandshake { token } => {
                put_tlv(&mut body, Tlv::SECURITY_TOKEN, token)?;
            }
            Self::SessionRequest {
                friendly_name,
                source_id,
                security,
            } => {
                put_tlv(
                    &mut body,
                    Tlv::FRIENDLY_NAME,
                    &encode_utf16le(friendly_name),
                )?;
                put_tlv(&mut body, Tlv::SOURCE_ID, source_id.as_bytes())?;
                put_tlv(&mut body, Tlv::SECURITY_OPTIONS, &[security.bits()])?;
            }
            Self::PinChallenge {
                source_id,
                challenge,
            } => {
                put_tlv(&mut body, Tlv::SOURCE_ID, source_id.as_bytes())?;
                put_tlv(&mut body, Tlv::PIN_CHALLENGE, challenge)?;
            }
            Self::PinResponse { source_id, reason } => {
                put_tlv(&mut body, Tlv::SOURCE_ID, source_id.as_bytes())?;
                put_tlv(&mut body, Tlv::PIN_RESPONSE_REASON, &[reason.bits()])?;
            }
        }
        // "The size of the entire message, in bytes" — this header included. Confirmed
        // against the §4 fixture, whose 0x3D counts its own four bytes.
        let total = HEADER_LEN + body.len();
        let size = u16::try_from(total).map_err(|_| MiceError::TooLong(total))?;
        let mut out = BytesMut::with_capacity(total);
        out.put_u16(size);
        out.put_u8(VERSION);
        out.put_u8(self.command());
        out.extend_from_slice(&body);
        Ok(out.freeze())
    }

    /// How many bytes the message starting at `buf` claims to be, if its header is here.
    ///
    /// For the socket shell: a stream needs to know how much to wait for before it can
    /// hand anything to [`MiceMessage::decode`], and it should not have to know the header
    /// layout to find out.
    ///
    /// # Errors
    /// [`MiceError::Truncated`] when there is not yet a header, and
    /// [`MiceError::ShortMessage`] for a size that does not even cover one.
    pub fn framed_len(buf: &[u8]) -> Result<usize, MiceError> {
        let Some(header) = buf.get(..HEADER_LEN) else {
            return Err(MiceError::Truncated);
        };
        let size = usize::from(u16::from_be_bytes([header[0], header[1]]));
        if size < HEADER_LEN {
            return Err(MiceError::ShortMessage(size));
        }
        Ok(size)
    }

    /// Decode one whole message.
    ///
    /// # Errors
    /// [`MiceError`] for a short buffer, an unknown version or command, a TLV that runs
    /// off the end, or a message missing a field its command requires.
    pub fn decode(buf: &[u8]) -> Result<Self, MiceError> {
        let size = Self::framed_len(buf)?;
        let Some(message) = buf.get(..size) else {
            return Err(MiceError::Truncated);
        };
        let version = message[2];
        if version != VERSION {
            return Err(MiceError::UnknownVersion(version));
        }
        let command = message[3];
        let tlvs = Tlv::decode_all(&message[HEADER_LEN..])?;

        let source_id = || -> Result<SourceId, MiceError> {
            let value = tlvs.find(Tlv::SOURCE_ID)?;
            let raw: [u8; 16] = value
                .as_ref()
                .try_into()
                .map_err(|_| MiceError::BadTlvLength(Tlv::SOURCE_ID, value.len()))?;
            Ok(SourceId::new(raw))
        };
        let name = |required: bool| -> Result<Option<String>, MiceError> {
            match tlvs.get(Tlv::FRIENDLY_NAME) {
                Some(value) => Ok(Some(decode_utf16le(&value)?)),
                None if required => Err(MiceError::MissingTlv(Tlv::FRIENDLY_NAME)),
                None => Ok(None),
            }
        };

        match command {
            Self::SOURCE_READY => Ok(Self::SourceReady {
                friendly_name: name(false)?,
                // "Default 7236" is the spec's, and it is load-bearing rather than
                // tidy: a source that omits the TLV is not misbehaving, it is saying
                // "the usual port".
                rtsp_port: match tlvs.get(Tlv::RTSP_PORT) {
                    Some(value) => be_u16(Tlv::RTSP_PORT, &value)?,
                    None => DEFAULT_RTSP_PORT,
                },
                source_id: source_id()?,
            }),
            Self::STOP_PROJECTION => Ok(Self::StopProjection {
                friendly_name: name(false)?,
                source_id: source_id()?,
            }),
            Self::SECURITY_HANDSHAKE => Ok(Self::SecurityHandshake {
                token: tlvs.find(Tlv::SECURITY_TOKEN)?,
            }),
            Self::SESSION_REQUEST => Ok(Self::SessionRequest {
                friendly_name: name(true)?.unwrap_or_default(),
                source_id: source_id()?,
                security: SecurityOptions::from_bits(one_byte(
                    Tlv::SECURITY_OPTIONS,
                    &tlvs.find(Tlv::SECURITY_OPTIONS)?,
                )?)?,
            }),
            Self::PIN_CHALLENGE => Ok(Self::PinChallenge {
                source_id: source_id()?,
                challenge: tlvs.find(Tlv::PIN_CHALLENGE)?,
            }),
            Self::PIN_RESPONSE => {
                let raw = one_byte(
                    Tlv::PIN_RESPONSE_REASON,
                    &tlvs.find(Tlv::PIN_RESPONSE_REASON)?,
                )?;
                Ok(Self::PinResponse {
                    source_id: source_id()?,
                    reason: PinResponseReason::from_bits(raw)
                        .ok_or(MiceError::UnknownPinReason(raw))?,
                })
            }
            other => Err(MiceError::UnknownCommand(other)),
        }
    }
}

/// The TLV types, and a decoded array of them.
struct Tlv;

impl Tlv {
    const FRIENDLY_NAME: u8 = 0x00;
    const RTSP_PORT: u8 = 0x02;
    const SOURCE_ID: u8 = 0x03;
    const SECURITY_TOKEN: u8 = 0x04;
    const SECURITY_OPTIONS: u8 = 0x05;
    const PIN_CHALLENGE: u8 = 0x06;
    const PIN_RESPONSE_REASON: u8 = 0x07;

    fn decode_all(mut buf: &[u8]) -> Result<Tlvs, MiceError> {
        let mut out = Vec::new();
        while !buf.is_empty() {
            let Some(header) = buf.get(..3) else {
                return Err(MiceError::Truncated);
            };
            let kind = header[0];
            let len = usize::from(u16::from_be_bytes([header[1], header[2]]));
            // "Length ≥ 1" is the spec's, and a zero-length TLV would otherwise be a
            // legal-looking way to spin this loop forever.
            if len == 0 {
                return Err(MiceError::BadTlvLength(kind, 0));
            }
            let Some(value) = buf.get(3..3 + len) else {
                return Err(MiceError::Truncated);
            };
            out.push((kind, Bytes::copy_from_slice(value)));
            buf = &buf[3 + len..];
        }
        Ok(Tlvs(out))
    }
}

/// A decoded TLV array, in the order it arrived.
struct Tlvs(Vec<(u8, Bytes)>);

impl Tlvs {
    fn get(&self, kind: u8) -> Option<Bytes> {
        self.0
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, v)| v.clone())
    }

    fn find(&self, kind: u8) -> Result<Bytes, MiceError> {
        self.get(kind).ok_or(MiceError::MissingTlv(kind))
    }
}

fn put_tlv(buf: &mut BytesMut, kind: u8, value: &[u8]) -> Result<(), MiceError> {
    let len = u16::try_from(value.len()).map_err(|_| MiceError::TooLong(value.len()))?;
    buf.put_u8(kind);
    buf.put_u16(len);
    buf.extend_from_slice(value);
    Ok(())
}

fn be_u16(kind: u8, value: &Bytes) -> Result<u16, MiceError> {
    let raw: [u8; 2] = value
        .as_ref()
        .try_into()
        .map_err(|_| MiceError::BadTlvLength(kind, value.len()))?;
    Ok(u16::from_be_bytes(raw))
}

fn one_byte(kind: u8, value: &Bytes) -> Result<u8, MiceError> {
    match value.first() {
        Some(byte) if value.len() == 1 => Ok(*byte),
        _ => Err(MiceError::BadTlvLength(kind, value.len())),
    }
}

/// UTF-16LE, no NUL — the friendly name's encoding, and the one place in this protocol
/// where a length is *not* big-endian.
fn encode_utf16le(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn decode_utf16le(value: &Bytes) -> Result<String, MiceError> {
    if !value.len().is_multiple_of(2) {
        return Err(MiceError::BadTlvLength(Tlv::FRIENDLY_NAME, value.len()));
    }
    let units: Vec<u16> = value
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        // A trailing NUL is forbidden by the spec and sent anyway by some stacks; a name
        // ending in a control character is worse on a two-metre screen than a lenient
        // parser is here.
        .filter(|unit| *unit != 0)
        .collect();
    String::from_utf16(&units).map_err(|_| MiceError::BadFriendlyName)
}

/// The salted PIN hash of [MS-MICE] §3.1.5.6.1.
///
/// ASCII PIN (no NUL) followed by the **binary** sender address, then SHA-256 — and each
/// side hashes with *its own* address, which is the part that makes this asymmetric and is
/// easy to get wrong in a way that only fails against a real Windows source.
///
/// Present and tested against the spec's own vectors even though the PIN flow is not
/// offered ([`Capability::insecure`]): it is thirty lines, the vectors are free, and
/// whoever turns the flow on should find this already correct rather than write it under
/// time pressure against a handset.
#[must_use]
pub fn pin_hash(pin: &str, address: std::net::IpAddr) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pin.as_bytes());
    match address {
        std::net::IpAddr::V4(v4) => hasher.update(v4.octets()),
        std::net::IpAddr::V6(v6) => hasher.update(v6.octets()),
    }
    hasher.finalize().into()
}

/// The capability byte of the WSC Vendor Extension attribute ([MS-MICE] §2.2.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    /// Whether MICE is offered at all. With this clear Windows *"MUST fall back to using
    /// standard Miracast"*, which is the honest thing to advertise from a build that
    /// cannot serve it.
    pub mice: bool,
    /// Whether the RTP and UIBC streams can be DTLS-encrypted.
    pub encryption: bool,
    /// Whether the sink can display a PIN. Requires `encryption`.
    pub pin: bool,
}

impl Capability {
    /// The version this protocol is at. Three bits wide.
    const VERSION: u8 = 1;

    /// MICE on, nothing else — the spec's own example byte, `0x05`.
    ///
    /// What this build advertises, and the reason the secured flows can be refused rather
    /// than half-implemented: a source reads this before it chooses an entry point, so
    /// declining here is declining at the only moment it costs nothing.
    #[must_use]
    pub const fn insecure() -> Self {
        Self {
            mice: true,
            encryption: false,
            pin: false,
        }
    }

    /// The byte as it travels.
    #[must_use]
    pub const fn bits(self) -> u8 {
        (self.mice as u8)
            | ((self.encryption as u8) << 1)
            | (Self::VERSION << 2)
            | ((self.pin as u8) << 5)
    }
}

/// The WSC Vendor Extension attribute a MICE sink puts in its Beacons and Probe Responses.
///
/// This is how Windows learns MICE is on offer — the mDNS registration alone is not
/// enough, because the source looks here first (§1.10). `host_name` must be *unqualified*:
/// the spec says a sink whose host name contains a period "MUST NOT be used", so a name
/// with one in it is refused here rather than advertised and quietly ignored.
///
/// # Errors
/// [`MiceError::QualifiedHostName`] for a name containing a period, and
/// [`MiceError::TooLong`] for one that will not fit its length field.
pub fn vendor_extension(capability: Capability, host_name: &str) -> Result<Bytes, MiceError> {
    /// WPS Vendor Extension attribute id.
    const VENDOR_EXTENSION: u16 = 0x1049;
    /// The WPS OUI, which identifies whose extension this is.
    const WPS_OUI: [u8; 3] = [0x00, 0x01, 0x37];
    const ATTR_CAPABILITY: u16 = 0x2001;
    const ATTR_HOST_NAME: u16 = 0x2002;

    if host_name.contains('.') {
        return Err(MiceError::QualifiedHostName(host_name.to_string()));
    }
    let mut body = BytesMut::new();
    body.extend_from_slice(&WPS_OUI);
    body.put_u16(ATTR_CAPABILITY);
    body.put_u16(1);
    body.put_u8(capability.bits());
    body.put_u16(ATTR_HOST_NAME);
    let name_len =
        u16::try_from(host_name.len()).map_err(|_| MiceError::TooLong(host_name.len()))?;
    body.put_u16(name_len);
    body.extend_from_slice(host_name.as_bytes());

    let mut out = BytesMut::with_capacity(4 + body.len());
    out.put_u16(VENDOR_EXTENSION);
    let len = u16::try_from(body.len()).map_err(|_| MiceError::TooLong(body.len()))?;
    out.put_u16(len);
    out.extend_from_slice(&body);
    Ok(out.freeze())
}

/// Where a control channel has got to.
///
/// Every transition is driven by a decoded message, and the spec's own rule for anything
/// that does not fit is blunt: *"any unexpected or unknown message for the current state"*
/// tears the channel down. So there is no "ignore and hope" arm anywhere below.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MiceState {
    /// Connected, nothing said yet.
    Fresh,
    /// `SOURCE_READY` seen: we know where to dial and the projection is on.
    Projecting {
        /// The source's name, if it gave one.
        friendly_name: Option<String>,
        /// Where to dial RTSP.
        rtsp_port: u16,
        /// Which source this is.
        source_id: SourceId,
    },
    /// Finished, for the reason given. Terminal.
    Closed(CloseReason),
}

/// Why a control channel ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CloseReason {
    /// The source said so.
    SourceStopped,
    /// RTSP was not established inside [`ESTABLISHMENT_TIMEOUT`].
    EstablishmentTimeout,
    /// The source asked for something this sink does not advertise.
    Unsupported(&'static str),
    /// A message arrived that this state has no meaning for.
    Unexpected(&'static str),
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceStopped => f.write_str("the source stopped projecting"),
            Self::EstablishmentTimeout => {
                f.write_str("RTSP was not established before the session timer expired")
            }
            Self::Unsupported(what) => {
                write!(f, "the source asked for {what}, which is not offered")
            }
            Self::Unexpected(what) => {
                write!(f, "a {what} arrived that this state has no meaning for")
            }
        }
    }
}

/// What the socket shell should do about a message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MiceOutput {
    /// Write this back on the control channel.
    Send(MiceMessage),
    /// Dial RTSP at this port on the peer that opened the channel, and run a session.
    Project {
        /// The port the source named, or [`DEFAULT_RTSP_PORT`].
        rtsp_port: u16,
        /// The source's name, for the panel.
        friendly_name: Option<String>,
    },
    /// Close the control channel, and end any session it started.
    Close(CloseReason),
}

/// One MICE control channel, as a pure state machine.
///
/// `fn(state, message) -> (state, outputs)` per ground rule 3: no socket, no timer, no
/// clock. The whole projection flow is therefore driven from the spec's own fixtures in
/// the tests below.
#[derive(Debug)]
pub struct MiceSession {
    state: MiceState,
    /// The sink's name, for the messages that carry one.
    friendly_name: String,
    /// How long this channel has existed without RTSP being established.
    since_open: Duration,
    /// Whether RTSP came up, which is what cancels the establishment timer.
    established: bool,
}

impl MiceSession {
    /// A fresh channel for a peer that has just connected.
    #[must_use]
    pub fn new(friendly_name: impl Into<String>) -> Self {
        Self {
            state: MiceState::Fresh,
            friendly_name: friendly_name.into(),
            since_open: Duration::ZERO,
            established: false,
        }
    }

    /// Where the channel has got to.
    #[must_use]
    pub const fn state(&self) -> &MiceState {
        &self.state
    }

    /// Note that RTSP came up, which cancels the establishment timer ([MS-MICE] §3.1.5.8).
    pub fn rtsp_established(&mut self) {
        self.established = true;
    }

    /// Advance the establishment timer.
    ///
    /// Time is passed in rather than read, so the timeout is asserted against a number
    /// rather than against how fast the host is (ground rule 3, and #156's lesson).
    pub fn tick(&mut self, elapsed: Duration) -> Vec<MiceOutput> {
        if self.established || matches!(self.state, MiceState::Closed(_)) {
            return Vec::new();
        }
        self.since_open = self.since_open.saturating_add(elapsed);
        if self.since_open < ESTABLISHMENT_TIMEOUT {
            return Vec::new();
        }
        self.close(CloseReason::EstablishmentTimeout)
    }

    /// Feed one decoded message in.
    pub fn on_message(&mut self, message: &MiceMessage) -> Vec<MiceOutput> {
        match (&self.state, message) {
            // The whole no-security flow, in one transition: the source says where it is
            // listening and the sink dials it. Everything after this is ordinary WFD RTSP.
            (
                MiceState::Fresh,
                MiceMessage::SourceReady {
                    friendly_name,
                    rtsp_port,
                    source_id,
                },
            ) => {
                self.state = MiceState::Projecting {
                    friendly_name: friendly_name.clone(),
                    rtsp_port: *rtsp_port,
                    source_id: *source_id,
                };
                vec![MiceOutput::Project {
                    rtsp_port: *rtsp_port,
                    friendly_name: friendly_name.clone(),
                }]
            }

            // Either side may stop, from either state, and it is the one message that is
            // never a surprise.
            (_, MiceMessage::StopProjection { .. }) => self.close(CloseReason::SourceStopped),

            // Asked for something we did not advertise. Refused in one message rather than
            // half-answered: a DTLS handshake this sink cannot finish is a projection that
            // hangs for the full establishment timer instead of failing now.
            (_, MiceMessage::SessionRequest { security, .. }) if security.dtls => {
                self.close(CloseReason::Unsupported("DTLS stream encryption"))
            }
            (_, MiceMessage::SecurityHandshake { .. }) => {
                self.close(CloseReason::Unsupported("a DTLS security handshake"))
            }
            (_, MiceMessage::PinChallenge { .. }) => {
                self.close(CloseReason::Unsupported("PIN-protected projection"))
            }

            // A `SESSION_REQUEST` with no security at all is legal and adds nothing: it
            // names the source before `SOURCE_READY` does. Accepted so the name is not
            // lost, and it stays `Fresh` because the message that matters has not arrived.
            (MiceState::Fresh, MiceMessage::SessionRequest { .. }) => Vec::new(),

            // Everything else. The spec is blunt about this and the bluntness is the
            // feature: a state with no meaning for a message is a disagreement, and a
            // channel carrying a disagreement is worse than no channel.
            (_, other) => self.close(CloseReason::Unexpected(other.name())),
        }
    }

    /// Stop projecting, and tell the source.
    #[must_use]
    pub fn stop(&mut self) -> Vec<MiceOutput> {
        let source_id = match &self.state {
            MiceState::Projecting { source_id, .. } => Some(*source_id),
            _ => None,
        };
        let mut out = Vec::new();
        if let Some(source_id) = source_id {
            out.push(MiceOutput::Send(MiceMessage::StopProjection {
                friendly_name: Some(self.friendly_name.clone()),
                source_id,
            }));
        }
        out.extend(self.close(CloseReason::SourceStopped));
        out
    }

    fn close(&mut self, reason: CloseReason) -> Vec<MiceOutput> {
        if matches!(self.state, MiceState::Closed(_)) {
            return Vec::new();
        }
        self.state = MiceState::Closed(reason.clone());
        vec![MiceOutput::Close(reason)]
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// [MS-MICE] §4's Source ID, which the golden `SOURCE_READY` carries.
    const FIXTURE_SOURCE_ID: [u8; 16] = [
        0x91, 0xF4, 0xAB, 0xE9, 0xEF, 0xF5, 0x46, 0x4A, 0xAE, 0xE2, 0x69, 0x72, 0x2A, 0xED, 0x11,
        0xB5,
    ];

    /// The golden `SOURCE_READY` from [MS-MICE] §4, byte for byte.
    fn source_ready_fixture() -> Vec<u8> {
        let mut out = vec![0x00, 0x3D, 0x01, 0x01];
        // Friendly name: "Dummy1-Kabylake", UTF-16LE, 30 bytes.
        out.extend_from_slice(&[0x00, 0x00, 0x1E]);
        out.extend_from_slice(&encode_utf16le("Dummy1-Kabylake"));
        // RTSP port 7236.
        out.extend_from_slice(&[0x02, 0x00, 0x02, 0x1C, 0x44]);
        // Source ID.
        out.extend_from_slice(&[0x03, 0x00, 0x10]);
        out.extend_from_slice(&FIXTURE_SOURCE_ID);
        out
    }

    #[test]
    fn the_specs_own_source_ready_decodes() {
        let bytes = source_ready_fixture();
        assert_eq!(bytes.len(), 0x3D, "the fixture is the length it claims");
        let message = MiceMessage::decode(&bytes).expect("the spec's own message");
        assert_eq!(
            message,
            MiceMessage::SourceReady {
                friendly_name: Some("Dummy1-Kabylake".into()),
                rtsp_port: 7236,
                source_id: SourceId::new(FIXTURE_SOURCE_ID),
            }
        );
    }

    #[test]
    fn the_size_field_counts_its_own_header() {
        // The one arithmetic fact in the whole message layout, and the one most likely to
        // be got wrong: 4 + (3+30) + (3+2) + (3+16) = 61 = 0x3D.
        let bytes = source_ready_fixture();
        assert_eq!(MiceMessage::framed_len(&bytes).unwrap(), bytes.len());
        let re_encoded = MiceMessage::decode(&bytes).unwrap().encode().unwrap();
        assert_eq!(
            re_encoded.as_ref(),
            bytes.as_slice(),
            "the spec's message must survive a round trip byte for byte"
        );
    }

    #[test]
    fn the_specs_own_vendor_extension_encodes() {
        let attribute = vendor_extension(Capability::insecure(), "Dummy1-Kabylake").unwrap();
        let expected: Vec<u8> = {
            let mut v = vec![0x10, 0x49, 0x00, 0x1B, 0x00, 0x01, 0x37];
            v.extend_from_slice(&[0x20, 0x01, 0x00, 0x01, 0x05]);
            v.extend_from_slice(&[0x20, 0x02, 0x00, 0x0F]);
            v.extend_from_slice(b"Dummy1-Kabylake");
            v
        };
        assert_eq!(attribute.as_ref(), expected.as_slice());
    }

    #[test]
    fn the_capability_byte_is_the_specs_example() {
        // 0x05 = MICE on, encryption off, version 1, no PIN. Version lives in bits 4:2, so
        // getting the shift wrong produces a byte that still looks plausible.
        assert_eq!(Capability::insecure().bits(), 0x05);
    }

    #[test]
    fn a_host_name_with_a_period_is_refused_rather_than_advertised() {
        // "A Sink having a host name that contains the period ('.') character MUST NOT be
        // used" — so a qualified name is a configuration error to report, not a string to
        // put in a beacon and hope about.
        assert!(matches!(
            vendor_extension(Capability::insecure(), "panel.local"),
            Err(MiceError::QualifiedHostName(_))
        ));
    }

    #[test]
    fn the_pin_vectors_from_the_spec_match() {
        // §3.1.5.6.1's two ready-made vectors. The flow is not offered, but the hash is
        // free to pin and is the part that only fails against a real Windows source.
        let v4 = pin_hash("12345678", "192.0.2.100".parse().unwrap());
        assert_eq!(
            hex(&v4),
            "605409f832308ad0b893a7f91be42b264c7372b36e9077506e1b4cc183de79da"
        );
        let v6 = pin_hash("98765432", "2001:db8:1f::4242".parse().unwrap());
        assert_eq!(
            hex(&v6),
            "b3452b2c46c83d28d8d464b6697a81d1af3f356107e1d0731ea9bb183803f9c7"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn a_source_ready_starts_a_projection_at_the_port_it_named() {
        let mut session = MiceSession::new("castaway");
        let message = MiceMessage::decode(&source_ready_fixture()).unwrap();
        assert_eq!(
            session.on_message(&message),
            vec![MiceOutput::Project {
                rtsp_port: 7236,
                friendly_name: Some("Dummy1-Kabylake".into()),
            }]
        );
        assert!(matches!(session.state(), MiceState::Projecting { .. }));
    }

    #[test]
    fn a_source_ready_with_no_port_takes_the_default() {
        // Not a malformed message: omitting the TLV is how a source says "the usual port".
        let message = MiceMessage::SourceReady {
            friendly_name: None,
            rtsp_port: DEFAULT_RTSP_PORT,
            source_id: SourceId::new(FIXTURE_SOURCE_ID),
        };
        let mut bytes = message.encode().unwrap().to_vec();
        // Strip the RTSP port TLV by hand and fix the size, which is the shape a source
        // that omits it actually produces.
        let without: Vec<u8> = {
            let mut v = vec![0x00, 0x00, 0x01, 0x01, 0x03, 0x00, 0x10];
            v.extend_from_slice(&FIXTURE_SOURCE_ID);
            let len = u16::try_from(v.len()).unwrap();
            v[0..2].copy_from_slice(&len.to_be_bytes());
            v
        };
        bytes.clear();
        bytes.extend_from_slice(&without);
        assert_eq!(
            MiceMessage::decode(&bytes).unwrap(),
            MiceMessage::SourceReady {
                friendly_name: None,
                rtsp_port: DEFAULT_RTSP_PORT,
                source_id: SourceId::new(FIXTURE_SOURCE_ID),
            }
        );
    }

    #[test]
    fn a_secured_session_is_refused_in_one_message_rather_than_hung_on() {
        // We advertise `0x05`, so a source asking for DTLS has ignored what it read. The
        // alternative to refusing now is a handshake we cannot finish and a projection
        // that dies at the establishment timer thirty seconds later, with nothing said.
        let mut session = MiceSession::new("castaway");
        let out = session.on_message(&MiceMessage::SessionRequest {
            friendly_name: "Dummy1-Kabylake".into(),
            source_id: SourceId::new(FIXTURE_SOURCE_ID),
            security: SecurityOptions {
                dtls: true,
                pin: false,
            },
        });
        assert_eq!(
            out,
            vec![MiceOutput::Close(CloseReason::Unsupported(
                "DTLS stream encryption"
            ))]
        );
    }

    #[test]
    fn a_pin_without_encryption_is_not_a_thing_a_source_can_ask_for() {
        // The PIN exchange travels inside the encrypted TLVs, so bit 1 without bit 0 is
        // describing something that cannot happen. Refused at the parse, which is the only
        // place it can be refused once.
        let mut bytes = vec![0x00, 0x00, 0x01, 0x04];
        bytes.extend_from_slice(&[0x00, 0x00, 0x02]);
        bytes.extend_from_slice(&encode_utf16le("x"));
        bytes.extend_from_slice(&[0x03, 0x00, 0x10]);
        bytes.extend_from_slice(&FIXTURE_SOURCE_ID);
        bytes.extend_from_slice(&[0x05, 0x00, 0x01, 0b10]);
        let len = u16::try_from(bytes.len()).unwrap();
        bytes[0..2].copy_from_slice(&len.to_be_bytes());
        assert!(matches!(
            MiceMessage::decode(&bytes),
            Err(MiceError::IllegalSecurityOptions(0b10))
        ));
    }

    #[test]
    fn an_unexpected_message_tears_the_channel_down() {
        // The spec's own rule, and the reason this state machine has no "ignore" arm: a
        // state with no meaning for a message is a disagreement between the two ends.
        let mut session = MiceSession::new("castaway");
        let out = session.on_message(&MiceMessage::PinResponse {
            source_id: SourceId::new(FIXTURE_SOURCE_ID),
            reason: PinResponseReason::Accepted,
        });
        assert_eq!(
            out,
            vec![MiceOutput::Close(CloseReason::Unexpected("PIN_RESPONSE"))]
        );
        assert!(matches!(session.state(), MiceState::Closed(_)));
    }

    #[test]
    fn a_channel_that_never_establishes_rtsp_is_given_up_on() {
        let mut session = MiceSession::new("castaway");
        session.on_message(&MiceMessage::decode(&source_ready_fixture()).unwrap());
        for _ in 0..29 {
            assert!(session.tick(Duration::from_secs(1)).is_empty());
        }
        assert_eq!(
            session.tick(Duration::from_secs(1)),
            vec![MiceOutput::Close(CloseReason::EstablishmentTimeout)]
        );
    }

    #[test]
    fn establishing_rtsp_cancels_the_timer() {
        let mut session = MiceSession::new("castaway");
        session.on_message(&MiceMessage::decode(&source_ready_fixture()).unwrap());
        session.rtsp_established();
        assert!(
            session.tick(ESTABLISHMENT_TIMEOUT * 10).is_empty(),
            "a projection that is running must not be timed out"
        );
    }

    #[test]
    fn stopping_tells_the_source_before_it_closes() {
        let mut session = MiceSession::new("castaway");
        session.on_message(&MiceMessage::decode(&source_ready_fixture()).unwrap());
        let out = session.stop();
        assert_eq!(
            out,
            vec![
                MiceOutput::Send(MiceMessage::StopProjection {
                    friendly_name: Some("castaway".into()),
                    source_id: SourceId::new(FIXTURE_SOURCE_ID),
                }),
                MiceOutput::Close(CloseReason::SourceStopped),
            ]
        );
    }

    #[test]
    fn every_message_survives_its_own_encoding() {
        let source_id = SourceId::new(FIXTURE_SOURCE_ID);
        let messages = [
            MiceMessage::SourceReady {
                friendly_name: Some("café \u{1f4fa}".into()),
                rtsp_port: 7236,
                source_id,
            },
            MiceMessage::StopProjection {
                friendly_name: None,
                source_id,
            },
            MiceMessage::SecurityHandshake {
                token: Bytes::from_static(&[1, 2, 3, 4]),
            },
            MiceMessage::SessionRequest {
                friendly_name: "x".into(),
                source_id,
                security: SecurityOptions {
                    dtls: true,
                    pin: true,
                },
            },
            MiceMessage::PinChallenge {
                source_id,
                challenge: Bytes::from_static(&[0xab; 32]),
            },
            MiceMessage::PinResponse {
                source_id,
                reason: PinResponseReason::WrongPin,
            },
        ];
        for message in messages {
            let encoded = message.encode().expect("encodes");
            assert_eq!(
                MiceMessage::decode(&encoded).expect("decodes"),
                message,
                "round trip lost something"
            );
        }
    }

    #[test]
    fn a_zero_length_tlv_is_refused_rather_than_looped_on() {
        // Length ≥ 1 is the spec's rule and the parser's own safety: a zero-length TLV
        // advances the cursor by three bytes and could otherwise be repeated forever.
        let bytes = vec![0x00, 0x07, 0x01, 0x01, 0x00, 0x00, 0x00];
        assert!(matches!(
            MiceMessage::decode(&bytes),
            Err(MiceError::BadTlvLength(0x00, 0))
        ));
    }

    #[test]
    fn a_tlv_running_off_the_end_is_truncation_not_a_panic() {
        let bytes = vec![0x00, 0x0A, 0x01, 0x01, 0x03, 0x00, 0x10, 0xff, 0xff, 0xff];
        assert!(matches!(
            MiceMessage::decode(&bytes),
            Err(MiceError::Truncated)
        ));
    }

    #[test]
    fn random_bytes_do_not_panic() {
        // The control channel faces the LAN.
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let len = (seed >> 33) as usize % 64;
            let bytes: Vec<u8> = (0..len)
                .map(|i| ((seed >> (i % 8 * 8)) & 0xff) as u8)
                .collect();
            let _ = MiceMessage::decode(&bytes);
            let _ = MiceMessage::framed_len(&bytes);
        }
    }
}
