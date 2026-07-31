//! Enhanced Retransmission Mode: the reliable, ordered, segmenting flavour of L2CAP.
//!
//! Basic mode carries everything A2DP needs, so this existed as a gap for a long time.
//! What forces it is **cover art**: AVRCP 1.6.3 §14 requires GOEP 2.0 for the image
//! transfer, and GOEP §7.1.2 requires the OBEX channel to be configured for ERTM. A stack
//! that answers the extended-features request with a zero mask and refuses the
//! retransmission option by name never gets an image, no matter how correct everything
//! above it is — the peer's OBEX CONNECT is answered with a configuration refusal and the
//! transfer dies there (Q29).
//!
//! Three things make ERTM more than "basic mode with sequence numbers":
//!
//! - **A control field.** Every frame carries a two-byte header naming its own sequence
//!   number and acknowledging the peer's, so the two ends can tell a lost frame from a
//!   reordered one.
//! - **Segmentation.** An SDU larger than the negotiated MPS is split across frames and
//!   reassembled at the far end, which is why the cover-art JPEG can exceed the ACL
//!   buffer without the layer above knowing.
//! - **A frame check sequence.** A CRC-16 over the whole PDU, *including the basic
//!   header* — the trap that makes an otherwise perfect implementation fail every frame.
//!
//! Pure and synchronous like the rest of the crate (ground rule 3): frames in, frames and
//! reassembled SDUs out, and time advanced explicitly by [`Ertm::tick`] rather than read
//! from a clock. That is what lets the retransmission timer be tested in microseconds.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use tracing::debug;

use crate::error::L2capError;
use crate::pdu::Cid;

/// Sequence numbers are six bits wide and wrap.
const SEQ_MODULO: u8 = 64;

/// Bytes of ERTM overhead on a frame that starts an SDU: control field, SDU length, FCS.
/// The most any one frame spends on being ERTM rather than basic.
pub const MAX_OVERHEAD: usize = 6;

/// Frames we may have outstanding before waiting for an acknowledgement.
pub const DEFAULT_TX_WINDOW: u8 = 32;

/// How many times one frame is retransmitted before the channel is declared dead.
pub const DEFAULT_MAX_TRANSMIT: u8 = 3;

/// How long an unacknowledged frame waits before we poll for its acknowledgement.
pub const DEFAULT_RETRANSMISSION_TIMEOUT: Duration = Duration::from_millis(2000);

/// How long a poll waits for its answer before being repeated.
pub const DEFAULT_MONITOR_TIMEOUT: Duration = Duration::from_millis(12000);

/// Which L2CAP mode a channel runs in.
///
/// Modelled as an enum rather than a `bool` because the wire field has five values and
/// the two we implement must not silently absorb the three we don't: a peer asking for
/// Streaming mode has to be told no, not handed a retransmitting channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ChannelMode {
    /// Basic mode: no sequencing, no retransmission, no segmentation.
    #[default]
    Basic,
    /// Enhanced Retransmission Mode.
    EnhancedRetransmission,
    /// A mode we don't implement, kept so a peer's proposal round-trips into the
    /// counter-proposal that refuses it.
    Other(u8),
}

impl ChannelMode {
    /// The wire value.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Self::Basic => 0x00,
            Self::EnhancedRetransmission => 0x03,
            Self::Other(raw) => raw,
        }
    }

    /// Parse the wire value.
    #[must_use]
    pub const fn from_bits(raw: u8) -> Self {
        match raw {
            0x00 => Self::Basic,
            0x03 => Self::EnhancedRetransmission,
            other => Self::Other(other),
        }
    }
}

/// The Retransmission and Flow Control configuration option's payload.
///
/// Whose number is whose is the part worth stating: in a configuration *request* every
/// field describes what the **sender of the request** will do or can accept, so their
/// `mps` is our send ceiling and their `tx_window` is how many frames they may have in
/// flight towards us. In a *response* the same fields are the responder's chance to
/// reduce what the requester asked for. Reading a request's `mps` as our own receive size
/// is the mistake that produces frames the peer quietly drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetransmissionConfig {
    /// Which mode this end is proposing or agreeing to.
    pub mode: ChannelMode,
    /// Frames the sender may have unacknowledged.
    pub tx_window: u8,
    /// Retransmissions of one frame before the channel is abandoned.
    pub max_transmit: u8,
    /// Retransmission time-out, in milliseconds. Zero in a request: the responder fills
    /// it in, and a requester that puts a real number here is proposing a value it has no
    /// standing to choose.
    pub retransmission_timeout_ms: u16,
    /// Monitor time-out, in milliseconds. Zero in a request, for the same reason.
    pub monitor_timeout_ms: u16,
    /// Maximum PDU payload the sender of this option can receive in one frame.
    pub mps: u16,
}

impl RetransmissionConfig {
    /// Bytes this option occupies on the wire, excluding the type/length header.
    pub const LEN: usize = 9;

    /// Basic mode, which is what an absent option means.
    #[must_use]
    pub const fn basic() -> Self {
        Self {
            mode: ChannelMode::Basic,
            tx_window: 0,
            max_transmit: 0,
            retransmission_timeout_ms: 0,
            monitor_timeout_ms: 0,
            mps: 0,
        }
    }

    /// An ERTM proposal for a channel whose largest receivable frame payload is `mps`.
    #[must_use]
    pub const fn ertm(mps: u16) -> Self {
        Self {
            mode: ChannelMode::EnhancedRetransmission,
            tx_window: DEFAULT_TX_WINDOW,
            max_transmit: DEFAULT_MAX_TRANSMIT,
            // Left at zero deliberately — see the field documentation.
            retransmission_timeout_ms: 0,
            monitor_timeout_ms: 0,
            mps,
        }
    }

    /// Encode the nine-byte body.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::LEN] {
        let retrans = self.retransmission_timeout_ms.to_le_bytes();
        let monitor = self.monitor_timeout_ms.to_le_bytes();
        let mps = self.mps.to_le_bytes();
        [
            self.mode.bits(),
            self.tx_window,
            self.max_transmit,
            retrans[0],
            retrans[1],
            monitor[0],
            monitor[1],
            mps[0],
            mps[1],
        ]
    }

    /// Decode the nine-byte body.
    ///
    /// # Errors
    /// [`L2capError::Truncated`] if the option body is shorter than nine bytes.
    pub fn decode(body: &[u8]) -> Result<Self, L2capError> {
        if body.len() < Self::LEN {
            return Err(L2capError::Truncated {
                what: "retransmission and flow control option",
                need: Self::LEN,
                have: body.len(),
            });
        }
        Ok(Self {
            mode: ChannelMode::from_bits(body[0]),
            tx_window: body[1],
            max_transmit: body[2],
            retransmission_timeout_ms: u16::from_le_bytes([body[3], body[4]]),
            monitor_timeout_ms: u16::from_le_bytes([body[5], body[6]]),
            mps: u16::from_le_bytes([body[7], body[8]]),
        })
    }
}

/// Whether a channel carries a frame check sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FcsType {
    /// No FCS. Only legal when *both* ends ask for it.
    None,
    /// The 16-bit CRC every ERTM channel uses unless told otherwise.
    #[default]
    Crc16,
    /// A type we don't implement.
    Other(u8),
}

impl FcsType {
    /// The wire value.
    #[must_use]
    pub const fn bits(self) -> u8 {
        match self {
            Self::None => 0x00,
            Self::Crc16 => 0x01,
            Self::Other(raw) => raw,
        }
    }

    /// Parse the wire value.
    #[must_use]
    pub const fn from_bits(raw: u8) -> Self {
        match raw {
            0x00 => Self::None,
            0x01 => Self::Crc16,
            other => Self::Other(other),
        }
    }

    /// Whether frames on this channel carry a trailing checksum.
    #[must_use]
    pub const fn is_present(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// What an S-frame is saying about the receiver's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Supervisory {
    /// Everything up to `req_seq` arrived; keep going.
    ReceiverReady,
    /// A frame was missed; retransmit from `req_seq`.
    Reject,
    /// Out of buffer space; stop sending.
    ReceiverNotReady,
    /// Retransmit exactly `req_seq` and nothing else.
    SelectiveReject,
    /// A supervisory function we don't model.
    Other(u8),
}

impl Supervisory {
    const fn bits(self) -> u8 {
        match self {
            Self::ReceiverReady => 0,
            Self::Reject => 1,
            Self::ReceiverNotReady => 2,
            Self::SelectiveReject => 3,
            Self::Other(raw) => raw & 0x03,
        }
    }

    const fn from_bits(raw: u8) -> Self {
        match raw & 0x03 {
            0 => Self::ReceiverReady,
            1 => Self::Reject,
            2 => Self::ReceiverNotReady,
            _ => Self::SelectiveReject,
        }
    }
}

/// Where a frame sits in the SDU it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Segmentation {
    /// A whole SDU in one frame.
    Unsegmented,
    /// The first frame of several; carries the total SDU length.
    Start,
    /// A middle frame.
    Continuation,
    /// The last frame.
    End,
}

impl Segmentation {
    const fn bits(self) -> u16 {
        match self {
            Self::Unsegmented => 0,
            Self::Start => 1,
            Self::End => 2,
            Self::Continuation => 3,
        }
    }

    const fn from_bits(raw: u16) -> Self {
        match raw & 0x03 {
            0 => Self::Unsegmented,
            1 => Self::Start,
            2 => Self::End,
            _ => Self::Continuation,
        }
    }
}

/// One ERTM frame, with the standard (two-byte) control field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Frame {
    /// An I-frame: data, plus an acknowledgement riding along with it.
    Information {
        /// This frame's own sequence number.
        tx_seq: u8,
        /// Everything below this number has been received.
        req_seq: u8,
        /// Set when answering a poll.
        final_bit: bool,
        /// Where this frame sits in its SDU.
        sar: Segmentation,
        /// Total SDU length, present only on [`Segmentation::Start`].
        sdu_len: Option<u16>,
        /// The segment.
        payload: Bytes,
    },
    /// An S-frame: acknowledgement and flow control, carrying no data.
    Supervisory {
        /// What the sender is saying.
        function: Supervisory,
        /// Everything below this number has been received.
        req_seq: u8,
        /// Set when demanding an answer.
        poll: bool,
        /// Set when answering one.
        final_bit: bool,
    },
}

mod ctrl {
    /// Bit 0 distinguishes S-frames from I-frames.
    pub const FRAME_TYPE_S: u16 = 0x0001;
    pub const TX_SEQ: u16 = 0x007E;
    pub const TX_SEQ_SHIFT: u16 = 1;
    pub const SUPERVISORY: u16 = 0x000C;
    pub const SUPERVISORY_SHIFT: u16 = 2;
    pub const POLL: u16 = 0x0010;
    pub const FINAL: u16 = 0x0080;
    pub const REQ_SEQ: u16 = 0x3F00;
    pub const REQ_SEQ_SHIFT: u16 = 8;
    pub const SAR: u16 = 0xC000;
    pub const SAR_SHIFT: u16 = 14;
}

impl Frame {
    /// The acknowledgement this frame carries, whichever kind it is.
    #[must_use]
    pub const fn req_seq(&self) -> u8 {
        match self {
            Self::Information { req_seq, .. } | Self::Supervisory { req_seq, .. } => *req_seq,
        }
    }

    /// Whether this frame is answering a poll.
    #[must_use]
    pub const fn final_bit(&self) -> bool {
        match self {
            Self::Information { final_bit, .. } | Self::Supervisory { final_bit, .. } => *final_bit,
        }
    }

    /// Encode as an L2CAP payload — control field, optional SDU length, data, and the
    /// frame check sequence if the channel negotiated one.
    ///
    /// `cid` is the *peer's* identifier for the channel, because the FCS covers the basic
    /// L2CAP header this payload is about to be wrapped in. Computing it over the payload
    /// alone produces a checksum that is wrong on every single frame, which presents as a
    /// channel that connects perfectly and then transfers nothing.
    #[must_use]
    pub fn encode(&self, cid: Cid, fcs: FcsType) -> Bytes {
        let mut body = BytesMut::with_capacity(8);
        match self {
            Self::Information {
                tx_seq,
                req_seq,
                final_bit,
                sar,
                sdu_len,
                payload,
            } => {
                let mut control = (u16::from(*tx_seq) << ctrl::TX_SEQ_SHIFT) & ctrl::TX_SEQ;
                control |= (u16::from(*req_seq) << ctrl::REQ_SEQ_SHIFT) & ctrl::REQ_SEQ;
                control |= (sar.bits() << ctrl::SAR_SHIFT) & ctrl::SAR;
                if *final_bit {
                    control |= ctrl::FINAL;
                }
                body.put_u16_le(control);
                if *sar == Segmentation::Start {
                    body.put_u16_le(sdu_len.unwrap_or(0));
                }
                body.extend_from_slice(payload);
            }
            Self::Supervisory {
                function,
                req_seq,
                poll,
                final_bit,
            } => {
                let mut control = ctrl::FRAME_TYPE_S;
                control |=
                    (u16::from(function.bits()) << ctrl::SUPERVISORY_SHIFT) & ctrl::SUPERVISORY;
                control |= (u16::from(*req_seq) << ctrl::REQ_SEQ_SHIFT) & ctrl::REQ_SEQ;
                if *poll {
                    control |= ctrl::POLL;
                }
                if *final_bit {
                    control |= ctrl::FINAL;
                }
                body.put_u16_le(control);
            }
        }
        if !fcs.is_present() {
            return body.freeze();
        }
        let total = body.len() + 2;
        let mut with_header = BytesMut::with_capacity(4 + total);
        with_header.put_u16_le(u16::try_from(total).unwrap_or(u16::MAX));
        with_header.put_u16_le(cid.raw());
        with_header.extend_from_slice(&body);
        let checksum = crc16(&with_header);
        body.put_u16_le(checksum);
        body.freeze()
    }

    /// Decode an L2CAP payload back into a frame, verifying the FCS.
    ///
    /// `cid` is *our* identifier for the channel — the one the peer addressed the PDU to,
    /// and therefore the one that was covered by the checksum it computed.
    ///
    /// # Errors
    /// [`L2capError::Truncated`] on a payload too short to hold a control field, or
    /// [`L2capError::BadFcs`] if the checksum does not match.
    pub fn decode(payload: &[u8], cid: Cid, fcs: FcsType) -> Result<Self, L2capError> {
        let body = if fcs.is_present() {
            if payload.len() < 4 {
                return Err(L2capError::Truncated {
                    what: "ertm frame with fcs",
                    need: 4,
                    have: payload.len(),
                });
            }
            let split = payload.len() - 2;
            let claimed = u16::from_le_bytes([payload[split], payload[split + 1]]);
            let mut covered = BytesMut::with_capacity(4 + payload.len());
            covered.put_u16_le(u16::try_from(payload.len()).unwrap_or(u16::MAX));
            covered.put_u16_le(cid.raw());
            covered.extend_from_slice(&payload[..split]);
            let computed = crc16(&covered);
            if computed != claimed {
                return Err(L2capError::BadFcs {
                    cid,
                    expected: computed,
                    actual: claimed,
                });
            }
            &payload[..split]
        } else {
            payload
        };

        if body.len() < 2 {
            return Err(L2capError::Truncated {
                what: "ertm control field",
                need: 2,
                have: body.len(),
            });
        }
        let control = u16::from_le_bytes([body[0], body[1]]);
        let req_seq = u8::try_from((control & ctrl::REQ_SEQ) >> ctrl::REQ_SEQ_SHIFT).unwrap_or(0);
        let final_bit = control & ctrl::FINAL != 0;

        if control & ctrl::FRAME_TYPE_S != 0 {
            return Ok(Self::Supervisory {
                function: Supervisory::from_bits(
                    u8::try_from((control & ctrl::SUPERVISORY) >> ctrl::SUPERVISORY_SHIFT)
                        .unwrap_or(0),
                ),
                req_seq,
                poll: control & ctrl::POLL != 0,
                final_bit,
            });
        }

        let sar = Segmentation::from_bits((control & ctrl::SAR) >> ctrl::SAR_SHIFT);
        let mut rest = &body[2..];
        let sdu_len = if sar == Segmentation::Start {
            if rest.len() < 2 {
                return Err(L2capError::Truncated {
                    what: "ertm sdu length",
                    need: 2,
                    have: rest.len(),
                });
            }
            let len = u16::from_le_bytes([rest[0], rest[1]]);
            rest = &rest[2..];
            Some(len)
        } else {
            None
        };
        Ok(Self::Information {
            tx_seq: u8::try_from((control & ctrl::TX_SEQ) >> ctrl::TX_SEQ_SHIFT).unwrap_or(0),
            req_seq,
            final_bit,
            sar,
            sdu_len,
            payload: Bytes::copy_from_slice(rest),
        })
    }
}

/// CRC-16/ARC — polynomial `0x8005` reflected, initial value zero.
///
/// The same function the kernel uses for L2CAP's FCS. Reflected, which matters: running
/// the polynomial the other way round produces a checksum that is stable, plausible, and
/// rejected by every peer.
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for byte in data {
        crc ^= u16::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// What the parties agreed on for one ERTM channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErtmParameters {
    /// Our identifier, needed to verify inbound checksums.
    pub local_cid: Cid,
    /// The peer's identifier, needed to compute outbound ones.
    pub remote_cid: Cid,
    /// Whether frames carry a checksum.
    pub fcs: FcsType,
    /// Largest segment we may put in one frame.
    pub send_mps: u16,
    /// Frames we may have unacknowledged.
    pub send_window: u8,
    /// Retransmissions before the channel is declared dead.
    pub max_transmit: u8,
    /// How long an unacknowledged frame waits before we poll.
    pub retransmission_timeout: Duration,
    /// How long a poll waits before being repeated.
    pub monitor_timeout: Duration,
    /// Largest SDU we will reassemble; a peer claiming more is refused rather than
    /// allowed to allocate on our behalf.
    pub local_mtu: u16,
}

/// What one call into the engine produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ErtmOutput {
    /// Payloads to write to the channel, in order.
    pub frames: Vec<Bytes>,
    /// SDUs that finished reassembling.
    pub sdus: Vec<Bytes>,
    /// Set when the channel has failed and must be torn down: a frame was retransmitted
    /// its full allowance and never acknowledged. Reported rather than retried forever,
    /// because a peer that has stopped answering is not going to start.
    pub failed: bool,
}

impl ErtmOutput {
    fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.sdus.is_empty() && !self.failed
    }
}

/// A frame we have sent and not yet seen acknowledged.
#[derive(Debug, Clone)]
struct Unacked {
    tx_seq: u8,
    sar: Segmentation,
    sdu_len: Option<u16>,
    payload: Bytes,
    transmissions: u8,
}

/// An SDU being put back together.
#[derive(Debug)]
struct Reassembly {
    expected: usize,
    buffer: BytesMut,
}

/// The ERTM engine for one channel.
///
/// Sans-I/O: every method returns the frames the caller should write, and time only moves
/// when [`Ertm::tick`] is called. The transmit and receive halves are independent — the
/// sequence numbers in each direction have nothing to do with each other — which is why
/// each field below says which side it belongs to.
#[derive(Debug)]
pub struct Ertm {
    params: ErtmParameters,

    // Transmit side.
    next_tx_seq: u8,
    unacked: VecDeque<Unacked>,
    queued: VecDeque<Unacked>,
    remote_busy: bool,
    /// Poll attempts made without an answer.
    polls: u8,
    waiting_for_final: bool,
    retransmission_timer: Option<Duration>,
    monitor_timer: Option<Duration>,

    // Receive side.
    expected_tx_seq: u8,
    /// Whether we have already asked for a retransmission and are waiting for it. Sending
    /// a second REJ for the same gap makes the peer retransmit the window twice.
    rejected: bool,
    reassembly: Option<Reassembly>,
    /// Frames discarded for a bad checksum or a length we could not honour. Kept as a
    /// number rather than logged per frame: on a bad link this is the hot path.
    discarded: u32,
}

impl Ertm {
    /// Start an engine for a freshly configured channel.
    #[must_use]
    pub fn new(params: ErtmParameters) -> Self {
        Self {
            params,
            next_tx_seq: 0,
            unacked: VecDeque::new(),
            queued: VecDeque::new(),
            remote_busy: false,
            polls: 0,
            waiting_for_final: false,
            retransmission_timer: None,
            monitor_timer: None,
            expected_tx_seq: 0,
            rejected: false,
            reassembly: None,
            discarded: 0,
        }
    }

    /// The parameters this channel negotiated.
    #[must_use]
    pub const fn parameters(&self) -> &ErtmParameters {
        &self.params
    }

    /// How many frames were dropped without being delivered.
    #[must_use]
    pub const fn discarded(&self) -> u32 {
        self.discarded
    }

    /// Queue an SDU, segmenting it across as many frames as the negotiated MPS needs.
    ///
    /// # Errors
    /// [`L2capError::TooLong`] if the SDU will not fit in the 16-bit length field the
    /// start-of-SDU frame carries.
    pub fn send(&mut self, sdu: Bytes) -> Result<ErtmOutput, L2capError> {
        let total = u16::try_from(sdu.len()).map_err(|_| L2capError::TooLong {
            len: sdu.len(),
            max: usize::from(u16::MAX),
        })?;
        let mps = usize::from(self.params.send_mps).max(1);
        if sdu.len() <= mps {
            self.queued.push_back(Unacked {
                tx_seq: 0,
                sar: Segmentation::Unsegmented,
                sdu_len: None,
                payload: sdu,
                transmissions: 0,
            });
        } else {
            // The start frame spends two of its payload bytes on the SDU length, so it
            // carries less data than the ones after it. Segmenting as though every frame
            // were the same size overruns the MPS by exactly two bytes on the first one.
            let first = mps.saturating_sub(2).max(1);
            let mut offset = first.min(sdu.len());
            self.queued.push_back(Unacked {
                tx_seq: 0,
                sar: Segmentation::Start,
                sdu_len: Some(total),
                payload: sdu.slice(..offset),
                transmissions: 0,
            });
            while offset < sdu.len() {
                let end = (offset + mps).min(sdu.len());
                let sar = if end == sdu.len() {
                    Segmentation::End
                } else {
                    Segmentation::Continuation
                };
                self.queued.push_back(Unacked {
                    tx_seq: 0,
                    sar,
                    sdu_len: None,
                    payload: sdu.slice(offset..end),
                    transmissions: 0,
                });
                offset = end;
            }
        }
        let mut out = ErtmOutput::default();
        self.pump(&mut out);
        Ok(out)
    }

    /// Feed one inbound L2CAP payload.
    ///
    /// A frame with a bad checksum is counted and dropped rather than reported: the
    /// sender's retransmission timer is the mechanism that recovers it, and answering a
    /// frame whose sequence number we could not trust would make things worse.
    ///
    /// # Errors
    /// [`L2capError::Truncated`] if the payload is too short to be a frame at all.
    pub fn receive(&mut self, payload: &[u8]) -> Result<ErtmOutput, L2capError> {
        let frame = match Frame::decode(payload, self.params.local_cid, self.params.fcs) {
            Ok(frame) => frame,
            Err(L2capError::BadFcs {
                expected, actual, ..
            }) => {
                self.discarded = self.discarded.saturating_add(1);
                // A dropped frame used to be a number nobody could see. It is the one
                // failure that looks exactly like a peer gone quiet from the layer above —
                // the transfer simply stops — so it says so once per frame, with the bytes.
                debug!(
                    cid = %self.params.local_cid,
                    expected,
                    actual,
                    discarded = self.discarded,
                    raw = %hex(payload),
                    "ertm: dropping a frame whose checksum does not match"
                );
                return Ok(ErtmOutput::default());
            }
            Err(other) => return Err(other),
        };
        debug!(cid = %self.params.local_cid, ?frame, "ertm: rx");

        let mut out = ErtmOutput::default();
        // An acknowledgement rides on every frame, data or not, so it is taken before the
        // frame's own business is looked at.
        self.acknowledge(frame.req_seq());
        if frame.final_bit() && self.waiting_for_final {
            self.waiting_for_final = false;
            self.monitor_timer = None;
            self.polls = 0;
            // The peer has answered our poll and told us where it got to; anything still
            // outstanding was lost rather than merely slow.
            self.retransmit_all(&mut out);
        }

        match frame {
            Frame::Supervisory {
                function,
                req_seq,
                poll,
                ..
            } => self.on_supervisory(function, req_seq, poll, &mut out),
            Frame::Information {
                tx_seq,
                sar,
                sdu_len,
                payload,
                ..
            } => self.on_information(tx_seq, sar, sdu_len, payload, &mut out),
        }
        self.pump(&mut out);
        self.arm_timers();
        Ok(out)
    }

    /// Advance time by `elapsed`.
    ///
    /// Explicit rather than read from a clock, so the retransmission path is a unit test
    /// rather than a twelve-second wait.
    pub fn tick(&mut self, elapsed: Duration) -> ErtmOutput {
        let mut out = ErtmOutput::default();
        if let Some(remaining) = self.monitor_timer {
            let left = remaining.saturating_sub(elapsed);
            if left.is_zero() {
                self.monitor_timer = None;
                if self.polls >= self.params.max_transmit {
                    out.failed = true;
                    return out;
                }
                self.poll(&mut out);
            } else {
                self.monitor_timer = Some(left);
            }
        } else if let Some(remaining) = self.retransmission_timer {
            let left = remaining.saturating_sub(elapsed);
            if left.is_zero() {
                self.retransmission_timer = None;
                self.poll(&mut out);
            } else {
                self.retransmission_timer = Some(left);
            }
        }
        if !out.is_empty() {
            self.pump(&mut out);
        }
        out
    }

    /// How long until something needs doing, if anything does.
    #[must_use]
    pub fn next_timeout(&self) -> Option<Duration> {
        self.monitor_timer.or(self.retransmission_timer)
    }

    /// Ask the peer where it has got to, and start waiting for the answer.
    fn poll(&mut self, out: &mut ErtmOutput) {
        debug!(
            cid = %self.params.local_cid,
            unacked = self.unacked.len(),
            attempt = self.polls.saturating_add(1),
            "ertm: nothing acknowledged within the retransmission timeout; polling the peer"
        );
        self.polls = self.polls.saturating_add(1);
        self.waiting_for_final = true;
        self.monitor_timer = Some(self.params.monitor_timeout);
        self.retransmission_timer = None;
        out.frames
            .push(self.supervisory(Supervisory::ReceiverReady, true, false));
    }

    fn supervisory(&self, function: Supervisory, poll: bool, final_bit: bool) -> Bytes {
        Frame::Supervisory {
            function,
            req_seq: self.expected_tx_seq,
            poll,
            final_bit,
        }
        .encode(self.params.remote_cid, self.params.fcs)
    }

    /// Retire every frame the peer says it has received.
    fn acknowledge(&mut self, req_seq: u8) {
        while let Some(front) = self.unacked.front() {
            // "Below req_seq" is modulo 64, so the comparison has to be a distance rather
            // than a `<`: at the wrap point the raw numbers say the opposite of the truth.
            if distance(front.tx_seq, req_seq) == 0
                || distance(front.tx_seq, req_seq) > SEQ_MODULO / 2
            {
                break;
            }
            self.unacked.pop_front();
        }
        if self.unacked.is_empty() {
            self.retransmission_timer = None;
        }
    }

    fn on_supervisory(
        &mut self,
        function: Supervisory,
        req_seq: u8,
        poll: bool,
        out: &mut ErtmOutput,
    ) {
        match function {
            Supervisory::ReceiverNotReady => self.remote_busy = true,
            Supervisory::ReceiverReady => self.remote_busy = false,
            Supervisory::Reject => {
                self.remote_busy = false;
                self.retransmit_all(out);
            }
            Supervisory::SelectiveReject => {
                self.remote_busy = false;
                self.retransmit_one(req_seq, out);
            }
            Supervisory::Other(_) => {}
        }
        if poll {
            // A poll must be answered with the final bit set, or the peer sits in its
            // monitor loop until it gives up on a channel that is perfectly healthy.
            out.frames
                .push(self.supervisory(Supervisory::ReceiverReady, false, true));
        }
    }

    fn on_information(
        &mut self,
        tx_seq: u8,
        sar: Segmentation,
        sdu_len: Option<u16>,
        payload: Bytes,
        out: &mut ErtmOutput,
    ) {
        if tx_seq != self.expected_tx_seq {
            // Either a retransmission of something already delivered, or a frame that
            // arrived after a gap. Both are answered by saying where we actually are; the
            // second additionally asks for the missing one, but only once.
            if !self.rejected && distance(self.expected_tx_seq, tx_seq) < SEQ_MODULO / 2 {
                self.rejected = true;
                out.frames
                    .push(self.supervisory(Supervisory::Reject, false, false));
            } else {
                out.frames
                    .push(self.supervisory(Supervisory::ReceiverReady, false, false));
            }
            return;
        }
        self.rejected = false;
        self.expected_tx_seq = next_seq(self.expected_tx_seq);

        match sar {
            Segmentation::Unsegmented => {
                self.reassembly = None;
                out.sdus.push(payload);
            }
            Segmentation::Start => {
                let expected = usize::from(sdu_len.unwrap_or(0));
                if expected > usize::from(self.params.local_mtu) {
                    // A peer announcing an SDU larger than the MTU it agreed to is either
                    // confused or hostile; either way we do not allocate for it.
                    self.discarded = self.discarded.saturating_add(1);
                    self.reassembly = None;
                    debug!(
                        cid = %self.params.local_cid,
                        expected,
                        local_mtu = self.params.local_mtu,
                        "ertm: refusing an sdu larger than the mtu we agreed to receive"
                    );
                } else {
                    let mut buffer = BytesMut::with_capacity(expected);
                    buffer.extend_from_slice(&payload);
                    self.reassembly = Some(Reassembly { expected, buffer });
                }
            }
            Segmentation::Continuation | Segmentation::End => {
                let complete = match &mut self.reassembly {
                    Some(state) => {
                        state.buffer.extend_from_slice(&payload);
                        sar == Segmentation::End
                    }
                    // A continuation with no start is a fragment of an SDU we already gave
                    // up on. Dropping it is the only honest option; delivering the tail
                    // alone would hand the layer above a truncated object.
                    None => {
                        self.discarded = self.discarded.saturating_add(1);
                        debug!(
                            cid = %self.params.local_cid,
                            "ertm: dropping a continuation with no start"
                        );
                        false
                    }
                };
                if complete {
                    if let Some(state) = self.reassembly.take() {
                        if state.buffer.len() == state.expected {
                            out.sdus.push(state.buffer.freeze());
                        } else {
                            self.discarded = self.discarded.saturating_add(1);
                            debug!(
                                cid = %self.params.local_cid,
                                got = state.buffer.len(),
                                expected = state.expected,
                                "ertm: dropping an sdu that did not reassemble to its declared length"
                            );
                        }
                    }
                }
            }
        }

        // Acknowledge immediately rather than waiting to piggyback on data we may never
        // send: this end mostly receives, and a delayed acknowledgement it never gets to
        // attach to stalls the peer's window.
        out.frames
            .push(self.supervisory(Supervisory::ReceiverReady, false, false));
    }

    /// Send whatever the window has room for.
    fn pump(&mut self, out: &mut ErtmOutput) {
        if self.remote_busy || self.waiting_for_final {
            return;
        }
        while usize::from(self.params.send_window) > self.unacked.len() {
            let Some(mut frame) = self.queued.pop_front() else {
                break;
            };
            frame.tx_seq = self.next_tx_seq;
            frame.transmissions = 1;
            self.next_tx_seq = next_seq(self.next_tx_seq);
            debug!(
                cid = %self.params.local_cid,
                tx_seq = frame.tx_seq,
                req_seq = self.expected_tx_seq,
                sar = ?frame.sar,
                len = frame.payload.len(),
                "ertm: tx"
            );
            out.frames.push(self.encode_information(&frame, false));
            self.unacked.push_back(frame);
        }
        self.arm_timers();
    }

    fn arm_timers(&mut self) {
        if self.unacked.is_empty() {
            self.retransmission_timer = None;
        } else if self.retransmission_timer.is_none() && self.monitor_timer.is_none() {
            self.retransmission_timer = Some(self.params.retransmission_timeout);
        }
    }

    fn encode_information(&self, frame: &Unacked, final_bit: bool) -> Bytes {
        Frame::Information {
            tx_seq: frame.tx_seq,
            req_seq: self.expected_tx_seq,
            final_bit,
            sar: frame.sar,
            sdu_len: frame.sdu_len,
            payload: frame.payload.clone(),
        }
        .encode(self.params.remote_cid, self.params.fcs)
    }

    fn retransmit_all(&mut self, out: &mut ErtmOutput) {
        let frames: Vec<Unacked> = self.unacked.iter().cloned().collect();
        for frame in frames {
            self.retransmit(&frame, out);
        }
    }

    fn retransmit_one(&mut self, tx_seq: u8, out: &mut ErtmOutput) {
        if let Some(frame) = self.unacked.iter().find(|f| f.tx_seq == tx_seq).cloned() {
            self.retransmit(&frame, out);
        }
    }

    fn retransmit(&mut self, frame: &Unacked, out: &mut ErtmOutput) {
        let attempts = frame.transmissions.saturating_add(1);
        debug!(
            cid = %self.params.local_cid,
            tx_seq = frame.tx_seq,
            attempts,
            max_transmit = self.params.max_transmit,
            "ertm: retransmitting"
        );
        if attempts > self.params.max_transmit {
            out.failed = true;
            return;
        }
        if let Some(slot) = self.unacked.iter_mut().find(|f| f.tx_seq == frame.tx_seq) {
            slot.transmissions = attempts;
        }
        out.frames.push(self.encode_information(frame, false));
        self.retransmission_timer = Some(self.params.retransmission_timeout);
    }
}

/// Bytes as hex, for the log lines that exist to be compared against a capture.
///
/// Truncated, because a frame is up to an ACL packet long and the interesting part of a
/// misframed one is always its head.
fn hex(bytes: &[u8]) -> String {
    const SHOWN: usize = 32;
    let mut out = String::with_capacity(bytes.len().min(SHOWN) * 2 + 8);
    for byte in bytes.iter().take(SHOWN) {
        out.push_str(&format!("{byte:02x}"));
    }
    if bytes.len() > SHOWN {
        out.push('…');
    }
    out
}

/// The next sequence number, wrapping at 64.
const fn next_seq(seq: u8) -> u8 {
    (seq + 1) % SEQ_MODULO
}

/// How far `to` is ahead of `from`, modulo 64.
const fn distance(from: u8, to: u8) -> u8 {
    (to + SEQ_MODULO - from) % SEQ_MODULO
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn params() -> ErtmParameters {
        ErtmParameters {
            local_cid: Cid::new(0x0041),
            remote_cid: Cid::new(0x0040),
            fcs: FcsType::Crc16,
            send_mps: 8,
            send_window: 4,
            max_transmit: 3,
            retransmission_timeout: Duration::from_millis(2000),
            monitor_timeout: Duration::from_millis(12000),
            local_mtu: 1024,
        }
    }

    /// Decode what an engine emitted, from the far end's point of view.
    fn peer_view(bytes: &Bytes, params: &ErtmParameters) -> Frame {
        Frame::decode(bytes, params.remote_cid, params.fcs).unwrap()
    }

    #[test]
    fn the_crc_matches_the_published_check_value() {
        // CRC-16/ARC's standard check value. Getting the reflection wrong yields a
        // checksum that is self-consistent and rejected by every peer, so this is
        // asserted against the published number rather than against ourselves.
        assert_eq!(crc16(b"123456789"), 0xBB3D);
    }

    #[test]
    fn the_fcs_covers_the_basic_l2cap_header_not_just_the_payload() {
        // The trap that makes an otherwise correct implementation fail every frame. The
        // same frame on two different channels must checksum differently, because the CID
        // is inside the covered bytes.
        let frame = Frame::Supervisory {
            function: Supervisory::ReceiverReady,
            req_seq: 3,
            poll: false,
            final_bit: false,
        };
        let on_40 = frame.encode(Cid::new(0x0040), FcsType::Crc16);
        let on_41 = frame.encode(Cid::new(0x0041), FcsType::Crc16);
        assert_eq!(on_40[..2], on_41[..2], "same control field");
        assert_ne!(on_40[2..], on_41[2..], "different checksum");
        // …and a frame checksummed for one channel is refused on the other.
        assert!(matches!(
            Frame::decode(&on_40, Cid::new(0x0041), FcsType::Crc16),
            Err(L2capError::BadFcs { .. })
        ));
    }

    #[test]
    fn control_fields_round_trip_in_both_frame_kinds() {
        let cid = Cid::new(0x0040);
        for frame in [
            Frame::Information {
                tx_seq: 5,
                req_seq: 9,
                final_bit: true,
                sar: Segmentation::Start,
                sdu_len: Some(600),
                payload: Bytes::from_static(&[1, 2, 3]),
            },
            Frame::Information {
                tx_seq: 63,
                req_seq: 0,
                final_bit: false,
                sar: Segmentation::Continuation,
                sdu_len: None,
                payload: Bytes::from_static(&[9]),
            },
            Frame::Supervisory {
                function: Supervisory::Reject,
                req_seq: 17,
                poll: true,
                final_bit: false,
            },
            Frame::Supervisory {
                function: Supervisory::ReceiverNotReady,
                req_seq: 62,
                poll: false,
                final_bit: true,
            },
        ] {
            let encoded = frame.encode(cid, FcsType::Crc16);
            assert_eq!(Frame::decode(&encoded, cid, FcsType::Crc16).unwrap(), frame);
            // …and the same without a checksum, since a channel may negotiate it away.
            let bare = frame.encode(cid, FcsType::None);
            assert_eq!(Frame::decode(&bare, cid, FcsType::None).unwrap(), frame);
        }
    }

    #[test]
    fn a_large_sdu_is_segmented_and_the_start_frame_leaves_room_for_the_length() {
        // The start frame spends two payload bytes on the SDU length. Segmenting as
        // though every frame were the same size overruns the negotiated MPS by exactly
        // two bytes on the first one — which the peer drops, silently.
        let params = params();
        let mut ertm = Ertm::new(params);
        let sdu = Bytes::from(vec![0x5Au8; 20]);
        let out = ertm.send(sdu.clone()).unwrap();
        assert_eq!(out.frames.len(), 3, "6 + 8 + 6 across an 8-byte mps");

        let mut seen = BytesMut::new();
        for (index, bytes) in out.frames.iter().enumerate() {
            assert!(
                bytes.len() <= usize::from(params.send_mps) + MAX_OVERHEAD,
                "frame {index} is {} bytes, past the mps",
                bytes.len()
            );
            let Frame::Information { sar, payload, .. } = peer_view(bytes, &params) else {
                panic!("expected an i-frame");
            };
            assert!(payload.len() <= usize::from(params.send_mps));
            match index {
                0 => assert_eq!(sar, Segmentation::Start),
                2 => assert_eq!(sar, Segmentation::End),
                _ => assert_eq!(sar, Segmentation::Continuation),
            }
            seen.extend_from_slice(&payload);
        }
        assert_eq!(seen.freeze(), sdu);
    }

    #[test]
    fn a_segmented_sdu_arrives_whole_at_the_other_end() {
        // The property the cover-art fetch depends on: a JPEG larger than one frame — and
        // larger than one send window — goes in one side and comes out the other, once,
        // intact. Both directions are shuttled, because the sender only gets to send its
        // last segments once the receiver's acknowledgements have retired the first ones.
        let mut sender = Ertm::new(params());
        let mut receiver = Ertm::new(ErtmParameters {
            local_cid: params().remote_cid,
            remote_cid: params().local_cid,
            ..params()
        });
        let sdu = Bytes::from((0..37u8).collect::<Vec<_>>());

        let mut to_receiver = sender.send(sdu.clone()).unwrap().frames;
        let mut to_sender: Vec<Bytes> = Vec::new();
        let mut delivered = Vec::new();
        for _ in 0..32 {
            if to_receiver.is_empty() && to_sender.is_empty() {
                break;
            }
            let mut next_to_receiver = Vec::new();
            for frame in std::mem::take(&mut to_receiver) {
                let out = receiver.receive(&frame).unwrap();
                delivered.extend(out.sdus);
                to_sender.extend(out.frames);
            }
            for frame in std::mem::take(&mut to_sender) {
                next_to_receiver.extend(sender.receive(&frame).unwrap().frames);
            }
            to_receiver = next_to_receiver;
        }
        assert_eq!(delivered, vec![sdu]);
    }

    #[test]
    fn every_delivered_frame_is_acknowledged() {
        let mut receiver = Ertm::new(params());
        let frame = Frame::Information {
            tx_seq: 0,
            req_seq: 0,
            final_bit: false,
            sar: Segmentation::Unsegmented,
            sdu_len: None,
            payload: Bytes::from_static(b"hello"),
        }
        .encode(params().local_cid, FcsType::Crc16);

        let out = receiver.receive(&frame).unwrap();
        assert_eq!(out.sdus, vec![Bytes::from_static(b"hello")]);
        let ack = peer_view(&out.frames[0], &params());
        assert_eq!(
            ack,
            Frame::Supervisory {
                function: Supervisory::ReceiverReady,
                req_seq: 1,
                poll: false,
                final_bit: false,
            },
            "the acknowledgement names the *next* frame expected, not the last received"
        );
    }

    #[test]
    fn a_gap_is_rejected_once_and_not_once_per_frame() {
        // A second REJ for the same gap makes the peer retransmit its whole window twice,
        // which on a busy link is how one lost frame becomes a stall.
        let mut receiver = Ertm::new(params());
        let out_of_order = |seq: u8| {
            Frame::Information {
                tx_seq: seq,
                req_seq: 0,
                final_bit: false,
                sar: Segmentation::Unsegmented,
                sdu_len: None,
                payload: Bytes::copy_from_slice(&[seq]),
            }
            .encode(params().local_cid, FcsType::Crc16)
        };

        let first = receiver.receive(&out_of_order(1)).unwrap();
        assert!(matches!(
            peer_view(&first.frames[0], &params()),
            Frame::Supervisory {
                function: Supervisory::Reject,
                ..
            }
        ));
        assert!(first.sdus.is_empty(), "a gap delivers nothing");

        let second = receiver.receive(&out_of_order(2)).unwrap();
        assert!(
            matches!(
                peer_view(&second.frames[0], &params()),
                Frame::Supervisory {
                    function: Supervisory::ReceiverReady,
                    ..
                }
            ),
            "the second out-of-order frame must not reject again"
        );
    }

    #[test]
    fn a_reject_retransmits_everything_still_outstanding() {
        let mut sender = Ertm::new(params());
        let out = sender.send(Bytes::from_static(b"one")).unwrap();
        let _ = sender.send(Bytes::from_static(b"two")).unwrap();
        assert_eq!(out.frames.len(), 1);

        let reject = Frame::Supervisory {
            function: Supervisory::Reject,
            req_seq: 0,
            poll: false,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        let replayed = sender.receive(&reject).unwrap();
        let sequences: Vec<u8> = replayed
            .frames
            .iter()
            .filter_map(|f| match peer_view(f, &params()) {
                Frame::Information { tx_seq, .. } => Some(tx_seq),
                Frame::Supervisory { .. } => None,
            })
            .collect();
        assert_eq!(
            sequences,
            vec![0, 1],
            "both unacknowledged frames come back"
        );
    }

    #[test]
    fn an_acknowledgement_retires_frames_and_stops_the_timer() {
        let mut sender = Ertm::new(params());
        sender.send(Bytes::from_static(b"one")).unwrap();
        assert!(sender.next_timeout().is_some(), "an unacked frame is timed");

        let ack = Frame::Supervisory {
            function: Supervisory::ReceiverReady,
            req_seq: 1,
            poll: false,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        sender.receive(&ack).unwrap();
        assert!(
            sender.next_timeout().is_none(),
            "nothing outstanding, nothing to time"
        );
    }

    #[test]
    fn an_unanswered_frame_is_polled_for_and_eventually_gives_up() {
        // The failure this exists to report: a peer that stops answering. Retrying
        // forever would leave a cover-art fetch pinned open for the life of the link.
        let mut sender = Ertm::new(params());
        sender.send(Bytes::from_static(b"one")).unwrap();

        let poll = sender.tick(Duration::from_millis(2000));
        assert!(!poll.failed);
        assert!(matches!(
            peer_view(&poll.frames[0], &params()),
            Frame::Supervisory { poll: true, .. }
        ));

        // Nothing answers, so the monitor timer repeats the poll until the allowance runs
        // out and the channel is declared dead.
        let mut failed = false;
        for _ in 0..8 {
            if sender.tick(Duration::from_millis(12_000)).failed {
                failed = true;
                break;
            }
        }
        assert!(failed, "a peer that never answers must fail the channel");
    }

    #[test]
    fn a_poll_is_answered_with_the_final_bit() {
        // A poll that goes unanswered leaves the peer in its monitor loop until it gives
        // up on a channel that is working perfectly.
        let mut receiver = Ertm::new(params());
        let poll = Frame::Supervisory {
            function: Supervisory::ReceiverReady,
            req_seq: 0,
            poll: true,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        let out = receiver.receive(&poll).unwrap();
        assert!(matches!(
            peer_view(&out.frames[0], &params()),
            Frame::Supervisory {
                final_bit: true,
                poll: false,
                ..
            }
        ));
    }

    #[test]
    fn a_corrupt_frame_is_dropped_rather_than_answered() {
        // We cannot trust the sequence number in a frame whose checksum failed, so there
        // is nothing safe to say about it. The sender's timer is what recovers it.
        let mut receiver = Ertm::new(params());
        let mut frame = Frame::Information {
            tx_seq: 0,
            req_seq: 0,
            final_bit: false,
            sar: Segmentation::Unsegmented,
            sdu_len: None,
            payload: Bytes::from_static(b"hello"),
        }
        .encode(params().local_cid, FcsType::Crc16)
        .to_vec();
        frame[3] ^= 0xFF;

        let out = receiver.receive(&frame).unwrap();
        assert!(out.frames.is_empty() && out.sdus.is_empty());
        assert_eq!(receiver.discarded(), 1);
    }

    #[test]
    fn a_peer_promising_more_than_the_mtu_is_not_allocated_for() {
        let mut receiver = Ertm::new(params());
        let start = Frame::Information {
            tx_seq: 0,
            req_seq: 0,
            final_bit: false,
            sar: Segmentation::Start,
            sdu_len: Some(60_000),
            payload: Bytes::from_static(&[0; 4]),
        }
        .encode(params().local_cid, FcsType::Crc16);
        let out = receiver.receive(&start).unwrap();
        assert!(out.sdus.is_empty());
        assert_eq!(receiver.discarded(), 1);
    }

    #[test]
    fn the_send_window_bounds_what_goes_out_at_once() {
        // Four frames of window, seven segments to send: the rest wait for an
        // acknowledgement rather than being written into a buffer the peer does not have.
        let mut sender = Ertm::new(ErtmParameters {
            send_window: 4,
            send_mps: 4,
            ..params()
        });
        let out = sender.send(Bytes::from(vec![0u8; 26])).unwrap();
        assert_eq!(out.frames.len(), 4, "the window, not the segment count");

        let ack = Frame::Supervisory {
            function: Supervisory::ReceiverReady,
            req_seq: 2,
            poll: false,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        let more = sender.receive(&ack).unwrap();
        assert_eq!(more.frames.len(), 2, "two retired, two more may go");
    }

    #[test]
    fn a_busy_peer_stops_the_flow_until_it_says_otherwise() {
        let mut sender = Ertm::new(params());
        let busy = Frame::Supervisory {
            function: Supervisory::ReceiverNotReady,
            req_seq: 0,
            poll: false,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        sender.receive(&busy).unwrap();
        let blocked = sender.send(Bytes::from_static(b"one")).unwrap();
        assert!(blocked.frames.is_empty(), "a busy peer gets nothing");

        let ready = Frame::Supervisory {
            function: Supervisory::ReceiverReady,
            req_seq: 0,
            poll: false,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        let resumed = sender.receive(&ready).unwrap();
        assert_eq!(resumed.frames.len(), 1, "and it flows again afterwards");
    }

    #[test]
    fn sequence_numbers_wrap_at_sixty_four() {
        assert_eq!(next_seq(63), 0);
        assert_eq!(distance(62, 1), 3, "distance is modular, not arithmetic");
        assert_eq!(distance(1, 1), 0);
    }

    #[test]
    fn acknowledgements_are_understood_across_the_wrap_point() {
        // The bug this catches: comparing raw sequence numbers with `<` retires the whole
        // window at the wrap, or none of it.
        let mut sender = Ertm::new(ErtmParameters {
            send_window: 8,
            ..params()
        });
        for _ in 0..62 {
            sender.send(Bytes::from_static(b"x")).unwrap();
            let ack = Frame::Supervisory {
                function: Supervisory::ReceiverReady,
                req_seq: sender.next_tx_seq,
                poll: false,
                final_bit: false,
            }
            .encode(params().local_cid, FcsType::Crc16);
            sender.receive(&ack).unwrap();
        }
        assert!(sender.unacked.is_empty());
        assert_eq!(sender.next_tx_seq, 62);

        // Straddle the wrap: send four, acknowledge two.
        for _ in 0..4 {
            sender.send(Bytes::from_static(b"x")).unwrap();
        }
        assert_eq!(sender.unacked.len(), 4);
        let ack = Frame::Supervisory {
            function: Supervisory::ReceiverReady,
            req_seq: 0, // 62 and 63 received; 0 is next
            poll: false,
            final_bit: false,
        }
        .encode(params().local_cid, FcsType::Crc16);
        sender.receive(&ack).unwrap();
        assert_eq!(sender.unacked.len(), 2, "two retired across the wrap");
    }
}
