//! # proto-bluetooth-audio
//!
//! A2DP sink and AVRCP: the profiles that turn an L2CAP link into music on the panel
//! with a now-playing card above it.
//!
//! The codec table in [`codec`] is the point of the whole exercise — SBC, AAC, aptX,
//! aptX HD and LDAC, where Windows' inbox sink offers SBC alone and gives no way to add
//! one (architecture-substrate.md §11.1).
//!
//! Pure and synchronous throughout (ground rule 3): [`avdtp::Message`] is bytes in, bytes
//! out, and the state machines are driven by the caller.
#![forbid(unsafe_code)]

pub mod avctp;
pub mod avdtp;
pub mod avrcp;
pub mod codec;
pub mod error;
pub mod media;
pub mod sink;

pub use avctp::{AvcFrame, AvctpMessage, Ctype};
pub use avdtp::{Message, MessageType, Seid, Signal, StreamEndpoint};
pub use avrcp::{TrackAttributes, VendorPdu};
pub use codec::{advertised, ChannelModes, CodecCapability, SampleRates};
pub use error::AudioError;
pub use media::Depacketizer;
pub use sink::{SinkEvent, SinkSession, StreamState};
