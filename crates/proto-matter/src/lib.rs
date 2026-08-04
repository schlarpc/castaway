//! # proto-matter
//!
//! The Matter Casting receiver: the **Casting Video Player** role.
//!
//! Matter Casting inverts the roles every other Matter deployment uses. The phone is the
//! *commissionee*; the panel is the *commissioner* and the fabric's administrator. So a
//! receiver is not one stack but two halves that meet in the middle:
//!
//! 1. It runs a certificate authority, commissions the phone onto a fabric it owns, and
//!    issues it a node operational certificate.
//! 2. It then serves the interaction model *back* to that phone, hosting a Video Player
//!    endpoint and one Content App endpoint per thing we can actually play.
//!
//! No media crosses this protocol. A `LaunchURL` is a sentence, not a stream — the app
//! named in it is expected to fetch its own bytes from its own backend, which is why the
//! panel's answer to a cast is a [`castaway_core::SessionEvent`] pointed at the browser or
//! the player, exactly as DIAL's is.
//!
//! ## What is ours and what is `rs-matter`'s
//!
//! The Matter core — TLV, MRP, PASE, CASE, the interaction model, certificates — is
//! `rs-matter` (DECISION-LOG D54). Everything that touches this LAN is ours:
//!
//! - [`udc`] — User Directed Commissioning, which `rs-matter` does not implement at all.
//! - [`node`] — the endpoint tree, the device types, and the media cluster handlers.
//! - [`player`] — what a cluster command *means* for the panel.
//! - [`adapter`] — the actor, the sockets, and the mDNS records, on our one responder.
#![forbid(unsafe_code)]

pub mod adapter;
pub mod discovery;
pub mod error;
pub mod fabric;
pub mod net;
pub mod node;
pub mod player;
pub mod server;
pub mod udc;

pub use adapter::{BrowserLaunch, MatterAdapter, MatterConfig};
pub use error::{MatterError, UdcError};
pub use player::{CastCommand, Catalogue, ContentApp, LaunchTarget, PlayerState, Surface};

/// The Matter operational UDP port (`rs_matter::MATTER_PORT`), re-exported so the
/// network-surface registry names one constant rather than reaching past this crate.
pub use rs_matter::MATTER_PORT;
pub use udc::{
    CdError, CommissionerDeclaration, IdentificationDeclaration, InstanceName, TargetApp,
    UdcRequest, UDC_PORT,
};

use castaway_core::ProtocolKind;

/// The protocol kind for Matter Casting sources.
#[must_use]
pub fn kind() -> ProtocolKind {
    ProtocolKind::MatterCast
}
