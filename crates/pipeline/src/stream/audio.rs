//! Tapping the panel's sound.
//!
//! ## There is one mixer, and this is a tap on it
//!
//! Every source's audio — Cast, AirPlay, DLNA, Spotify, Bluetooth, and the browser's
//! captured page audio — is summed by [`crate::mixer::AudioMixer`] and written to one
//! device. [`AudioMix`] is a [`MixTap`] on that mixer, so what the stream carries is
//! literally the samples the speakers were given, at the instant they were given them.
//!
//! It used to be a reconstruction. Until #111 there was no mixer to tap: each session held
//! its own device and the OS did the summing, so "what the panel is playing" did not exist
//! anywhere in this process. This module rebuilt it by wrapping the output factory, giving
//! every session a tee, resampling each one to 48 kHz a second time, and following each
//! with its own cursor — plus a resync threshold, a lead cap and a settle window to keep
//! sessions that raced (a file through a null sink) from laying down hours of audio in
//! seconds. All of that is gone. The mixer produces one stream at real-time pace, so a
//! block is placed where it arrives because that is where it belongs.
//!
//! ## The mix is indexed by wall clock, not by arrival order
//!
//! A queue would drift. The stream has to produce a continuous 48 kHz timeline whether or
//! not anything is playing — a silent panel is the *normal* case, and an audio track that
//! simply stops is one a player stalls on rather than one it treats as quiet.
//!
//! So the mix is a window of the shared [`Timeline`], addressed by absolute sample
//! position. A block written at instant *t* lands at position `t - origin`; anything nobody
//! wrote is silence because the buffer is zeroed. Video slots are derived from the same
//! origin, so the two tracks cannot drift apart no matter how long the stream runs.
//!
//! That zero-fill is also why a tap does not keep the audio device open: an idle panel
//! holds no sink, and the stream stays continuous regardless.
//!
//! ## What this is not
//!
//! It is not sample-accurate. A block is placed where it was *handed to the device*, and
//! the device plays it some tens of milliseconds later — so the stream's audio leads the
//! panel's own speakers by roughly the output queue's depth. Relative to the stream's own
//! video, which is captured at the moment it is composited, that is the same relationship
//! the panel itself has, which is what matters for watching it. Sub-frame lip sync is not
//! on offer and the readback path could not deliver it anyway.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::timeline::Timeline;
use crate::mixer::MixTap;

/// The rate the stream's audio track runs at.
///
/// The mixer's rate, not a second opinion: the stream carries what the speakers were
/// given, so a conversion here would be one nobody asked for.
pub use crate::mixer::{CHANNELS, RATE};

/// How much sound the mix will hold before it starts discarding the oldest.
///
/// A backstop, not a buffer: the encode loop drains this several times a second. It
/// matters only if that loop dies, and then the alternative is a `Vec` that grows until
/// the panel is killed by the OOM reaper — which is a much worse way to lose audio.
const MAX_BUFFERED: Duration = Duration::from_secs(4);

/// The panel's audio, on the stream's timeline.
#[derive(Debug)]
pub struct AudioMix {
    timeline: Arc<Timeline>,
    inner: Mutex<Window>,
}

#[derive(Debug)]
struct Window {
    /// Absolute frame position of the first frame in `samples`.
    base: u64,
    /// Interleaved stereo at [`RATE`]. Zeroed regions are silence, which is the whole
    /// trick: nothing has to write silence for silence to be what comes out.
    samples: VecDeque<f32>,
}

impl AudioMix {
    /// A mix on `timeline`.
    #[must_use]
    pub fn new(timeline: Arc<Timeline>) -> Self {
        Self {
            timeline,
            inner: Mutex::new(Window {
                base: 0,
                samples: VecDeque::new(),
            }),
        }
    }

    /// How many frames of [`RATE`] audio fit in `elapsed`.
    fn frames_at(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_nanos() * u128::from(RATE) / 1_000_000_000).unwrap_or(u64::MAX)
    }

    /// Add interleaved stereo, taken to start playing at `now`.
    ///
    /// Silently does nothing before the timeline is anchored: audio that arrives ahead of
    /// the first composited frame has nowhere on the timeline to go, and placing it at
    /// position zero would put sound under a picture that had not been drawn.
    pub fn add(&self, now: Instant, stereo: &[f32]) {
        let Some(elapsed) = self.timeline.elapsed(now) else {
            return;
        };
        let at = Self::frames_at(elapsed);
        let Ok(mut window) = self.inner.lock() else {
            return;
        };
        // Part of this block belongs to sample positions the encoder has already taken.
        // That part is gone; the rest still lands where it should, so a late block loses
        // its head rather than shifting everything after it.
        let skip_frames = window.base.saturating_sub(at);
        let channels = usize::from(CHANNELS);
        let Some(tail) = stereo.get(
            usize::try_from(skip_frames)
                .unwrap_or(usize::MAX)
                .saturating_mul(channels)..,
        ) else {
            return;
        };
        let start = usize::try_from(at.saturating_add(skip_frames) - window.base).unwrap_or(0);
        let needed = (start + tail.len() / channels) * channels;
        if window.samples.len() < needed {
            window.samples.resize(needed, 0.0);
        }
        // Summed, not replaced: two sessions overlapping is rare but real — a Spotify
        // stream that has not stopped when a cast starts — and the last writer winning
        // would silence one of them at random.
        for (i, sample) in tail.iter().enumerate() {
            if let Some(slot) = window.samples.get_mut(start * channels + i) {
                *slot += *sample;
            }
        }
        // `base` counts frames and `samples` holds interleaved ones, so the trim has to
        // drop a whole frame at a time. Popping one sample per `base += 1` advanced the
        // label at twice the rate for stereo: what was still in the window came back
        // relabelled half a trim late, and once pinned at cap the base outran the wall
        // clock — heads of live blocks lost to `skip_frames`, and `take` refusing until
        // the clock caught up. It lasted until the next `restart()`.
        let cap = usize::try_from(Self::frames_at(MAX_BUFFERED)).unwrap_or(usize::MAX) * channels;
        while window.samples.len() > cap {
            for _ in 0..channels {
                window.samples.pop_front();
            }
            window.base += 1;
        }
    }

    /// Take `frames` frames from the front, if they are far enough in the past to be
    /// settled.
    ///
    /// `settle` is how long a block gets to arrive late and still land in the right place;
    /// it is the one knob trading the stream's audio latency against how much of a slow
    /// session's block gets clipped off its front.
    pub fn take(&self, now: Instant, frames: usize, settle: Duration) -> Option<Vec<f32>> {
        let elapsed = self.timeline.elapsed(now)?.checked_sub(settle)?;
        let settled = Self::frames_at(elapsed);
        let mut window = self.inner.lock().ok()?;
        if window.base + frames as u64 > settled {
            return None;
        }
        let channels = usize::from(CHANNELS);
        let wanted = frames * channels;
        // Short means nothing wrote that far, which is silence rather than a shortfall.
        if window.samples.len() < wanted {
            window.samples.resize(wanted, 0.0);
        }
        let out: Vec<f32> = window.samples.drain(..wanted).collect();
        window.base += frames as u64;
        Some(out)
    }

    /// Where the mix has been drained to, in frames since the origin. For tests and for
    /// the encode loop's own bookkeeping.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.inner.lock().map_or(0, |w| w.base)
    }

    /// Throw away everything held and go back to position zero.
    ///
    /// Paired with [`Timeline::reset`] when a stream restarts: what is in the window
    /// belongs to the old origin, and keeping it would put the previous run's last second
    /// of sound at the start of the new one.
    pub fn clear(&self) {
        if let Ok(mut window) = self.inner.lock() {
            window.base = 0;
            window.samples.clear();
        }
    }
}

/// The audio half of the output stream, as the app holds it.
///
/// Created once, at startup, because the factory it wraps is installed once — where the
/// video tap comes and goes with whoever is watching. [`Self::restart`] is what reconciles
/// those two lifetimes.
#[derive(Debug)]
pub struct StreamAudio {
    timeline: Arc<Timeline>,
    mix: Arc<AudioMix>,
}

impl Default for StreamAudio {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamAudio {
    /// A fresh, unanchored timeline and an empty mix.
    #[must_use]
    pub fn new() -> Self {
        let timeline = Arc::new(Timeline::new());
        Self {
            mix: Arc::new(AudioMix::new(Arc::clone(&timeline))),
            timeline,
        }
    }

    /// The tap to register with the panel's mixer.
    ///
    /// Held as a trait object by the mixer, so the stream's audio arrives without this
    /// module knowing anything about sessions — which is the whole difference from the tee
    /// it replaced. A session cannot be added later that reaches the speakers and misses
    /// the stream, because it does not reach the speakers except through the mixer.
    #[must_use]
    pub fn tap(&self) -> Arc<dyn MixTap> {
        Arc::clone(&self.mix) as Arc<dyn MixTap>
    }

    /// The timeline both tracks are measured against.
    #[must_use]
    pub fn timeline(&self) -> Arc<Timeline> {
        Arc::clone(&self.timeline)
    }

    /// The mix the encode loop pulls from.
    #[must_use]
    pub fn mix(&self) -> Arc<AudioMix> {
        Arc::clone(&self.mix)
    }

    /// Begin a new presentation: forget the origin and everything buffered against it.
    pub fn restart(&self) {
        self.timeline.reset();
        self.mix.clear();
    }
}

impl MixTap for AudioMix {
    fn mixed(&self, at: Instant, stereo: &[f32]) {
        self.add(at, stereo);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn mix() -> (Arc<Timeline>, AudioMix) {
        let timeline = Arc::new(Timeline::new());
        (Arc::clone(&timeline), AudioMix::new(timeline))
    }

    /// `frames` frames of interleaved stereo at `value`.
    fn tone(frames: usize, value: f32) -> Vec<f32> {
        vec![value; frames * 2]
    }

    const SETTLE: Duration = Duration::from_millis(100);

    #[test]
    fn a_silent_panel_still_produces_a_timeline() {
        // The normal case. An audio track that stops when nothing is playing is one a
        // player stalls on, so silence has to be *produced* rather than merely not
        // written — which is what a zeroed window does for free.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        let out = mix
            .take(t0 + Duration::from_secs(1), 1024, SETTLE)
            .expect("a second has passed, so the first frames are settled");
        assert_eq!(out.len(), 2048);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn the_overflow_trim_drops_whole_frames_not_half_of_one() {
        // The backstop fires when the encode loop — the mix's only reader — stalls past
        // MAX_BUFFERED while a session keeps writing. It used to pop one *sample* per
        // `base += 1`, so for stereo the label ran at twice the rate the audio was
        // actually discarded: what stayed in the window came back half a trim late
        // against the video it shares a timeline with, and once pinned at cap `base`
        // outran the wall clock — live blocks losing their heads to `skip_frames`, and
        // `take` refusing to settle until the clock caught up.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);

        let capacity = usize::try_from(AudioMix::frames_at(MAX_BUFFERED)).unwrap();
        // Write past the cap in one go, at position zero.
        mix.add(t0, &tone(capacity + 4800, 0.5));

        let (base, held) = {
            let window = mix.inner.lock().unwrap();
            (window.base, window.samples.len())
        };
        assert_eq!(held, capacity * 2, "trimmed back to the cap");
        // A frame of audio discarded is a frame of `base`, and no more.
        assert_eq!(
            base, 4800,
            "base must advance by the frames dropped, not by the samples"
        );
        // …and the label still points at real audio: the next take from `base` is the
        // tone, not silence read off the end.
        let out = mix
            .take(t0 + MAX_BUFFERED + Duration::from_secs(1), 1024, SETTLE)
            .expect("well past settle");
        assert!(
            out.iter().all(|s| (*s - 0.5).abs() < f32::EPSILON),
            "{out:?}"
        );
    }

    #[test]
    fn a_block_lands_where_the_clock_says_it_was_played() {
        // Not where it happened to arrive in a queue. This is the whole difference between
        // a mix that stays in step with the video and one that drifts.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        // 100 ms in: frame 4800.
        mix.add(t0 + Duration::from_millis(100), &tone(480, 0.5));

        let now = t0 + Duration::from_secs(1);
        let first = mix.take(now, 4800, SETTLE).unwrap();
        assert!(first.iter().all(|s| *s == 0.0), "nothing before 100 ms");
        let second = mix.take(now, 480, SETTLE).unwrap();
        assert!(second.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        let third = mix.take(now, 480, SETTLE).unwrap();
        assert!(third.iter().all(|s| *s == 0.0), "silence after it again");
    }

    #[test]
    fn two_sessions_overlapping_are_summed() {
        // Rare but real: a Spotify stream that has not stopped when a cast starts. The
        // last writer winning would silence one of them at random.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        mix.add(t0, &tone(480, 0.25));
        mix.add(t0, &tone(480, 0.5));
        let out = mix.take(t0 + Duration::from_secs(1), 480, SETTLE).unwrap();
        assert!(
            out.iter().all(|s| (*s - 0.75).abs() < 1e-6),
            "{:?}",
            &out[..4]
        );
    }

    #[test]
    fn nothing_is_taken_until_it_has_settled() {
        // The reader must stay behind the writers, or a block that arrives a few
        // milliseconds late finds its position already consumed.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        assert!(mix
            .take(t0 + Duration::from_millis(50), 4800, SETTLE)
            .is_none());
        assert!(mix
            .take(t0 + Duration::from_millis(250), 4800, SETTLE)
            .is_some());
    }

    #[test]
    fn audio_arriving_before_the_first_frame_is_dropped_rather_than_stacked_at_zero() {
        // The timeline is anchored by the *video*. A session already playing when somebody
        // opens the stream would otherwise pile its backlog onto position zero.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        mix.add(t0, &tone(48_000, 1.0));
        timeline.anchor(t0);
        let out = mix.take(t0 + Duration::from_secs(1), 4800, SETTLE).unwrap();
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn a_block_that_is_partly_too_late_keeps_the_part_that_is_not() {
        // Losing its head is right; shifting it later would push everything after it out
        // of step for the rest of the session.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        // Drain the first 4800 frames, so the window now starts at 100 ms.
        mix.take(t0 + Duration::from_secs(1), 4800, SETTLE).unwrap();
        assert_eq!(mix.position(), 4800);
        // A block claiming to start at 50 ms: half of it is already gone.
        mix.add(t0 + Duration::from_millis(50), &tone(4800, 1.0));
        let out = mix.take(t0 + Duration::from_secs(1), 4800, SETTLE).unwrap();
        assert!(
            out[..10].iter().all(|s| (*s - 1.0).abs() < 1e-6),
            "the surviving half"
        );
        assert!(
            out[2 * 2400..].iter().all(|s| *s == 0.0),
            "and silence after it"
        );
    }

    #[test]
    fn a_reader_that_stopped_does_not_grow_the_window_without_bound() {
        // If the encode thread dies, writers keep writing. The alternative to discarding
        // is a `Vec` that grows until the panel is killed by the OOM reaper.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        for second in 0..30 {
            mix.add(t0 + Duration::from_secs(second), &tone(48_000, 0.1));
        }
        let held = mix.inner.lock().unwrap().samples.len();
        assert!(
            held <= 48_000 * 2 * 5,
            "held {held} samples, which is more than the cap"
        );
        assert!(
            mix.position() > 0,
            "the window moved on rather than stalling"
        );
    }

    #[test]
    fn a_source_that_writes_faster_than_real_time_is_still_laid_out_in_real_time() {
        // The failure this exists for: a box with no audio device gets a `NullAudioOut`,
        // which accepts a block instantly, so the decoder races through a file as fast as
        // it can read it. Placing blocks by arrival then puts a minute of audio into the
        // first second — which was exactly what the first run against a real panel
        // produced.
        //
        // This module used to defend against that itself, with a per-session cursor, a
        // resync threshold and a lead cap, because there was nothing else that could: each
        // session wrote to its own device and nothing in this process ran at the panel's
        // pace. Since #111 the mixer does, so the defence is structural — a block arrives
        // here when the speakers got it, and a source that races is blocked by the mixer
        // long before this code sees it. What is pinned here is that end-to-end property,
        // through the real mixer, because it is the reason the cursor could be deleted.
        let mixer = crate::mixer::AudioMixer::new(Arc::new(|| {
            Box::new(crate::audio_out::NullAudioOut::new())
        }));
        let stream = StreamAudio::new();
        mixer.add_tap(stream.tap());
        let t0 = Instant::now();
        stream.timeline().anchor(t0);

        let mut input = mixer.input();
        // A second of audio, written as fast as the loop goes.
        for _ in 0..10 {
            input
                .write(&castaway_core::PcmFrame {
                    sample_rate: RATE,
                    channels: CHANNELS,
                    samples: vec![0.5; 4800 * usize::from(CHANNELS)],
                    pts: Duration::ZERO,
                })
                .unwrap();
        }
        // Let the last of it drain out of the in-flight budget.
        std::thread::sleep(Duration::from_millis(400));

        // A second of audio should occupy a second of timeline. Measured over two, so the
        // answer is a proportion rather than an edge: stacked blocks would put everything
        // in a fraction of the first second and leave the rest silent.
        let out = stream
            .mix()
            .take(t0 + Duration::from_secs(3), 96_000, SETTLE)
            .unwrap();
        let loud = out.iter().filter(|s| s.abs() > 1e-6).count();
        let fraction = loud as f64 / out.len() as f64;
        assert!(
            (0.40..=0.60).contains(&fraction),
            "{:.1}% of two seconds carries audio; one second of blocks should be half",
            fraction * 100.0
        );
    }
}
