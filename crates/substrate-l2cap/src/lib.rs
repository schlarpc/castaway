//! # substrate-l2cap
//!
//! BR/EDR L2CAP in basic mode: PDU framing, the signaling channel, and a sans-I/O
//! [`Multiplexer`] that runs channels through connect → configure → open → disconnect.
//!
//! This is the layer Windows has no user-mode access to at all (Winsock's `AF_BTH`
//! exposes RFCOMM and nothing else), which is why the whole stack is ours rather than the
//! OS's — see architecture-substrate.md §11.1. Everything an A2DP sink does rides here:
//! AVDTP on PSM `0x0019`, AVCTP on `0x0017`, and the OBEX cover-art channel on whichever
//! PSM the peer's SDP record names.
//!
//! Pure and synchronous by design (ground rule 3): feed it reassembled PDUs, write out
//! the [`L2capEvent::Send`]s it returns. No sockets, so the full handshake is testable
//! with no radio.
#![forbid(unsafe_code)]

pub mod error;
pub mod mux;
pub mod pdu;
pub mod signaling;

pub use error::L2capError;
pub use mux::{Channel, ChannelState, L2capEvent, Multiplexer, DEFAULT_MTU};
pub use pdu::{Cid, L2capPdu, Psm};
pub use signaling::{ConfigOption, ConfigResult, ConnectionResult, Signal};
