//! # proto-cast
//!
//! A Google Cast (CASTv2) receiver. The [`framing`], [`proto`], [`messages`], and
//! [`session`] modules are pure and socket-free (ground rule 3): they fold sender
//! messages into outgoing messages + [`castaway_core::SessionEvent`]s, unit-tested
//! against constructed `CastMessage`s. [`actor`] is the thin TLS shell that composes
//! them with I/O — it makes no protocol decisions of its own.
//!
//! Both casting paths are implemented. The **media-URL** path (Default Media Receiver
//! `LOAD`) hands the pipeline a URI; the **mirroring** path negotiates in [`mirror`],
//! reassembles RTP in [`rtp`]/[`receiver`], reports back with [`rtcp`], and is driven
//! over UDP by [`rtp_actor`].
#![forbid(unsafe_code)]

pub mod actor;
pub mod auth;
pub mod control;
pub mod error;
pub mod framing;
pub mod messages;
pub mod mirror;
pub mod platform;
pub mod platform_actor;
pub mod proto;
pub mod receiver;
pub mod replay;
pub mod rtcp;
pub mod rtp;
pub mod rtp_actor;
pub mod session;

pub use actor::{CastIdentity, CastReceiver, TlsIdentity};
pub use auth::CastAuthResponder;
pub use control::CastRemote;
pub use error::CastError;
pub use messages::{ns, DEFAULT_MEDIA_RECEIVER_APP_ID};
pub use mirror::{Codec, MediaKind, MirrorConfig, StreamConfig};
pub use platform::{
    AppIdentity, DeviceCapabilities, DisconnectReason, IpcFrame, PlatformEvent, PlatformSession,
};
pub use platform_actor::{HostEvent, PlatformHost, PlatformServer};
pub use proto::CastMessage;
pub use receiver::{CastRtpReceiver, Consume, Delivered, Received};
pub use replay::{ReplayAuthResponder, ReplayIdentity};
pub use rtcp::{BuildReport, Feedback};
pub use rtp::{
    CastRtpPacket, CastRtpStream, EncryptedFrame, FrameCollector, FrameId, NackTarget, PacketId,
    PacketNack, RtpError,
};
pub use rtp_actor::{MirrorRtp, MirrorSocket};
pub use session::{CastSession, DeviceAuthResponder, Reaction};

/// The default Cast TLS port senders connect to.
pub const CAST_PORT: u16 = 8009;

/// The mDNS service type Cast senders browse for.
pub const CAST_SERVICE_TYPE: &str = "_googlecast._tcp";
