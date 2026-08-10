//! Receiving a WebRTC mirror: a sender offers its screen, the panel answers (#248).
//!
//! The mirror image of [`crate::remote`], and worth saying out loud because the two look
//! alike and point opposite ways. There the panel *sends* its own duplicate to a browser,
//! adds a local track and pumps frames into it; here a sender sends **us** its screen, we
//! add no track at all, and the pictures arrive on a `TrackRemote` we poll. FCast v4 is
//! the first protocol to ask for this, and the signalling is entirely its own — the offer
//! arrives as a `MirroringSessionDescription` on its control connection and the answer
//! goes back the same way — so nothing here knows what FCast is.
//!
//! ## Host candidates only
//!
//! No ICE servers, exactly as [`crate::remote::RemoteService`]: this is a LAN receiver, a
//! STUN round trip would add latency to every connection, and the reflexive address it
//! returns is useless to a peer on the same network. Non-trickle for a harder reason than
//! there — FCast's signalling has *one* message for the answer and no way to send a
//! second, so the answer either carries every candidate or the session never connects.
//!
//! ## What is compiled when
//!
//! [`assemble`] is pure and always here, so the reassembly and clock fixtures run in every
//! build. The transport needs the `remote` feature, whose meaning is "webrtc is linked"
//! rather than "the remote-control page exists" — see D55 on why there is not a second
//! feature for the same dependency.

pub mod assemble;

pub use assemble::{MirrorCodec, TrackAssembler};

#[cfg(feature = "remote")]
mod service;

#[cfg(feature = "remote")]
pub use service::{MirrorReceiver, MirrorReceiverConfig};
