//! The position to draw between the readings a source publishes.
//!
//! Position arrives as discrete snapshots, roughly once a second, from every source:
//! Bluetooth peers that subscribe report `PLAYBACK_POS_CHANGED` on their own interval,
//! peers that refuse it are polled with `GetPlayStatus` at the same cadence (#162), and
//! Spotify Connect and AirPlay publish on schedules of their own. On a two-metre panel a
//! bar that jumps once a second is noticeably worse than one that slides, and everything
//! needed to slide it is already in hand: the last reading, the instant it arrived, and
//! whether the state is playing (#165).
//!
//! So the *source* keeps meaning what it says — `NowPlaying.position` is still "the last
//! position this source stated" — and the interpolation lives here, next to the thing that
//! draws it, once rather than once per adapter.
//!
//! # Reconciling
//!
//! The projection runs ahead of the last reading and every new reading disagrees with it
//! slightly. There are two reasons for a disagreement and they want opposite treatment,
//! which is the whole trap in this:
//!
//! - **A small error is drift** — our clock against the sender's, plus transport latency.
//!   Correcting it by jumping would put a visible twitch in the bar once a second, which
//!   is worse than the stepping this replaces. It is absorbed instead: the projection
//!   continues from exactly where it is and its *rate* is trimmed so it converges on the
//!   source's timeline over the next couple of seconds, invisibly.
//! - **A large error is a seek** — a genuine discontinuity, somebody dragged the phone's
//!   scrubber — and it must snap. Slewing to it would glide the bar across the track over
//!   a second or two, which reads as a bug rather than as smoothing.
//!
//! [`SEEK`] is where one becomes the other.

use std::time::{Duration, Instant};

/// Where drift stops being drift and becomes a seek.
///
/// Latency between a phone reporting a position and the panel drawing it is tens of
/// milliseconds; a clock disagreement over a one-second reporting interval is less again.
/// Half a second is comfortably above both and far below any seek a person makes on
/// purpose — a scrub gesture that moved the position by less than this would not have been
/// worth making.
pub const SEEK: Duration = Duration::from_millis(500);

/// How long the projection is given to absorb a drift error.
///
/// Long enough that the rate trim is invisible, short enough that the projection is
/// honest again before the reading after next.
const CONVERGE: Duration = Duration::from_secs(2);

/// The most the projection's rate may be trimmed by, as a fraction of real time.
///
/// Pinned to [`SEEK`] and [`CONVERGE`] rather than chosen: the largest error that is not a
/// seek is `SEEK`, and absorbing it over `CONVERGE` needs exactly `SEEK / CONVERGE`. A
/// looser clamp could not be reached; a tighter one would silently stop converging and
/// leave a standing error nothing ever corrects.
const MAX_TRIM: f64 = SEEK.as_nanos() as f64 / CONVERGE.as_nanos() as f64;

/// A position that moves, anchored to what the source last said.
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    /// The position this projection was last anchored at.
    anchor: Duration,
    /// When it was anchored there.
    anchor_at: Instant,
    /// How fast the projection advances against real time, `1.0` being neither ahead nor
    /// behind. Away from `1.0` only while an absorbed drift error converges.
    rate: f64,
    /// Whether the projection is running at all. A paused track's position is constant,
    /// and one that crept forward would be a scrubber that lies — in the direction that
    /// makes the next seek land somewhere nobody asked for.
    running: bool,
    /// The total length, if known, so the projection cannot run past the end of the item
    /// when a source stops publishing.
    duration: Option<Duration>,
}

impl Projection {
    /// Start from a reading the source has just stated.
    #[must_use]
    pub fn new(
        position: Duration,
        now: Instant,
        running: bool,
        duration: Option<Duration>,
    ) -> Self {
        Self {
            anchor: position,
            anchor_at: now,
            rate: 1.0,
            running,
            duration,
        }
    }

    /// The position to draw at `now`.
    #[must_use]
    pub fn at(&self, now: Instant) -> Duration {
        if !self.running {
            return self.clamp(self.anchor);
        }
        let elapsed = now.saturating_duration_since(self.anchor_at);
        self.clamp(self.anchor + elapsed.mul_f64(self.rate))
    }

    /// Take a reading from the source.
    ///
    /// Absorbs a small disagreement and snaps a large one; see the module docs for why
    /// those are different things.
    pub fn observe(
        &mut self,
        position: Duration,
        now: Instant,
        running: bool,
        duration: Option<Duration>,
    ) {
        let shown = self.at(now);
        let was_running = self.running;
        self.duration = duration;
        self.running = running;

        // A state change is a discontinuity by itself: a pause stops the projection dead
        // at whatever the source says, and a resume starts a fresh one. Absorbing across
        // one would spend the next two seconds converging on a position that is no longer
        // moving.
        if was_running != running {
            self.anchor = position;
            self.anchor_at = now;
            self.rate = 1.0;
            return;
        }

        // Signed, and the sign matters: `Duration` cannot hold it, so the two directions
        // are separated here rather than by subtracting and hoping.
        let (error, ahead) = if position >= shown {
            (position - shown, false)
        } else {
            (shown - position, true)
        };

        if error >= SEEK || !running {
            self.anchor = position;
            self.anchor_at = now;
            self.rate = 1.0;
            return;
        }

        // Drift. Continue from exactly where the bar already is — so nothing moves at this
        // instant — and trim the rate so that in `CONVERGE` the projection and the source
        // are at the same place. The source will have advanced by `CONVERGE` too, which is
        // why the trim is `error / CONVERGE` and not something that also has to make up
        // the interval.
        let trim = error.as_secs_f64() / CONVERGE.as_secs_f64();
        self.anchor = shown;
        self.anchor_at = now;
        self.rate = if ahead {
            1.0 - trim.min(MAX_TRIM)
        } else {
            1.0 + trim.min(MAX_TRIM)
        };
    }

    /// Whether the projection is advancing.
    #[must_use]
    pub const fn running(&self) -> bool {
        self.running
    }

    /// How long until the projection advances by `step`, from `now`.
    ///
    /// What the render loop's demand calculation asks: a stopped projection never changes
    /// and wants no frame at all, and a running one wants the next frame when the bar
    /// would actually move.
    #[must_use]
    pub fn time_to_advance(&self, now: Instant, step: Duration) -> Option<Duration> {
        if !self.running || self.rate <= 0.0 {
            return None;
        }
        // Past the end nothing changes either, and a projection that has run off a short
        // item would otherwise ask for a frame every tick forever.
        if self.duration.is_some_and(|total| self.at(now) >= total) {
            return None;
        }
        Some(step.div_f64(self.rate))
    }

    fn clamp(&self, position: Duration) -> Duration {
        match self.duration {
            Some(total) => position.min(total),
            None => position,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed origin, so every instant below is chosen rather than measured.
    fn origin() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_playing_projection_advances_with_the_clock() {
        let t0 = origin();
        let p = Projection::new(Duration::from_secs(10), t0, true, None);
        assert_eq!(p.at(t0), Duration::from_secs(10));
        assert_eq!(
            p.at(t0 + Duration::from_millis(500)),
            Duration::from_millis(10_500)
        );
        assert_eq!(p.at(t0 + Duration::from_secs(3)), Duration::from_secs(13));
    }

    #[test]
    fn a_paused_projection_stands_still() {
        let t0 = origin();
        let p = Projection::new(Duration::from_secs(10), t0, false, None);
        assert_eq!(p.at(t0 + Duration::from_secs(30)), Duration::from_secs(10));
    }

    #[test]
    fn a_small_disagreement_is_absorbed_without_the_bar_moving() {
        // The reading the panel gets is always a little stale — transport latency plus
        // whatever the two clocks disagree about. Correcting it by jumping would put a
        // visible twitch in the bar once a second, on every source, forever.
        let t0 = origin();
        let mut p = Projection::new(Duration::from_secs(10), t0, true, None);

        // A second later we are showing 11.000 and the phone says 10.900.
        let t1 = t0 + Duration::from_secs(1);
        let before = p.at(t1);
        p.observe(Duration::from_millis(10_900), t1, true, None);
        assert_eq!(
            p.at(t1),
            before,
            "the drawn position must not move at the instant a reading lands"
        );

        // …and it is converged on the source's timeline shortly after, rather than
        // carrying the error forever.
        let converged = p.at(t1 + CONVERGE);
        let source_would_be = Duration::from_millis(10_900) + CONVERGE;
        let off = converged.as_secs_f64() - source_would_be.as_secs_f64();
        assert!(
            off.abs() < 0.001,
            "converged to {converged:?}, source at {source_would_be:?}"
        );
    }

    #[test]
    fn a_projection_that_has_fallen_behind_is_sped_up_not_jumped() {
        let t0 = origin();
        let mut p = Projection::new(Duration::from_secs(10), t0, true, None);
        let t1 = t0 + Duration::from_secs(1);
        // The phone is *ahead* of us by 300 ms.
        p.observe(Duration::from_millis(11_300), t1, true, None);
        assert_eq!(p.at(t1), Duration::from_secs(11), "no jump");
        assert!(
            p.at(t1 + Duration::from_millis(100)) > Duration::from_millis(11_100),
            "a projection behind the source runs fast until it catches up"
        );
    }

    #[test]
    fn a_seek_snaps() {
        // Somebody dragged the phone's scrubber. Slewing to it would glide the bar across
        // the track over a couple of seconds, which reads as a bug rather than as smoothing.
        let t0 = origin();
        let mut p = Projection::new(Duration::from_secs(10), t0, true, None);
        let t1 = t0 + Duration::from_secs(1);
        p.observe(Duration::from_secs(90), t1, true, None);
        assert_eq!(p.at(t1), Duration::from_secs(90), "a seek is immediate");
    }

    #[test]
    fn a_seek_backwards_snaps_too() {
        let t0 = origin();
        let mut p = Projection::new(Duration::from_secs(90), t0, true, None);
        let t1 = t0 + Duration::from_secs(1);
        p.observe(Duration::from_secs(10), t1, true, None);
        assert_eq!(p.at(t1), Duration::from_secs(10));
    }

    #[test]
    fn the_threshold_is_where_absorbing_becomes_snapping() {
        let t0 = origin();
        let t1 = t0 + Duration::from_secs(1);

        // Just inside: absorbed, so the bar does not move now.
        let mut absorbed = Projection::new(Duration::ZERO, t0, true, None);
        absorbed.observe(
            Duration::from_millis(1000) + SEEK - Duration::from_millis(1),
            t1,
            true,
            None,
        );
        assert_eq!(absorbed.at(t1), Duration::from_secs(1));

        // Just outside: snapped.
        let mut snapped = Projection::new(Duration::ZERO, t0, true, None);
        let reading = Duration::from_millis(1000) + SEEK + Duration::from_millis(1);
        snapped.observe(reading, t1, true, None);
        assert_eq!(snapped.at(t1), reading);
    }

    #[test]
    fn pausing_stops_the_projection_where_the_source_says() {
        // Not where we had projected to: a pause is a discontinuity, and absorbing across
        // one would spend two seconds converging on a position that is no longer moving.
        let t0 = origin();
        let mut p = Projection::new(Duration::from_secs(10), t0, true, None);
        let t1 = t0 + Duration::from_secs(1);
        p.observe(Duration::from_millis(10_950), t1, false, None);
        assert_eq!(p.at(t1), Duration::from_millis(10_950));
        assert_eq!(
            p.at(t1 + Duration::from_secs(60)),
            Duration::from_millis(10_950),
            "a paused projection does not creep"
        );
    }

    #[test]
    fn resuming_starts_a_fresh_projection() {
        let t0 = origin();
        let mut p = Projection::new(Duration::from_secs(10), t0, false, None);
        let t1 = t0 + Duration::from_secs(30);
        p.observe(Duration::from_secs(10), t1, true, None);
        assert_eq!(p.at(t1), Duration::from_secs(10));
        assert_eq!(p.at(t1 + Duration::from_secs(2)), Duration::from_secs(12));
    }

    #[test]
    fn the_projection_does_not_run_past_the_end_of_the_item() {
        // A source that stops publishing at the end of a track must not leave a bar
        // marching off the right-hand side of the panel.
        let t0 = origin();
        let total = Duration::from_secs(30);
        let p = Projection::new(Duration::from_secs(29), t0, true, Some(total));
        assert_eq!(p.at(t0 + Duration::from_secs(600)), total);
        assert!(
            p.time_to_advance(t0 + Duration::from_secs(600), Duration::from_millis(20))
                .is_none(),
            "nothing at the end of a track needs another frame"
        );
    }

    #[test]
    fn a_paused_projection_asks_for_no_frames() {
        let t0 = origin();
        let p = Projection::new(Duration::from_secs(10), t0, false, None);
        assert!(p.time_to_advance(t0, Duration::from_millis(20)).is_none());
    }

    #[test]
    fn the_frame_interval_follows_the_rate() {
        let t0 = origin();
        let p = Projection::new(Duration::ZERO, t0, true, None);
        assert_eq!(
            p.time_to_advance(t0, Duration::from_millis(50)),
            Some(Duration::from_millis(50)),
            "at nominal rate, a 50 ms step takes 50 ms"
        );
    }

    #[test]
    fn repeated_absorbing_converges_rather_than_oscillating() {
        // The failure mode of a control system that over-corrects: a rate trim big enough
        // to overshoot turns a standing error into a wobble, which on a scrubber reads as
        // the bar breathing. Feed it a source that is persistently 200 ms ahead and check
        // the error shrinks monotonically.
        let t0 = origin();
        let mut p = Projection::new(Duration::ZERO, t0, true, None);
        let mut worst = f64::MAX;
        for i in 1..=10 {
            let now = t0 + Duration::from_secs(i);
            // The source's true position, 200 ms ahead of where we started.
            let truth = Duration::from_secs(i) + Duration::from_millis(200);
            let error = (p.at(now).as_secs_f64() - truth.as_secs_f64()).abs();
            assert!(
                error <= worst + 1e-9,
                "error grew at reading {i}: {error} after {worst}"
            );
            worst = error;
            p.observe(truth, now, true, None);
        }
        assert!(worst < 0.05, "still {worst}s out after ten readings");
    }
}
