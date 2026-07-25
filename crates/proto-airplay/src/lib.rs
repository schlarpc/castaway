//! # proto-airplay
//!
//! An AirPlay receiver. What works today, pure and tested: the mDNS advertisements that
//! make "castaway" appear in the AirPlay picker ([`advert`]), the `/info` capabilities
//! plist ([`info`]), and the RTSP dispatch state machine ([`session`]) — driven over
//! real sockets by [`actor`], which listens on both the AirPlay and RAOP ports.
//!
//! What's gated: mirroring media requires the FairPlay-SAP session key, and HomeKit
//! transient pairing isn't implemented — both are captured-tables / RE work (Q1). The
//! dispatch models those transactions and returns `501` at the boundary, so the control
//! flow is real and the media plane slots in once the crypto lands.
#![forbid(unsafe_code)]

pub mod actor;
pub mod advert;
pub mod error;
pub mod info;
pub mod session;

use castaway_core::ProtocolKind;

pub use actor::{AirPlayReceiver, Channel};
pub use advert::{AirPlayIdentity, AIRPLAY_PORT, RAOP_PORT};
pub use error::AirPlayError;
pub use session::{AirPlayResponse, AirPlaySession};

/// The protocol kind for AirPlay sources.
#[must_use]
pub fn kind() -> ProtocolKind {
    ProtocolKind::AirPlay
}
