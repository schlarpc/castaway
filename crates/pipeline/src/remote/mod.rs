//! The remote-control transport: the panel's duplicate, and the contacts that come back
//! (#18).
//!
//! ## Why WebRTC and not the HLS the same encoder already feeds
//!
//! Latency. `/stream/*` is one-second segments with a window of eight, so three to six
//! seconds glass-to-glass — fine for "show me what the panel is doing", unusable for
//! "drive it", where you cannot tell which tap did what. The other half of the argument is
//! the deployment: the far end is a phone on Wi-Fi, and a fixed-bitrate stream over a TCP
//! socket turns a lossy link into an unbounded stall, with "fall behind, then seek to the
//! live edge" as the only recovery. UDP with a jitter buffer degrades instead.
//!
//! ## Why the input rides the same connection
//!
//! A data channel defaults to reliable and ordered, which is exactly what input needs: a
//! lost `Up` after a `Down` strands a contact for the rest of the session. Given that, the
//! reason to prefer it over a second socket is that one `PeerConnection` is *one
//! lifecycle* — "the peer went away" is a single event with a single handler, and the
//! cancel-on-disconnect path is where the nastiest bug in this feature lives. Two
//! connections would mean reconciling which is alive, which identity binds them, and what
//! happens to a finger that is down when only one of them notices.
//!
//! ## Signalling
//!
//! WHEP, near enough: the peer POSTs an SDP offer to `/remote/whep` and gets an answer. No
//! trickle — the answer is not sent until gathering completes, so one request is the whole
//! negotiation and there is nothing to keep open. The route is the app's; this module is
//! handed an offer and returns an answer.
//!
//! ## Where the sockets come from
//!
//! From `[remote.ice_ports]`, one per peer, never ephemeral. `crates/app/src/surface.rs`
//! generates the firewall, so a candidate outside a declared range is one the deployed box
//! silently drops — the connection would negotiate and then carry nothing.
//!
//! ## What is compiled when
//!
//! [`page`] is a string and is always here: a build with no transport still serves the
//! player, which then says what is wrong where whoever pressed the button is looking.
//! Everything else needs the `remote` feature.

pub mod page;
pub use page::PLAYER;

#[cfg(feature = "remote")]
mod service;

#[cfg(feature = "remote")]
pub use service::{RemoteConfig, RemoteService};
