//! # proto-airplay
//!
//! An AirPlay receiver. What works today, pure and tested: the mDNS advertisements that
//! make "castaway" appear in the AirPlay picker ([`advert`]), the `/info` capabilities
//! plist ([`info`]), and the RTSP dispatch state machine ([`session`]) — driven over a
//! real socket by [`actor`].
//!
//! The target is **AirPlay 1 audio** (RAOP), which needs neither pairing nor FairPlay:
//! the media key arrives in the `ANNOUNCE` SDP rather than from `/fp-setup`. What is
//! advertised is therefore deliberately narrow, and [`advert`] is where the reasoning
//! lives — every feature bit is a promise, and a bit set ahead of the code behind it
//! sends a sender down a flow that ends in a `501` with nothing on screen to say why.
//!
//! What's gated: mirroring needs the FairPlay key unwrap, and HomeKit pairing is not
//! implemented. The dispatch models those transactions and returns `501` at the
//! boundary, so the control flow is real and the media plane slots in behind it.
//! `docs/airplay-research.md` is the record of what each costs.
#![forbid(unsafe_code)]

pub mod actor;
pub mod advert;
pub mod audio;
pub mod clock;
pub mod control;
pub mod error;
pub mod info;
pub mod sdp;
pub mod session;
pub mod transport;

use castaway_core::ProtocolKind;

pub use actor::AirPlayReceiver;
pub use advert::{AirPlayIdentity, Features, AIRPLAY_PORT};
pub use clock::{NtpTime, ResendRequest, ResendTracker, TimingClient};
pub use control::{ControlUpdate, Progress, Volume};
pub use error::{AirPlayError, ControlError, SdpError, TransportError};
pub use sdp::{AlacConfig, AnnounceParams, RaopCodec, SessionKey, StreamCrypto};
pub use session::{AirPlayResponse, AirPlaySession};
pub use transport::{ReceiverPorts, SenderPorts};

/// The protocol kind for AirPlay sources.
#[must_use]
pub fn kind() -> ProtocolKind {
    ProtocolKind::AirPlay
}
