//! # castaway-core
//!
//! The internal API every casting protocol funnels into. Adapters speak their own
//! wire protocol, then reduce everything to a [`SessionEvent`]; the [`session`]
//! manager arbitrates a single active source and drives a [`Pipeline`].
//!
//! Nothing here touches the GPU, a socket, or a codec — this crate is pure types
//! and traits so that protocol logic stays testable without hardware (ground rule 3).
#![forbid(unsafe_code)]

pub mod adapter;
pub mod control;
pub mod display;
pub mod error;
pub mod event;
pub mod nowplaying;
pub mod osd;
pub mod pipeline;
pub mod session;
pub mod source;
pub mod types;

pub use adapter::{MiracastBackend, SessionSink, SourceAdapter, SourceId, SourceMessage};
pub use control::{ControlCapabilities, RemoteControl};
pub use display::{DisplayControl, DisplayInput};
pub use error::CoreError;
pub use event::{Advertisement, ControlTxn, SessionEvent};
pub use nowplaying::{Artwork, ImageFormat, NowPlaying, PlaybackState, QueueItem};
pub use osd::{osd_channel, OsdCommand, OsdMessage, OsdReceiver, OsdSink};
pub use pipeline::Pipeline;
pub use session::{SessionConfig, SessionManager};
pub use source::SourceDescription;
pub use types::{
    AudioCodec, AudioFormat, ColorInfo, ColorRange, ColorSpace, DecodedFrame, EncodedFrame,
    FrameImage, FrameSource, FriendlyName, GpuSurface, MediaUri, PcmFrame, PixelFormat,
    ProtocolKind, VideoCodec,
};
