//! # substrate-hci
//!
//! The Host Controller Interface: framing, the BR/EDR command/event subset an A2DP sink
//! needs, and [`HciTransport`] — the seam that is the *entire* platform-specific surface
//! of the Bluetooth stack.
//!
//! Why we are down here at all rather than using the OS: Windows' inbox A2DP sink is
//! SBC-only and never hands us the stream, and Winsock exposes no user-mode L2CAP to
//! route around it; BlueZ would give us every codec but never surfaces AVRCP's cover-art
//! handle. Owning everything above HCI is the only configuration that gets all five
//! codecs, album art, and one implementation tested once
//! (architecture-substrate.md §11).
//!
//! The crate is pure: [`packet`] does framing, [`command`]/[`event`] do semantics, and
//! nothing here opens a socket. The transports live behind [`HciTransport`], and
//! [`ScriptedTransport`] lets the whole stack above be tested with no radio present
//! (ground rules 3 and 6).
#![forbid(unsafe_code)]

pub mod addr;
pub mod command;
pub mod eir;
pub mod error;
pub mod event;
pub mod flow;
pub mod opcode;
pub mod packet;
pub mod status;
pub mod transport;

pub use addr::BdAddr;
pub use command::{
    AcceptRole, AuthRequirements, ClassOfDevice, Command, IoCapability, LinkKey, ScanEnable,
};
pub use eir::Eir;
pub use error::HciError;
pub use event::{BufferSize, Event, LinkType};
pub use flow::{AclCredits, CommandCredits};
pub use opcode::{Ogf, OpCode};
pub use packet::{AclPacket, Broadcast, ConnectionHandle, HciPacket, PacketBoundary, PacketType};
pub use status::Status;
pub use transport::{HciTransport, Reassembler, ScriptedTransport};
