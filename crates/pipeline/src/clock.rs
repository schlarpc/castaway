//! The media clock a pulled A/V session is presented against.
//!
//! The media-URL path — DLNA, Cast `LOAD`, AirPlay video — pulls from a file or an HTTP
//! server as fast as it can read. Nothing in that chain is a clock: the decoder does not
//! sleep, the compositor presents whatever it was last handed, and the audio output
//! accepts blocks as fast as they arrive. So without this, a two-hour film decodes in as
//! long as the CPU takes and the panel shows a slideshow of whichever frames happened to
//! win a slot in the bounded channel.
//!
//! **Audio is the master.** That is not a coin toss: a video frame arriving late can be
//! dropped and a frame arriving early can be held, and at 24–60 Hz nobody sees either.
//! Audio has no such slack — stretching it changes the pitch and gapping it is a click,
//! and both are immediately obvious in a room. So the audio thread paces itself to real
//! time (`audio_session::Pace`) and publishes where it has got to; video waits for it.
//!
//! When a file has no audio track there is nothing to follow, so the clock runs off the
//! wall instead, seeded at the first frame. That is the same policy stated twice rather
//! than a special case: *something* must be real-time, and it is audio when audio exists.
//!
//! Note what this is deliberately *not* used for. Live mirroring (Cast/AirPlay pixel
//! streams) is paced by the sender, and ground rule 4 says to drop late frames there
//! rather than wait — holding a mirrored frame to match a clock adds latency to the one
//! path where latency is the whole complaint.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How far ahead of the speaker the audio thread is allowed to run.
///
/// Defined here and used by `audio_session` rather than the other way round, because two
/// copies of this number would be two copies that drift: the audio thread sleeps to stay
/// at most this far ahead, so at any moment roughly this much of what it has submitted
/// has not been heard yet, and subtracting it is what turns "what I have queued" into
/// "what the room is hearing" — which is what video has to match. A mismatch would show
/// up as lip sync that is consistently off by the difference, which is the kind of wrong
/// that people notice and cannot describe.
///
/// It is also the buffer that absorbs a scheduling hiccup on either side: too small and
/// any stall is a dropout, too large and a pause takes that long to actually go quiet.
pub const OUTPUT_LEAD: Duration = Duration::from_millis(250);

/// Where a session has got to, in media time.
///
/// Cheap to read from the video thread and cheap to write from the audio thread — one
/// mutex around a few words, taken once per audio block (tens of times a second) and once
/// per video frame.
///
/// **One** mutex, and that is load-bearing rather than tidy. The anchor and the freeze
/// used to have a lock each, and `set_paused` took the freeze lock and then called `now()`,
/// which takes it again: a `std::sync::Mutex` is not reentrant, so the first pause of any
/// session deadlocked the thread that asked for it, permanently. On the box that is a tokio
/// worker gone for the life of the process; here it was the whole test suite timing out
/// rather than failing. Two locks over one piece of state is what made that expressible —
/// with one, it cannot be written.
#[derive(Debug, Default)]
pub struct MediaClock {
    state: Mutex<State>,
}

/// Everything the clock knows, under the one lock.
#[derive(Debug, Default, Clone, Copy)]
struct State {
    /// The last thing the master said, or `None` before anything has.
    anchor: Option<Anchor>,
    /// Whether the clock is frozen.
    ///
    /// Separate from `frozen`, because "paused" and "paused *somewhere*" are different
    /// facts: a session can be paused before its first frame has been decoded, and
    /// conflating the two made that pause a silent no-op — a control point that sent
    /// SetAVTransportURI, Play and Pause in quick succession got playback anyway.
    paused: bool,
    /// Where it froze. `None` while `paused` means the clock had not started yet, and the
    /// first anchor to arrive is what it freezes at.
    frozen: Option<Duration>,
}

/// The last thing the master said, and when it said it.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    /// Media position the master had reached.
    media: Duration,
    /// When it reached it.
    at: Instant,
    /// Whether `media` is a queue position that has not been heard yet (audio) or a
    /// position already presented (video-as-master).
    buffered: bool,
}

impl State {
    /// Where the anchor says the media is at `at`, ignoring any freeze.
    fn running_at(&self, at: Instant) -> Option<Duration> {
        let anchor = self.anchor?;
        let elapsed = at.saturating_duration_since(anchor.at);
        Some(if anchor.buffered {
            // A buffered anchor names a queue position, and a queue nobody refills runs
            // dry in exactly [`OUTPUT_LEAD`] — after which the room is hearing silence
            // and media time stands still. So the extrapolation is bounded by the lead:
            // in steady state the next block re-anchors long before the bound matters,
            // and during a stall the clock stops at the last thing the room heard
            // instead of free-running on the wall and jumping backwards when the source
            // returns (#232). `is_hopeless` reads this same value, so the runaway also
            // discarded every video frame arriving just after a rebuffer as seconds
            // late.
            //
            // Saturating rather than wrapping: at the very start less than a lead's
            // worth has been submitted, and the honest answer there is "the beginning",
            // not a negative time.
            (anchor.media + elapsed.min(OUTPUT_LEAD)).saturating_sub(OUTPUT_LEAD)
        } else {
            // A video master has no queue and no other clock: the wall is the pace, and
            // free-running is the design rather than a failure mode.
            anchor.media + elapsed
        })
    }

    /// Take a new anchor, and freeze on it if a pause is waiting for somewhere to land.
    fn anchor_at(&mut self, anchor: Anchor) {
        self.anchor = Some(anchor);
        if self.paused && self.frozen.is_none() {
            self.frozen = self.running_at(anchor.at);
        }
    }
}

impl MediaClock {
    /// A clock that has not started.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The state, recovering a poisoned lock rather than failing on it — a panic somewhere
    /// else is not a reason to stop telling the video thread what time it is.
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Record that audio through `media` has been handed to the output.
    ///
    /// The output has not played all of it — that is what [`OUTPUT_LEAD`] accounts for —
    /// so this is the *queue* position, not the heard position.
    pub fn observe_audio(&self, media: Duration) {
        self.observe_audio_at(media, Instant::now());
    }

    /// [`MediaClock::observe_audio`] with the instant supplied, so a test can assert on
    /// the interpolation exactly rather than within a tolerance.
    pub fn observe_audio_at(&self, media: Duration, at: Instant) {
        self.state().anchor_at(Anchor {
            media,
            at,
            buffered: true,
        });
    }

    /// Start the clock from the wall, for a session with no audio to follow.
    ///
    /// Idempotent: only the first frame seeds it, because restarting on every frame would
    /// make `now()` permanently zero and every frame instantly due.
    pub fn start_video_master(&self, first_frame: Duration) {
        self.start_video_master_at(first_frame, Instant::now());
    }

    /// [`MediaClock::start_video_master`] with the instant supplied, for tests.
    pub fn start_video_master_at(&self, first_frame: Duration, at: Instant) {
        let mut state = self.state();
        if state.anchor.is_none() {
            state.anchor_at(Anchor {
                media: first_frame,
                at,
                buffered: false,
            });
        }
    }

    /// Freeze or release the clock.
    ///
    /// Resuming re-anchors to the frozen position, so the time spent paused does not
    /// count as playback — otherwise every pause would leave the picture that much
    /// behind the sound for the rest of the item.
    ///
    /// Pausing before the first frame is honoured rather than dropped. The clock has
    /// nowhere to freeze yet, so it freezes on whatever arrives first; what matters
    /// immediately is that [`MediaClock::is_paused`] says so, because that is what holds
    /// the video thread.
    pub fn set_paused(&self, paused: bool) {
        let mut state = self.state();
        if state.paused == paused {
            return;
        }
        state.paused = paused;
        if paused {
            state.frozen = state.running_at(Instant::now());
        } else if let Some(position) = state.frozen.take() {
            // Re-anchor as a *presented* position rather than a queued one: the output's
            // buffer was drained by the pause, so there is nothing unheard left to discount.
            state.anchor = Some(Anchor {
                media: position,
                at: Instant::now(),
                buffered: false,
            });
        }
    }

    /// Move the clock to `position`, because the media moved there.
    ///
    /// Unlike [`MediaClock::start_video_master`] this is not idempotent — a seek is
    /// precisely the case where the clock must be overruled rather than left alone. The
    /// new anchor is a *presented* position, not a queued one: everything that had been
    /// submitted to the output was thrown away with the seek, so there is nothing unheard
    /// left to discount.
    ///
    /// A paused session keeps its pause and moves underneath it. Scrubbing while paused is
    /// how people find a spot, and resuming has to start from the spot they found rather
    /// than the one they left.
    pub fn seek_to(&self, position: Duration) {
        let mut state = self.state();
        state.anchor = Some(Anchor {
            media: position,
            at: Instant::now(),
            buffered: false,
        });
        if state.paused {
            state.frozen = Some(position);
        }
    }

    /// Whether the clock is currently frozen.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.state().paused
    }

    /// Media time as of now, or `None` before the clock has started.
    #[must_use]
    pub fn now(&self) -> Option<Duration> {
        self.now_at(Instant::now())
    }

    /// [`MediaClock::now`] against a supplied instant, so the interpolation is testable
    /// without sleeping.
    #[must_use]
    pub fn now_at(&self, at: Instant) -> Option<Duration> {
        let state = self.state();
        if state.paused {
            return state.frozen;
        }
        state.running_at(at)
    }

    /// How long to wait before presenting a frame at `pts`, or `None` if it is due now.
    ///
    /// `None` also covers a clock that has not started: the first video frame of an A/V
    /// file is decoded before any audio has been submitted, and holding it until audio
    /// appears would blank the panel for the length of the lead at the start of every
    /// item.
    #[must_use]
    pub fn wait_for(&self, pts: Duration) -> Option<Duration> {
        let now = self.now()?;
        pts.checked_sub(now).filter(|d| !d.is_zero())
    }

    /// Whether a frame at `pts` is so far behind that presenting it is worse than
    /// dropping it.
    ///
    /// The threshold is generous on purpose. A frame a little late is still the best
    /// picture available and showing it beats showing nothing; a frame *seconds* late
    /// means the decoder lost a race it is not going to win, and playing catch-up frame
    /// by frame keeps it behind forever.
    #[must_use]
    pub fn is_hopeless(&self, pts: Duration) -> bool {
        const TOO_LATE: Duration = Duration::from_millis(400);
        self.now()
            .is_some_and(|now| now > pts && now - pts > TOO_LATE)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_clock_that_has_not_started_knows_it() {
        let c = MediaClock::new();
        assert!(c.now().is_none());
        // And a frame is due immediately rather than waiting forever for a master that
        // may not exist — the first frame of an A/V file arrives before any audio has.
        assert!(c.wait_for(Duration::from_secs(5)).is_none());
        assert!(!c.is_hopeless(Duration::ZERO));
    }

    #[test]
    fn audio_position_discounts_what_has_not_been_heard_yet() {
        let c = MediaClock::new();
        let at = Instant::now();
        c.observe_audio_at(Duration::from_secs(10), at);
        // Ten seconds submitted, a quarter-second of it still in the device's buffer.
        assert_eq!(c.now_at(at).unwrap(), Duration::from_millis(9_750));
    }

    #[test]
    fn a_stalled_audio_master_stops_the_clock_instead_of_running_ahead() {
        // #232: during a rebuffer the PCM thread parks in `recv_timeout` and stops
        // anchoring, and the clock used to free-run on the wall for the whole stall —
        // then jump backwards when the source returned. `is_hopeless` reads the same
        // value, so video arriving just after a rebuffer was discarded as seconds late.
        //
        // The bound is physical, not a tolerance: a queue nobody refills runs dry in
        // exactly OUTPUT_LEAD, and from then on the room is hearing silence — media time
        // stands still at the last thing it heard.
        let c = MediaClock::new();
        let t0 = Instant::now();
        c.observe_audio_at(Duration::from_secs(10), t0);
        // In range, the interpolation is untouched.
        assert_eq!(
            c.now_at(t0 + Duration::from_millis(100)).unwrap(),
            Duration::from_millis(9_850)
        );
        // The queue drains dry at the lead, and the clock stops with it.
        assert_eq!(c.now_at(t0 + OUTPUT_LEAD).unwrap(), Duration::from_secs(10));
        assert_eq!(
            c.now_at(t0 + Duration::from_secs(30)).unwrap(),
            Duration::from_secs(10),
            "half a minute of stall is still the same silence"
        );
        // The source comes back; the new anchor is the truth, and the step is forward.
        let resumed = t0 + Duration::from_secs(30);
        c.observe_audio_at(Duration::from_millis(10_500), resumed);
        assert_eq!(
            c.now_at(resumed).unwrap(),
            Duration::from_millis(10_250),
            "resumes from the new queue position, discounting the lead again"
        );
    }

    #[test]
    fn the_start_of_a_track_does_not_report_a_negative_position() {
        let c = MediaClock::new();
        c.observe_audio(Duration::from_millis(20));
        assert_eq!(c.now().unwrap(), Duration::ZERO);
    }

    #[test]
    fn the_clock_interpolates_between_audio_blocks() {
        let c = MediaClock::new();
        let at = Instant::now();
        c.observe_audio_at(Duration::from_secs(10), at);
        let later = c.now_at(at + Duration::from_millis(100)).unwrap();
        let now = c.now_at(at).unwrap();
        assert_eq!(later - now, Duration::from_millis(100));
    }

    #[test]
    fn a_frame_ahead_of_the_clock_waits_and_one_behind_does_not() {
        let c = MediaClock::new();
        c.observe_audio(Duration::from_secs(10)); // heard: ~9.75s
        assert!(c.wait_for(Duration::from_secs(11)).is_some());
        assert!(c.wait_for(Duration::from_secs(9)).is_none());
    }

    /// A frame slightly late is still the best picture available. One seconds late means
    /// the decoder lost a race, and catching up frame by frame never finishes.
    #[test]
    fn only_hopelessly_late_frames_are_dropped() {
        let c = MediaClock::new();
        c.observe_audio(Duration::from_secs(10)); // heard: ~9.75s
        assert!(!c.is_hopeless(Duration::from_millis(9_600)));
        assert!(c.is_hopeless(Duration::from_secs(5)));
    }

    /// Pausing has to *return*.
    ///
    /// It did not: `set_paused` held the freeze lock and then called `now()`, which takes
    /// the same lock, and a `std::sync::Mutex` is not reentrant — so the first pause of
    /// any session deadlocked the thread that asked for it, for good. On the box that is a
    /// tokio worker; in the suite it was the whole test run timing out.
    ///
    /// Written as "on another thread, with a deadline" rather than as a plain call,
    /// because the failure mode is a hang. A test that hangs reports nothing and reads as
    /// an infrastructure problem; this one names the bug.
    #[test]
    fn pausing_returns_rather_than_deadlocking_the_thread_that_asked() {
        let clock = std::sync::Arc::new(MediaClock::new());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let clock = std::sync::Arc::clone(&clock);
            move || {
                clock.observe_audio(Duration::from_secs(5));
                clock.set_paused(true);
                clock.set_paused(false);
                clock.set_paused(true);
                let _ = done_tx.send(());
            }
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "set_paused never came back — the clock has deadlocked on its own lock",
        );
        worker.join().expect("the pausing thread panicked");
        assert!(clock.is_paused());
    }

    /// A paused clock stops. If it kept running, resuming would dump every frame it had
    /// "missed" and the position readout would show a place the music never reached.
    #[test]
    fn pausing_freezes_the_clock_and_resuming_does_not_replay_the_gap() {
        let c = MediaClock::new();
        c.observe_audio(Duration::from_secs(10));
        let before_pause = c.now().unwrap();
        c.set_paused(true);
        assert!(c.is_paused());

        // Where it actually froze, read once. Not compared against the reading taken
        // *before* the pause: the clock is still running between those two lines, so the
        // difference is real elapsed time and asserting them equal would be asserting that
        // two statements execute at the same instant.
        let at_pause = c.now().unwrap();
        assert!(
            at_pause >= before_pause && at_pause - before_pause < Duration::from_millis(50),
            "froze at {at_pause:?} from {before_pause:?}"
        );

        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(
            c.now().unwrap(),
            at_pause,
            "a frozen clock must not advance"
        );

        c.set_paused(false);
        assert!(!c.is_paused());
        let after = c.now().unwrap();
        assert!(
            after >= at_pause && after < at_pause + Duration::from_millis(60),
            "resumed at {after:?} from {at_pause:?}: the pause was counted as playback"
        );
    }

    #[test]
    fn video_master_runs_off_the_wall_from_the_first_frame() {
        let c = MediaClock::new();
        let at = Instant::now();
        c.start_video_master_at(Duration::from_secs(100), at);
        // No output lead to discount: nothing is buffered, the position *is* the position.
        assert_eq!(c.now_at(at).unwrap(), Duration::from_secs(100));
        assert_eq!(
            c.now_at(at + Duration::from_millis(500)).unwrap(),
            Duration::from_millis(100_500)
        );
    }

    /// Seeding on every frame would peg the clock to the newest frame's own timestamp,
    /// making every frame instantly due — which is the no-clock behaviour this exists to
    /// replace, arrived at by a different route.
    #[test]
    fn video_master_is_seeded_once() {
        let c = MediaClock::new();
        c.start_video_master(Duration::from_secs(1));
        c.start_video_master(Duration::from_secs(50));
        assert!(c.now().unwrap() < Duration::from_secs(2));
    }

    /// A seek overrules the clock, where the first-frame seed deliberately does not.
    /// Without it every frame after a jump forward is "hopelessly late" and dropped, and
    /// every frame after a jump back waits for a turn that is minutes away.
    #[test]
    fn a_seek_moves_the_clock_where_a_seed_would_not() {
        let c = MediaClock::new();
        c.observe_audio(Duration::from_secs(10));
        c.seek_to(Duration::from_secs(600));
        let now = c.now().unwrap();
        assert!(
            now >= Duration::from_secs(600) && now < Duration::from_secs(601),
            "landed at {now:?}"
        );
        // No output lead to discount: the seek threw away everything that was queued.
        assert!(!c.is_hopeless(Duration::from_secs(600)));
    }

    /// Pausing before the first frame has been decoded is an ordinary thing for a control
    /// point to do — `SetAVTransportURI`, `Play`, `Pause`, faster than a fetch — and it
    /// used to be dropped on the floor, because "paused" was stored as "paused *at* a
    /// position" and there was no position yet. The session played on.
    #[test]
    fn a_pause_that_arrives_before_the_first_frame_still_pauses() {
        let c = MediaClock::new();
        c.set_paused(true);
        assert!(
            c.is_paused(),
            "the pause was dropped for want of a position"
        );
        assert_eq!(c.now(), None, "and there is still nothing to report");

        // The first thing to arrive is where it freezes, rather than starting it running.
        c.start_video_master(Duration::from_secs(7));
        assert!(c.is_paused());
        assert_eq!(c.now(), Some(Duration::from_secs(7)));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(c.now(), Some(Duration::from_secs(7)), "it must not advance");

        c.set_paused(false);
        let after = c.now().unwrap();
        assert!(
            after >= Duration::from_secs(7) && after < Duration::from_millis(7_100),
            "resumed at {after:?} rather than where it was held"
        );
    }

    /// Scrubbing while paused is how people find a spot; resuming has to start from the
    /// spot they found, not the one they left.
    #[test]
    fn seeking_while_paused_moves_the_frozen_position() {
        let c = MediaClock::new();
        c.observe_audio(Duration::from_secs(10));
        c.set_paused(true);
        c.seek_to(Duration::from_secs(120));
        assert!(c.is_paused());
        assert_eq!(c.now(), Some(Duration::from_secs(120)));

        c.set_paused(false);
        let after = c.now().unwrap();
        assert!(
            after >= Duration::from_secs(120) && after < Duration::from_secs(121),
            "resumed at {after:?} rather than where the scrub landed"
        );
    }

    /// Audio takes over from a video-master seed: a file whose audio starts late should
    /// end up following the audio, not the wall.
    #[test]
    fn audio_supersedes_a_video_seed() {
        let c = MediaClock::new();
        c.start_video_master(Duration::from_secs(1));
        c.observe_audio(Duration::from_secs(30));
        assert!(c.now().unwrap() > Duration::from_secs(29));
    }
}
