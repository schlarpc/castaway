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
/// mutex around two words, taken once per audio block (tens of times a second) and once
/// per video frame.
#[derive(Debug, Default)]
pub struct MediaClock {
    inner: Mutex<Option<Anchor>>,
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

impl MediaClock {
    /// A clock that has not started.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        self.set(Anchor {
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
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_none() {
            *guard = Some(Anchor {
                media: first_frame,
                at,
                buffered: false,
            });
        }
    }

    fn set(&self, anchor: Anchor) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(anchor);
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
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let anchor = (*guard)?;
        let elapsed = at.saturating_duration_since(anchor.at);
        let position = anchor.media + elapsed;
        Some(if anchor.buffered {
            // What is queued minus what has not been heard yet. Saturating rather than
            // wrapping: at the very start less than a lead's worth has been submitted, and
            // the honest answer there is "the beginning", not a negative time.
            position.saturating_sub(OUTPUT_LEAD)
        } else {
            position
        })
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
