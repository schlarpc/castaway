//! Typed failure modes for the FCast session (ground rule 7).

use crate::wire::Opcode;

/// Everything that can go wrong between the wire and a [`crate::session::Session`].
///
/// Each variant is a distinguishable wire fault, and each one names what the peer did —
/// the actor logs the variant and drops the connection, which is the reference
/// receiver's behaviour for every one of these too. There is no in-band error reply
/// for a malformed *frame*: `PlaybackError` (opcode 9) is about media, not framing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FCastError {
    /// The 4-byte size field was zero. A header must at least carry its opcode, and
    /// protocol v4 spells out that a `Size = 0` packet means disconnect immediately.
    #[error("zero-size frame")]
    ZeroSizeFrame,

    /// The declared size exceeds the 32 000-byte packet ceiling the v1-v3 spec sets.
    /// Reading it anyway would let one confused sender balloon our buffer.
    #[error("frame of {0} bytes exceeds the 32000-byte packet ceiling")]
    FrameTooLarge(usize),

    /// An opcode outside the v1-v3 table (0..=19). Opcode 20+ is protocol v4's
    /// FlatBuffers surface; per the scope note on #241 we decline what we cannot
    /// faithfully speak rather than guess at it.
    #[error("unknown opcode {0}")]
    UnknownOpcode(u8),

    /// A body that is not UTF-8. Every v1-v3 body is a UTF-8 JSON document.
    #[error("body is not UTF-8")]
    BodyNotUtf8,

    /// A body that is not the JSON document its opcode requires.
    #[error("malformed {opcode:?} body: {detail}")]
    MalformedBody {
        /// The opcode whose body failed to parse.
        opcode: Opcode,
        /// The serde error, flattened to text.
        detail: String,
    },

    /// A message that needs a body arrived without one.
    #[error("{0:?} requires a body")]
    MissingBody(Opcode),

    /// An opcode that is not legal for the negotiated session version — a v1 sender
    /// has no `SetSpeed`, a v2 sender no `SetPlaylistItem`. Accepting it would mean
    /// running a protocol the sender never agreed to.
    #[error("opcode {opcode:?} is not part of protocol v{version}")]
    IllegalOpcode {
        /// The offending opcode.
        opcode: Opcode,
        /// The session version it is illegal under.
        version: u64,
    },

    /// `Version { version: 0 }`. Zero is not a protocol version; the reference
    /// receiver rejects it too.
    #[error("illegal protocol version 0")]
    IllegalVersion,

    /// A `Version` message after the session already negotiated one. Re-negotiation
    /// mid-session is not part of any published version.
    #[error("version renegotiation after the session was established")]
    UnexpectedVersion,

    /// The peer went silent past the heartbeat deadline ([`crate::session::DEAD_AFTER`]).
    #[error("no traffic for the heartbeat deadline; connection presumed dead")]
    HeartbeatTimeout,

    /// TLS identity or handshake failure on the v4 path (#248).
    #[error("v4 TLS: {0}")]
    Tls(String),

    /// A v4 `Flatbuf` body that fails the verifier, or a verified union whose
    /// required member is absent. Session-fatal, as in the reference receiver —
    /// unlike an unknown-but-well-formed payload type, which gets a polite
    /// `Error{{InvalidPayloadType}}` reply.
    #[error("malformed v4 packet: {0}")]
    MalformedFlatbuf(String),
}
