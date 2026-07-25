//! The single internal command surface. Every protocol reduces to a [`SessionEvent`].

use std::sync::Arc;
use std::time::Duration;

use crate::control::RemoteControl;
use crate::nowplaying::NowPlaying;
use crate::source::SourceDescription;
use crate::types::{FrameSource, MediaUri};

/// What an adapter needs advertised on the network to be discoverable.
///
/// The session/discovery layer collects these from every enabled adapter and drives
/// the shared mDNS / SSDP responders (one of each, not five racing — the whole point).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Advertisement {
    /// An mDNS-SD service instance (`_airplay._tcp`, `_googlecast._tcp`, …).
    MdnsService {
        /// Service type, e.g. `_googlecast._tcp`.
        ty: String,
        /// The instance name to publish. Usually the receiver's friendly name — but the
        /// adapter decides, because the convention is per-protocol: RAOP requires
        /// `<deviceid>@<name>`. Adapters are handed the friendly name at construction.
        instance: String,
        /// TCP port the service listens on.
        port: u16,
        /// TXT record key/value pairs.
        txt: Vec<(String, String)>,
    },
    /// An SSDP/UPnP device advertised on 1900 with a description URL.
    SsdpDevice {
        /// Search target, e.g. `urn:dial-multiscreen-org:service:dial:1`.
        st: String,
        /// Path (on the shared HTTP host) serving the device description XML.
        description_path: String,
    },
    /// Miracast Wi-Fi Direct P2P beacon — L2, OS-specific, not IP multicast.
    WifiDirect {
        /// The device name broadcast in the P2P beacon.
        device_name: String,
    },
}

/// Transport-control transactions over the active session (play/pause/seek/volume/queue).
///
/// Closed enum: a new control verb forces every handler to acknowledge it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ControlTxn {
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Stop and tear down the current media (distinct from [`SessionEvent::End`],
    /// which ends the whole source session).
    Stop,
    /// Seek to an absolute position from the start of the current item.
    Seek(Duration),
    /// Set output volume in the range `0.0..=1.0`.
    Volume(f32),
    /// Mute or unmute without changing the volume level.
    Mute(bool),
    /// Skip to the next item in the sender's queue.
    Next,
    /// Skip to the previous item in the sender's queue.
    Previous,
    /// Replace the play queue with an ordered list of media URIs (Lounge `setPlaylist`).
    SetQueue {
        /// Ordered queue items.
        items: Vec<MediaUri>,
        /// Index of the item to start on.
        start_index: usize,
    },
}

/// The internal command every protocol adapter emits. The session manager consumes
/// these and drives the pipeline + display; adapters never touch the GPU.
#[derive(Debug)]
#[non_exhaustive]
pub enum SessionEvent {
    /// Media-URL casting: the receiver fetches & decodes (Cast LOAD, AirPlay video,
    /// DLNA, Lounge).
    Play {
        /// The media to fetch and play.
        source: MediaUri,
        /// Optional start offset.
        start: Option<Duration>,
    },
    /// Live pixel mirroring: a stream of frames the pipeline decodes/composites.
    Mirror {
        /// Video frame source.
        video: FrameSource,
        /// Optional accompanying audio frame source.
        audio: Option<FrameSource>,
    },
    /// A live audio-only session: the adapter pushes encoded audio it has already
    /// depacketized, and there is no video and no URL.
    ///
    /// Distinct from both siblings because neither fits: [`SessionEvent::Play`] needs a
    /// [`MediaUri`] the receiver can open, and [`SessionEvent::Mirror`] requires video.
    /// Bluetooth A2DP is the first source of this shape — the screen shows a now-playing
    /// card ([`SessionEvent::NowPlaying`]) rather than pixels from the sender.
    Audio {
        /// Encoded audio frames from the adapter.
        source: FrameSource,
    },
    /// Track metadata for the now-playing surface — a full snapshot, re-emitted whenever
    /// any part of it changes (including artwork arriving late).
    NowPlaying(NowPlaying),
    /// Who connected and what was negotiated. Distinct from [`SessionEvent::NowPlaying`]
    /// because it changes on a different schedule — once per session rather than once
    /// per track — and arrives in pieces as each fact becomes known.
    SourceInfo(SourceDescription),
    /// The source's control channel came up: the receiver may now drive the *sender*.
    ///
    /// Separate from the session-start events on purpose. For Bluetooth this is a second
    /// L2CAP channel (AVCTP) that routinely connects *after* audio is already flowing, so
    /// folding it into [`SessionEvent::Audio`] would be a lie about the wire behaviour.
    ControlSurface(Arc<dyn RemoteControl>),
    /// Transport control over the active session.
    Control(ControlTxn),
    /// The source session has ended; release the pipeline and drop the source.
    End,
}
