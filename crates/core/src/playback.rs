//! The channel back *up* from the pipeline: where the item has got to, and how it ended.
//!
//! Every other seam in this crate points one way. An adapter emits [`SessionEvent`]s, the
//! session manager drives a [`Pipeline`], and nothing ever answers. That is right for a
//! source that *is* the player at the far end — Bluetooth and Spotify both know their own
//! position and their own end-of-track, and asking us would be asking the wrong party.
//!
//! It is exactly wrong for the media-URL sources. When a DLNA control point pushes a URL,
//! or Cast sends `LOAD`, the receiver is the player: the phone has no idea where playback
//! has reached or whether the fetch even succeeded, and the protocol obliges us to tell
//! it. Without this module both questions were answered by inventing something —
//! `GetPositionInfo` returned a sentinel forever and `GetTransportInfo` said PLAYING/OK
//! for a URL the box could not fetch, so a control point's queue never advanced and the
//! phone showed a healthy session over a blank panel.
//!
//! Two shapes, because the two questions have different rhythms:
//!
//! - [`PlaybackReport`] is **pulled**. A control point polls `GetPositionInfo` roughly
//!   once a second and position is never evented (AVTransport §2.3.1 excludes it from
//!   `LastChange`), so a push would be a timer pretending to be an event.
//! - [`PlaybackEnd`] is **pushed**. It happens once, it is the thing a queue waits on, and
//!   a control point that has stopped polling — because it thinks the item is still
//!   playing — would never discover it by asking.
//!
//! [`SessionEvent`]: crate::event::SessionEvent
//! [`Pipeline`]: crate::pipeline::Pipeline

use std::time::Duration;

/// How far through the current item playback has reached.
///
/// `duration` is optional and its absence is meaningful rather than a gap in our
/// knowledge: a live stream genuinely has no end, and a control point told one anyway
/// draws a progress bar that lies. AVTransport spells the same distinction
/// `NOT_IMPLEMENTED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlaybackProgress {
    /// Where the item is now, in media time.
    pub position: Duration,
    /// How long the whole item is, when the container knows.
    pub duration: Option<Duration>,
}

impl PlaybackProgress {
    /// A progress report at `position` with an unknown total length.
    #[must_use]
    pub const fn at(position: Duration) -> Self {
        Self {
            position,
            duration: None,
        }
    }

    /// The same report with a known total length.
    #[must_use]
    pub const fn of(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }
}

/// Why the item the pipeline was playing stopped playing.
///
/// Preemption is deliberately not a variant. Another source taking the screen is not this
/// item ending — the session that owned it is simply no longer the one on screen, the
/// session manager already knows, and reporting it as an end would have the outgoing
/// source clear a card that now belongs to somebody else.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlaybackEnd {
    /// The item played through to its end.
    Finished,
    /// The fetch or the decode failed, with whatever the pipeline could say about it.
    ///
    /// Carried as a string rather than a typed error because it crosses a crate boundary
    /// in the direction dependencies do not flow: `pipeline` knows about ffmpeg and
    /// `core` must not. Consumers surface it to a human, they do not match on it.
    Failed(String),
}

impl PlaybackEnd {
    /// Whether this end was a failure rather than a normal finish.
    ///
    /// The distinction reaches a person: AVTransport has a whole `TransportStatus` for it
    /// (`ERROR_OCCURRED` vs `OK`), and it is the difference between a phone that shows the
    /// next track and one that shows why there isn't one.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

impl std::fmt::Display for PlaybackEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Finished => f.write_str("the media finished"),
            Self::Failed(reason) => write!(f, "playback failed: {reason}"),
        }
    }
}

/// How many end reports may be in flight before one is dropped.
///
/// Room for several rather than one because a session that is torn down and restarted
/// quickly — a control point advancing a queue — can have two decode threads winding down
/// at once, and a report dropped on a full channel is a transport state that never
/// corrects itself. Small because the alternative to a bound is a queue that grows when
/// nothing is reading, which is the failure this project keeps finding.
const END_CHANNEL_DEPTH: usize = 4;

/// The channel a pipeline reports [`PlaybackEnd`]s on.
///
/// Handed out as a pair so the two halves can be wired in the order that reads left to
/// right — sender into the pipeline, receiver into the session manager — rather than
/// having to ask the manager for a sender it can only produce once it already owns the
/// pipeline.
#[must_use]
pub fn end_channel() -> (
    tokio::sync::mpsc::Sender<PlaybackEnd>,
    tokio::sync::mpsc::Receiver<PlaybackEnd>,
) {
    tokio::sync::mpsc::channel(END_CHANNEL_DEPTH)
}

/// What the pipeline can be asked about the item it is playing.
///
/// Handed to an adapter at construction rather than published per session, because a
/// renderer's position question outlives any one item: a control point may poll
/// `GetPositionInfo` before it has sent anything, between tracks, and after the item
/// ended, and [`None`] is the honest answer to all three.
pub trait PlaybackReport: Send + Sync {
    /// Where the media-URL session in flight has reached, or [`None`] when there is no
    /// such session — nothing playing, or a session that some other source's protocol is
    /// pacing, where our clock is not the authority.
    fn progress(&self) -> Option<PlaybackProgress>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_stream_reports_a_position_with_no_end() {
        let p = PlaybackProgress::at(Duration::from_secs(42));
        assert_eq!(p.position, Duration::from_secs(42));
        assert!(
            p.duration.is_none(),
            "an unknown length must stay unknown, not become zero"
        );
        assert_eq!(
            p.of(Duration::from_secs(300)).duration,
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn only_a_failure_reads_as_one() {
        assert!(!PlaybackEnd::Finished.is_failure());
        assert!(PlaybackEnd::Failed("connection refused".into()).is_failure());
        assert!(PlaybackEnd::Failed("connection refused".into())
            .to_string()
            .contains("connection refused"));
    }
}
