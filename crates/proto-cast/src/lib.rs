//! # proto-cast
//!
//! A Google Cast (CASTv2) receiver. The [`framing`], [`proto`], [`messages`], and
//! [`session`] modules are pure and socket-free (ground rule 3): they fold sender
//! messages into outgoing messages + [`castaway_core::SessionEvent`]s, unit-tested
//! against constructed `CastMessage`s. [`actor`] is the thin TLS shell that composes
//! them with I/O — it makes no protocol decisions of its own.
//!
//! Today this implements the **media-URL** path (Default Media Receiver `LOAD`).
//! Mirroring (offer/answer + custom RTP) is the next Cast milestone.
#![forbid(unsafe_code)]

pub mod actor;
pub mod auth;
pub mod error;
pub mod framing;
pub mod messages;
pub mod mirror;
pub mod proto;
pub mod session;

pub use actor::{CastReceiver, TlsIdentity};
pub use auth::CastAuthResponder;
pub use error::CastError;
pub use messages::{ns, DEFAULT_MEDIA_RECEIVER_APP_ID};
pub use mirror::{Codec, MirrorConfig, StreamConfig};
pub use proto::CastMessage;
pub use session::{CastSession, DeviceAuthResponder, Reaction};

/// The default Cast TLS port senders connect to.
pub const CAST_PORT: u16 = 8009;

/// The mDNS service type Cast senders browse for.
pub const CAST_SERVICE_TYPE: &str = "_googlecast._tcp";
