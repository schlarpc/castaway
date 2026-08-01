//! The one instant both tracks measure themselves against.
//!
//! Video slots and audio sample positions are each derived from wall-clock time, and they
//! have to be derived from the *same* wall-clock time or the two tracks drift apart. A
//! stream whose audio runs 40 ppm fast against its video is in sync for the first minute
//! and half a frame out by the tenth, which is exactly the kind of fault that gets blamed
//! on the network.
//!
//! So there is one origin, shared. Video sample *n* covers `[n/fps, (n+1)/fps)` past it
//! and audio sample *n* sits at `n/rate`; neither track ever consults the other, and they
//! agree anyway. It also means a rebase — [`super::cadence::Cadence`] giving up on a gap
//! it will not paper over — moves both at once, because it moves the thing they are both
//! measured from.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A shared wall-clock origin.
///
/// Anchored by the first thing to need it rather than at construction: a tap is installed
/// and then waits for the render loop, and a timeline that started ticking at construction
/// would owe that wait in duplicated frames and silence before the stream had a picture.
#[derive(Debug, Default)]
pub struct Timeline {
    origin: Mutex<Option<Instant>>,
}

impl Timeline {
    /// An unanchored timeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How far past the origin `now` is, anchoring the timeline here if nothing has yet.
    ///
    /// The first caller gets zero, which is the definition of the origin rather than a
    /// special case.
    pub fn anchor(&self, now: Instant) -> Duration {
        let Ok(mut origin) = self.origin.lock() else {
            return Duration::ZERO;
        };
        now.saturating_duration_since(*origin.get_or_insert(now))
    }

    /// How far past the origin `now` is, or `None` while nothing has anchored it.
    ///
    /// This is the read a *passenger* makes — the audio mixer, which must not start the
    /// timeline itself. Audio that arrives before the first frame has nowhere to go, and
    /// putting it at position zero would place a second of sound under a picture that had
    /// not been drawn yet.
    pub fn elapsed(&self, now: Instant) -> Option<Duration> {
        let origin = (*self.origin.lock().ok()?)?;
        Some(now.saturating_duration_since(origin))
    }

    /// Discard `by` of wall-clock time: everything measured from here jumps forward.
    ///
    /// What a rebase *is*. The stream's own timeline stays contiguous — which is what a
    /// player reconstructs — and only its agreement with the wall clock is given up.
    pub fn rebase(&self, by: Duration) {
        if let Ok(mut origin) = self.origin.lock() {
            if let Some(at) = origin.as_mut() {
                *at += by;
            }
        }
    }

    /// Forget the origin, so the next [`Self::anchor`] establishes a new one.
    ///
    /// A stream that retired and started again is a new presentation with a new init
    /// segment and a segment counter back at one, so its timeline starts again too.
    pub fn reset(&self) {
        if let Ok(mut origin) = self.origin.lock() {
            *origin = None;
        }
    }

    /// Whether anything has anchored it yet.
    #[must_use]
    pub fn anchored(&self) -> bool {
        self.origin.lock().is_ok_and(|o| o.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_caller_defines_the_origin() {
        let t = Timeline::new();
        let t0 = Instant::now();
        assert!(!t.anchored());
        assert_eq!(t.anchor(t0), Duration::ZERO);
        assert!(t.anchored());
        assert_eq!(
            t.anchor(t0 + Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn a_passenger_does_not_start_the_clock() {
        // The audio mixer reads this before the first frame is composited. If that read
        // anchored the timeline, the stream would open with however much sound arrived
        // before there was a picture to put it under.
        let t = Timeline::new();
        let t0 = Instant::now();
        assert_eq!(t.elapsed(t0), None);
        assert!(!t.anchored());
        t.anchor(t0);
        assert_eq!(
            t.elapsed(t0 + Duration::from_millis(500)),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn a_rebase_moves_every_reader_together() {
        // The whole reason this is shared: video giving up on a gap has to take audio with
        // it, or the two tracks are out by the length of the gap for the rest of the run.
        let t = Timeline::new();
        let t0 = Instant::now();
        t.anchor(t0);
        let now = t0 + Duration::from_secs(10);
        assert_eq!(t.elapsed(now), Some(Duration::from_secs(10)));
        t.rebase(Duration::from_secs(8));
        assert_eq!(t.elapsed(now), Some(Duration::from_secs(2)));
    }

    #[test]
    fn a_reset_timeline_starts_again_rather_than_resuming() {
        // A stream that retired and restarted writes a fresh init segment and counts
        // segments from one again, so resuming an old origin would open it with however
        // many minutes of duplicated frames had elapsed in between.
        let t = Timeline::new();
        let t0 = Instant::now();
        t.anchor(t0);
        t.reset();
        assert!(!t.anchored());
        let later = t0 + Duration::from_secs(600);
        assert_eq!(t.anchor(later), Duration::ZERO);
    }

    #[test]
    fn time_running_backwards_reads_as_the_origin_rather_than_wrapping() {
        let t = Timeline::new();
        let t0 = Instant::now();
        t.anchor(t0 + Duration::from_secs(5));
        assert_eq!(t.elapsed(t0), Some(Duration::ZERO));
    }
}
