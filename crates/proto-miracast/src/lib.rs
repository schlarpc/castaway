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

pub mod actor;
/// The Linux Wi-Fi Direct backend. Unix-only: it speaks to a wpa_supplicant control
/// socket, which has no equivalent elsewhere (ground rule 5 — the seam is
/// [`castaway_core::MiracastBackend`], and this is one impl of it).
#[cfg(unix)]
pub mod backend_linux;
pub mod error;
pub mod ie;
pub mod media;
/// Miracast over Infrastructure — the [MS-MICE] control channel (#166). Pure.
pub mod mice;
/// The socket shell for [`mice`]: the 7250 listener and the hand-off to RTSP.
pub mod mice_actor;
pub mod p2p;
pub mod params;
pub mod session;
pub mod ts;
pub mod uibc;
pub mod video;

use castaway_core::ProtocolKind;

pub use actor::{bind_rtp, connect_control, run_session, MiceService, MiracastAdapter};
#[cfg(unix)]
pub use backend_linux::{GroupSubnet, LinuxMiracastBackend, P2pConfig, WpaControl};
pub use error::{IeError, MiceError, MiracastError, ParamError};
pub use ie::{
    DeviceInformation, DeviceType, ExtendedCapability, SessionAvailability, Subelement,
    SubelementId, WfdInformationElement,
};
pub use media::{MediaReceiver, MP2T_PAYLOAD_TYPE};
pub use mice::{
    vendor_extension, Capability, CloseReason as MiceCloseReason, MiceMessage, MiceOutput,
    MiceSession, MiceState, SourceId as MiceSourceId,
};
pub use p2p::{MacAddr, WpaCommand, WpaEvent, WpaReply};
pub use params::{
    AudioCodecs, AudioFormat, ClientRtpPorts, ConnectorType, ContentProtection, ParamBody,
    ParamName, PresentationUrls, RtpProfile, SinkCapabilities, TriggerMethod,
};
pub use session::{
    NegotiatedConfig, OutgoingRequest, OutgoingResponse, SessionState, SinkOutput, WfdRequest,
    WfdResponse, WfdSession, DEFAULT_CONTROL_PORT,
};
pub use ts::{Pid, StreamType, TsDemux, TS_PACKET_LEN};
pub use uibc::{
    GenericInput, HidcMessage, InputCategory, Pointer, Scroll, SourcePixel, UibcFrame,
    VideoGeometry,
};
pub use video::{
    pick_best_format, H264Codec, NegotiatedVideo, Profile, ResolutionIndex, ResolutionMask,
    ResolutionTable, VideoFormats, VideoMode,
};

/// The protocol kind for Miracast sources.
#[must_use]
pub fn kind() -> ProtocolKind {
    ProtocolKind::Miracast
}
