//! Lip-sync drift between two clocks that share a rate but not an origin (#278).
//!
//! The browser session has exactly two timestamps to work with, and they are *not* on one
//! clock. The audio side is the media element's `currentTime`, captured with every tapped
//! block — seconds since the start of the media. The video side is the compositor's paint
//! timestamp from Electron's OSR frame metadata — microseconds on an origin Chromium
//! chooses and does not document. Subtracting one from the other, which is what
//! `av_skew_ms` used to be, produces a number whose value is the *difference of the two
//! origins*: `av_skew_ms=17455` holding rock-steady for 90 s says only that both clocks
//! advance at real time (#177, #278). The same defect shape as #79, where AirPlay's two
//! planes were decoded in two different units and shared an origin that made the error
//! internally consistent.
//!
//! There is no way to put both sides on one origin: the paint timestamp is stamped by the
//! compositor, which knows nothing of the media element, and the audio tap runs in the
//! page world, which never sees a paint. So this does the other thing the issue allows —
//! **measure the offset at the first pairing and subtract it**. What is left is *drift
//! since the session started measuring*: zero by construction at the first pairing, and
//! from then on exactly the quantity a pacing loop needs — "is the sound falling behind
//! the pictures" — because both clocks run at real time when everything is healthy and
//! the difference moves only when one of them stalls or slews.
//!
//! The offset is re-measured for the first [`SETTLE`] rather than latched at the very
//! first pairing (#318): that pairing lands while the page's audio pipeline is still
//! filling, and the transient would otherwise be the session's zero for ever.
//!
//! What the number can no longer claim is the *absolute* lip-sync offset; that would need
//! a common origin neither side can give. The docs on `Electron::av_skew_ms` say so.
//!
//! Everything here is pure: the caller reads the clock once at the actor boundary — the
//! browser reader thread, where both message kinds arrive — and passes `now` in (#208,
//! #236). Nothing inside reads a clock.

use std::time::{Duration, Instant};

/// How stale the newest audio block may be and still be paired with a paint.
///
/// Audio blocks arrive every few milliseconds while an element is playing (the tap posts
/// per worklet quantum), so a mark older than this means the audio has *stopped* —
/// paused, ended, or the element went away. Pairing a live paint against a dead clock
/// would report the wall time since the sound stopped as if it were sync error, growing
/// at a second per second, which is precisely the class of self-consistent nonsense this
/// module replaces.
const STALE: Duration = Duration::from_millis(250);

/// An offset step, between consecutive pairings, past which the clocks did not drift —
/// one of them *jumped*.
///
/// A seek moves `currentTime` by seconds in one step; a page swap starts a new element at
/// zero; a pause freezes the media clock while the paint clock runs on. All arrive here
/// as a discontinuity in the offset between two consecutive pairings, and none of them is
/// drift: the honest report is "unknown at the seam", then measure against the new
/// offset. Genuine drift cannot trip this — consecutive pairings are at most [`STALE`]
/// plus a paint interval apart, so even a clock running at *half* rate steps the offset
/// by a few hundred milliseconds at the very most.
const JUMP_MS: i64 = 1_000;

/// How long a fresh baseline keeps being re-taken before it is believed (#318).
///
/// The first pairing of a session lands while the page's audio pipeline is still filling
/// — the element has started, the worklet is a few blocks in, `currentTime` is not yet
/// tracking the paints — and whatever offset exists at *that* instant used to become the
/// baseline for the whole session. The live measurement read a steady ~93 ms rather than
/// ~0 for exactly that reason: not drift, just the moment the stopwatch was started.
///
/// So the baseline is re-taken on every pairing for the first second and only then held.
/// The cost is honest and worth naming: drift that happens *inside* the window is
/// absorbed rather than reported, because nothing here can tell it apart from the
/// startup transient it exists to discard. Drift is a property of a session that has been
/// running, and a second is far shorter than the intervals a pacing loop cares about.
///
/// A discontinuity ([`JUMP_MS`]) restarts the window as well: a seek or a resume refills
/// the same pipeline from scratch, so the pairing right after one is the same
/// not-yet-tracking reading as the pairing at session start.
const SETTLE: Duration = Duration::from_secs(1);

/// The newest audio clock reading, and when it was taken.
#[derive(Debug, Clone, Copy)]
struct Mark {
    /// The media element's `currentTime`, in milliseconds.
    media_ms: i64,
    /// When the block carrying it arrived, on the caller's clock.
    at: Instant,
}

/// Pairs the page's audio clock with the compositor's paint clock and reports drift.
///
/// Feed it every audio block ([`Self::audio`], [`Self::pause`]) and every page paint that
/// carries a timestamp ([`Self::video`]); the latter returns the skew when there is one
/// to report. Positive means the picture's clock has gained on the sound's since the
/// session's first pairing.
#[derive(Debug, Default)]
pub struct SkewGauge {
    audio: Option<Mark>,
    /// What the reported drift is measured against — `None` until the first pairing.
    baseline: Option<Baseline>,
    /// The offset at the previous pairing, for the discontinuity check.
    last_offset_ms: Option<i64>,
}

/// The offset the two origins differ by, which is the part of the subtraction that means
/// nothing and is therefore removed — and the instant it stops being re-taken.
#[derive(Debug, Clone, Copy)]
struct Baseline {
    /// The offset this baseline was last taken at.
    offset_ms: i64,
    /// When [`SETTLE`] expires. Pairings before it re-take `offset_ms`; the first pairing
    /// at or after it is the first one reported as drift.
    settled_at: Instant,
}

impl Baseline {
    /// A baseline taken now, with its settling window open.
    fn taken(offset_ms: i64, now: Instant) -> Self {
        Self {
            offset_ms,
            settled_at: now + SETTLE,
        }
    }
}

impl SkewGauge {
    /// A gauge that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An audio block arrived carrying the media clock, read as `media_ms` milliseconds.
    pub fn audio(&mut self, media_ms: i64, now: Instant) {
        self.audio = Some(Mark { media_ms, at: now });
    }

    /// The element reported itself paused: its clock is not running, so the mark is not
    /// a clock any more.
    ///
    /// Without this, the tail block a pause produces would sit fresh-enough for
    /// [`STALE`] and pair with paints made after the sound stopped. With it, the paints
    /// during a pause report `None`, and the resume re-baselines through the
    /// discontinuity check — the paint clock ran through the pause, the media clock did
    /// not, so their offset jumped by the pause's length.
    pub fn pause(&mut self) {
        self.audio = None;
    }

    /// A page paint arrived stamped `media_ms` on the compositor's clock. Returns the
    /// drift since the first pairing, `None` where there is nothing honest to say.
    ///
    /// `None` means: no audio yet, the audio is stale ([`STALE`]), or this pairing is the
    /// seam of a discontinuity ([`JUMP_MS`]) and the next one measures against the new
    /// offset.
    pub fn video(&mut self, media_ms: i64, now: Instant) -> Option<i64> {
        let mark = self.audio?;
        let age = now.saturating_duration_since(mark.at);
        if age > STALE {
            // The sound stopped and nothing said `paused` (an element removed mid-play,
            // a renderer gone). Old pairings are also no longer comparable: whatever
            // resumes gets a fresh baseline through the jump check.
            return None;
        }
        // The mark is a few milliseconds old — the audio block cadence — while the paint
        // is now. Both clocks run at real time, so advance the audio reading by the age
        // of its mark rather than paying the cadence as jitter. The extrapolation cannot
        // hide a stalled audio clock: a stall freezes `media_ms` while marks keep
        // arriving, so the offset still grows at wall rate and is reported.
        let age_ms = i64::try_from(age.as_millis()).unwrap_or(i64::MAX);
        let offset = media_ms.saturating_sub(mark.media_ms.saturating_add(age_ms));
        if let Some(previous) = self.last_offset_ms {
            if (offset - previous).abs() > JUMP_MS {
                // A seek, a new element, or a resume: the offset moved by more than any
                // drift could between consecutive pairings. Re-baseline and say nothing
                // for this sample — reporting the jump itself would be the wild
                // excursion the gauge exists to stop.
                self.baseline = Some(Baseline::taken(offset, now));
                self.last_offset_ms = Some(offset);
                return None;
            }
        }
        self.last_offset_ms = Some(offset);
        match &mut self.baseline {
            // Still settling: this pairing replaces the baseline rather than being
            // measured against it (#318). Zero, and honestly so — the gauge has not
            // finished deciding where zero is.
            Some(baseline) if now < baseline.settled_at => {
                baseline.offset_ms = offset;
                Some(0)
            }
            Some(baseline) => Some(offset - baseline.offset_ms),
            None => {
                self.baseline = Some(Baseline::taken(offset, now));
                // Zero by construction, and honestly so: "drift since the session started
                // measuring" is zero at the first pairing.
                Some(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn nothing_is_reported_until_both_clocks_have_spoken() {
        let mut gauge = SkewGauge::new();
        let now = t0();
        assert_eq!(gauge.video(1_000, now), None, "no audio yet");
        gauge.audio(0, now);
        assert!(gauge.video(1_000, now).is_some());
    }

    #[test]
    fn two_unrelated_origins_read_as_zero_not_as_their_difference() {
        // #278 itself. The paint clock is microseconds since some Chromium origin — call
        // it 17 455 seconds — and the audio clock is seconds into the media. The old
        // subtraction reported the origin difference as 17 455 000 ms of "skew", constant
        // for as long as anyone watched. Both clocks at real time is *zero drift*.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(17_455_000, start), Some(0));
        // Ninety seconds on, both clocks having advanced at exactly real time.
        let later = start + Duration::from_secs(90);
        gauge.audio(90_000, later);
        assert_eq!(gauge.video(17_455_000 + 90_000, later), Some(0));
    }

    #[test]
    fn genuine_drift_is_reported_with_its_sign() {
        // The audio clock falls 50 ms behind over ten seconds; the picture is ahead of
        // the sound and the number is positive, matching the field's documented sign.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(1_000_000, start), Some(0));
        let later = start + Duration::from_secs(10);
        gauge.audio(10_000 - 50, later);
        assert_eq!(gauge.video(1_000_000 + 10_000, later), Some(50));
    }

    #[test]
    fn the_audio_block_cadence_is_not_reported_as_jitter() {
        // The newest audio mark is always a few milliseconds old when a paint arrives.
        // Without extrapolation every reading would carry the cadence as noise; with it,
        // a healthy session reads zero regardless of when in the block cycle the paint
        // lands.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(5_000, start), Some(0));
        // The paint arrives 21 ms after the newest block; both clocks kept real time.
        gauge.audio(1_000, start + Duration::from_millis(1_000));
        let skew = gauge
            .video(5_000 + 1_021, start + Duration::from_millis(1_021))
            .unwrap();
        assert_eq!(skew, 0, "the mark's age is not sync error");
    }

    #[test]
    fn a_stalled_audio_clock_is_not_hidden_by_the_extrapolation() {
        // `currentTime` freezes while blocks keep arriving (a wedged pipeline, #177's
        // cousin). The extrapolation covers only the newest mark's age; the frozen clock
        // accumulates against the wall and the drift is reported.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(0, start), Some(0));
        for i in 1..=10u64 {
            // Blocks keep coming, all claiming the same media time.
            gauge.audio(0, start + Duration::from_millis(i * 100));
        }
        let skew = gauge.video(1_000, start + Duration::from_secs(1)).unwrap();
        assert_eq!(
            skew, 1_000,
            "a second of wall time the sound did not advance"
        );
    }

    #[test]
    fn stale_audio_refuses_to_pair_rather_than_counting_the_silence() {
        // The sound stops without a `paused` flag. Paints keep coming; the honest answer
        // is unknown, not a skew growing at a second per second.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(0, start), Some(0));
        assert_eq!(
            gauge.video(5_000, start + Duration::from_secs(5)),
            None,
            "the newest audio mark is five seconds old"
        );
    }

    #[test]
    fn a_fresh_mark_still_pairs_at_the_stale_boundary() {
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert!(gauge.video(0, start + STALE).is_some());
        assert!(gauge.video(0, start + STALE + MS).is_none());
    }

    #[test]
    fn a_seek_rebaselines_instead_of_reporting_an_excursion() {
        // The viewer seeks forward a minute. The offset between the clocks jumps by
        // exactly that minute; nothing drifted. One `None` at the seam, then zero again.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(100_000, start), Some(0));
        // The element jumps to t=60 s; the paint clock ticks on undisturbed.
        let after = start + Duration::from_millis(50);
        gauge.audio(60_000, after);
        assert_eq!(gauge.video(100_050, after), None, "the seam says nothing");
        // …and from the new baseline, real time on both sides is zero drift again.
        let later = after + Duration::from_secs(5);
        gauge.audio(65_000, later);
        assert_eq!(gauge.video(105_050, later), Some(0));
    }

    #[test]
    fn a_pause_clears_the_mark_and_the_resume_rebaselines() {
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(0, start), Some(0));
        gauge.pause();
        assert_eq!(
            gauge.video(100, start + Duration::from_millis(100)),
            None,
            "a paused clock is not a clock"
        );
        // Ten seconds of pause: the media clock held at 0 while the paint clock ran on.
        let resumed = start + Duration::from_secs(10);
        gauge.audio(0, resumed);
        assert_eq!(
            gauge.video(10_000, resumed),
            None,
            "the offset jumped by the pause; the seam says nothing"
        );
        let later = resumed + Duration::from_secs(2);
        gauge.audio(2_000, later);
        assert_eq!(gauge.video(12_000, later), Some(0));
    }

    #[test]
    fn drift_within_the_jump_threshold_is_never_mistaken_for_a_seek() {
        // Slow accumulation crosses any absolute threshold eventually; what marks a seek
        // is the *step* between consecutive pairings. Walk 3 s of drift in 100 ms steps
        // and every reading must be a number, not a re-baseline.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(0, start), Some(0));
        // Out of the settling window with both clocks locked, so the walk below is
        // measured against a baseline that is no longer moving under it.
        let settled = start + SETTLE;
        gauge.audio(1_000, settled);
        assert_eq!(gauge.video(1_000, settled), Some(0));
        let mut skew = 0;
        for i in 1..=30u64 {
            let now = settled + Duration::from_millis(i * 200);
            let step = i64::try_from(i).unwrap();
            // The audio clock runs at half rate: 100 ms behind per 200 ms step.
            gauge.audio(1_000 + step * 100, now);
            skew = gauge
                .video(1_000 + step * 200, now)
                .expect("gradual drift must keep reporting");
        }
        assert_eq!(skew, 3_000, "thirty steps of 100 ms each");
    }

    #[test]
    fn the_baseline_settles_instead_of_latching_the_startup_transient() {
        // #318. The first pairing catches the page's audio pipeline mid-fill: here the
        // media clock is 93 ms behind where it will sit once the element is really
        // running, which is what the live session measured as a rock-steady 93 ms of
        // "drift". Latched, every later reading carries that offset for ever; settled,
        // the pairings inside the window replace it and a healthy session reads zero.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(-93, start);
        assert_eq!(gauge.video(0, start), Some(0));
        // Half a second in, the element is tracking: the offset is the one this session
        // will actually run at, and it is inside the window, so it becomes the baseline.
        let filling = start + Duration::from_millis(500);
        gauge.audio(500, filling);
        assert_eq!(gauge.video(500, filling), Some(0));
        // Long after the window, both clocks at real time: zero, not the 93 ms the first
        // pairing happened to see.
        let later = start + Duration::from_secs(30);
        gauge.audio(30_000, later);
        assert_eq!(gauge.video(30_000, later), Some(0));
    }

    #[test]
    fn the_window_shuts_and_real_drift_after_it_is_reported() {
        // The other half of the settling window: it is a window, not a mode. The shipped
        // SETTLE is asserted in virtual time (#208) rather than waited out — a pairing at
        // the instant it expires is already measuring.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(0, start), Some(0));
        let settled = start + SETTLE;
        // 40 ms of genuine drift, at the first pairing the window no longer covers.
        gauge.audio(1_000 - 40, settled);
        assert_eq!(
            gauge.video(1_000, settled),
            Some(40),
            "the window is half-open: the pairing that closes it is measured, not absorbed"
        );
    }

    #[test]
    fn a_seek_settles_again_rather_than_latching_the_first_pairing_after_it() {
        // A seek refills the same pipeline the session start did, so the pairing right
        // after one is the same not-yet-tracking reading — the window restarts with the
        // baseline.
        let mut gauge = SkewGauge::new();
        let start = t0();
        gauge.audio(0, start);
        assert_eq!(gauge.video(0, start), Some(0));
        // Seek forward a minute, well past the first settling window.
        let seek = start + Duration::from_secs(10);
        gauge.audio(70_000, seek);
        assert_eq!(gauge.video(10_000, seek), None, "the seam says nothing");
        // The refill: 60 ms out at the first pairing after the seek, tracking by the
        // second one. Both are inside the new window, so neither is drift.
        let refilling = seek + Duration::from_millis(100);
        gauge.audio(70_100 - 60, refilling);
        assert_eq!(gauge.video(10_100, refilling), Some(0));
        let tracking = seek + Duration::from_millis(600);
        gauge.audio(70_600, tracking);
        assert_eq!(gauge.video(10_600, tracking), Some(0));
        // And afterwards the session reads zero rather than the transient.
        let later = seek + Duration::from_secs(30);
        gauge.audio(100_000, later);
        assert_eq!(gauge.video(40_000, later), Some(0));
    }
}
