//! # substrate-l2cap
//!
//! BR/EDR L2CAP: PDU framing, the signaling channel, and a sans-I/O [`Multiplexer`] that
//! runs channels through connect → configure → open → disconnect — in basic mode, or in
//! [Enhanced Retransmission Mode](ertm) where the profile demands it.
//!
//! This is the layer Windows has no user-mode access to at all (Winsock's `AF_BTH`
//! exposes RFCOMM and nothing else), which is why the whole stack is ours rather than the
//! OS's — see architecture-substrate.md §11.1. Everything an A2DP sink does rides here:
//! AVDTP on PSM `0x0019`, AVCTP on `0x0017`, and the OBEX cover-art channel on whichever
//! PSM the peer's SDP record names — the last of which is why ERTM exists here at all,
//! since GOEP 2.0 requires it and album art rides on GOEP (Q29).
//!
//! Pure and synchronous by design (ground rule 3): feed it reassembled PDUs, write out
//! the [`L2capEvent::Send`]s it returns, and advance retransmission timers with
//! [`Multiplexer::tick`]. No sockets and no clock, so the full handshake — retransmission
//! included — is testable with no radio.
#![forbid(unsafe_code)]

pub mod error;
pub mod ertm;
pub mod mux;
pub mod pdu;
pub mod signaling;

pub use error::L2capError;
pub use ertm::{ChannelMode, Ertm, ErtmParameters, FcsType, Frame, RetransmissionConfig};
pub use mux::{Channel, ChannelState, L2capEvent, ModeParameters, Multiplexer, DEFAULT_MTU};
pub use pdu::{Cid, L2capPdu, Psm};
pub use signaling::{ConfigOption, ConfigResult, ConnectionResult, Signal};
