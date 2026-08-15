//! The two questions a wedged process cannot answer for itself (#368).
//!
//! When the receiver stopped serving on 2026-08-15 there was nothing in the log after the
//! last ordinary line, and everything that could have said more was inside the stall: the
//! HTTP host could not answer, the render loop could not draw, and a thread spinning at
//! two cores left no trace at all. What the box could still say — `Get-Process` — was that
//! two threads were `Running` and twenty-four were not, which names nothing.
//!
//! So the process says it itself, from a place the stall does not reach:
//!
//! - [`Heartbeat`] is a liveness stamp. Whoever is running touches it; anyone else reads
//!   how long ago that was. A thread outside the runtime can therefore report that the
//!   runtime has stopped scheduling *while it is stopped*, rather than after it recovers —
//!   which in the observed wedge never happened.
//! - [`SpinWatch`] is a rate guard on a loop. A loop that turns a hundred thousand times a
//!   second is not a loop anyone wants logged per turn; what is wanted is one line naming
//!   it, at most once a window.
//!
//! Neither reads a clock. `now` is passed in (ground rule 6), which is what lets both be
//! tested against a number a test chose rather than against how fast the host happened to
//! be.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A liveness stamp shared between the thing that is running and whoever is watching it.
///
/// Cheap on the hot side on purpose — one relaxed store — because the point is that a
/// loop can afford to touch it on every pass.
///
/// The epoch is fixed when the heartbeat is created and every reading is relative to it,
/// so the stamp fits in a `u64` of milliseconds and needs no lock. A clone shares the
/// stamp; that is how the watcher and the watched hold the same one.
#[derive(Debug, Clone)]
pub struct Heartbeat {
    epoch: Instant,
    last: Arc<AtomicU64>,
}

impl Heartbeat {
    /// A heartbeat that has just beaten, with `now` as its epoch.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            last: Arc::new(AtomicU64::new(0)),
        }
    }

    /// "I am still going."
    pub fn beat(&self, now: Instant) {
        let since = now.saturating_duration_since(self.epoch);
        #[allow(clippy::cast_possible_truncation)]
        self.last.store(since.as_millis() as u64, Ordering::Relaxed);
    }

    /// How long since the last beat.
    #[must_use]
    pub fn age(&self, now: Instant) -> Duration {
        let last = Duration::from_millis(self.last.load(Ordering::Relaxed));
        now.saturating_duration_since(self.epoch)
            .saturating_sub(last)
    }
}

/// What a [`SpinWatch`] hands back when a loop turned more often than it should have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpinReport {
    /// Turns counted in the window.
    pub turns: u64,
    /// How long the window actually was — reported rather than assumed, because a caller
    /// that turns rarely closes its window late and a rate computed from the nominal
    /// window would overstate it.
    pub elapsed: Duration,
}

impl SpinReport {
    /// Turns per second, for the log line.
    #[must_use]
    pub fn per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            #[allow(clippy::cast_precision_loss)]
            return self.turns as f64;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.turns as f64 / secs
        }
    }
}

/// A guard on how fast a loop is allowed to turn.
///
/// Report at most one line per window, and only when the count in that window exceeded
/// the limit — so an ordinary loop costs a counter increment and a comparison, and a
/// runaway one costs a line a window no matter how fast it runs.
#[derive(Debug)]
pub struct SpinWatch {
    window: Duration,
    limit: u64,
    turns: u64,
    /// `None` until the first turn: a loop that never runs has no window, and starting
    /// the clock at construction would blame it for however long it waited to start.
    opened: Option<Instant>,
}

impl SpinWatch {
    /// A watch that reports when a loop turns more than `limit` times in `window`.
    #[must_use]
    pub const fn new(window: Duration, limit: u64) -> Self {
        Self {
            window,
            limit,
            turns: 0,
            opened: None,
        }
    }

    /// Count one turn, and answer whether this window was a runaway.
    ///
    /// The window closes on the first turn *after* it elapses rather than on a timer, so
    /// a loop that stops turning stops being watched — which is correct: it is no longer
    /// spinning, and there is nothing to report.
    pub fn turn(&mut self, now: Instant) -> Option<SpinReport> {
        let opened = *self.opened.get_or_insert(now);
        self.turns += 1;
        let elapsed = now.saturating_duration_since(opened);
        if elapsed < self.window {
            return None;
        }
        let report = SpinReport {
            turns: self.turns,
            elapsed,
        };
        self.turns = 0;
        self.opened = Some(now);
        (report.turns > self.limit).then_some(report)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn epoch() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_heartbeat_ages_from_its_last_beat_rather_than_from_its_epoch() {
        let t0 = epoch();
        let beat = Heartbeat::new(t0);
        assert_eq!(beat.age(t0), Duration::ZERO);
        // Ten seconds pass with one beat in the middle: the age is measured from the beat.
        beat.beat(t0 + Duration::from_secs(4));
        assert_eq!(
            beat.age(t0 + Duration::from_secs(10)),
            Duration::from_secs(6)
        );
    }

    #[test]
    fn a_clone_of_a_heartbeat_is_the_same_heartbeat() {
        // The whole point: the watcher holds one and the watched holds the other.
        let t0 = epoch();
        let watched = Heartbeat::new(t0);
        let watcher = watched.clone();
        watched.beat(t0 + Duration::from_secs(3));
        assert_eq!(watcher.age(t0 + Duration::from_secs(3)), Duration::ZERO);
    }

    #[test]
    fn a_loop_within_its_limit_is_never_reported() {
        let t0 = epoch();
        let mut watch = SpinWatch::new(Duration::from_secs(1), 100);
        // Sixty turns a second for five seconds — a display-rate loop, which is the thing
        // that must not produce a line.
        for i in 0..300u32 {
            let now = t0 + Duration::from_millis(u64::from(i) * 1000 / 60);
            assert_eq!(watch.turn(now), None, "turn {i}");
        }
    }

    #[test]
    fn a_runaway_loop_is_reported_once_a_window_with_the_rate_it_actually_ran_at() {
        let t0 = epoch();
        let mut watch = SpinWatch::new(Duration::from_secs(1), 100);
        let mut reports = Vec::new();
        // 10 000 turns a second for three seconds: two whole windows close inside it,
        // and the third is still open when the loop ends.
        for i in 0..30_000u64 {
            if let Some(report) = watch.turn(t0 + Duration::from_micros(i * 100)) {
                reports.push(report);
            }
        }
        assert_eq!(
            reports.len(),
            2,
            "one line per window, not per turn: {reports:?}"
        );
        for report in &reports {
            let rate = report.per_second();
            assert!(
                (rate - 10_000.0).abs() < 500.0,
                "reported {rate}/s for a loop running at 10 000/s"
            );
        }
    }

    #[test]
    fn a_loop_that_stops_turning_stops_being_watched() {
        // The window closes on a turn, not on a timer. A loop that ran hot and then went
        // quiet has nothing to report — and, more to the point, a watch that reported on
        // a timer would need a thread of its own to do it.
        let t0 = epoch();
        let mut watch = SpinWatch::new(Duration::from_secs(1), 10);
        for i in 0..50u64 {
            watch.turn(t0 + Duration::from_millis(i));
        }
        // An hour later, one turn: it closes the window that was open, and the count in it
        // is the fifty turns that really happened — over the limit, and truthfully spread
        // over an hour rather than over a second.
        let late = watch.turn(t0 + Duration::from_secs(3600)).unwrap();
        assert_eq!(late.turns, 51);
        assert!(late.per_second() < 1.0, "{:?}", late.per_second());
    }
}
