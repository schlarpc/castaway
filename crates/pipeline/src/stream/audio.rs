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
//! block belongs after the one before it and nothing here has to work out where.
//!
//! ## The mixer lays the samples out; the clock only says how long the track must be
//!
//! The stream has to produce a continuous 48 kHz timeline whether or not anything is
//! playing — a silent panel is the *normal* case, and an audio track that simply stops is
//! one a player stalls on rather than one it treats as quiet. So the two questions this
//! module answers are kept apart, because conflating them is #208:
//!
//! - **Where does a block go?** After the last one. The mixer is this mix's only writer
//!   and its passes abut ([`MixTap`]), so the answer needs no clock at all and the write
//!   is an append. It used to be an indexed sum against `t - origin`, which addressed one
//!   stream as though it were several: a burst of passes sharing an instant — what a
//!   freshly opened sink produces every single time — was summed on top of itself, and
//!   everything but the first quantum of it was destroyed. Nothing counted that, because
//!   nothing was dropped.
//! - **How long must the track be?** As long as the video, which is what the shared
//!   [`Timeline`] is for. That is asked in [`AudioMix::take`], on the way out, and where
//!   the mixer has not produced enough the shortfall is filled with silence and counted
//!   ([`AudioMix::invented`]).
//!
//! The clock still bounds the write in one direction: audio whose position the reader has
//! already passed cannot be un-emitted, so it is clipped rather than allowed to shift
//! everything after it ([`AudioMix::clipped`]). In steady state neither correction fires —
//! the mixer paces to the device queue, so its stream runs `DEVICE_LEAD` *ahead* of the
//! wall clock and that lead is the hysteresis.
//!
//! Video slots are derived from the same origin, so the two tracks cannot drift apart no
//! matter how long the stream runs. The fill in `take` is also why a tap does not keep the
//! audio device open: an idle panel holds no sink, the mixer parks, and the stream stays
//! continuous regardless.
//!
//! When the video gives up on a long stall — [`super::cadence::Cadence`] rebasing the
//! shared timeline — a span of wall time is deleted, and the sound played in that span is
//! deleted with it ([`Window::reconcile`]). The sound before it stays under the frames
//! the video papered over; the sound after it lands against the new clock with no seam.
//! What a rebase costs the audio track is counted, not silent: [`AudioMix::rebase_discarded`]
//! and [`AudioMix::clipped`].
//!
//! ## What this is not
//!
//! It is not sample-accurate. A block is placed where it was *handed to the device*, and
//! the device plays it some tens of milliseconds later — so the stream's audio leads the
//! panel's own speakers by roughly the output queue's depth. Relative to the stream's own
//! video, which is captured at the moment it is composited, that is the same relationship
//! the panel itself has, which is what matters for watching it. Sub-frame lip sync is not
//! on offer and the readback path could not deliver it anyway.
//!
//! [`MixTap`]: crate::mixer::MixTap

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::timeline::{Reading, Timeline};
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
    /// Locked directly only by the clock-free accessors — the counters, [`Self::position`]
    /// and [`Self::clear`]. Anything holding a [`Reading`] goes through [`Self::window`],
    /// which is what keeps a stored position from being addressed by a clock it has not
    /// caught up with.
    inner: Mutex<Window>,
}

#[derive(Debug)]
struct Window {
    /// Absolute frame position of the first frame in `samples`.
    ///
    /// Two quantities that agree by construction: the count of frames handed to the
    /// encoder — the track position sound taken next will land at — and the timeline
    /// label of the window's front. A rebase is the one event that could split them,
    /// which is what [`Self::reconcile`] is for; `base` itself never moves backwards,
    /// because the frames it counts cannot be unsent.
    base: u64,
    /// The timeline's rebase total this window last saw. When a fresh [`Reading`]
    /// disagrees, a rebase has deleted a span of wall time since the last touch, and
    /// whatever is held past the new present was played inside it.
    reconciled: Duration,
    /// Interleaved stereo at [`RATE`], in the order the mixer produced it. Contiguous by
    /// construction: appended at one end, drained at the other, never indexed.
    samples: VecDeque<f32>,
    /// Frames rebases have deleted, along with the span of timeline they were played in.
    rebase_discarded: u64,
    /// Frames clipped off the front of arriving blocks because the reader had already
    /// passed their positions — the catch-up after a rebase that deleted time the encoder
    /// had been fed, or a mixer that fell far enough behind for `take` to fill for it.
    clipped: u64,
    /// Frames of silence the *track* needed and nothing played: the mixer produced less
    /// than the timeline says has elapsed, so [`AudioMix::take`] filled for it.
    ///
    /// #175 at this boundary. Sustained nonzero means the mixer thread is not keeping up
    /// or is parked while the stream is live, and it is now the difference between "the
    /// panel was quiet" and "no one produced" — which an aggregate `starved` on the mixer
    /// cannot express, because it counts a source failing an input, not the mix failing
    /// the track.
    invented: u64,
}

impl Window {
    /// Catch up with a rebase: the timeline deleted a span of wall time ending at the
    /// present, so the sound played in that span — everything held past `at`, the
    /// present under the new clock — goes with it, exactly as its video slots did.
    ///
    /// What is *kept* keeps its place twice over: sound before the cut sits under the
    /// slots the video papered over, and blocks arriving after the rebase land against
    /// the new clock right where the cut left off. `base` does not move — the track the
    /// encoder is building stays contiguous, and the label and the count still agree.
    /// The exception is a rebase that deletes time already taken (`base` past `at`,
    /// possible only when the encoder kept draining through the stall): those frames
    /// cannot be unsent, so arriving sound is clipped by [`AudioMix::add`] until the
    /// clock reaches the count again — sync is kept, the loss is bounded by the overrun
    /// rather than by the rebase, and `clipped` says what it cost.
    fn reconcile(&mut self, at: u64, rebased: Duration) {
        if rebased == self.reconciled {
            return;
        }
        self.reconciled = rebased;
        let keep = usize::try_from(at.saturating_sub(self.base))
            .unwrap_or(usize::MAX)
            .saturating_mul(usize::from(CHANNELS));
        if self.samples.len() > keep {
            let dropped = (self.samples.len() - keep) / usize::from(CHANNELS);
            self.rebase_discarded += dropped as u64;
            self.samples.truncate(keep);
            tracing::debug!(
                frames = dropped,
                "a rebase deleted the span this sound was played in"
            );
        }
    }
}

impl AudioMix {
    /// A mix on `timeline`.
    #[must_use]
    pub fn new(timeline: Arc<Timeline>) -> Self {
        Self {
            timeline,
            inner: Mutex::new(Window {
                base: 0,
                reconciled: Duration::ZERO,
                samples: VecDeque::new(),
                rebase_discarded: 0,
                clipped: 0,
                invented: 0,
            }),
        }
    }

    /// How many frames of [`RATE`] audio fit in `elapsed`.
    fn frames_at(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_nanos() * u128::from(RATE) / 1_000_000_000).unwrap_or(u64::MAX)
    }

    /// The window, already caught up with `reading`, and the present in frames.
    ///
    /// The one door for any access that addresses the window by timeline position. #208
    /// was an accessor doing exactly that with stored state a rebase had left behind, and
    /// an accessor that *remembered to reconcile first* would only be the same bug waiting
    /// on the next author — so the door reconciles, rather than handing the guard and the
    /// obligation separately. An accessor built the obvious way — take a [`Reading`], ask
    /// for the window — cannot see pre-rebase state; recreating #208 now requires locking
    /// `inner` directly *and* computing positions on the side, which no longer has a
    /// shorter spelling than the correct thing.
    ///
    /// The cut lands at the present, which is at least `settle` ahead of anything
    /// [`Self::take`] hands out — a rebase never truncates sound a take was about to.
    fn window(&self, reading: Reading) -> Option<(MutexGuard<'_, Window>, u64)> {
        let at = Self::frames_at(reading.elapsed);
        let mut window = self.inner.lock().ok()?;
        window.reconcile(at, reading.rebased);
        Some((window, at))
    }

    /// Append the mixer's next block of interleaved stereo, which it ran at `now`.
    ///
    /// Silently does nothing before the timeline is anchored: audio that arrives ahead of
    /// the first composited frame has nowhere on the timeline to go, and placing it at
    /// position zero would put sound under a picture that had not been drawn.
    pub fn add(&self, now: Instant, stereo: &[f32]) {
        let Some(reading) = self.timeline.read(now) else {
            return;
        };
        let Some((mut window, at)) = self.window(reading) else {
            return;
        };
        let channels = usize::from(CHANNELS);
        // Part of this block belongs to positions the reader has already handed to the
        // encoder — a rebase that deleted time already encoded, or a stall long enough
        // that `take` filled the span in. Those frames cannot be un-emitted, so sync wins
        // over content: the block loses its head rather than shifting everything after it.
        //
        // Against `base` and not against the end of the window, because everything still
        // *held* is this same stream and belongs after what precedes it. That distinction
        // is the whole fix: measuring the overlap against the window's end instead turns
        // a burst of passes — one stream, arriving fast — into a stream summed onto
        // itself (#208).
        let skip = usize::try_from(window.base.saturating_sub(at)).unwrap_or(usize::MAX);
        if skip > 0 {
            window.clipped += (skip as u64).min((stereo.len() / channels) as u64);
        }
        let Some(tail) = stereo.get(skip.saturating_mul(channels)..) else {
            return;
        };
        // And then it simply follows the last block. No index, so no way to express two
        // blocks landing on top of each other.
        window.samples.extend(tail.iter().copied());
        // `base` counts frames and `samples` holds interleaved ones, so the trim has to
        // drop a whole frame at a time. Popping one sample per `base += 1` advanced the
        // label at twice the rate for stereo: what was still in the window came back
        // relabelled half a trim late, and once pinned at cap the base outran the wall
        // clock — heads of live blocks lost to `skip`, and `take` refusing until the clock
        // caught up. It lasted until the next `restart()`.
        let cap = usize::try_from(Self::frames_at(MAX_BUFFERED)).unwrap_or(usize::MAX) * channels;
        while window.samples.len() > cap {
            for _ in 0..channels {
                window.samples.pop_front();
            }
            window.base += 1;
            window.clipped += 1;
        }
    }

    /// Take `frames` frames from the front, if they are far enough in the past to be
    /// settled.
    ///
    /// `settle` is how far behind the wall clock the reader stays, so that ordinary
    /// jitter in the mixer's pace is absorbed rather than filled in. Filling is a ratchet:
    /// silence handed to the encoder cannot be taken back, so the audio it stands in for
    /// arrives to find its place gone (`clipped`). The knob trades the stream's audio
    /// latency against how much of a stalled mixer's output is lost that way.
    pub fn take(&self, now: Instant, frames: usize, settle: Duration) -> Option<Vec<f32>> {
        let reading = self.timeline.read(now)?;
        let settled = Self::frames_at(reading.elapsed.checked_sub(settle)?);
        let (mut window, _at) = self.window(reading)?;
        if window.base + frames as u64 > settled {
            return None;
        }
        let channels = usize::from(CHANNELS);
        let wanted = frames * channels;
        // The track has to be as long as the video whatever the mixer managed, so a
        // shortfall is filled here — and *only* here, which is what keeps the write side
        // free of the clock. Nothing produced this sound, so it is counted rather than
        // left to look like a quiet panel (#175).
        if window.samples.len() < wanted {
            window.invented += ((wanted - window.samples.len()) / channels) as u64;
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

    /// Frames of held sound that rebases have deleted, along with the span of timeline
    /// they were played in. A running total for the life of the mix.
    ///
    /// This is video's `resynced` warning as audio sees it — the deliberate cost of a
    /// rebase, where [`Self::clipped`] is a loss. Left uncounted, it spent months being
    /// read as an encoder fault (#208, #175).
    #[must_use]
    pub fn rebase_discarded(&self) -> u64 {
        self.inner.lock().map_or(0, |w| w.rebase_discarded)
    }

    /// Frames clipped off the front of arriving blocks because the reader had already
    /// passed their positions. A running total for the life of the mix.
    ///
    /// Nonzero means sound was lost: a rebase deleting time the encoder had already been
    /// fed, or a mixer stalled long enough that [`Self::take`] filled the span in first.
    #[must_use]
    pub fn clipped(&self) -> u64 {
        self.inner.lock().map_or(0, |w| w.clipped)
    }

    /// Frames of silence [`Self::take`] filled in because the mixer had not produced that
    /// far. A running total for the life of the mix.
    ///
    /// The counter #175 asked for, at the boundary where the answer exists. `starved` on
    /// the mixer says a *source* failed to feed an input; this says the *mix* failed to
    /// fill the track, which is the number that decides whether audio arriving late is
    /// the panel's fault or the stream's. Zero on a quiet panel: silence nobody played is
    /// still silence the mixer produced, and it arrives here as samples.
    #[must_use]
    pub fn invented(&self) -> u64 {
        self.inner.lock().map_or(0, |w| w.invented)
    }

    /// Frames held but not yet taken: how far the mixer's stream is running ahead of the
    /// encoder. For diagnostics — a standing value well past `settle` is a device whose
    /// clock is faster than nominal, which shows up nowhere else.
    #[must_use]
    pub fn held(&self) -> u64 {
        self.inner
            .lock()
            .map_or(0, |w| w.samples.len() as u64 / u64::from(CHANNELS))
    }

    /// Throw away everything held and go back to position zero.
    ///
    /// Paired with [`Timeline::reset`] when a stream restarts: what is in the window
    /// belongs to the old origin, and keeping it would put the previous run's last second
    /// of sound at the start of the new one — which is also why the reconciled rebase
    /// total starts over, while the loss counters keep counting across presentations.
    pub fn clear(&self) {
        if let Ok(mut window) = self.inner.lock() {
            window.base = 0;
            window.reconciled = Duration::ZERO;
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
    fn a_block_lands_where_the_mixer_played_it() {
        // The mixer is one writer producing one continuous stream, so a quiet panel is
        // not "nothing written" — it is silence, delivered as samples like anything else.
        // What has to hold is that the tone occupies the tenth 10 ms of the track because
        // it was the tenth 10 ms the mixer produced.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        for i in 0..10u32 {
            mix.add(t0 + Duration::from_millis(10) * i, &tone(480, 0.0));
        }
        mix.add(t0 + Duration::from_millis(100), &tone(480, 0.5));

        let now = t0 + Duration::from_secs(1);
        let first = mix.take(now, 4800, SETTLE).unwrap();
        assert!(first.iter().all(|s| *s == 0.0), "nothing before 100 ms");
        let second = mix.take(now, 480, SETTLE).unwrap();
        assert!(second.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        // Nothing was written past the tone, so the track is filled out — and said so.
        let third = mix.take(now, 480, SETTLE).unwrap();
        assert!(third.iter().all(|s| *s == 0.0), "silence after it again");
        assert_eq!(mix.invented(), 480, "the fill is counted, not silent");
    }

    #[test]
    fn a_burst_of_passes_is_one_stream_and_not_several() {
        // #208, in the shape that actually reaches the panel. A freshly opened sink has
        // an empty queue, so the mixer runs passes back to back until it is full — a
        // whole `DEVICE_LEAD` of audio inside one wall millisecond, measured at 60 ms
        // inside 100 µs, and it happens at the head of *every* presentation.
        //
        // Placing those blocks by the instant the pass ran summed them on top of each
        // other: six passes into one quantum, fifty of the sixty milliseconds destroyed,
        // and every loss counter rightly zero because nothing was dropped. Each pass
        // carries its index here, so a summed track cannot be mistaken for an ordered one.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        for i in 0..6u16 {
            // A hundred microseconds apart: five frames of clock for 480 of audio.
            mix.add(
                t0 + Duration::from_micros(100) * u32::from(i),
                &tone(480, f32::from(i) + 1.0),
            );
        }
        let out = mix.take(t0 + Duration::from_secs(1), 2880, SETTLE).unwrap();
        for i in 0..6usize {
            let got = out[i * 480 * 2];
            let want = i as f32 + 1.0;
            assert!(
                (got - want).abs() < 1e-6,
                "pass {i} should own its own 10 ms; found {got} where {want} belongs"
            );
        }
        assert_eq!(mix.clipped(), 0, "a burst is early, not late");
        assert_eq!(mix.invented(), 0, "and nothing had to be filled in");
    }

    #[test]
    fn what_two_sessions_playing_at_once_sound_like_is_the_mixers_business() {
        // This used to be asserted here, by writing two blocks at one instant and
        // expecting their sum. That was the tee's world: before #111 every session had
        // its own device and this module reconstructed the panel's output, so it really
        // did have several writers. It has had exactly one since — `mix_pass` sums the
        // sessions and hands over the result — and keeping the summation on this side is
        // what let a burst of passes be read as overlapping sources (#208).
        let mixer = crate::mixer::AudioMixer::new(Arc::new(|| {
            Box::new(crate::audio_out::NullAudioOut::new())
        }));
        let stream = StreamAudio::new();
        mixer.add_tap(stream.tap());
        let t0 = Instant::now();
        stream.timeline().anchor(t0);

        let block = |value: f32| castaway_core::PcmFrame {
            sample_rate: RATE,
            channels: CHANNELS,
            samples: vec![value; 4800 * usize::from(CHANNELS)],
            pts: Duration::ZERO,
        };
        let mut spotify = mixer.input(crate::mixer::Backpressure::Pull);
        let mut cast = mixer.input(crate::mixer::Backpressure::Pull);
        for _ in 0..4 {
            spotify.write(&block(0.25)).unwrap();
            cast.write(&block(0.5)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(500));

        // A whole second, not the first quantum: the two writers cannot arrange "at the
        // same time" (the same race the volume test in `audio_session` had), so the head
        // of the track may legitimately carry one source alone. What must hold is that
        // the overlap exists *somewhere* — each source wrote 400 ms within milliseconds
        // of the other — and a peak over the second finds it wherever it landed.
        let out = stream
            .mix()
            .take(t0 + Duration::from_secs(2), 48_000, SETTLE)
            .unwrap();
        let loudest = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!(
            (loudest - 0.75).abs() < 1e-6,
            "the panel plays both at once: peak {loudest}"
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
    fn a_rebase_deletes_the_stalls_own_sound_and_nothing_after_it() {
        // #208/#175, the shared root cause. The cadence gives up on a long stall by
        // rebasing the shared timeline; the window's stored position did not move, so it
        // was left in the future of the clock that addressed it — every block arriving
        // next was discarded whole and the encoder was refused until the wall clock
        // caught up. A hole the length of the rebase, landing on whatever played *after*
        // the stall: the lip-sync shift when it clipped a tone, the peak-of-0 track when
        // it swallowed one. The sound a rebase may delete is the stall's own — the same
        // span the video deleted — and nothing else.
        //
        // Every 100 ms block carries its index as a constant sample value. Blocks are
        // written at disjoint instants, so nothing sums, and the value read back says
        // which block a track position landed in — a marker `add`'s summation cannot
        // forge, which is what makes asserting exact positions sound here.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        let block = Duration::from_millis(100);
        // A session plays continuously through the stall: wall 0 s..2.0 s.
        for i in 0..20u16 {
            mix.add(t0 + block * u32::from(i), &tone(4800, f32::from(i) + 1.0));
        }
        // The encoder keeps pace until the pump stalls at 0.5 s: emitted through 0.4 s.
        mix.take(t0 + block * 5, 4800 * 4, SETTLE)
            .expect("settled well before the stall");
        assert_eq!(mix.position(), 19_200);
        // At 2.0 s the pump comes back. The video papers half a second of duplicates and
        // rebases away the other whole second, exactly as `Cadence::take` would.
        timeline.rebase(Duration::from_secs(1));
        // The session plays on: wall 2.0 s..2.5 s, which the new clock calls 1.0 s..1.5 s.
        for i in 0..5u16 {
            mix.add(
                t0 + Duration::from_secs(2) + block * u32::from(i),
                &tone(4800, f32::from(i) + 21.0),
            );
        }
        // Everything settled comes out in one go: track frames 19 200..72 000.
        let out = mix
            .take(t0 + Duration::from_millis(2600), 52_800, SETTLE)
            .expect("a rebase must not make the mix refuse the encoder");
        let frame = |n: usize| out[n * 2];
        // The session never stopped playing, so nowhere in the track is silence.
        assert!(out.iter().all(|s| *s != 0.0), "no invented silence");
        // The papered half second and the sound leading into the stall keep their place:
        // wall 0.4 s..1.0 s under the duplicated frames.
        assert!((frame(0) - 5.0).abs() < 1e-6, "track 19 200 is wall 0.4 s");
        assert!((frame(28_799) - 10.0).abs() < 1e-6, "up to wall 1.0 s");
        // The deleted second — wall 1.0 s..2.0 s, played mid-stall — is gone whole, and
        // counted rather than mistaken for an encoder fault.
        assert_eq!(mix.rebase_discarded(), 48_000);
        // What played after the stall lands exactly where the rebased clock says, with
        // no seam: wall 2.0 s is the new clock's 1.0 s, track frame 48 000.
        assert!((frame(28_800) - 21.0).abs() < 1e-6, "wall 2.0 s, in sync");
        assert!((frame(43_200) - 24.0).abs() < 1e-6, "wall 2.3 s, mid-span");
        assert!((frame(52_799) - 25.0).abs() < 1e-6, "wall 2.5 s");
        // The encoder never ran into the deleted span, so nothing arriving was clipped.
        assert_eq!(mix.clipped(), 0);
    }

    #[test]
    fn a_rebase_past_what_was_already_encoded_costs_the_overrun_and_no_more() {
        // The other topology, from #208's measured trace: under load the encoder kept
        // draining right through the stall, so its count is already inside the span the
        // rebase deletes. Those frames cannot be unsent. Sync wins over content — the
        // same length of *arriving* sound is clipped, and counted, instead of the track
        // drifting by the rebase for the rest of the run. The loss is bounded by the
        // overrun; before this it was the whole rebase, plus a refusal as long again.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        let block = Duration::from_millis(100);
        // Wall 0 s..1.0 s, playing continuously.
        for i in 0..10u16 {
            mix.add(t0 + block * u32::from(i), &tone(4800, f32::from(i) + 1.0));
        }
        // The encoder is right at the settle edge when the rebase lands: taken to 0.9 s.
        mix.take(t0 + Duration::from_secs(1), 43_200, SETTLE)
            .expect("settled");
        assert_eq!(mix.position(), 43_200);
        // The rebase deletes wall 0.5 s..1.0 s — the encoder is 0.4 s inside it.
        timeline.rebase(Duration::from_millis(500));
        // The session plays on: wall 1.0 s..1.6 s, new clock 0.5 s..1.1 s.
        for i in 0..6u16 {
            mix.add(
                t0 + Duration::from_secs(1) + block * u32::from(i),
                &tone(4800, f32::from(i) + 21.0),
            );
        }
        // What was held at the rebase — wall 0.9 s..1.0 s — was inside the deleted span:
        // gone, and counted.
        assert_eq!(mix.rebase_discarded(), 4_800);
        // Arriving blocks lose their heads only up to the encoder's count: 0.4 s, the
        // overrun exactly, not the 0.5 s of the rebase.
        assert_eq!(mix.clipped(), 19_200);
        // And the track resumes in sync at the settle edge, not after a refusal the
        // length of the rebase: wall 1.4 s..1.6 s is the new clock's 0.9 s..1.1 s, which
        // is track frames 43 200..52 800 — no gap, no drift.
        let out = mix
            .take(t0 + Duration::from_millis(1700), 9_600, SETTLE)
            .expect("resumes as soon as sound has settled");
        assert!(out.iter().all(|s| *s != 0.0), "no invented silence");
        assert!((out[0] - 25.0).abs() < 1e-6, "wall 1.4 s");
        assert!((out[(9_600 - 1) * 2] - 26.0).abs() < 1e-6, "wall 1.6 s");
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

        let mut input = mixer.input(crate::mixer::Backpressure::Pull);
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
