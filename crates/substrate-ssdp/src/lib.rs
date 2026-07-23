//! # substrate-ssdp
//!
//! The shared SSDP/UPnP discovery substrate: one responder answering `M-SEARCH` and
//! emitting periodic `NOTIFY` on UDP 1900, plus one HTTP host serving device/service
//! description XML. DLNA and DIAL mount their SOAP/REST handlers on the same host —
//! we advertise once, not five racing responders (architecture §1d).
//!
//! The message layer ([`message`]) and description model ([`device`]) are pure and
//! socket-free so they unit-test against captured datagrams (ground rule 3). The
//! [`responder`] actor is the thin I/O shell around them.
#![forbid(unsafe_code)]

pub mod device;
pub mod error;
pub mod message;
pub mod responder;

pub use device::{SsdpDevice, Target};
pub use error::SsdpError;
pub use message::{SearchTarget, SsdpRequest, SsdpResponse};
pub use responder::{Responder, ResponderConfig};

/// The SSDP multicast group address.
pub const SSDP_MULTICAST_ADDR: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 255, 250);

/// The SSDP port.
pub const SSDP_PORT: u16 = 1900;
