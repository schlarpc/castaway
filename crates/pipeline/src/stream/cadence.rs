//! The stream's own clock.
//!
//! The panel presents when the panel has something to present — a spring mid-flight, a
//! video frame arriving, a second ticking over on the transport strip. A stream cannot
//! work that way: HLS describes a track by sample durations, and a player reconstructs
//! the timeline by adding them up. So the stream runs on a fixed grid of slots, and every
//! slot is filled.
//!
//! **This is where ground rule 4 inverts.** For the glass, a late frame is dropped —
//! latency beats freshness, and nobody sees the frame that never arrived. For a stream, a
//! dropped frame is not a dropped frame, it is a *hole in the timeline*: either the
//! presentation clock runs slow by exactly that much, or the player stalls. So a slot the
//! compositor did not fill is filled with the previous picture. An encoder codes a
//! repeated frame as almost nothing, which is why this is the cheap answer as well as the
//! correct one.
//!
//! Pure: `Instant` in, slot arithmetic out. No sleeping happens here.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::fmp4::TIMESCALE;
use super::timeline::Timeline;

/// How many pictures a second the stream publishes.
///
/// A newtype rather than a bare `u32` because zero is the one value that turns the whole
/// grid into a division by zero, and because "frames per second" and "seconds per frame"
/// are both plausible readings of an integer parameter at a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate(NonZeroU32);

impl FrameRate {
    /// 30 fps: what a wall panel's web duplicate wants. Enough for the motion to read as
    /// motion, half the encode cost of matching a 60 Hz display, and it divides
    /// [`TIMESCALE`] exactly.
    pub const DEFAULT: Self = Self(NonZeroU32::new(30).expect("30 is not zero"));

    /// A rate, or `None` for zero.
    #[must_use]
    pub const fn new(fps: u32) -> Option<Self> {
        match NonZeroU32::new(fps) {
            Some(fps) => Some(Self(fps)),
            None => None,
        }
    }

    /// The rate as a number.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    /// One frame's duration in [`TIMESCALE`] ticks — what goes in a `trun` entry.
    ///
    /// Exact for every rate that divides 90 000, which is every rate anyone would pick.
    /// For one that does not, the remainder is dropped here and the timeline runs
    /// fractionally slow rather than accumulating a rounding error per segment.
    #[must_use]
    pub const fn sample_duration_ticks(self) -> u32 {
        TIMESCALE / self.0.get()
    }

    /// The nominal wall-clock spacing of two slots.
    ///
    /// Convenience for callers that want to sleep or log; the grid itself never uses it.
    /// At 60 fps this rounds off two thirds of a nanosecond, and a grid built by adding
    /// it up would be a second and a half out by the end of a day — so [`Cadence`] works
    /// from slot *indices* and multiplies, which is exact.
    #[must_use]
    pub const fn period(self) -> Duration {
        Duration::from_nanos(1_000_000_000 / self.0.get() as u64)
    }

    /// How far into the grid slot `n` begins. Exact: one division, at the end.
    #[must_use]
    pub fn slot_offset(self, n: u64) -> Duration {
        Duration::from_nanos(
            u64::try_from(u128::from(n) * 1_000_000_000 / u128::from(self.0.get()))
                .unwrap_or(u64::MAX),
        )
    }

    /// Which slot `elapsed` past the origin falls in. The inverse of
    /// [`Self::slot_offset`], and exact for the same reason.
    #[must_use]
    pub fn slot_at(self, elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_nanos() * u128::from(self.0.get()) / 1_000_000_000)
            .unwrap_or(u64::MAX)
    }
}

/// What a slot boundary asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish {
    /// How many repeats of the previously published picture go in first, filling slots
    /// the compositor did not present into.
    pub duplicates: u32,
    /// Whether the grid was rebased because the gap was longer than [`Cadence`] is
    /// willing to paper over. The timeline stays contiguous either way; this says that
    /// wall-clock time and stream time no longer agree, which is worth a log line and is
    /// not worth failing over.
    pub resynced: bool,
}

/// A fixed grid of publication slots.
#[derive(Debug)]
pub struct Cadence {
    rate: FrameRate,
    /// The most repeats one gap may be papered over with. Beyond this the grid is rebased
    /// instead: a panel that stopped presenting for a minute should not produce a minute
    /// of identical frames that a viewer then has to sit through to reach the present.
    max_duplicates: u32,
    /// The wall-clock origin slot zero sits on — shared with the audio mixer, which is
    /// what keeps the two tracks from drifting apart (see [`Timeline`]). Established by
    /// the first frame rather than at construction, so a tap that is installed and then
    /// waits for the render loop does not open with a burst of duplicates for slots that
    /// elapsed before it saw anything.
    timeline: Arc<Timeline>,
    /// The next slot owed a picture.
    next_slot: u64,
}

impl Cadence {
    /// A grid at `rate` on `timeline`, papering over gaps of up to `max_gap` before
    /// rebasing.
    #[must_use]
    pub fn new(rate: FrameRate, max_gap: Duration, timeline: Arc<Timeline>) -> Self {
        Self {
            rate,
            max_duplicates: u32::try_from(rate.slot_at(max_gap)).unwrap_or(u32::MAX),
            timeline,
            next_slot: 0,
        }
    }

    /// The rate this grid runs at.
    #[must_use]
    pub const fn rate(&self) -> FrameRate {
        self.rate
    }

    /// Whether a slot is owed a picture as of `now`.
    ///
    /// This is what the tap answers `wants_frame` with, and it is the whole reason a
    /// 60 Hz panel feeding a 30 fps stream costs one readback in two rather than two.
    #[must_use]
    pub fn due(&self, now: Instant) -> bool {
        let Some(elapsed) = self.timeline.elapsed(now) else {
            return true;
        };
        elapsed >= self.rate.slot_offset(self.next_slot)
    }

    /// Consume the slot that is due, and say what to publish.
    ///
    /// Advances past every slot that has already elapsed, so the grid tracks wall time
    /// rather than drifting behind it by however long the caller took to notice.
    pub fn take(&mut self, now: Instant) -> Publish {
        let elapsed = self.timeline.anchor(now);
        // Which slot `now` falls in, clamped forward: the arithmetic below must never
        // hand back a slot behind the one already published.
        let slot = self.rate.slot_at(elapsed).max(self.next_slot);
        let missed = slot - self.next_slot;
        let resynced = missed > u64::from(self.max_duplicates);
        let duplicates = if resynced {
            // Rebase: pull the origin forward so the slots being skipped never existed.
            // Stream time stays contiguous — which is what the player reconstructs — and
            // only its agreement with the wall clock is given up.
            let skipped = missed - u64::from(self.max_duplicates);
            self.timeline.rebase(self.rate.slot_offset(skipped));
            self.max_duplicates
        } else {
            // Fits in a `u32` because it is at most `max_duplicates`, which is one.
            u32::try_from(missed).unwrap_or(self.max_duplicates)
        };
        self.next_slot += u64::from(duplicates) + 1;
        Publish {
            duplicates,
            resynced,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cadence() -> Cadence {
        Cadence::new(
            FrameRate::DEFAULT,
            Duration::from_secs(2),
            Arc::new(Timeline::new()),
        )
    }

    #[test]
    fn thirty_frames_a_second_is_three_thousand_ticks_each() {
        // Exactness is the point of a 90 kHz timescale: a stream whose sample durations
        // do not sum to its wall-clock length drifts out of sync with itself.
        assert_eq!(FrameRate::DEFAULT.sample_duration_ticks(), 3000);
        assert_eq!(
            u64::from(FrameRate::DEFAULT.sample_duration_ticks())
                * u64::from(FrameRate::DEFAULT.get()),
            u64::from(TIMESCALE)
        );
    }

    #[test]
    fn the_first_frame_is_always_due() {
        // The grid starts when the first picture arrives, not when the tap was installed:
        // otherwise a tap that waits a second for the render loop opens with a second of
        // duplicates of a frame it has not got yet.
        let mut c = cadence();
        let t0 = Instant::now();
        assert!(c.due(t0));
        assert_eq!(c.take(t0).duplicates, 0);
    }

    #[test]
    fn a_faster_panel_publishes_every_other_frame() {
        // The 60 Hz case, and the reason `due` is asked before the readback: half the
        // presented frames cost nothing at all.
        let mut c = cadence();
        let t0 = Instant::now();
        c.take(t0);
        // The 60 Hz instants are computed from the index, not accumulated: a step of
        // `1s/60` rounds down two thirds of a nanosecond and would make this measure the
        // test's own drift rather than the grid's.
        let mut published = 1;
        for i in 1..60u64 {
            let now = t0 + Duration::from_nanos(i * 1_000_000_000 / 60);
            if c.due(now) {
                assert_eq!(c.take(now).duplicates, 0, "nothing was missed");
                published += 1;
            }
        }
        assert_eq!(published, 30, "one second of a 30 fps stream: slots 0..=29");
    }

    #[test]
    fn a_slower_panel_fills_the_slots_it_missed() {
        // 10 fps in, 30 fps out: every published frame owes the timeline two duplicates.
        // Without them the stream's clock runs at a third speed and a player falls two
        // seconds further behind per second — which looks like a network problem and is
        // not one.
        let mut c = cadence();
        let t0 = Instant::now();
        c.take(t0);
        let mut slots = 1;
        for i in 1..=10u32 {
            let now = t0 + Duration::from_millis(100) * i;
            assert!(c.due(now));
            let p = c.take(now);
            assert_eq!(p.duplicates, 2);
            assert!(!p.resynced, "a third of the rate is not a stall");
            slots += 1 + p.duplicates;
        }
        assert_eq!(slots, 31, "one second of slots, all of them filled");
    }

    #[test]
    fn a_long_stall_rebases_instead_of_replaying_it() {
        // A panel that stopped presenting for a minute must not produce a minute of
        // identical frames the viewer then has to sit through to reach the present. The
        // timeline stays contiguous; only its agreement with the wall clock is given up.
        let mut c = cadence();
        let t0 = Instant::now();
        c.take(t0);
        let p = c.take(t0 + Duration::from_secs(60));
        assert!(p.resynced);
        assert_eq!(p.duplicates, 60, "two seconds' worth, and no more");

        // …and the grid is back in step with the wall clock straight afterwards.
        let p = c.take(t0 + Duration::from_secs(60) + FrameRate::DEFAULT.period());
        assert!(!p.resynced);
        assert_eq!(p.duplicates, 0);
    }

    #[test]
    fn the_grid_does_not_drift_over_a_long_run() {
        // Accumulating the period per slot loses the 33 ns it rounds off at 30 fps, which
        // is a second and a half a day. Slot offsets are computed from the index instead,
        // and this is what says so.
        let mut c = cadence();
        let t0 = Instant::now();
        c.take(t0);
        let mut published = 1u64;
        // An hour of a 60 Hz panel, stepped exactly.
        for i in 1..216_000u64 {
            let now = t0 + Duration::from_nanos(i * 1_000_000_000 / 60);
            if c.due(now) {
                let p = c.take(now);
                assert!(!p.resynced);
                published += 1 + u64::from(p.duplicates);
            }
        }
        assert_eq!(published, 108_000, "an hour of 30 fps, to the frame");
    }

    #[test]
    fn time_running_backwards_does_not_rewind_the_timeline() {
        // `Instant` is monotonic, but the frame's instant is taken by the render loop and
        // handed here, and a stream whose slots went backwards would write a `trun` the
        // player reads as a seek.
        let mut c = cadence();
        let t0 = Instant::now();
        c.take(t0 + Duration::from_secs(1));
        let p = c.take(t0);
        assert_eq!(p.duplicates, 0);
        assert!(!p.resynced);
    }
}
