//! FCast framing: a 4-byte **little**-endian size, one opcode byte, then an optional
//! UTF-8 JSON body. `size` counts the opcode and body, so an empty message is
//! `size = 1`. Pure — the actor feeds it socket reads (ground rule 3).
//!
//! The shape is fixed across protocol v1-v3; what varies by version is which opcodes
//! are legal, and that is [`crate::session`]'s business, not framing's.

use crate::error::FCastError;

/// Maximum total packet (opcode + body) the v1-v3 spec allows: 32 000 bytes.
pub const MAX_PACKET: usize = 32_000;

/// The v1-v3 opcode table.
///
/// Direction notes are the spec's: "sender" messages come from the phone, "receiver"
/// messages are ours. Both appear here because framing decodes both — a session
/// decides which are legal to *receive*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Opcode {
    /// Opcode 0, "not used". Decoded and then ignored, as the reference receiver does.
    None,
    /// Sender: load and play media. Body is `PlayMessage`.
    Play,
    /// Sender: pause. No body.
    Pause,
    /// Sender: resume. No body.
    Resume,
    /// Sender: stop. No body.
    Stop,
    /// Sender: seek. Body is `SeekMessage`.
    Seek,
    /// Receiver: playback state changed. Body is a versioned `PlaybackUpdateMessage`.
    PlaybackUpdate,
    /// Receiver: volume changed. Body is a versioned `VolumeUpdateMessage`.
    VolumeUpdate,
    /// Sender: change volume. Body is `SetVolumeMessage`.
    SetVolume,
    /// Receiver (v2+): a playback error happened. Body is `PlaybackErrorMessage`.
    PlaybackError,
    /// Sender (v2+): change playback speed. Body is `SetSpeedMessage`.
    SetSpeed,
    /// Both (v2+): announce the highest supported protocol version. Body is
    /// `VersionMessage`.
    Version,
    /// Both (v2+): probe an idle connection. No body.
    Ping,
    /// Both (v2+): answer a probe. No body.
    Pong,
    /// Both (v3): identify the device. Body is `InitialSenderMessage` from a sender,
    /// `InitialReceiverMessage` from us.
    Initial,
    /// Receiver (v3): tell every sender the loaded content changed. Body is
    /// `PlayUpdateMessage`.
    PlayUpdate,
    /// Sender (v3): jump to a playlist item. Body is `SetPlaylistItemMessage`.
    SetPlaylistItem,
    /// Sender (v3): subscribe to a receiver event. Body is `SubscribeEventMessage`.
    SubscribeEvent,
    /// Sender (v3): unsubscribe from a receiver event. Body is
    /// `UnsubscribeEventMessage`.
    UnsubscribeEvent,
    /// Receiver (v3): a subscribed event occurred. Body is `EventMessage`.
    Event,
}

impl Opcode {
    /// Parse a wire opcode byte.
    ///
    /// # Errors
    /// [`FCastError::UnknownOpcode`] for anything outside the v1-v3 table — opcode 20+
    /// is protocol v4's surface and is declined, not skipped.
    pub const fn from_wire(byte: u8) -> Result<Self, FCastError> {
        Ok(match byte {
            0 => Self::None,
            1 => Self::Play,
            2 => Self::Pause,
            3 => Self::Resume,
            4 => Self::Stop,
            5 => Self::Seek,
            6 => Self::PlaybackUpdate,
            7 => Self::VolumeUpdate,
            8 => Self::SetVolume,
            9 => Self::PlaybackError,
            10 => Self::SetSpeed,
            11 => Self::Version,
            12 => Self::Ping,
            13 => Self::Pong,
            14 => Self::Initial,
            15 => Self::PlayUpdate,
            16 => Self::SetPlaylistItem,
            17 => Self::SubscribeEvent,
            18 => Self::UnsubscribeEvent,
            19 => Self::Event,
            other => return Err(FCastError::UnknownOpcode(other)),
        })
    }

    /// The wire byte for this opcode.
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Play => 1,
            Self::Pause => 2,
            Self::Resume => 3,
            Self::Stop => 4,
            Self::Seek => 5,
            Self::PlaybackUpdate => 6,
            Self::VolumeUpdate => 7,
            Self::SetVolume => 8,
            Self::PlaybackError => 9,
            Self::SetSpeed => 10,
            Self::Version => 11,
            Self::Ping => 12,
            Self::Pong => 13,
            Self::Initial => 14,
            Self::PlayUpdate => 15,
            Self::SetPlaylistItem => 16,
            Self::SubscribeEvent => 17,
            Self::UnsubscribeEvent => 18,
            Self::Event => 19,
        }
    }
}

/// One decoded packet: an opcode and its raw body bytes (empty when the message has
/// none — the wire cannot distinguish "no body" from "empty body", so neither do we).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// What the packet is.
    pub opcode: Opcode,
    /// The body bytes, still unparsed. [`crate::messages`] turns them into types.
    pub body: Vec<u8>,
}

impl Frame {
    /// A body-less frame.
    #[must_use]
    pub const fn bare(opcode: Opcode) -> Self {
        Self {
            opcode,
            body: Vec::new(),
        }
    }

    /// A frame carrying a JSON body.
    #[must_use]
    pub const fn with_body(opcode: Opcode, body: Vec<u8>) -> Self {
        Self { opcode, body }
    }
}

/// Encode a frame with its little-endian size prefix.
///
/// # Errors
/// [`FCastError::FrameTooLarge`] if opcode + body would exceed [`MAX_PACKET`]. Our own
/// bodies are all far smaller; this guards a future caller, not a live path.
pub fn encode(frame: &Frame) -> Result<Vec<u8>, FCastError> {
    let size = 1 + frame.body.len();
    if size > MAX_PACKET {
        return Err(FCastError::FrameTooLarge(size));
    }
    let mut out = Vec::with_capacity(4 + size);
    // `MAX_PACKET` fits comfortably in a u32; the check above already ran.
    #[allow(clippy::cast_possible_truncation)]
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.push(frame.opcode.to_wire());
    out.extend_from_slice(&frame.body);
    Ok(out)
}

/// Try to decode one frame from the front of `buf`.
///
/// Returns `Ok(None)` if `buf` doesn't yet hold a complete frame (caller reads more).
/// On success returns the frame and the number of bytes consumed, so the caller can
/// drain them from its read buffer.
///
/// # Errors
/// [`FCastError::ZeroSizeFrame`], [`FCastError::FrameTooLarge`], or
/// [`FCastError::UnknownOpcode`] — all of which mean "disconnect", per the spec's own
/// instruction for malformed headers.
pub fn try_decode(buf: &[u8]) -> Result<Option<(Frame, usize)>, FCastError> {
    let Some(header) = buf.first_chunk::<4>() else {
        return Ok(None);
    };
    let size = u32::from_le_bytes(*header) as usize;
    if size == 0 {
        return Err(FCastError::ZeroSizeFrame);
    }
    if size > MAX_PACKET {
        return Err(FCastError::FrameTooLarge(size));
    }
    let total = 4 + size;
    if buf.len() < total {
        return Ok(None);
    }
    let opcode = Opcode::from_wire(buf[4])?;
    Ok(Some((
        Frame::with_body(opcode, buf[5..total].to_vec()),
        total,
    )))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The greeting our session sends, byte-for-byte as the capture harness recorded
    /// it being accepted by the reference sender (`tests/fixtures/*.jsonl`, `out` rows).
    const VERSION_3_GREETING: &[u8] = &[
        0x0e, 0x00, 0x00, 0x00, 0x0b, b'{', b'"', b'v', b'e', b'r', b's', b'i', b'o', b'n', b'"',
        b':', b'3', b'}',
    ];

    #[test]
    fn encode_then_decode() {
        let frame = Frame::with_body(Opcode::Version, br#"{"version":3}"#.to_vec());
        let bytes = encode(&frame).unwrap();
        assert_eq!(bytes, VERSION_3_GREETING);
        let (back, consumed) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(back, frame);
    }

    #[test]
    fn a_bare_frame_is_five_bytes() {
        let bytes = encode(&Frame::bare(Opcode::Pause)).unwrap();
        assert_eq!(bytes, [0x01, 0x00, 0x00, 0x00, 0x02]);
        let (back, consumed) = try_decode(&bytes).unwrap().unwrap();
        assert_eq!(consumed, 5);
        assert_eq!(back, Frame::bare(Opcode::Pause));
    }

    #[test]
    fn partial_frame_returns_none() {
        let bytes = encode(&Frame::with_body(Opcode::Seek, br#"{"time":1.0}"#.to_vec())).unwrap();
        assert!(try_decode(&bytes[..2]).unwrap().is_none()); // header incomplete
        assert!(try_decode(&bytes[..bytes.len() - 1]).unwrap().is_none()); // body short
    }

    #[test]
    fn two_frames_consumed_individually() {
        let m1 = Frame::with_body(Opcode::Version, br#"{"version":4}"#.to_vec());
        let m2 = Frame::bare(Opcode::Resume);
        let mut stream = encode(&m1).unwrap();
        stream.extend(encode(&m2).unwrap());
        let (d1, n1) = try_decode(&stream).unwrap().unwrap();
        assert_eq!(d1, m1);
        let (d2, _n2) = try_decode(&stream[n1..]).unwrap().unwrap();
        assert_eq!(d2, m2);
    }

    /// v4 §Overview: a packet with `Size = 0` has no opcode, and the peer must
    /// disconnect. The size field alone is enough to fault — no waiting for bytes
    /// that cannot come.
    #[test]
    fn a_zero_size_header_is_a_fault_not_a_stall() {
        assert!(matches!(
            try_decode(&[0, 0, 0, 0]),
            Err(FCastError::ZeroSizeFrame)
        ));
    }

    /// The 32 000-byte ceiling is enforced from the header, before any body arrives —
    /// a sender declaring 4 GiB must not make us wait for 4 GiB.
    #[test]
    fn an_oversize_declaration_faults_from_the_header_alone() {
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.push(Opcode::Play.to_wire());
        assert!(matches!(
            try_decode(&bytes),
            Err(FCastError::FrameTooLarge(_))
        ));
    }

    /// Opcode 20 is protocol v4's `Flatbuf`. The scope note on #241: decline messages
    /// from newer versions rather than guessing.
    #[test]
    fn a_v4_opcode_is_declined_not_skipped() {
        let bytes = [0x01, 0x00, 0x00, 0x00, 20];
        assert!(matches!(
            try_decode(&bytes),
            Err(FCastError::UnknownOpcode(20))
        ));
    }

    #[test]
    fn every_opcode_survives_the_wire_roundtrip() {
        for byte in 0..=19u8 {
            let opcode = Opcode::from_wire(byte).unwrap();
            assert_eq!(opcode.to_wire(), byte);
        }
    }

    /// A frame we would refuse to read, we also refuse to write.
    #[test]
    fn encode_holds_the_same_ceiling_decode_does() {
        let frame = Frame::with_body(Opcode::Play, vec![b'x'; MAX_PACKET]);
        assert!(matches!(encode(&frame), Err(FCastError::FrameTooLarge(_))));
    }
}
