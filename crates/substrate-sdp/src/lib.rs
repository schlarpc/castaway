//! # substrate-sdp
//!
//! Service Discovery Protocol: the records we publish about ourselves, and the one query
//! we make of the peer.
//!
//! Both halves matter, for different reasons. As a **server** this is how a phone learns
//! we are an A2DP sink on PSM `0x0019` and an AVRCP controller on `0x0017`. As a
//! **client** it is the only route to album art: AVRCP 1.6 publishes the cover-art OBEX
//! PSM in the peer's Target record, `bluetoothd` parses that record and never surfaces
//! the field, and `obexd` has no BIP client — which is precisely why owning the stack is
//! what makes artwork reachable at all (architecture-substrate.md §11.1).
//!
//! Pure and synchronous: [`SdpServer::handle`] is bytes in, bytes out (ground rule 3).
#![forbid(unsafe_code)]

pub mod client;
pub mod element;
pub mod error;
pub mod pdu;
pub mod record;
pub mod server;
pub mod uuid;

pub use client::Query;
pub use element::DataElement;
pub use error::SdpError;
pub use pdu::{AttributeRange, Continuation, SdpRequest, SdpResponse};
pub use record::{a2dp_sink, avrcp_controller, avrcp_target, ServiceRecord};
pub use server::{parse_records, SdpServer};
pub use uuid::{Uuid, UuidWidth};
