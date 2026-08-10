//! # proto-fcast
//!
//! FCast receiver: FUTO's open casting protocol, the cast button in Grayjay (#241).
//! A media-URL protocol in the DLNA shape — the sender pushes a URL (or a playlist),
//! the receiver fetches and plays it — carried as length-prefixed JSON messages over
//! one TCP connection on port 46899, discovered via `_fcast._tcp` mDNS.
//!
//! Implements protocol v1-v3 as published and advertises v3; see
//! [`session`] for the version-scope reasoning (v4 is TLS + FlatBuffers +
//! mirroring, issue #248).
//!
//! Layered per ground rule 3: [`wire`] (framing), [`messages`] (bodies),
//! [`session`] (per-connection state machine) and [`player`] (receiver-side
//! playback model, which owns the playlist) are pure and replay the transcripts
//! captured from the reference sender; [`adapter`] is the tokio shell that owns the
//! listener and the clock. [`control`] is the panel's transport strip over a live
//! session.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod connect_url;
pub mod control;
pub mod error;
pub mod identity;
pub mod messages;
pub mod player;
pub mod session;
pub mod session_v4;
pub mod v4msg;
pub mod wire;

pub use adapter::{FCastReceiver, FCAST_PORT, FCAST_SERVICE_TYPE};
pub use error::FCastError;
