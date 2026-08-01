//! Tapping the panel's sound.
//!
//! ## There is no mixer to tap
//!
//! The panel has no central audio mixer, on purpose: [`AudioOutputFactory`] hands *each
//! session its own device*, because two sessions writing to one device fight rather than
//! mix, and the OS is the thing that mixes. So "what the panel is playing" does not exist
//! anywhere in this process as a single stream.
//!
//! What does exist is the factory. Every session's audio — Cast, AirPlay, DLNA, Spotify,
//! Bluetooth, and the browser's captured page audio — is written to a `Box<dyn AudioOut>`
//! that came from it, and the app installs exactly one. So [`tee`] wraps the factory, and
//! every output it produces writes to the real device *and* into [`AudioMix`]. Nothing
//! else in the audio path changes, and a session cannot be added later that misses this
//! without also missing its own sound.
//!
//! ## The mix is indexed by wall clock, not by arrival
//!
//! A queue would drift. Sessions start and stop, sample rates differ, and the stream has
//! to produce a continuous 48 kHz timeline whether or not anything is playing — a silent
//! panel is the *normal* case, and an audio track that simply stops is one a player
//! stalls on rather than one it treats as quiet.
//!
//! So the mix is a window of the shared [`Timeline`], addressed by absolute sample
//! position. A block written at instant *t* lands at position `t - origin`; anything
//! nobody wrote is silence because the buffer is zeroed; two sessions overlapping sum.
//! Video slots are derived from the same origin, so the two tracks cannot drift apart no
//! matter how long the stream runs.
//!
//! ## What this is not
//!
//! It is not sample-accurate. A session's first block is placed where it was *handed to
//! the device*, and the device plays it some tens of milliseconds later — so the stream's
//! audio leads the panel's own speakers by roughly the output queue's depth. Relative to
//! the stream's own video, which is captured at the moment it is composited, that is the
//! same relationship the panel itself has, which is what matters for watching it.
//! Sub-frame lip sync is not on offer and the readback path could not deliver it anyway.
//!
//! ## Sessions are followed, not sampled
//!
//! Each session carries a cursor — its first block's instant plus the duration of every
//! block since — and blocks are placed at *that*, not at the instant they arrive. It
//! matters because "the session writes in real time" is only true when something is
//! consuming in real time: on a box with a real device the queue blocks and paces the
//! decoder, and on one with a null sink the decoder races through a file as fast as it can
//! read it. Placing by arrival puts a whole file's audio in the first second there.
//!
//! The cursor is snapped back to the clock when it diverges by more than
//! [`RESYNC`] — a session that stalled, or a sender whose clock is not ours running for
//! long enough to matter.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::timeline::Timeline;
use crate::audio_decode::PcmBlock;
use crate::audio_out::{AudioOut, AudioOutputFactory};
use crate::error::PipelineError;
use crate::resample::Resampler;

/// The rate the stream's audio track runs at.
///
/// 48 kHz because it is what every source in this box ends up at, what AAC encoders are
/// happiest with, and what browsers output — so the common case resamples nothing.
pub const RATE: u32 = 48_000;

/// Stereo. The panel is a display with speakers, and an HLS duplicate of it in a browser
/// tab is not the place to carry a surround mix.
pub const CHANNELS: u16 = 2;

/// How much sound the mix will hold before it starts discarding the oldest.
///
/// A backstop, not a buffer: the encode loop drains this several times a second. It
/// matters only if that loop dies, and then the alternative is a `Vec` that grows until
/// the panel is killed by the OOM reaper — which is a much worse way to lose audio.
const MAX_BUFFERED: Duration = Duration::from_secs(4);

/// How far a session's cursor may fall *behind* the wall clock before it is snapped back.
///
/// Large enough that ordinary jitter — a decode thread descheduled, a device queue
/// refilling in bursts — is followed rather than fought, and small enough that a stall or
/// a genuinely different clock is corrected before anyone hears it as lip-sync error.
///
/// Deliberately one-sided. A cursor *ahead* of the clock is a session that has produced
/// audio it has not played yet, which is what every source with a buffer does and is
/// exactly right; a cursor behind it is audio being laid down where the encoder has
/// already been, which is silently lost.
const RESYNC: Duration = Duration::from_millis(250);

/// How far ahead of the clock a session may run before its blocks are dropped.
///
/// A racing session — a file decoded through a null sink, which accepts every block
/// instantly — will otherwise produce hours of audio in seconds, and the mix cannot hold
/// it. Dropping is the honest answer for a live duplicate: this is a mirror of what the
/// panel is playing *now*, not a player with a queue.
const MAX_LEAD: Duration = Duration::from_secs(3);

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
        let cap = usize::try_from(Self::frames_at(MAX_BUFFERED)).unwrap_or(usize::MAX) * channels;
        while window.samples.len() > cap {
            window.samples.pop_front();
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

    /// Wrap `inner` so every session it produces is also mixed here.
    #[must_use]
    pub fn factory(&self, inner: AudioOutputFactory) -> AudioOutputFactory {
        tee(inner, Arc::clone(&self.mix))
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

/// Wrap an audio factory so everything it produces is also mixed into `mix`.
///
/// The returned factory is a drop-in for the one it wraps: same type, same behaviour on
/// the device, and a session cannot tell the difference.
#[must_use]
pub fn tee(inner: AudioOutputFactory, mix: Arc<AudioMix>) -> AudioOutputFactory {
    Arc::new(move || {
        Box::new(TeeAudioOut {
            inner: inner(),
            mix: Arc::clone(&mix),
            convert: None,
            cursor: None,
        })
    })
}

/// One session's output, going two places.
struct TeeAudioOut {
    inner: Box<dyn AudioOut>,
    mix: Arc<AudioMix>,
    /// Set by `start`, which is where the session's shape is finally known. `None` means
    /// this session's audio is not being mixed — it still plays.
    convert: Option<Convert>,
    /// Where this session's next block begins. `None` before its first, and reset by
    /// `start`, so a session that restarts is followed from wherever it restarted.
    cursor: Option<Instant>,
}

/// What it takes to get one session's blocks to [`RATE`] stereo.
struct Convert {
    channels: u16,
    /// Absent when the session is already at [`RATE`].
    resampler: Option<Resampler>,
}

impl Convert {
    fn open(sample_rate: u32, channels: u16) -> Result<Self, PipelineError> {
        let resampler = if sample_rate == RATE {
            None
        } else {
            Some(Resampler::new(sample_rate, RATE, channels.max(1))?)
        };
        Ok(Self {
            channels: channels.max(1),
            resampler,
        })
    }

    /// One block as interleaved [`RATE`] stereo.
    fn stereo(&mut self, block: &PcmBlock) -> Result<Vec<f32>, PipelineError> {
        let resampled = match self.resampler.as_mut() {
            Some(resampler) => resampler.convert(block)?,
            None => block.samples.clone(),
        };
        Ok(to_stereo(&resampled, self.channels))
    }
}

/// Interleaved `channels`-channel audio as interleaved stereo.
///
/// Mono is duplicated. Anything wider than stereo keeps its first two channels, which in
/// every standard layout are front left and front right: a proper downmix would need the
/// layout and a set of coefficients, and what it would buy is a centre channel in a
/// browser tab's duplicate of a wall panel.
fn to_stereo(samples: &[f32], channels: u16) -> Vec<f32> {
    let channels = usize::from(channels.max(1));
    match channels {
        2 => samples.to_vec(),
        1 => samples.iter().flat_map(|s| [*s, *s]).collect(),
        n => samples
            .chunks_exact(n)
            .flat_map(|frame| [frame[0], frame[1]])
            .collect(),
    }
}

/// Where a session's next block goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Here, on the wall clock.
    At(Instant),
    /// Nowhere: the session is running further ahead than the mix can hold.
    Drop,
}

/// Follow a session's cursor, correcting it where it has stopped meaning anything.
fn follow(cursor: Option<Instant>, now: Instant) -> Place {
    let Some(cursor) = cursor else {
        return Place::At(now);
    };
    if now.saturating_duration_since(cursor) > RESYNC {
        // Behind: whatever happened, carrying on from here would write into the past.
        return Place::At(now);
    }
    if cursor.saturating_duration_since(now) > MAX_LEAD {
        return Place::Drop;
    }
    Place::At(cursor)
}

impl AudioOut for TeeAudioOut {
    fn start(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError> {
        self.cursor = None;
        self.convert = match Convert::open(sample_rate, channels) {
            Ok(convert) => Some(convert),
            Err(e) => {
                // The session still plays. Losing it from the stream is worth a line and
                // is not worth failing a cast over.
                tracing::warn!(error = %e, sample_rate, channels, "this session will not reach the output stream");
                None
            }
        };
        self.inner.start(sample_rate, channels)
    }

    fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
        // Mixed before the device write, which may block on a full queue: the instant this
        // block is *placed* should be as close as possible to the instant it was produced.
        if let Some(convert) = self.convert.as_mut() {
            match convert.stereo(block) {
                Ok(stereo) => {
                    if let Place::At(at) = follow(self.cursor, Instant::now()) {
                        self.mix.add(at, &stereo);
                        let frames = stereo.len() / usize::from(CHANNELS);
                        self.cursor = Some(
                            at + Duration::from_nanos(
                                (frames as u64).saturating_mul(1_000_000_000) / u64::from(RATE),
                            ),
                        );
                    }
                }
                Err(e) => tracing::debug!(error = %e, "a block did not reach the output stream"),
            }
        }
        self.inner.write(block)
    }

    fn stop(&mut self) {
        self.cursor = None;
        // Deliberately no drain of the resampler's delay line into the mix. It is ~15 ms
        // held back at the very end of a session, and flushing it would place those frames
        // at the instant of `stop` rather than where they belong — which is worse than
        // losing them.
        self.convert = None;
        self.inner.stop();
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
    fn a_session_that_writes_faster_than_real_time_is_still_laid_out_in_real_time() {
        // The failure this exists for: a box with no audio device gets a `NullAudioOut`,
        // which accepts a block instantly, so the decoder races through a file as fast as
        // it can read it. Placing blocks by arrival puts a minute of audio into the first
        // second — which was exactly what the first run against a real panel produced.
        let (timeline, mix) = mix();
        let t0 = Instant::now();
        timeline.anchor(t0);
        let mix = Arc::new(mix);
        let mut out = tee(
            Arc::new(|| Box::new(crate::audio_out::NullAudioOut::new())),
            Arc::clone(&mix),
        )();
        out.start(RATE, 2).unwrap();
        // Ten blocks of 100 ms, written as fast as the loop goes.
        for _ in 0..10 {
            out.write(&PcmBlock {
                sample_rate: RATE,
                channels: 2,
                samples: vec![0.5; 4800 * 2],
                pts: Duration::ZERO,
            })
            .unwrap();
        }
        // A second of audio should occupy a second of timeline. Measured over two, so the
        // answer is a proportion rather than an edge: stacked blocks would put everything
        // in a fraction of the first second and leave the rest silent.
        let out = mix
            .take(t0 + Duration::from_secs(3), 96_000, SETTLE)
            .unwrap();
        let loud = out.iter().filter(|s| s.abs() > 1e-6).count();
        let fraction = loud as f64 / out.len() as f64;
        assert!(
            (0.45..=0.55).contains(&fraction),
            "{:.1}% of two seconds carries audio; one second of blocks should be half",
            fraction * 100.0
        );
    }

    #[test]
    fn a_session_that_stalls_is_snapped_back_to_the_clock() {
        // A cursor that fell a long way behind — a session descheduled, a sender whose
        // clock is not ours — must not keep laying audio down in the past, where the
        // encoder has already been.
        let now = Instant::now();
        assert_eq!(follow(None, now), Place::At(now));
        assert_eq!(
            follow(Some(now - RESYNC - Duration::from_secs(1)), now),
            Place::At(now)
        );

        // …ordinary jitter is followed rather than fought…
        let jittered = now - Duration::from_millis(20);
        assert_eq!(follow(Some(jittered), now), Place::At(jittered));

        // …a session with a buffer runs ahead, which is what every source does…
        let ahead = now + Duration::from_secs(1);
        assert_eq!(follow(Some(ahead), now), Place::At(ahead));

        // …and one racing through a file faster than real time is dropped rather than
        // buffered, because a live duplicate has nowhere to keep it.
        assert_eq!(
            follow(Some(now + MAX_LEAD + Duration::from_secs(1)), now),
            Place::Drop
        );
    }

    #[test]
    fn mono_is_heard_in_both_ears() {
        assert_eq!(to_stereo(&[0.5, -0.5], 1), vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn stereo_passes_through_untouched() {
        assert_eq!(
            to_stereo(&[0.1, 0.2, 0.3, 0.4], 2),
            vec![0.1, 0.2, 0.3, 0.4]
        );
    }

    #[test]
    fn a_wider_layout_keeps_its_front_pair() {
        // 5.1 interleaves as L R C LFE Ls Rs, so the first two are the ones to keep.
        let frame = [0.1, 0.2, 0.9, 0.9, 0.9, 0.9];
        assert_eq!(to_stereo(&frame, 6), vec![0.1, 0.2]);
    }
}
