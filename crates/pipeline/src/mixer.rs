//! The panel's one audio output: the device, the mix, and the volume (#111).
//!
//! Every source that makes a sound — Cast, AirPlay, DLNA, Spotify, Bluetooth, the
//! browser's captured page audio, and whatever comes next — writes into a [`MixInput`]
//! taken from the single [`AudioMixer`]. The mixer sums them, applies the panel's one
//! [`Gain`], and writes the result to the one device.
//!
//! ## What this replaced, and why the old justification did not hold
//!
//! Until this module, `AudioOutputFactory` handed *each session its own device*, on the
//! stated grounds that "two sessions writing to one device fight rather than mix". That is
//! true of a raw ALSA `hw:` device and of nothing else this project ships: PipeWire gives
//! each stream its own node, WASAPI shared mode mixes, and ALSA through `default`/`dmix`
//! mixes. On every backend here the OS was already doing this job, one layer further out
//! and somewhere we could not see it.
//!
//! The design was really carried by policy — the panel is single-source, and `stop`
//! preempts one session when another starts — not by the hazard it named. Meanwhile the
//! codebase already disagreed with itself: [`Gain`] was a single shared value applied N
//! times at N sinks, on the argument that "the panel has one pair of speakers, so it has
//! one volume". That is the mixer argument, and it is applied here once, at one sink.
//!
//! ## The in-flight budget is the pacing, and it is the clock's invariant
//!
//! There is no separate pacing step any more. [`MixInput::write`] holds a source to [`LEAD`]
//! of audio in flight, and that is what paces the decode thread behind it — uniformly,
//! instead of per-backend and by accident.
//!
//! *How* it holds it depends on the source, and [`Backpressure`] is that choice. Parking
//! the writer is flow control only if something upstream will wait; against a phone it is
//! not flow control at all, it just moves the loss into the protocol's own queue, where it
//! costs whole encoded packets and seconds of latency instead of a click.
//!
//! "In flight" is deliberately the *sum* of what is sitting in this input's ring and what
//! the mixer has queued at the device but not yet heard:
//!
//! ```text
//! ring_frames + device_inflight  <=  LEAD
//! ```
//!
//! Bounding the sum rather than the ring alone is what preserves the media clock's one
//! invariant. `clock::MediaClock` reads `submitted - OUTPUT_LEAD` to turn what a session
//! has handed over into what a listener has actually heard, so a session may lead the
//! speakers by [`LEAD`] and no more. Bounding only the ring would have added the device's
//! own queue on top, and every media-URL cast would have played its video that much early.
//!
//! ## The device is the mixer's problem, not a session's
//!
//! A session cannot see a device error, because it no longer holds a device. When the sink
//! disappears — the panel sleeps and PipeWire removes the HDMI node (#55), something
//! claims it exclusively, someone changes the selection — the mixer drops it, keeps pacing
//! on a monotonic timer so every source carries on draining rather than stalling, and
//! retries opening every [`REOPEN_AFTER`]. Sound stops and comes back; sessions do not
//! notice, and a Bluetooth session survives the display sleeping.
//!
//! ## Resampling happens once
//!
//! The mix is a fixed [`RATE`]/[`CHANNELS`] `f32` format. Each input converts to it on the
//! way in, and the mixer converts at most once more on the way out if the device will not
//! take it. Before this there were two unconditional conversions on the streaming path:
//! each session converted to its device's rate, and `stream::audio` converted again to
//! 48 kHz to reconstruct a mix the OS had already computed.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use castaway_core::{PcmFrame as PcmBlock, Volume};
use tracing::{info, warn};

use crate::audio_out::{AudioOut, AudioOutputFactory};
use crate::error::PipelineError;
use crate::resample::Resampler;

/// The rate the mix runs at.
///
/// 48 kHz because it is what every source in this box ends up at, what the AAC encoder on
/// the streaming path is happiest with, and what browsers output — so the common case
/// resamples nothing.
pub const RATE: u32 = 48_000;

/// Stereo. The panel is a display with a pair of speakers.
pub const CHANNELS: u16 = 2;

/// How far ahead of the speakers a source may run.
///
/// Shared with [`crate::clock`] rather than restated: the media clock subtracts exactly
/// this to turn what has been submitted into what has been heard, and two copies would
/// drift into lip sync quietly off by the difference.
pub const LEAD: Duration = crate::clock::OUTPUT_LEAD;

/// How much audio the mixer keeps queued at the device.
///
/// The rest of [`LEAD`] lives in the inputs' rings. Big enough that an ordinary scheduling
/// hiccup on the mixer thread does not reach the device callback as a dropout, small
/// enough to leave most of the budget where backpressure can act on it.
const DEVICE_LEAD: Duration = Duration::from_millis(100);

/// The most audio one mixer pass will produce.
///
/// A pass holds each input's lock in turn, so a long one is a long time for a writer to
/// wait; a short one is a syscall per few milliseconds. This is also the granularity at
/// which a newly arrived input starts being heard.
const QUANTUM: Duration = Duration::from_millis(10);

/// How long the mixer sleeps when the device is full enough and there is nothing to do.
const IDLE_POLL: Duration = Duration::from_millis(5);

/// How long the device stays open with no inputs.
///
/// The one genuine virtue of the old per-session design was that an idle panel held no
/// sink, and it was nowhere written down. This preserves it. Long enough that skipping
/// between tracks, or between two casts, does not close and reopen the device.
const IDLE_CLOSE: Duration = Duration::from_secs(5);

/// How often the mixer retries a device it could not open, or that failed.
const REOPEN_AFTER: Duration = Duration::from_secs(1);

/// How much a live input banks before the mixer starts drawing on it.
///
/// The jitter buffer this path did not have (#175). A source arriving in ordinary,
/// well-behaved packets — 8 ms of ALAC, 23 ms of AAC — still delivers in lumps, while the
/// mixer tops the device up whenever it has room, several times per packet. Without a
/// reserve every pass falling between two packets finds the ring empty and emits silence,
/// so the output is real audio and holes interleaved at sub-millisecond granularity. That
/// measured at ~100% of the output on a session the protocol said was arriving perfectly.
///
/// Bigger than the largest packet any live source sends, and small beside [`LEAD`]: this is
/// spent out of the same budget, and #111 deliberately spent that budget on letting a source
/// run *ahead*. Only [`Backpressure::Live`] inputs prime — a pull source is kept full by the
/// writer parking, so it has no jitter to absorb and priming it would only add latency.
const PREBUFFER: Duration = Duration::from_millis(60);

/// How often the mixer says what the speakers did not get.
///
/// Matches the AirPlay diagnostics cadence deliberately, so the two lines can be read
/// against each other: that one says what arrived, this one says what came out.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// How long [`MixInput::write`] waits for room before giving up on a block.
///
/// Reaching this means the mixer is not draining — it is wedged, or the process is
/// shutting down — and a writer parked forever would take the session's stop flag with it,
/// which is how a preempted session used to hold a device it was no longer playing to. A
/// dropped block is audible; a thread that never checks whether it should exit is worse.
const WRITE_DEADLINE: Duration = Duration::from_secs(2);

/// How many frames of mix audio fit in `d`.
fn frames_in(d: Duration) -> u64 {
    u64::try_from(d.as_nanos() * u128::from(RATE) / 1_000_000_000).unwrap_or(u64::MAX)
}

/// Output gain, shared between the mixer thread and whoever holds the remote.
///
/// This exists because a volume command had nowhere to land. AVRCP `SET_ABSOLUTE_VOLUME`
/// was parsed, answered `Accepted`, and emitted as a `ControlTxn` that the pipeline logged
/// and dropped — so a phone's volume rocker did nothing, and a phone that entered
/// absolute-volume mode on the strength of our Target record stopped attenuating locally
/// and pinned playback at full scale.
///
/// Applied at the output stage rather than in each protocol or each session: the panel has
/// one pair of speakers, so it has one volume, and a source-side gain would leave every
/// other source at whatever the last one set. Before #111 that argument was already
/// written down here and then implemented N times at N sinks; now there is one sink and it
/// is applied there.
///
/// Stored as bits in an atomic so the mixer thread never takes a lock — a mutex here would
/// put the remote's contention on the path that must not stall.
#[derive(Debug)]
pub struct Gain {
    level: AtomicU32,
    muted: AtomicBool,
}

impl Default for Gain {
    fn default() -> Self {
        Self {
            level: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
        }
    }
}

impl Gain {
    /// Set the level.
    ///
    /// Takes a [`Volume`] rather than an `f32` because the number a sender sends and the
    /// number this multiplies by are different scales that look identical (#85). The
    /// conversion happened at whichever protocol boundary parsed the wire; by the time it
    /// arrives here there is nothing left to interpret, and no way to hand it a slider
    /// position by accident.
    pub fn set(&self, level: Volume) {
        self.level
            .store(level.amplitude().to_bits(), Ordering::Relaxed);
    }

    /// The current level, as the amplitude samples are multiplied by.
    ///
    /// Deliberately not a [`Volume`]: there is no constructor from a bare amplitude, and
    /// there should not be one. Every sender that needs its slider told where it ended up
    /// keeps its own authoritative copy in its own scale — Cast a position, DLNA a
    /// percent, AirPlay a dBFS figure — so nothing has to reverse the taper to answer.
    #[must_use]
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Mute or unmute without disturbing the level.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// Whether output is muted.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// What every sample should be multiplied by right now.
    fn factor(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            self.level()
        }
    }

    /// Scale interleaved samples in place.
    pub fn apply(&self, samples: &mut [f32]) {
        let factor = self.factor();
        // Unity is the overwhelmingly common case — every source that never touches the
        // volume — and skipping it keeps the whole mechanism free when unused.
        if (factor - 1.0).abs() < f32::EPSILON {
            return;
        }
        for sample in samples {
            *sample *= factor;
        }
    }
}

/// Something that wants a copy of everything the panel plays.
///
/// Fed the exact samples the device was given, at the instant it was given them. The
/// output stream's audio track (#101) is one; the visualiser (#15) is another. A tap is
/// pull-free and must not block — it is called on the mixer thread, between the sum and
/// the device write.
pub trait MixTap: Send + Sync {
    /// `stereo` is interleaved [`CHANNELS`]-channel audio at [`RATE`].
    fn mixed(&self, at: Instant, stereo: &[f32]);
}

/// One source's way into the mix.
///
/// Created per session from [`AudioMixer::input`], and removed from the mix when dropped.
/// Replaces the `Box<dyn AudioOut>` a session used to own: a session no longer has a
/// device, and cannot be handed one by mistake.
/// What a source does when the mix has no room for it.
///
/// Not a tuning knob — it is the answer to "can anything upstream be told to wait", and
/// only one of the two answers is available for any given source. Getting it wrong is not
/// a slow path, it is lost audio in the worst available place, so it is stated at the call
/// site rather than defaulted.
///
/// The distinction was already written down in this codebase before it was enforced:
/// `audio_session::PacedSession` is present exactly when *we* are the player and absent
/// exactly when "the sender is the clock".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backpressure {
    /// Park until there is room.
    ///
    /// Correct only when something upstream will wait: a file or an HTTP body we are
    /// pulling from, where not reading is the flow control and the bytes stay put.
    Pull,
    /// Take the newest audio and let the oldest go.
    ///
    /// The only honest answer for a sender that is its own clock — a phone on A2DP or
    /// AirPlay, Spotify, a browser page. Nothing here can slow it down, so audio in excess
    /// of [`LEAD`] is going to be lost whatever this returns; all this decides is *where*.
    /// Shedding it here costs a click. Refusing it pushes the loss up into the protocol's
    /// own queue, where the unit is an encoded packet — a decoder desync rather than a gap
    /// — and where a queue that fills once never drains again, because it can only drain
    /// at real time. That queue is seconds deep, so it also becomes a permanent latency
    /// floor. See #111 and the regression it caused.
    Live,
}

pub struct MixInput {
    /// What to do when this input is already [`LEAD`] ahead.
    backpressure: Backpressure,
    state: Arc<InputState>,
    shared: Arc<Shared>,
    /// Rebuilt whenever the source's shape changes. `None` until the first
    /// [`MixInput::format`] or the first [`MixInput::write`].
    convert: Option<Convert>,
    /// What `convert` was built for, so an unchanged shape costs nothing.
    shape: Option<(u32, u16)>,
    /// Blocks abandoned at [`WRITE_DEADLINE`], which only a [`Backpressure::Pull`] input
    /// can do. Reported once, on drop.
    dropped: u64,
    /// Frames shed to keep a [`Backpressure::Live`] source inside its budget. Counted
    /// separately from `dropped` because they are not the same event: this one is the
    /// design working, and it is only alarming in bulk.
    shed: u64,
}

/// The shared half of an input: what the mixer pulls from.
#[derive(Debug)]
struct InputState {
    ring: Mutex<Ring>,
    /// Signalled by the mixer after it drains, and by [`InputState::close`].
    room: Condvar,
    /// Whether this source can be told to wait, which is also what decides whether it
    /// primes. The mixer thread needs it and only sees this half of the input.
    backpressure: Backpressure,
}

#[derive(Debug, Default)]
struct Ring {
    /// Whether this input has banked [`PREBUFFER`] and is being drawn on.
    ///
    /// Set once and never cleared. Re-priming on an empty ring was tried and is much worse
    /// than the problem: a live source hits empty routinely, so rebuilding the reserve each
    /// time replaces a sub-millisecond hole with a [`PREBUFFER`]-long one. A momentary
    /// empty ring is ordinary starvation — counted, and small.
    primed: bool,
    /// Interleaved [`CHANNELS`]-channel [`RATE`] audio waiting to be mixed.
    samples: VecDeque<f32>,
    /// The [`MixInput`] is gone. The mixer plays out what is left, then retires this.
    closed: bool,
}

impl InputState {
    fn close(&self) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.closed = true;
        }
        self.room.notify_all();
    }

    /// Frames currently waiting. Saturates rather than failing: a poisoned ring is one
    /// whose writer panicked, and reporting it as full parks that input instead of
    /// spinning.
    ///
    /// The mixer itself never calls this — it is already holding the lock wherever it
    /// would want the answer — so it exists for the tests that assert on what a flush left
    /// behind.
    #[cfg(test)]
    fn queued_frames(&self) -> u64 {
        self.ring
            .lock()
            .map_or(u64::MAX, |r| r.samples.len() as u64 / u64::from(CHANNELS))
    }
}

/// What it takes to get one source's blocks to [`RATE`] stereo.
struct Convert {
    channels: u16,
    /// Absent when the source is already at [`RATE`].
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
/// layout and a set of coefficients, and what it would buy is a centre channel on a pair
/// of panel speakers.
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

impl MixInput {
    /// Declare the shape of what follows.
    ///
    /// Optional: [`MixInput::write`] notices a change on its own, because every
    /// [`PcmBlock`] states its own rate and channel count. Worth calling explicitly where
    /// the shape is known before the first block, so a resampler that cannot be built
    /// fails at the start of a session rather than in the middle of one.
    ///
    /// # Errors
    /// [`PipelineError`] if no resampler to [`RATE`] can be built for this shape.
    pub fn format(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError> {
        if self.shape == Some((sample_rate, channels)) {
            return Ok(());
        }
        self.convert = Some(Convert::open(sample_rate, channels)?);
        self.shape = Some((sample_rate, channels));
        Ok(())
    }

    /// Hand over samples.
    ///
    /// **Blocks while this input already has [`LEAD`] of audio in flight.** That is the
    /// backpressure that paces the decoder behind it; see the module docs for why the
    /// budget spans the device's queue as well as this input's ring.
    ///
    /// # Errors
    /// [`PipelineError`] if the block's shape has no resampler to [`RATE`].
    pub fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
        self.format(block.sample_rate, block.channels)?;
        let Some(convert) = self.convert.as_mut() else {
            return Ok(());
        };
        let stereo = convert.stereo(block)?;
        if stereo.is_empty() {
            return Ok(());
        }
        self.enqueue(&stereo);
        Ok(())
    }

    /// Make room for `stereo` inside this input's share of [`LEAD`], then append.
    ///
    /// How the room is made is [`Backpressure`]'s whole subject: a pull source parks until
    /// the mixer has drained, a live one sheds its oldest audio and carries on.
    fn enqueue(&mut self, stereo: &[f32]) {
        let budget = frames_in(LEAD);
        let deadline = Instant::now() + WRITE_DEADLINE;
        let Ok(mut ring) = self.state.ring.lock() else {
            return;
        };
        loop {
            let queued = ring.samples.len() as u64 / u64::from(CHANNELS);
            let inflight = queued + self.shared.device_inflight.load(Ordering::Relaxed);
            if inflight < budget || ring.closed {
                break;
            }
            if self.backpressure == Backpressure::Live {
                // Keep the newest and let the oldest go, so the room this source has is
                // spent on the audio nearest to now. Dropping the *front* rather than
                // refusing the block is what bounds the latency: refusing leaves the stale
                // audio in place and the source's own queue absorbs the difference, which
                // is exactly the failure this enum exists to prevent.
                let incoming = stereo.len() as u64 / u64::from(CHANNELS);
                let room =
                    budget.saturating_sub(self.shared.device_inflight.load(Ordering::Relaxed));
                let keep = room.saturating_sub(incoming) * u64::from(CHANNELS);
                let keep = usize::try_from(keep).unwrap_or(usize::MAX);
                if ring.samples.len() > keep {
                    let shed = ring.samples.len() - keep;
                    ring.samples.drain(..shed);
                    let frames = shed as u64 / u64::from(CHANNELS);
                    self.shed += frames;
                    self.shared.shed.fetch_add(frames, Ordering::Relaxed);
                }
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // The mixer is not draining. Losing this block keeps the caller's loop
                // alive to notice it has been told to stop.
                self.dropped += 1;
                return;
            }
            let Ok((next, _)) = self.state.room.wait_timeout(ring, remaining) else {
                return;
            };
            ring = next;
        }
        ring.samples.extend(stereo.iter().copied());
    }

    /// Throw away everything queued but not yet mixed.
    ///
    /// What a seek does: the queued audio is from before it, and the device's own share of
    /// the budget drains within [`DEVICE_LEAD`], so nothing has to reopen anything.
    pub fn flush(&mut self) {
        if let Ok(mut ring) = self.state.ring.lock() {
            ring.samples.clear();
        }
        self.state.room.notify_all();
        if let Some(convert) = self.convert.as_mut() {
            // The delay line holds pre-seek samples too, and a resampler with no way to
            // reset is cheaper to rebuild than to drain.
            if let (Some((rate, channels)), true) = (self.shape, convert.resampler.is_some()) {
                if let Ok(fresh) = Convert::open(rate, channels) {
                    *convert = fresh;
                }
            }
        }
    }
}

impl std::fmt::Debug for MixInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MixInput")
            .field("shape", &self.shape)
            .field("dropped", &self.dropped)
            .finish_non_exhaustive()
    }
}

impl Drop for MixInput {
    fn drop(&mut self) {
        if self.dropped > 0 {
            warn!(
                blocks = self.dropped,
                "mixer: an input gave up on blocks the mixer never drained"
            );
        }
        if self.shed > 0 {
            // At INFO because a live source shedding a little is the design working. In
            // bulk it says the source is outrunning the panel's clock, which is worth
            // being able to see without turning on debug logging.
            info!(
                frames = self.shed,
                "mixer: a live source ran ahead of the speakers and its oldest audio was shed"
            );
        }
        self.state.close();
        self.shared.work.notify_all();
    }
}

/// State shared between the mixer thread, its inputs and its taps.
struct Shared {
    /// Live inputs. The mixer retires an entry once its [`MixInput`] is gone and its ring
    /// has played out.
    inputs: Mutex<Vec<Arc<InputState>>>,
    /// Signalled when an input appears or disappears, so an idle mixer wakes.
    work: Condvar,
    taps: Mutex<Vec<Arc<dyn MixTap>>>,
    /// Frames the mixer emitted as silence because an input's ring was short.
    ///
    /// The one number this module was missing. A starved input is *padded*, not dropped or
    /// refused, so until this existed an input running dry was indistinguishable in the
    /// logs from one playing perfectly — no counter, no warning, and a third of the audio
    /// replaced by holes. See #175.
    starved: AtomicU64,
    /// Frames of silence emitted while a live input was still banking its reserve.
    ///
    /// Counted apart from `starved` because the cause is different — this one is a source
    /// starting up — but counted, because exempting it is how a prebuffer bug hid behind
    /// `starved_pct=0.0`. Silence is silence; where it came from is a label, not a reason
    /// to leave it out of the total.
    priming: AtomicU64,
    /// Frames shed from a [`Backpressure::Live`] input that had run ahead.
    ///
    /// Also on `MixInput` for the teardown line, and here as well because a counter that
    /// only reports when a session *ends* is no use while one is going wrong.
    shed: AtomicU64,
    gain: Arc<Gain>,
    /// Frames the mixer has written to the device but the device has not played.
    ///
    /// Published for the writers: it is the other half of their in-flight budget. Read
    /// with `Relaxed` because it is a pacing hint that is re-read every wait, not a
    /// synchronisation point.
    device_inflight: AtomicU64,
    shutdown: AtomicBool,
}

/// The panel's one audio output.
///
/// Owns the device and the thread that drives it. Dropping it stops the thread and
/// releases the device; the [`MixInput`]s it handed out keep working and go quiet, which
/// is the right behaviour during shutdown.
pub struct AudioMixer {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for AudioMixer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioMixer").finish_non_exhaustive()
    }
}

impl AudioMixer {
    /// Start a mixer that opens its device through `device`.
    ///
    /// The factory is called on first use and again after a failure or a close, so a
    /// selection changed on the settings screen reaches the *next* open — and, because the
    /// mixer reopens rather than each session doing so, that is now a matter of seconds
    /// rather than of whenever the current session happens to end.
    #[must_use]
    pub fn new(device: AudioOutputFactory) -> Self {
        Self::with_gain(device, Arc::new(Gain::default()))
    }

    /// As [`AudioMixer::new`], with a [`Gain`] the caller already holds.
    #[must_use]
    pub fn with_gain(device: AudioOutputFactory, gain: Arc<Gain>) -> Self {
        let shared = Arc::new(Shared {
            inputs: Mutex::new(Vec::new()),
            work: Condvar::new(),
            taps: Mutex::new(Vec::new()),
            gain,
            device_inflight: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            starved: AtomicU64::new(0),
            priming: AtomicU64::new(0),
            shed: AtomicU64::new(0),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("audio-mixer".into())
                .spawn(move || run(&shared, &device))
                .ok()
        };
        if thread.is_none() {
            warn!("mixer: could not start the mixer thread; the panel will be silent");
        }
        Self { shared, thread }
    }

    /// A way in for one source.
    ///
    /// `backpressure` has to be stated because there is no answer that is safe for both
    /// kinds of source — see [`Backpressure`].
    #[must_use]
    pub fn input(&self, backpressure: Backpressure) -> MixInput {
        let state = Arc::new(InputState {
            ring: Mutex::new(Ring::default()),
            room: Condvar::new(),
            backpressure,
        });
        if let Ok(mut inputs) = self.shared.inputs.lock() {
            inputs.push(Arc::clone(&state));
        }
        self.shared.work.notify_all();
        MixInput {
            backpressure,
            state,
            shared: Arc::clone(&self.shared),
            convert: None,
            shape: None,
            dropped: 0,
            shed: 0,
        }
    }

    /// Send everything the panel plays to `tap` as well.
    pub fn add_tap(&self, tap: Arc<dyn MixTap>) {
        if let Ok(mut taps) = self.shared.taps.lock() {
            taps.push(tap);
        }
    }

    /// The panel's one volume.
    #[must_use]
    pub fn gain(&self) -> Arc<Gain> {
        Arc::clone(&self.shared.gain)
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        // Set under the same lock [`park`] waits on, or this races it: the mixer can check
        // `shutdown`, find it clear, and only *then* take the lock and wait — after the
        // notify has already gone by. The wait has a timeout, so the cost was not a hang,
        // it was `IDLE_CLOSE` of it. Dropping the mixer happens on the way out of the
        // process, so that is five seconds of a panel that has been told to exit.
        {
            let _held = self.shared.inputs.lock();
            self.shared.shutdown.store(true, Ordering::Relaxed);
        }
        self.shared.work.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How much the mixer owes the device: the gap between where it wants to be and where it
/// has submitted to, in frames.
///
/// Pure, and split out for the same reason `audio_session::Pace::overshoot` was: the
/// property that matters — "a device running slow slows the mixer down" — is about
/// numbers, and timing a thread to prove it is a flaky test.
fn owed(target: u64, submitted: u64, max: u64) -> usize {
    usize::try_from(target.saturating_sub(submitted).min(max)).unwrap_or(0)
}

/// Where the mix is going, and what has been given to it.
///
/// There is always one, even when the box has no sound card and even when the sink has
/// just vanished — the fallback is [`NullAudioOut`], which accounts for everything and
/// plays nothing. That is what makes the "no device" case need no second code path: it
/// keeps time exactly as a real device does, so the inputs keep draining in real time and
/// nothing behind them stalls. It is also what keeps a Bluetooth session alive across a
/// panel that went to sleep and took the HDMI sink with it (#55).
struct Sink {
    out: Box<dyn AudioOut>,
    /// Whether `out` is a real device. False for the silence above, which is what says
    /// there is something to retry.
    real: bool,
    /// Frames handed to this sink since it was opened.
    submitted: u64,
    /// When it was opened, for a sink that cannot count.
    started: Instant,
}

impl Sink {
    /// A sink that plays nothing, for as long as no device will have us.
    fn silent() -> Self {
        let mut out: Box<dyn AudioOut> = Box::new(crate::audio_out::NullAudioOut::new());
        // The null sink accepts every shape; the result is checked anyway because a
        // library crate does not get to assume that on a runtime path.
        if out.start(RATE, CHANNELS).is_err() {
            warn!("mixer: even the null sink refused the mix");
        }
        Self {
            out,
            real: false,
            submitted: 0,
            started: Instant::now(),
        }
    }

    /// Frames this sink has played: by its own clock where it has one, by the wall where
    /// it does not.
    ///
    /// "How fast should silence be consumed" has no answer but real time, which is why the
    /// fallback is the wall rather than zero — pacing against a counter that never moves
    /// would stall every source instead of letting them drain.
    fn played(&self, now: Instant) -> u64 {
        self.out
            .frames_played()
            .unwrap_or_else(|| frames_in(now.saturating_duration_since(self.started)))
    }

    fn close(&mut self) {
        self.out.stop();
    }
}

/// The mixer thread.
fn run(shared: &Arc<Shared>, device: &AudioOutputFactory) {
    let mut sink = Sink::silent();
    // When the last input went away, so the device closes after a quiet spell rather than
    // between two tracks.
    let mut empty_since: Option<Instant> = None;
    // When a real device may next be tried, after a failure or a refusal. A box with no
    // sound card must not spend the whole session looking for one.
    let mut retry_at: Option<Instant> = None;
    let mut last_report = Instant::now();

    while !shared.shutdown.load(Ordering::Relaxed) {
        let inputs = live_inputs(shared);
        let now = Instant::now();
        report(shared, now, &mut last_report, inputs.len());

        if inputs.is_empty() {
            shared.device_inflight.store(0, Ordering::Relaxed);
            let since = *empty_since.get_or_insert(now);
            if sink.real && now.saturating_duration_since(since) >= IDLE_CLOSE {
                info!("mixer: no sources for a while; releasing the output device");
                sink.close();
                sink = Sink::silent();
                retry_at = None;
            }
            park(shared);
            continue;
        }
        empty_since = None;

        if !sink.real && retry_at.is_none_or(|at| now >= at) {
            match open(device) {
                Some(fresh) => {
                    sink.close();
                    sink = fresh;
                    retry_at = None;
                }
                None => retry_at = Some(now + REOPEN_AFTER),
            }
        }

        let target = sink.played(now) + frames_in(DEVICE_LEAD);
        let want = owed(target, sink.submitted, frames_in(QUANTUM));
        if want == 0 {
            std::thread::sleep(IDLE_POLL);
            continue;
        }

        let mixed = mix_pass(
            &inputs,
            want,
            &shared.gain,
            Some(&shared.starved),
            Some(&shared.priming),
        );
        publish(shared, now, &mixed);

        if let Err(e) = sink.out.write(&block_of(&mixed)) {
            warn!(error = %e, "mixer: the output device failed; carrying on in silence");
            sink = Sink::silent();
            retry_at = Some(now + REOPEN_AFTER);
        } else {
            sink.submitted += want as u64;
        }
        // Republished every pass, because it is half of every writer's budget: a device
        // that stops draining has to show up as a full budget, or the sources ahead of it
        // would carry on producing into a ring nobody empties.
        let inflight = sink.submitted.saturating_sub(sink.played(Instant::now()));
        shared.device_inflight.store(inflight, Ordering::Relaxed);
    }

    sink.close();
}

/// Everything still worth pulling from, retiring anything closed and drained.
fn live_inputs(shared: &Arc<Shared>) -> Vec<Arc<InputState>> {
    let Ok(mut inputs) = shared.inputs.lock() else {
        return Vec::new();
    };
    inputs.retain(|input| {
        input
            .ring
            .lock()
            .is_ok_and(|ring| !ring.closed || !ring.samples.is_empty())
    });
    inputs.clone()
}

/// Sleep until an input appears or the mixer is told to stop.
fn park(shared: &Arc<Shared>) {
    let Ok(inputs) = shared.inputs.lock() else {
        std::thread::sleep(IDLE_POLL);
        return;
    };
    // Both conditions re-checked *under the lock* before waiting. Checking them outside it
    // is the classic lost-wakeup: whoever set them could notify in the gap, and this would
    // then wait out the whole timeout for an event that had already happened.
    if shared.shutdown.load(Ordering::Relaxed) || !inputs.is_empty() {
        return;
    }
    // The guard is dropped immediately; what matters is having waited.
    drop(shared.work.wait_timeout(inputs, IDLE_CLOSE));
}

/// Open a real device, or say why not.
fn open(device: &AudioOutputFactory) -> Option<Sink> {
    let mut out = device();
    match out.start(RATE, CHANNELS) {
        Ok(()) => {
            info!(
                rate = RATE,
                channels = CHANNELS,
                "mixer: output device open"
            );
            Some(Sink {
                out,
                real: true,
                submitted: 0,
                started: Instant::now(),
            })
        }
        Err(e) => {
            warn!(error = %e, "mixer: the output device refused the mix; retrying");
            None
        }
    }
}

/// Pull `frames` from every input, sum them, and apply the panel's volume.
fn mix_pass(
    inputs: &[Arc<InputState>],
    frames: usize,
    gain: &Gain,
    starved: Option<&AtomicU64>,
    priming: Option<&AtomicU64>,
) -> Vec<f32> {
    let wanted = frames * usize::from(CHANNELS);
    let mut mixed = vec![0.0f32; wanted];
    for input in inputs {
        let Ok(mut ring) = input.ring.lock() else {
            continue;
        };
        // Still filling its reserve, so it contributes silence and says nothing about it:
        // this is the buffer working, not the starvation the counter exists to show. A
        // closed input never primes — what is left in it is all there will ever be, and
        // waiting for more would truncate the last moment of every session.
        if input.backpressure == Backpressure::Live && !ring.closed && !ring.primed {
            if (ring.samples.len() / usize::from(CHANNELS)) < frames_in(PREBUFFER) as usize {
                if let Some(priming) = priming {
                    priming.fetch_add(frames as u64, Ordering::Relaxed);
                }
                continue;
            }
            ring.primed = true;
        }
        // Rounded down to a whole frame: taking an odd number of samples would swap this
        // input's channels for the rest of the session, which is the kind of fault that
        // sounds like "something is subtly wrong with the stereo" and never gets found.
        let channels = usize::from(CHANNELS);
        let take = wanted.min(ring.samples.len()) / channels * channels;
        for slot in mixed.iter_mut().take(take) {
            // An input shorter than the pass contributes silence for the rest, which is
            // what an input that has not produced yet *is*.
            if let Some(sample) = ring.samples.pop_front() {
                *slot += sample;
            }
        }
        // What this input could not supply, which the loop above has just written as
        // zeroes. Counted here because this is the only place that knows the difference
        // between "quiet" and "had nothing to give".
        if let Some(starved) = starved {
            let short = (wanted - take) / usize::from(CHANNELS);
            if short > 0 {
                starved.fetch_add(short as u64, Ordering::Relaxed);
            }
        }
        drop(ring);
        input.room.notify_all();
    }
    gain.apply(&mut mixed);
    // Summing can leave the unit range even at unity gain — two sources at once, which is
    // the case this mixer exists to make possible. Hard-clipped rather than left to the
    // backend, where an out-of-range `f32` is anything from a clamp to a wrap.
    for sample in &mut mixed {
        *sample = sample.clamp(-1.0, 1.0);
    }
    mixed
}

/// Say what the speakers did not get, if anything, at most every [`REPORT_EVERY`].
///
/// Silence and shedding are both *normal* in small amounts — a source starting, a track
/// changing — so this reports a rate rather than a total and stays quiet at zero. What it
/// exists to make visible is the sustained case, where the figure is a share of the output
/// that simply was not there.
fn report(shared: &Arc<Shared>, now: Instant, last: &mut Instant, inputs: usize) {
    let window = now.saturating_duration_since(*last);
    if window < REPORT_EVERY {
        return;
    }
    *last = now;
    let starved = shared.starved.swap(0, Ordering::Relaxed);
    let priming = shared.priming.swap(0, Ordering::Relaxed);
    let shed = shared.shed.swap(0, Ordering::Relaxed);
    if starved == 0 && priming == 0 && shed == 0 {
        return;
    }
    let expected = window.as_secs_f64() * f64::from(RATE);
    #[allow(clippy::cast_precision_loss)]
    let share = if expected > 0.0 {
        starved as f64 / expected * 100.0
    } else {
        0.0
    };
    #[allow(clippy::cast_precision_loss)]
    let priming_share = if expected > 0.0 {
        priming as f64 / expected * 100.0
    } else {
        0.0
    };
    info!(
        starved_frames = starved,
        starved_pct = format!("{share:.1}"),
        priming_frames = priming,
        priming_pct = format!("{priming_share:.1}"),
        shed_frames = shed,
        inputs,
        "mixer: audio the speakers did not get"
    );
}

/// Hand the mix to every tap.
fn publish(shared: &Arc<Shared>, now: Instant, mixed: &[f32]) {
    let Ok(taps) = shared.taps.lock() else {
        return;
    };
    for tap in taps.iter() {
        tap.mixed(now, mixed);
    }
}

/// The mix as a block the device API takes.
fn block_of(mixed: &[f32]) -> PcmBlock {
    PcmBlock {
        sample_rate: RATE,
        channels: CHANNELS,
        samples: mixed.to_vec(),
        pts: Duration::ZERO,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A device that plays in real time and remembers everything it was given.
    ///
    /// `frames_played` advances on the wall clock from `start`, which is what a real
    /// device's counter does — so the mixer paces against this exactly as it would against
    /// a sound card, and a test can assert on how long a writer took.
    #[derive(Debug)]
    struct Recorder {
        start: Mutex<Option<Instant>>,
        heard: Mutex<Vec<f32>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                start: Mutex::new(None),
                heard: Mutex::new(Vec::new()),
            })
        }

        /// Everything written so far.
        fn heard(&self) -> Vec<f32> {
            self.heard.lock().unwrap().clone()
        }

        /// The loudest sample seen, which is what says whether a sum or a gain landed.
        fn peak(&self) -> f32 {
            self.heard()
                .iter()
                .copied()
                .fold(0.0f32, |peak, s| peak.max(s.abs()))
        }
    }

    impl AudioOut for Arc<Recorder> {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), PipelineError> {
            *self.start.lock().unwrap() = Some(Instant::now());
            Ok(())
        }

        fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
            self.heard.lock().unwrap().extend_from_slice(&block.samples);
            Ok(())
        }

        fn stop(&mut self) {}

        fn frames_played(&self) -> Option<u64> {
            let start = (*self.start.lock().unwrap())?;
            Some(frames_in(start.elapsed()))
        }
    }

    fn mixer_with(device: &Arc<Recorder>) -> AudioMixer {
        let device = Arc::clone(device);
        AudioMixer::new(Arc::new(move || Box::new(Arc::clone(&device))))
    }

    /// `frames` frames of interleaved stereo at `value`, already at the mix rate.
    fn tone(frames: usize, value: f32) -> PcmBlock {
        PcmBlock {
            sample_rate: RATE,
            channels: CHANNELS,
            samples: vec![value; frames * usize::from(CHANNELS)],
            pts: Duration::ZERO,
        }
    }

    /// Half amplitude, the long way round: [`Volume`] is stored as amplitude but has no
    /// constructor from one, deliberately — every sender speaks a slider position or a
    /// dBFS figure, and a bare multiplier is not a scale anyone sends.
    fn half_scale() -> Volume {
        Volume::from_dbfs(0.5f32.log10() * 20.0)
    }

    /// A `Shared` with no mixer thread behind it, so a test can step the retirement rule
    /// by hand.
    fn shared_with(inputs: Vec<Arc<InputState>>) -> Arc<Shared> {
        Arc::new(Shared {
            inputs: Mutex::new(inputs),
            work: Condvar::new(),
            taps: Mutex::new(Vec::new()),
            gain: Arc::new(Gain::default()),
            device_inflight: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            starved: AtomicU64::new(0),
            priming: AtomicU64::new(0),
            shed: AtomicU64::new(0),
        })
    }

    fn filled(samples: Vec<f32>) -> Arc<InputState> {
        Arc::new(InputState {
            ring: Mutex::new(Ring {
                // Already playing: every existing test means "an input with audio in it",
                // and priming is a separate property with its own tests below.
                primed: true,
                samples: samples.into(),
                closed: false,
            }),
            room: Condvar::new(),
            backpressure: Backpressure::Pull,
        })
    }

    #[test]
    fn the_mixer_owes_the_gap_between_where_it_wants_to_be_and_where_it_is() {
        // Nothing submitted, a full lead wanted: produce the lead, capped by the quantum.
        assert_eq!(owed(1000, 0, 480), 480);
        // Caught up: nothing owed, which is what makes the thread sleep instead of spin.
        assert_eq!(owed(1000, 1000, 480), 0);
        // Ahead of the target — a device that ran slower than the mixer expected. Not a
        // negative debt, and not a panic.
        assert_eq!(owed(1000, 1200, 480), 0);
        assert_eq!(owed(1100, 1000, 480), 100);
    }

    #[test]
    fn mono_is_duplicated_and_anything_wider_keeps_its_front_pair() {
        assert_eq!(to_stereo(&[0.5, 0.25], 1), vec![0.5, 0.5, 0.25, 0.25]);
        assert_eq!(to_stereo(&[0.5, 0.25], 2), vec![0.5, 0.25]);
        // 5.1 in the usual order: L R C LFE Ls Rs.
        let surround = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(to_stereo(&surround, 6), vec![1.0, 2.0]);
    }

    #[test]
    fn two_sources_are_summed_rather_than_one_replacing_the_other() {
        // The whole point of the change: before this, two sessions each held their own
        // device and the OS did this — invisibly, and with no way to express a policy.
        let a = filled(vec![0.25; 8]);
        let b = filled(vec![0.5; 8]);
        let mixed = mix_pass(&[a, b], 4, &Gain::default(), None, None);
        assert_eq!(mixed, vec![0.75; 8]);
    }

    #[test]
    fn an_input_shorter_than_the_pass_contributes_silence_for_the_rest() {
        // Not a shortfall to correct: an input that has not produced yet *is* silence,
        // and stretching what it did produce would change its pitch.
        let short = filled(vec![1.0; 4]);
        let mixed = mix_pass(&[short], 4, &Gain::default(), None, None);
        assert_eq!(mixed, vec![1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_sum_that_leaves_the_unit_range_is_clipped_here_rather_than_in_the_backend() {
        // Two loud sources at once is exactly the case this mixer makes possible, and an
        // out-of-range f32 is anything from a clamp to a wrap depending on the backend.
        let a = filled(vec![0.8; 4]);
        let b = filled(vec![0.8; 4]);
        let mixed = mix_pass(&[a, b], 2, &Gain::default(), None, None);
        assert_eq!(mixed, vec![1.0; 4]);
    }

    #[test]
    fn the_panels_volume_is_applied_once_to_the_sum() {
        // Applied to the mix, not per input: half volume over two sources at 0.25 each is
        // 0.25, not 0.125 twice summed to 0.25 by luck. The distinction shows up the
        // moment per-input gain exists.
        let gain = Gain::default();
        gain.set(half_scale());
        let a = filled(vec![0.25; 4]);
        let b = filled(vec![0.25; 4]);
        let mixed = mix_pass(&[a, b], 2, &gain, None, None);
        for sample in mixed {
            assert!((sample - 0.25).abs() < 1e-4, "{sample}");
        }
    }

    #[test]
    fn muting_silences_the_mix_without_disturbing_the_level() {
        let gain = Gain::default();
        gain.set(Volume::from_dbfs(0.75f32.log10() * 20.0));
        gain.set_muted(true);
        let mixed = mix_pass(&[filled(vec![1.0; 4])], 2, &gain, None, None);
        assert_eq!(mixed, vec![0.0; 4]);
        assert!(
            (gain.level() - 0.75).abs() < 1e-4,
            "the level survives a mute"
        );
    }

    #[test]
    fn a_closed_input_is_retired_only_after_what_it_left_has_played_out() {
        // Dropping a MixInput mid-block must not truncate the block: the last ~10 ms of
        // every track would go missing, on every source.
        //
        // Stepped by hand against a `Shared` with no thread behind it. Driving it through
        // a live mixer would be a race against that mixer draining the input before the
        // first assertion ran — which is exactly what it did.
        let state = filled(vec![0.5; 960]);
        let shared = shared_with(vec![Arc::clone(&state)]);
        // What `MixInput::drop` does.
        state.close();
        assert!(
            !live_inputs(&shared).is_empty(),
            "closed, but it still has audio in it"
        );
        // Drain it the way the mixer would.
        let _ = mix_pass(&[Arc::clone(&state)], 480, &Gain::default(), None, None);
        assert!(
            live_inputs(&shared).is_empty(),
            "closed and drained, so it should be gone"
        );
    }

    #[test]
    fn what_two_live_sources_write_reaches_the_device_summed() {
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut a = mixer.input(Backpressure::Pull);
        let mut b = mixer.input(Backpressure::Pull);
        for _ in 0..10 {
            a.write(&tone(480, 0.25)).unwrap();
            b.write(&tone(480, 0.5)).unwrap();
        }
        // Long enough for the mixer to have drained both at real-time pace.
        std::thread::sleep(Duration::from_millis(300));
        let peak = device.peak();
        assert!(
            (peak - 0.75).abs() < 1e-3,
            "both sources should be audible, summed: peak {peak}"
        );
    }

    #[test]
    fn a_source_cannot_run_further_than_the_lead_ahead_of_the_speakers() {
        // The pacing, as an observable: writing a second of audio into a device that plays
        // in real time has to take about a second, minus the lead the source is allowed to
        // run ahead by. Before the mixer this was `audio_session::Pace`, per session and
        // against a mix of the wall clock and the device's; now it is the ring, once.
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Pull);
        let start = Instant::now();
        // 1 s in 50 ms blocks.
        for _ in 0..20 {
            input.write(&tone(2400, 0.5)).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "a second of audio went in in {elapsed:?}; nothing is pacing the source"
        );
        assert!(
            elapsed < Duration::from_millis(1500),
            "a second of audio took {elapsed:?}; the pacing is not letting it run at all"
        );
    }

    /// A source that needs resampling is drained at *its own* rate, in real time.
    ///
    /// The test above establishes the pacing at [`RATE`], where no conversion happens. This
    /// is the same property one layer out, and it is the one a phone exercises: A2DP is
    /// 44.1 kHz, so every block goes through [`Convert`] before it reaches the ring, and
    /// the contract is that the input still accepts 44 100 frames per second of wall clock.
    ///
    /// Why it has to hold rather than merely being nice: [`MixInput::write`] parks until
    /// this source is under [`LEAD`] ahead of the speakers, which is flow control for a
    /// file and a fiction for a radio. A phone cannot be slowed down, so whatever this
    /// declines to accept is dropped further up — at the A2DP queue, where the unit of loss
    /// is an encoded packet and the sound of it is corruption rather than a gap. A deficit
    /// here does not show up as "slightly behind"; it shows up as an upstream queue that
    /// fills once and then never drains, which is both a permanent drop rate and a
    /// permanent latency floor of however deep that queue is.
    ///
    /// Measured two ways, because they fail differently and neither alone is convincing:
    /// what the input accepted per second of wall clock, and how much of what reached the
    /// device carried audio rather than the silence the mixer pads a starved input with.
    #[test]
    fn a_source_that_needs_resampling_is_drained_in_real_time() {
        const SOURCE_RATE: u32 = 44_100;
        /// One AAC frame, which is what an A2DP packet carries.
        const BLOCK: usize = 1024;
        /// Long enough to fill the budget and open the device, so what follows is steady
        /// state rather than the ring filling.
        const WARMUP: Duration = Duration::from_millis(1000);
        const WINDOW: Duration = Duration::from_millis(2000);

        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Pull);
        input.format(SOURCE_RATE, CHANNELS).unwrap();
        let block = PcmBlock {
            sample_rate: SOURCE_RATE,
            channels: CHANNELS,
            samples: vec![0.5; BLOCK * usize::from(CHANNELS)],
            pts: Duration::ZERO,
        };

        let warm_until = Instant::now() + WARMUP;
        while Instant::now() < warm_until {
            input.write(&block).unwrap();
        }

        let from = device.heard.lock().unwrap().len();
        let started = Instant::now();
        let mut accepted = 0u64;
        while started.elapsed() < WINDOW {
            input.write(&block).unwrap();
            accepted += BLOCK as u64;
        }
        let elapsed = started.elapsed();
        let heard = device.heard.lock().unwrap()[from..].to_vec();

        let rate = accepted as f64 / elapsed.as_secs_f64();
        let frames = heard.chunks_exact(usize::from(CHANNELS));
        let total = frames.len();
        let audible = frames.filter(|f| f.iter().any(|s| *s != 0.0)).count();
        let carried = audible as f64 / total as f64;
        println!(
            "accepted {rate:.0} frames/s of {SOURCE_RATE} ({:.1}%); \
             {audible}/{total} device frames carried audio ({:.1}%)",
            rate / f64::from(SOURCE_RATE) * 100.0,
            carried * 100.0,
        );

        assert!(
            rate > f64::from(SOURCE_RATE) * 0.97,
            "the input accepted {rate:.0} frames/s of a {SOURCE_RATE} Hz source; \
             a live sender's queue fills at the difference and never drains again"
        );
        assert!(
            carried > 0.97,
            "only {:.1}% of what reached the device carried audio; \
             the mixer is padding a starved input with silence",
            carried * 100.0
        );
    }

    /// A live source is never made to wait, however far ahead it runs.
    ///
    /// This is the property the two real-time-drain tests could not check, and the reason
    /// they both passed while a phone was losing a fifth of its audio: their producer was a
    /// loop calling `write`, so it was *flow-controlled by the thing under test* and could
    /// never get ahead. A phone is not. It fires packets on its own clock into a queue we
    /// do not own, so the only question that matters is whether this call returns.
    ///
    /// Five seconds of audio offered as fast as a thread can produce it, which is what a
    /// sender running ahead looks like from here. Under [`Backpressure::Pull`] that takes
    /// five seconds, by the test above. Under [`Backpressure::Live`] it must not: every
    /// millisecond spent parked here is a millisecond the source's own queue spends filling,
    /// and that queue is seconds deep and newest-drops.
    /// A starved input is counted, not only padded.
    ///
    /// The padding itself is right and is asserted above — an input that has not produced
    /// contributes silence, because the alternative is stretching what it did produce and
    /// changing its pitch. What was missing is that it left no trace: no counter, no
    /// warning, so an input running dry for minutes read exactly like one playing.
    /// A live input banks a reserve before it is drawn on, and is not called starved for it.
    #[test]
    fn a_live_input_primes_before_it_is_mixed() {
        let starved = AtomicU64::new(0);
        let state = Arc::new(InputState {
            ring: Mutex::new(Ring::default()),
            room: Condvar::new(),
            backpressure: Backpressure::Live,
        });
        // One packet's worth, far short of the reserve.
        state.ring.lock().unwrap().samples.extend(vec![1.0; 800]);
        let mixed = mix_pass(
            &[Arc::clone(&state)],
            240,
            &Gain::default(),
            Some(&starved),
            None,
        );
        assert_eq!(
            mixed,
            vec![0.0; 480],
            "still filling, so it contributes nothing"
        );
        assert_eq!(
            starved.load(Ordering::Relaxed),
            0,
            "priming is the buffer working, not the fault the counter reports"
        );
        assert!(!state.ring.lock().unwrap().primed);

        // Past the reserve, it plays.
        let banked = frames_in(PREBUFFER) as usize * usize::from(CHANNELS);
        state.ring.lock().unwrap().samples.extend(vec![1.0; banked]);
        let mixed = mix_pass(
            &[Arc::clone(&state)],
            240,
            &Gain::default(),
            Some(&starved),
            None,
        );
        assert_eq!(mixed, vec![1.0; 480], "primed, so the reserve is drawn on");
        assert!(state.ring.lock().unwrap().primed);
    }

    /// The reserve is what a source arriving in packets needs, and the point of the whole
    /// mechanism: a pass falling between two packets draws on the bank instead of punching
    /// a hole. Without priming this same sequence is silence in every gap — which measured
    /// at ~100% of the output on a real session (#175).
    #[test]
    fn a_primed_input_covers_the_gap_between_two_packets() {
        let starved = AtomicU64::new(0);
        let state = Arc::new(InputState {
            ring: Mutex::new(Ring::default()),
            room: Condvar::new(),
            backpressure: Backpressure::Live,
        });
        let reserve = frames_in(PREBUFFER) as usize * usize::from(CHANNELS);
        state
            .ring
            .lock()
            .unwrap()
            .samples
            .extend(vec![1.0; reserve]);
        // Prime.
        let _ = mix_pass(
            &[Arc::clone(&state)],
            1,
            &Gain::default(),
            Some(&starved),
            None,
        );
        // Now three passes with nothing arriving at all, as happens between packets.
        for _ in 0..3 {
            let mixed = mix_pass(
                &[Arc::clone(&state)],
                240,
                &Gain::default(),
                Some(&starved),
                None,
            );
            assert!(
                mixed.iter().all(|s| (*s - 1.0).abs() < 1e-6),
                "the reserve has to cover a gap between packets: {mixed:?}"
            );
        }
        assert_eq!(
            starved.load(Ordering::Relaxed),
            0,
            "nothing was invented; it came out of the bank"
        );
    }

    /// Priming is silence and is counted as such, and an input primes exactly once.
    ///
    /// Both halves are corrections of the first attempt at this, and both were the reason
    /// it made things worse rather than better. Re-priming on an empty ring sounds prudent
    /// and is not: a live source hits empty routinely, so it replaced a sub-millisecond
    /// hole with a [`PREBUFFER`]-long one, over and over. And exempting priming from the
    /// counters meant all of that silence reported as `starved_pct=0.0` — the one number
    /// that existed to catch it could not see it.
    #[test]
    fn priming_is_counted_and_happens_once() {
        let starved = AtomicU64::new(0);
        let priming = AtomicU64::new(0);
        let state = Arc::new(InputState {
            ring: Mutex::new(Ring::default()),
            room: Condvar::new(),
            backpressure: Backpressure::Live,
        });
        // A pass while it is still filling: silence, and said so.
        let mixed = mix_pass(
            &[Arc::clone(&state)],
            240,
            &Gain::default(),
            Some(&starved),
            Some(&priming),
        );
        assert_eq!(mixed, vec![0.0; 480]);
        assert_eq!(
            priming.load(Ordering::Relaxed),
            240,
            "silence has to be counted"
        );
        assert_eq!(
            starved.load(Ordering::Relaxed),
            0,
            "and attributed to the right cause"
        );

        // Bank the reserve and play it all out, so the ring ends empty.
        let reserve = frames_in(PREBUFFER) as usize * usize::from(CHANNELS);
        state
            .ring
            .lock()
            .unwrap()
            .samples
            .extend(vec![1.0; reserve]);
        while !state.ring.lock().unwrap().samples.is_empty() {
            let _ = mix_pass(
                &[Arc::clone(&state)],
                240,
                &Gain::default(),
                Some(&starved),
                Some(&priming),
            );
        }
        let priming_before = priming.load(Ordering::Relaxed);

        // Dry, and one packet arrives. It must play immediately rather than starting a
        // fresh 60 ms of silence.
        state.ring.lock().unwrap().samples.extend(vec![1.0; 8]);
        let mixed = mix_pass(
            &[Arc::clone(&state)],
            4,
            &Gain::default(),
            Some(&starved),
            Some(&priming),
        );
        assert_eq!(
            mixed,
            vec![1.0; 8],
            "a primed input does not re-prime when it runs dry"
        );
        assert_eq!(
            priming.load(Ordering::Relaxed),
            priming_before,
            "and nothing further is charged to priming"
        );
    }

    /// An input that closes plays out what it has rather than waiting for a reserve that
    /// is never coming — otherwise the last moment of every session is truncated.
    #[test]
    fn a_closing_input_plays_out_without_priming() {
        let state = Arc::new(InputState {
            ring: Mutex::new(Ring::default()),
            room: Condvar::new(),
            backpressure: Backpressure::Live,
        });
        {
            let mut ring = state.ring.lock().unwrap();
            ring.samples.extend(vec![1.0; 8]);
            ring.closed = true;
        }
        let mixed = mix_pass(&[state], 4, &Gain::default(), None, None);
        assert_eq!(mixed, vec![1.0; 8], "the tail has to reach the speakers");
    }

    #[test]
    fn an_input_that_runs_dry_is_counted_and_not_only_padded() {
        let starved = AtomicU64::new(0);
        // Two frames offered against a four-frame pass.
        let mixed = mix_pass(
            &[filled(vec![1.0; 4])],
            4,
            &Gain::default(),
            Some(&starved),
            None,
        );
        assert_eq!(mixed, vec![1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(
            starved.load(Ordering::Relaxed),
            2,
            "half the pass was invented and has to say so"
        );
    }

    #[test]
    fn a_live_source_is_never_made_to_wait() {
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Live);
        let start = Instant::now();
        // 100 blocks of 50 ms: five seconds of audio, twenty times the budget.
        for _ in 0..100 {
            input.write(&tone(2400, 0.5)).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "a live source was parked for {elapsed:?} offering five seconds of audio;              its own queue fills for exactly that long, and it newest-drops"
        );
        // Not "discarded everything": the newest audio still has to reach the speakers.
        std::thread::sleep(Duration::from_millis(200));
        let peak = device.peak();
        assert!(
            (peak - 0.5).abs() < 1e-3,
            "the live source shed its backlog but should still be audible: peak {peak}"
        );
    }

    #[test]
    fn a_box_with_no_device_still_drains_its_sources_in_real_time() {
        // #55's shape: the sink goes away and the session must carry on rather than wedge.
        // A factory that always refuses stands in for a panel that has gone to sleep.
        struct Refuses;
        impl AudioOut for Refuses {
            fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), PipelineError> {
                Err(PipelineError::Audio("no device".into()))
            }
            fn write(&mut self, _block: &PcmBlock) -> Result<(), PipelineError> {
                Err(PipelineError::Audio("no device".into()))
            }
            fn stop(&mut self) {}
        }
        let mixer = AudioMixer::new(Arc::new(|| Box::new(Refuses)));
        let mut input = mixer.input(Backpressure::Pull);
        let start = Instant::now();
        for _ in 0..10 {
            input.write(&tone(2400, 0.5)).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1200),
            "the source stalled behind a device that will not open: {elapsed:?}"
        );
        assert_eq!(
            input.dropped, 0,
            "nothing should have hit the write deadline"
        );
    }

    #[test]
    fn a_tap_is_given_exactly_what_the_device_was_given() {
        #[derive(Default)]
        struct Collect(Mutex<Vec<f32>>);
        impl MixTap for Collect {
            fn mixed(&self, _at: Instant, stereo: &[f32]) {
                self.0.lock().unwrap().extend_from_slice(stereo);
            }
        }
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let tap = Arc::new(Collect::default());
        mixer.add_tap(Arc::clone(&tap) as Arc<dyn MixTap>);
        let mut input = mixer.input(Backpressure::Pull);
        for _ in 0..8 {
            input.write(&tone(480, 0.5)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(250));
        let tapped = tap.0.lock().unwrap().clone();
        let heard = device.heard();
        assert!(!tapped.is_empty(), "the tap saw nothing");
        assert_eq!(
            tapped, heard,
            "the tap and the device must see the same samples, or the stream is a \
             reconstruction rather than a copy"
        );
    }

    #[test]
    fn a_source_at_another_rate_is_resampled_on_the_way_in() {
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Pull);
        input
            .write(&PcmBlock {
                sample_rate: 44_100,
                channels: 1,
                samples: vec![0.5; 4410],
                pts: Duration::ZERO,
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let heard = device.heard();
        assert!(
            !heard.is_empty(),
            "a 44.1 kHz mono source produced nothing at the device"
        );
        // Counted over the *audible* part, not over everything the device was given: the
        // mixer produces a continuous stream and pads with silence whenever the inputs
        // have nothing, which is the property the tap depends on. 100 ms in at 44.1 kHz
        // mono is ~100 ms out at 48 kHz stereo, give or take the resampler's delay line.
        let audible = heard.iter().filter(|s| s.abs() > 0.01).count();
        let frames = audible / usize::from(CHANNELS);
        assert!(
            (3600..=5400).contains(&frames),
            "expected about 4800 frames of audio out, got {frames}"
        );
    }

    #[test]
    fn a_flush_drops_what_was_queued_before_a_seek() {
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Pull);
        input.write(&tone(4800, 0.5)).unwrap();
        input.flush();
        assert_eq!(
            input.state.queued_frames(),
            0,
            "pre-seek audio survived the flush"
        );
    }
}
