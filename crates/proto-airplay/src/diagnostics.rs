//! What a session is actually doing, in numbers, for when there is a real device in the
//! room.
//!
//! Most of what is unresolved about this receiver cannot be settled without an iPhone:
//! whether the advertisement provokes the flow we expect, whether the FairPlay unwrap
//! works against a real `ekey` rather than a published vector, how far apart the two
//! planes of a mirror actually land, whether the clock settles. A device session is
//! expensive to arrange and easy to waste, and the way it gets wasted is coming away
//! with an impression instead of a number.
//!
//! So this is deliberately not debug logging. It is a fixed set of counters that every
//! open question maps onto, emitted as one structured line on a timer, so the answer to
//! "was it in sync" is a figure in the journal rather than a memory of watching a screen.
//!
//! ## The number that matters most
//!
//! [`Snapshot::av_skew_ms`]. Both planes stamp their frames against one origin in the
//! sender's clock, so at any wall instant the difference between the newest video
//! presentation time and the newest audio one *is* the lip-sync error. A steady figure
//! near zero means the streams already track each other and the presentation clock is
//! machinery nobody needs; a steady non-zero figure is a constant offset to correct; a
//! figure that grows is drift, and only that last one needs a real clock.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Sentinel for "no frame has been seen on this plane yet".
///
/// A real presentation time of `u64::MAX` nanoseconds is 584 years, so it cannot collide
/// with a measurement.
const UNSET: u64 = u64::MAX;

/// Counters shared by every task in one AirPlay session.
///
/// Atomics rather than a lock because the writers are the audio receive loop and the
/// mirror receive loop, both of which touch this once per packet: a mutex there would put
/// the two planes in each other's way for the sake of numbers neither of them reads.
#[derive(Debug)]
pub struct SessionDiagnostics {
    started: Instant,
    video_frames: AtomicU64,
    video_dropped: AtomicU64,
    video_last_pts_ns: AtomicU64,
    audio_frames: AtomicU64,
    audio_dropped: AtomicU64,
    audio_last_pts_ns: AtomicU64,
    /// Packets discarded because they predate a `FLUSH`.
    audio_stale: AtomicU64,
    audio_duplicate: AtomicU64,
    /// Packets discarded because no sync packet had placed them on the timeline yet.
    audio_awaiting_sync: AtomicU64,
    resends_sent: AtomicU64,
    /// `remote - local`, from the best timing sample. `i64::MIN` until one lands.
    clock_offset_ns: AtomicI64,
    clock_delay_ns: AtomicU64,
    clock_samples: AtomicU64,
    /// The latency the *sender* declares in its sync packets, in frames.
    sender_latency_frames: AtomicU64,
}

impl Default for SessionDiagnostics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            video_frames: AtomicU64::new(0),
            video_dropped: AtomicU64::new(0),
            video_last_pts_ns: AtomicU64::new(UNSET),
            audio_frames: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            audio_last_pts_ns: AtomicU64::new(UNSET),
            audio_stale: AtomicU64::new(0),
            audio_duplicate: AtomicU64::new(0),
            audio_awaiting_sync: AtomicU64::new(0),
            resends_sent: AtomicU64::new(0),
            clock_offset_ns: AtomicI64::new(i64::MIN),
            clock_delay_ns: AtomicU64::new(0),
            clock_samples: AtomicU64::new(0),
            sender_latency_frames: AtomicU64::new(0),
        }
    }
}

impl SessionDiagnostics {
    /// A fresh set of counters, timed from now.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A video frame reached the pipeline.
    pub fn video_frame(&self, pts: Duration) {
        self.video_frames.fetch_add(1, Ordering::Relaxed);
        self.video_last_pts_ns
            .store(pts_nanos(pts), Ordering::Relaxed);
    }

    /// A video frame was dropped because the pipeline was behind.
    pub fn video_drop(&self) {
        self.video_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// An audio frame reached the pipeline.
    pub fn audio_frame(&self, pts: Duration) {
        self.audio_frames.fetch_add(1, Ordering::Relaxed);
        self.audio_last_pts_ns
            .store(pts_nanos(pts), Ordering::Relaxed);
    }

    /// An audio frame was dropped because the pipeline was behind.
    pub fn audio_drop(&self) {
        self.audio_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// A packet was discarded as predating a `FLUSH`.
    /// A redundant copy of a frame already delivered — see `audio::Delivered`. Counted
    /// rather than ignored because it is how much of the sender's bandwidth is
    /// redundancy, and a figure of *zero* on a mirroring session means the dedupe is
    /// looking at the wrong thing.
    pub fn audio_duplicate(&self) {
        self.audio_duplicate.fetch_add(1, Ordering::Relaxed);
    }

    pub fn audio_stale(&self) {
        self.audio_stale.fetch_add(1, Ordering::Relaxed);
    }

    /// A packet was discarded for arriving before the stream was anchored.
    pub fn audio_awaiting_sync(&self) {
        self.audio_awaiting_sync.fetch_add(1, Ordering::Relaxed);
    }

    /// We asked the sender to retransmit `count` packets.
    pub fn resend(&self, count: u16) {
        self.resends_sent
            .fetch_add(u64::from(count), Ordering::Relaxed);
    }

    /// A timing round trip completed.
    pub fn timing_sample(&self, offset_ns: i64, delay_ns: u64) {
        self.clock_offset_ns.store(offset_ns, Ordering::Relaxed);
        self.clock_delay_ns.store(delay_ns, Ordering::Relaxed);
        self.clock_samples.fetch_add(1, Ordering::Relaxed);
    }

    /// The sender declared its playback latency.
    pub fn sender_latency(&self, frames: u32) {
        self.sender_latency_frames
            .store(u64::from(frames), Ordering::Relaxed);
    }

    /// Read everything at once.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let video_pts = self.video_last_pts_ns.load(Ordering::Relaxed);
        let audio_pts = self.audio_last_pts_ns.load(Ordering::Relaxed);
        Snapshot {
            elapsed: self.started.elapsed(),
            video_frames: self.video_frames.load(Ordering::Relaxed),
            video_dropped: self.video_dropped.load(Ordering::Relaxed),
            audio_frames: self.audio_frames.load(Ordering::Relaxed),
            audio_dropped: self.audio_dropped.load(Ordering::Relaxed),
            audio_stale: self.audio_stale.load(Ordering::Relaxed),
            audio_duplicate: self.audio_duplicate.load(Ordering::Relaxed),
            audio_awaiting_sync: self.audio_awaiting_sync.load(Ordering::Relaxed),
            resends_sent: self.resends_sent.load(Ordering::Relaxed),
            clock_offset_ns: match self.clock_offset_ns.load(Ordering::Relaxed) {
                i64::MIN => None,
                v => Some(v),
            },
            clock_delay_ns: self.clock_delay_ns.load(Ordering::Relaxed),
            clock_samples: self.clock_samples.load(Ordering::Relaxed),
            sender_latency_frames: self.sender_latency_frames.load(Ordering::Relaxed),
            // Positive means video is ahead of audio, which is the direction a viewer
            // reads as "the sound is late".
            av_skew_ms: match (video_pts, audio_pts) {
                (UNSET, _) | (_, UNSET) => None,
                (v, a) => Some(
                    (i64::try_from(v).unwrap_or(i64::MAX) - i64::try_from(a).unwrap_or(i64::MAX))
                        / 1_000_000,
                ),
            },
        }
    }
}

/// Clamp a presentation time into nanoseconds, avoiding the "unset" sentinel.
fn pts_nanos(pts: Duration) -> u64 {
    let ns = u64::try_from(pts.as_nanos()).unwrap_or(UNSET - 1);
    if ns == UNSET {
        UNSET - 1
    } else {
        ns
    }
}

/// Everything the counters say, read together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Snapshot {
    /// How long the session has been running.
    pub elapsed: Duration,
    /// Video frames handed to the pipeline.
    pub video_frames: u64,
    /// Video frames dropped because the pipeline was behind.
    pub video_dropped: u64,
    /// Audio frames handed to the pipeline.
    pub audio_frames: u64,
    /// Audio frames dropped because the pipeline was behind.
    pub audio_dropped: u64,
    /// Packets discarded as predating a `FLUSH`.
    pub audio_stale: u64,
    /// Redundant copies dropped: frames the sender sent more than once on purpose.
    pub audio_duplicate: u64,
    /// Packets discarded for arriving before the stream was anchored.
    pub audio_awaiting_sync: u64,
    /// Packets we asked the sender to retransmit.
    pub resends_sent: u64,
    /// The offset between our clock and the sender's, if a timing round trip landed.
    pub clock_offset_ns: Option<i64>,
    /// The round-trip delay of the best timing sample.
    pub clock_delay_ns: u64,
    /// How many timing round trips have completed.
    pub clock_samples: u64,
    /// The playback latency the sender declares, in frames.
    pub sender_latency_frames: u64,
    /// **The lip-sync number.** Video presentation time minus audio's, in milliseconds,
    /// once both planes have produced a frame. Positive means video is ahead — what a
    /// viewer reads as the sound being late.
    pub av_skew_ms: Option<i64>,
}

impl Snapshot {
    /// Emit the whole thing as one structured line.
    ///
    /// One line rather than several because the fields are only meaningful together: a
    /// skew figure without the frame counts beside it cannot be told apart from a stream
    /// that stopped.
    pub fn log(&self) {
        tracing::info!(
            elapsed_s = self.elapsed.as_secs(),
            video_frames = self.video_frames,
            video_dropped = self.video_dropped,
            audio_frames = self.audio_frames,
            audio_dropped = self.audio_dropped,
            audio_stale = self.audio_stale,
            audio_duplicate = self.audio_duplicate,
            audio_awaiting_sync = self.audio_awaiting_sync,
            resends = self.resends_sent,
            av_skew_ms = ?self.av_skew_ms,
            clock_offset_ns = ?self.clock_offset_ns,
            clock_delay_us = self.clock_delay_ns / 1_000,
            clock_samples = self.clock_samples,
            sender_latency_frames = self.sender_latency_frames,
            "AirPlay session"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn skew_is_unknown_until_both_planes_have_produced_a_frame() {
        // Reporting zero before audio starts would read as "perfectly in sync" at exactly
        // the moment there is nothing to be in sync with.
        let d = SessionDiagnostics::new();
        assert_eq!(d.snapshot().av_skew_ms, None);
        d.video_frame(Duration::from_millis(100));
        assert_eq!(d.snapshot().av_skew_ms, None);
        d.audio_frame(Duration::from_millis(100));
        assert_eq!(d.snapshot().av_skew_ms, Some(0));
    }

    #[test]
    fn skew_is_positive_when_video_runs_ahead_of_the_sound() {
        // The sign is the thing a person has to be able to read off the log without
        // rederiving it: positive means the picture is ahead, which is heard as late
        // audio.
        let d = SessionDiagnostics::new();
        d.video_frame(Duration::from_millis(250));
        d.audio_frame(Duration::from_millis(100));
        assert_eq!(d.snapshot().av_skew_ms, Some(150));

        d.video_frame(Duration::from_millis(100));
        d.audio_frame(Duration::from_millis(250));
        assert_eq!(d.snapshot().av_skew_ms, Some(-150));
    }

    #[test]
    fn the_clock_offset_is_absent_rather_than_zero_before_it_settles() {
        // Zero is a plausible offset, so it cannot double as "no measurement" — that is
        // the difference between "the clocks agree" and "we never asked".
        let d = SessionDiagnostics::new();
        assert_eq!(d.snapshot().clock_offset_ns, None);
        d.timing_sample(0, 1_000);
        assert_eq!(d.snapshot().clock_offset_ns, Some(0));
    }

    #[test]
    fn counters_accumulate_what_each_plane_reports() {
        let d = SessionDiagnostics::new();
        d.video_frame(Duration::ZERO);
        d.video_drop();
        d.audio_frame(Duration::ZERO);
        d.audio_stale();
        d.audio_awaiting_sync();
        d.resend(3);
        d.sender_latency(7_497);
        let s = d.snapshot();
        assert_eq!((s.video_frames, s.video_dropped), (1, 1));
        assert_eq!((s.audio_frames, s.audio_stale), (1, 1));
        assert_eq!(s.audio_awaiting_sync, 1);
        // Counted in packets asked for, not requests sent: one request for three packets
        // is three packets we did not have.
        assert_eq!(s.resends_sent, 3);
        assert_eq!(s.sender_latency_frames, 7_497);
    }

    #[test]
    fn a_presentation_time_cannot_collide_with_the_unset_sentinel() {
        let d = SessionDiagnostics::new();
        d.video_frame(Duration::from_nanos(UNSET));
        d.audio_frame(Duration::ZERO);
        // Still reported, rather than read back as "no frame yet".
        assert!(d.snapshot().av_skew_ms.is_some());
    }
}
