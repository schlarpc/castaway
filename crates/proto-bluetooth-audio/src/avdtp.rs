//! AVDTP signaling: message framing, signal identifiers, and the sink's stream endpoints.

use bytes::{BufMut, Bytes, BytesMut};

use crate::codec::CodecCapability;
use crate::error::AudioError;

/// AVDTP signal identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Signal {
    /// Enumerate the peer's stream endpoints.
    Discover,
    /// Read one endpoint's capabilities (AVDTP 1.0 form).
    GetCapabilities,
    /// Choose a configuration for an endpoint.
    SetConfiguration,
    /// Read the configuration in force.
    GetConfiguration,
    /// Change configuration without tearing down.
    Reconfigure,
    /// Open the media transport channel.
    Open,
    /// Begin streaming.
    Start,
    /// Close the stream.
    Close,
    /// Pause streaming without closing.
    Suspend,
    /// Abandon the stream immediately.
    Abort,
    /// Content-protection passthrough.
    SecurityControl,
    /// Read capabilities including AVDTP 1.3 additions (delay reporting).
    GetAllCapabilities,
    /// Report the sink's rendering latency back to the source.
    DelayReport,
}

impl Signal {
    const fn id(self) -> u8 {
        match self {
            Self::Discover => 0x01,
            Self::GetCapabilities => 0x02,
            Self::SetConfiguration => 0x03,
            Self::GetConfiguration => 0x04,
            Self::Reconfigure => 0x05,
            Self::Open => 0x06,
            Self::Start => 0x07,
            Self::Close => 0x08,
            Self::Suspend => 0x09,
            Self::Abort => 0x0A,
            Self::SecurityControl => 0x0B,
            Self::GetAllCapabilities => 0x0C,
            Self::DelayReport => 0x0D,
        }
    }

    const fn from_id(id: u8) -> Result<Self, AudioError> {
        Ok(match id {
            0x01 => Self::Discover,
            0x02 => Self::GetCapabilities,
            0x03 => Self::SetConfiguration,
            0x04 => Self::GetConfiguration,
            0x05 => Self::Reconfigure,
            0x06 => Self::Open,
            0x07 => Self::Start,
            0x08 => Self::Close,
            0x09 => Self::Suspend,
            0x0A => Self::Abort,
            0x0B => Self::SecurityControl,
            0x0C => Self::GetAllCapabilities,
            0x0D => Self::DelayReport,
            other => return Err(AudioError::UnknownSignal(other)),
        })
    }

    /// A stable name for logs and errors.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::GetCapabilities => "get_capabilities",
            Self::SetConfiguration => "set_configuration",
            Self::GetConfiguration => "get_configuration",
            Self::Reconfigure => "reconfigure",
            Self::Open => "open",
            Self::Start => "start",
            Self::Close => "close",
            Self::Suspend => "suspend",
            Self::Abort => "abort",
            Self::SecurityControl => "security_control",
            Self::GetAllCapabilities => "get_all_capabilities",
            Self::DelayReport => "delay_report",
        }
    }
}

/// Whether a message is a command or which kind of response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageType {
    /// A command from the peer.
    Command,
    /// "I don't implement that signal at all."
    GeneralReject,
    /// Accepted.
    ResponseAccept,
    /// Refused, with an error code.
    ResponseReject,
}

impl MessageType {
    const fn bits(self) -> u8 {
        match self {
            Self::Command => 0b00,
            Self::GeneralReject => 0b01,
            Self::ResponseAccept => 0b10,
            Self::ResponseReject => 0b11,
        }
    }

    const fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => Self::Command,
            0b01 => Self::GeneralReject,
            0b10 => Self::ResponseAccept,
            _ => Self::ResponseReject,
        }
    }
}

/// A stream endpoint identifier. Six bits, and zero is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Seid(u8);

impl Seid {
    /// Largest legal identifier.
    pub const MAX: u8 = 0x3E;

    /// Build a SEID, rejecting 0 and anything above [`Seid::MAX`].
    ///
    /// # Errors
    /// [`AudioError::InvalidSeid`] outside `1..=0x3E`.
    pub const fn new(raw: u8) -> Result<Self, AudioError> {
        if raw == 0 || raw > Self::MAX {
            return Err(AudioError::InvalidSeid(raw));
        }
        Ok(Self(raw))
    }

    /// The raw value.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// The value as it sits in a wire byte — shifted left two, because the low bits
    /// carry the in-use flag. Writing the SEID unshifted is a classic AVDTP bug that
    /// addresses endpoint `n/4`.
    #[must_use]
    pub const fn shifted(self) -> u8 {
        self.0 << 2
    }

    /// Parse from a wire byte carrying the SEID in its top six bits.
    ///
    /// # Errors
    /// [`AudioError::InvalidSeid`] if the extracted value is out of range.
    pub const fn from_shifted(byte: u8) -> Result<Self, AudioError> {
        Self::new(byte >> 2)
    }
}

/// One AVDTP message, already reassembled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Correlates a response with its command.
    pub transaction: u8,
    /// Command or response.
    pub message_type: MessageType,
    /// Which signal.
    pub signal: Signal,
    /// Signal-specific payload.
    pub payload: Bytes,
}

impl Message {
    /// The refusal for a signal we do not implement at all.
    ///
    /// Encoded straight from the two header bytes rather than from a parsed `Message`,
    /// because the case this exists for is precisely the one where parsing failed: an
    /// unknown signal id has no `Signal` to put in a struct. AVDTP's General Reject is
    /// the header alone — transaction label, message type 0b01, and the signal id being
    /// refused, with no payload (BlueZ `avdtp_unknown_cmd`, which sends `NULL, 0`).
    ///
    /// Refusing matters because AVDTP has no "ignored": a peer that gets silence waits
    /// out its signal timeout, retries, and typically aborts the link.
    #[must_use]
    pub fn general_reject(transaction: u8, signal_id: u8) -> Bytes {
        let mut buf = BytesMut::with_capacity(2);
        buf.put_u8((transaction << 4) | MessageType::GeneralReject.bits());
        buf.put_u8(signal_id);
        buf.freeze()
    }

    /// Read the header of a message we could not parse: its transaction label, whether it
    /// is a command, and the signal id it named.
    ///
    /// `None` for anything too short to have a header, where there is nothing to address
    /// a refusal to.
    #[must_use]
    pub fn refusable_header(buf: &[u8]) -> Option<(u8, u8)> {
        let header = *buf.first()?;
        let signal_id = *buf.get(1)?;
        // Only a *command* is owed a refusal; refusing a response would be a message the
        // peer has no transaction open for.
        (MessageType::from_bits(header) == MessageType::Command).then_some((header >> 4, signal_id))
    }

    /// Build a command.
    #[must_use]
    pub fn command(transaction: u8, signal: Signal, payload: Bytes) -> Self {
        Self {
            transaction,
            message_type: MessageType::Command,
            signal,
            payload,
        }
    }

    /// Build an accept response to `command`.
    #[must_use]
    pub fn accept(command: &Self, payload: Bytes) -> Self {
        Self {
            transaction: command.transaction,
            message_type: MessageType::ResponseAccept,
            signal: command.signal,
            payload,
        }
    }

    /// Build a reject response carrying an AVDTP error code.
    #[must_use]
    pub fn reject(command: &Self, code: u8) -> Self {
        Self {
            transaction: command.transaction,
            message_type: MessageType::ResponseReject,
            signal: command.signal,
            payload: Bytes::copy_from_slice(&[code]),
        }
    }

    /// Build a `DELAYREPORT` command: how long this sink holds audio before it is heard.
    ///
    /// **SNK→SRC, which is why this is the one command a sink originates.** Everything
    /// else on this channel is the phone asking and us answering; this is us telling the
    /// phone a number only we know, so that it can delay its *video* to match. Without
    /// it the phone assumes zero and video is out of lip-sync by the whole depth of our
    /// output path, with nothing anywhere reporting a fault (#89).
    ///
    /// The payload is the ACP SEID — *our* endpoint, the one the source configured —
    /// followed by the delay in tenths of a millisecond, big-endian (AVDTP 1.3 §8.19).
    #[must_use]
    pub fn delay_report(transaction: u8, seid: Seid, delay: SinkDelay) -> Self {
        let mut payload = BytesMut::with_capacity(3);
        payload.put_u8(seid.shifted());
        payload.put_u16(delay.tenths_of_a_millisecond());
        Self::command(transaction, Signal::DelayReport, payload.freeze())
    }

    /// Encode as a single-packet message.
    ///
    /// Fragmentation (start/continue/end packets) is deliberately not implemented on the
    /// send side: every message a sink emits fits comfortably inside the smallest L2CAP
    /// MTU, and a fragmenting encoder that is never exercised is a liability rather than
    /// an asset. The *decoder* still recognises fragmented input — see
    /// [`Message::decode`].
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(2 + self.payload.len());
        // transaction label (4) | packet type = single (2) | message type (2)
        buf.put_u8((self.transaction << 4) | self.message_type.bits());
        buf.put_u8(self.signal.id());
        buf.extend_from_slice(&self.payload);
        buf.freeze()
    }

    /// Decode a single-packet message.
    ///
    /// # Errors
    /// [`AudioError::Truncated`] if shorter than the header,
    /// [`AudioError::UnknownSignal`] for an identifier we don't model, or
    /// [`AudioError::BadMediaPacket`] for a fragmented message, which we do not
    /// reassemble because nothing a phone sends a sink is large enough to need it.
    pub fn decode(buf: &[u8]) -> Result<Self, AudioError> {
        if buf.len() < 2 {
            return Err(AudioError::Truncated {
                what: "avdtp header",
                need: 2,
                have: buf.len(),
            });
        }
        let packet_type = (buf[0] >> 2) & 0b11;
        if packet_type != 0 {
            return Err(AudioError::BadMediaPacket(
                "fragmented AVDTP signaling is not supported",
            ));
        }
        Ok(Self {
            transaction: buf[0] >> 4,
            message_type: MessageType::from_bits(buf[0]),
            signal: Signal::from_id(buf[1])?,
            payload: Bytes::copy_from_slice(&buf[2..]),
        })
    }
}

/// Service capability categories.
pub mod category {
    /// The transport itself. Every endpoint has one.
    pub const MEDIA_TRANSPORT: u8 = 0x01;
    /// The codec block: what is being sent, and how.
    pub const MEDIA_CODEC: u8 = 0x07;
    /// AVDTP 1.3's delay reporting. An initiator that names it in `SET_CONFIGURATION` is
    /// asking for `DELAYREPORT` on this stream.
    pub const DELAY_REPORTING: u8 = 0x08;
}

/// AVDTP error codes we emit.
pub mod error_code {
    /// The requested SEID is not one of ours.
    pub const BAD_ACP_SEID: u8 = 0x22;
    /// The endpoint is already in use.
    pub const SEP_IN_USE: u8 = 0x23;
    /// The command is invalid in the endpoint's current state.
    pub const BAD_STATE: u8 = 0x31;
    /// The configuration named a capability we don't support.
    pub const UNSUPPORTED_CONFIGURATION: u8 = 0x29;
    /// The codec parameters were not a single valid configuration.
    pub const INVALID_CODEC_PARAMETER: u8 = 0xE2;
}

/// One of our stream endpoints: a codec we are willing to receive.
#[derive(Debug, Clone)]
pub struct StreamEndpoint {
    /// Our identifier for it.
    pub seid: Seid,
    /// What it accepts.
    pub capability: CodecCapability,
    /// Whether a sender has configured it.
    pub in_use: bool,
}

impl StreamEndpoint {
    /// The two bytes DISCOVER returns for this endpoint.
    ///
    /// Byte 1's `tsep` bit says sink, not source. Getting it backwards makes a phone
    /// believe we are offering it audio, and it will never configure the endpoint.
    #[must_use]
    pub const fn discover_bytes(&self) -> [u8; 2] {
        const MEDIA_TYPE_AUDIO: u8 = 0x00;
        const TSEP_SINK: u8 = 1 << 3;
        [
            self.seid.shifted() | ((self.in_use as u8) << 1),
            (MEDIA_TYPE_AUDIO << 4) | TSEP_SINK,
        ]
    }

    /// The capability list GET_CAPABILITIES returns for this endpoint.
    ///
    /// Media Transport must come first and must be present — an endpoint without it is
    /// not a streaming endpoint, and senders reject the whole record.
    #[must_use]
    pub fn capability_bytes(&self, include_delay_reporting: bool) -> Bytes {
        let mut buf = BytesMut::with_capacity(24);
        buf.put_u8(category::MEDIA_TRANSPORT);
        buf.put_u8(0);
        let codec = self.capability.encode();
        buf.put_u8(category::MEDIA_CODEC);
        buf.put_u8(u8::try_from(codec.len()).unwrap_or(u8::MAX));
        buf.extend_from_slice(&codec);
        if include_delay_reporting {
            buf.put_u8(category::DELAY_REPORTING);
            buf.put_u8(0);
        }
        buf.freeze()
    }
}

/// The latency a sink reports to a source, in the unit AVDTP puts on the wire.
///
/// A newtype because the wire unit is *tenths of a millisecond* and the number is
/// otherwise indistinguishable from milliseconds — an off-by-ten here is a phone
/// compensating for 30 ms when the sink is holding 300, which looks like the sync error
/// it was meant to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SinkDelay(u16);

impl SinkDelay {
    /// The delay this duration represents, saturating at the 6.5 s the field can hold.
    ///
    /// Saturating rather than failing: a delay too large to express is still better
    /// reported as the largest expressible one than not reported at all, and no real
    /// output path is anywhere near it.
    #[must_use]
    pub fn from_duration(delay: std::time::Duration) -> Self {
        Self(u16::try_from(delay.as_micros() / 100).unwrap_or(u16::MAX))
    }

    /// The raw field: tenths of a millisecond.
    #[must_use]
    pub const fn tenths_of_a_millisecond(self) -> u16 {
        self.0
    }
}

/// Whether a capability list names `category`.
///
/// Used on a `SET_CONFIGURATION` payload: an initiator that lists
/// [`category::DELAY_REPORTING`] there is saying it will accept `DELAYREPORT` on this
/// stream, which is the protocol's own answer to "may we send one".
///
/// A malformed list reads as "not named" rather than as an error: this decides whether to
/// send an optional report, and the configuration itself is validated elsewhere.
#[must_use]
pub fn lists_category(mut buf: &[u8], category: u8) -> bool {
    while buf.len() >= 2 {
        let len = usize::from(buf[1]);
        if buf[0] == category {
            return true;
        }
        if buf.len() < 2 + len {
            return false;
        }
        buf = &buf[2 + len..];
    }
    false
}

/// Pull the Media Codec capability out of a capability list.
///
/// # Errors
/// [`AudioError::Truncated`] on a malformed list, or whatever
/// [`CodecCapability::decode`] returns.
pub fn find_codec_capability(mut buf: &[u8]) -> Result<CodecCapability, AudioError> {
    while buf.len() >= 2 {
        let cat = buf[0];
        let len = usize::from(buf[1]);
        if buf.len() < 2 + len {
            return Err(AudioError::Truncated {
                what: "service capability",
                need: 2 + len,
                have: buf.len(),
            });
        }
        if cat == category::MEDIA_CODEC {
            return CodecCapability::decode(&buf[2..2 + len]);
        }
        buf = &buf[2 + len..];
    }
    Err(AudioError::Truncated {
        what: "media codec capability in list",
        need: 2,
        have: buf.len(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use hex_literal::hex;

    use super::*;
    use crate::codec::{ChannelModes, SampleRates};

    #[test]
    fn a_discover_command_round_trips() {
        let msg = Message::command(3, Signal::Discover, Bytes::new());
        assert_eq!(&msg.encode()[..], &hex!("30 01"));
        assert_eq!(Message::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn a_response_carries_the_commands_transaction_label() {
        // Responses are matched to commands by this label; a mismatch leaves the sender
        // waiting on a reply it already received.
        let cmd = Message::command(7, Signal::GetCapabilities, Bytes::from_static(&[0x04]));
        let rsp = Message::accept(&cmd, Bytes::from_static(&[0x01, 0x00]));
        let back = Message::decode(&rsp.encode()).unwrap();
        assert_eq!(back.transaction, 7);
        assert_eq!(back.message_type, MessageType::ResponseAccept);
        assert_eq!(back.signal, Signal::GetCapabilities);
    }

    #[test]
    fn seids_sit_in_the_top_six_bits_of_their_byte() {
        // The low two bits are the in-use flag and a reserved bit. Writing the SEID
        // unshifted addresses endpoint n/4 — usually endpoint 0, which is illegal.
        let seid = Seid::new(1).unwrap();
        assert_eq!(seid.shifted(), 0x04);
        assert_eq!(Seid::from_shifted(0x04).unwrap(), seid);
        assert_eq!(
            Seid::from_shifted(0x05).unwrap(),
            seid,
            "in-use bit ignored"
        );
    }

    #[test]
    fn seid_zero_and_overlarge_seids_are_refused() {
        assert!(Seid::new(0).is_err());
        assert!(Seid::new(0x3F).is_err());
        assert!(Seid::new(Seid::MAX).is_ok());
    }

    #[test]
    fn discover_advertises_us_as_a_sink_not_a_source() {
        // If the tsep bit says source, the phone thinks *we* are offering audio and will
        // never configure the endpoint — a silent no-op rather than an error.
        let sep = StreamEndpoint {
            seid: Seid::new(2).unwrap(),
            capability: CodecCapability::AptX {
                rates: SampleRates::COMMON,
                channels: ChannelModes::JOINT_STEREO,
            },
            in_use: false,
        };
        let [b0, b1] = sep.discover_bytes();
        assert_eq!(b0 >> 2, 2, "seid");
        assert_eq!(b0 & 0b10, 0, "not in use");
        assert_eq!(b1 >> 4, 0, "audio media type");
        assert_ne!(b1 & (1 << 3), 0, "tsep must say sink");
    }

    #[test]
    fn an_in_use_endpoint_sets_its_flag() {
        let sep = StreamEndpoint {
            seid: Seid::new(5).unwrap(),
            capability: CodecCapability::Ldac {
                rate_bits: 1 << 5,
                channel_bits: 1,
            },
            in_use: true,
        };
        assert_ne!(sep.discover_bytes()[0] & 0b10, 0);
    }

    #[test]
    fn a_capability_list_starts_with_media_transport_and_carries_the_codec() {
        // Senders reject an endpoint whose list lacks Media Transport, and it must come
        // first — this is a conformance detail with no useful error attached.
        let sep = StreamEndpoint {
            seid: Seid::new(1).unwrap(),
            capability: CodecCapability::Sbc {
                rates: SampleRates::ALL,
                channels: ChannelModes::ALL,
                block_lengths: 0b1111,
                subbands: 0b11,
                allocations: 0b11,
                min_bitpool: 2,
                max_bitpool: 53,
            },
            in_use: false,
        };
        let caps = sep.capability_bytes(true);
        assert_eq!(caps[0], category::MEDIA_TRANSPORT);
        assert_eq!(caps[1], 0);
        let found = find_codec_capability(&caps).unwrap();
        assert_eq!(found, sep.capability);
    }

    #[test]
    fn delay_reporting_is_advertised_only_when_asked_for() {
        let sep = StreamEndpoint {
            seid: Seid::new(1).unwrap(),
            capability: CodecCapability::AptX {
                rates: SampleRates::COMMON,
                channels: ChannelModes::JOINT_STEREO,
            },
            in_use: false,
        };
        assert!(sep
            .capability_bytes(true)
            .contains(&category::DELAY_REPORTING));
        let without = sep.capability_bytes(false);
        // The codec payload could coincidentally contain 0x08, so check the length
        // instead: two bytes shorter is the delay-reporting entry being absent.
        assert_eq!(without.len(), sep.capability_bytes(true).len() - 2);
    }

    #[test]
    fn fragmented_signaling_is_refused_rather_than_misparsed() {
        // Packet type 1 = start of a fragmented message. Treating its header as a single
        // packet reads the fragment count as the signal id.
        let start = hex!("34 02 01");
        assert!(matches!(
            Message::decode(&start),
            Err(AudioError::BadMediaPacket(_))
        ));
    }

    #[test]
    fn an_unknown_signal_is_named_in_the_error() {
        assert!(matches!(
            Message::decode(&hex!("30 7f")),
            Err(AudioError::UnknownSignal(0x7f))
        ));
    }
}
