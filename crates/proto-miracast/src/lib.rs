//! # proto-miracast
//!
//! A Miracast / Wi-Fi Display **sink**.
//!
//! Miracast is the odd one out in this workspace (architecture §1e): it does not use IP
//! multicast discovery at all, so there is no mDNS or SSDP to share. Discovery is
//! Wi-Fi Direct at L2 — privileged, driver-dependent, and per-OS. Everything *above*
//! that is portable and lives here: the WFD information element the P2P layer carries,
//! the `wfd_*` parameter grammar, the M1–M16 RTSP exchange, and MPEG2-TS-over-RTP.
//!
//! The split follows ground rule 5. The pure layers below have no idea whether a
//! wpa_supplicant P2P group or a WinRT `WiFiDirectAdvertisement` put a socket in front
//! of them; the platform seam is [`castaway_core::MiracastBackend`], and the only thing
//! that differs across it is who owns the radio.
#![forbid(unsafe_code)]

pub mod error;
pub mod media;
pub mod p2p;
pub mod ts;
pub mod video;

use castaway_core::ProtocolKind;

pub use error::{IeError, MiracastError, ParamError};
pub use media::{MediaReceiver, MP2T_PAYLOAD_TYPE};
pub use p2p::{MacAddr, WpaCommand, WpaEvent, WpaReply};
pub use ts::{Pid, StreamType, TsDemux, TS_PACKET_LEN};
pub use video::{
    pick_best_format, H264Codec, NegotiatedVideo, Profile, ResolutionIndex, ResolutionMask,
    ResolutionTable, VideoFormats, VideoMode,
};

/// The protocol kind for Miracast sources.
#[must_use]
pub fn kind() -> ProtocolKind {
    ProtocolKind::Miracast
}
