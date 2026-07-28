//! # proto-gamestream
//!
//! The GameStream/Sunshine *client* — the one protocol where castaway dials out. Every
//! other adapter is a receiver a sender connects to; here the panel is the Moonlight
//! side of the wire: it browses for `_nvstream._tcp` hosts, pairs with a PIN, asks
//! NVHTTP for the app list, launches one, and then hands the session to
//! moonlight-common-c, which owns the RTSP handshake, ENet control stream, FEC'd RTP
//! video, and encrypted audio (DECISION-LOG D37 — linked, not reimplemented, at the
//! user's direction).
//!
//! What is ours, and pure: [`pairing`] (the gen-7 challenge crypto, sans-I/O),
//! [`nvhttp`] (request building and XML responses as rich types), [`identity`] (the
//! client certificate that *is* the pairing credential), [`discovery`] (mDNS browse
//! results → typed hosts). The I/O shims are [`http`] (TLS with our client cert and the
//! pinned server cert) and [`adapter`] (the tokio actor + chooser router).
//!
//! Unsafe is quarantined: the crate is `deny(unsafe_code)` and only `stream::ffi` — the
//! trampolines moonlight-common-c calls back into — opts out, module by module, with a
//! SAFETY comment per block (rule 8). The `stream` feature gates everything that links
//! the C library; without it this crate is pure Rust and fully testable.
#![deny(unsafe_code)]

pub mod adapter;
pub mod client;
pub mod discovery;
pub mod error;
pub mod http;
pub mod identity;
pub mod nvhttp;
pub mod pairing;
#[cfg(feature = "stream")]
pub mod stream;

pub use adapter::{GameStreamAdapter, GameStreamCommand, PairingStore, SessionPreferences};
pub use client::{generate_session_keys, GameStreamClient};
pub use discovery::HostCandidate;
pub use error::GameStreamError;
pub use identity::ClientIdentity;
pub use nvhttp::{App, LaunchParams, ServerInfo, UniqueId};
pub use pairing::PairedServer;

/// The mDNS service type GameStream hosts (Sunshine, GFE) advertise.
pub const NVSTREAM_SERVICE_TYPE: &str = "_nvstream._tcp";

/// The protocol this crate implements, for `SourceId` tagging.
#[must_use]
pub fn kind() -> castaway_core::ProtocolKind {
    castaway_core::ProtocolKind::GameStream
}
