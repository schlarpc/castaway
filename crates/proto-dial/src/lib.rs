//! # proto-dial
//!
//! DIAL app-launch plus the YouTube Lounge (MDX) bind channel — the real YouTube cast
//! button. [`dial`] is the launch/stop/state REST surface mounted on the shared HTTP
//! host; [`lounge`] is the pure BrowserChannel parser that turns the server's pushed
//! commands into [`castaway_core::SessionEvent`]s.
//!
//! Flow: sender hits the cast button → DIAL `POST /apps/YouTube` launches → the app
//! layer registers a Lounge screen and long-polls the bind channel → [`lounge::to_event`]
//! maps `setPlaylist`/`play`/`pause`/`seekTo` into session events driving the player.
//! The playback backend is CEF (YouTube's TV surface) or yt-dlp → pipeline.
#![forbid(unsafe_code)]

pub mod dial;
pub mod error;
pub mod lounge;

pub use dial::{DialService, LaunchParams, DIAL_SERVICE_TYPE};
pub use error::DialError;
pub use lounge::{parse_chunks, to_event, LoungeCommand};

use castaway_core::ProtocolKind;

/// The protocol kind for YouTube Lounge sources.
#[must_use]
pub fn kind() -> ProtocolKind {
    ProtocolKind::YouTubeLounge
}
