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
//! Three budgets live near each other here and belong to three different owners — mixing
//! them up cost most of a day on #175. [`LEAD`] is the *media clock's* invariant, enforced
//! where a paced writer parks. [`LIVE_BUDGET`] is a *jitter and burst allowance* for a
//! sender that is its own clock and answers to no media clock at all. [`DEVICE_LEAD`] and
//! [`DEVICE_FLOOR`] are the *sound card's*: how much the mixer keeps between itself and
//! the device callback.
//!
//! ## Silence is invented in exactly one place
//!
//! The drain loop cannot ask for more than the scarcest input has. A pass either takes
//! what every contributing input can actually supply ([`Pass::Take`]), or — only when the
//! device is within [`DEVICE_FLOOR`] of running dry — emits a backstop pass that pads the
//! shortfall ([`Pass::Backstop`]). That is the whole policy, and it is the load-bearing
//! change of #175.
//!
//! The first drain loop did the opposite: it topped the device up to [`DEVICE_LEAD`]
//! whenever it had room, several passes per arriving packet, padding every input that
//! could not fill the pass. Against a source that delivers in packets — 8 ms of ALAC,
//! 23 ms of AAC — that *invented silence ahead of the deadline*, interleaving holes into a
//! stream that was arriving perfectly (#174's baseline defect). Two mechanisms were then
//! stacked on top to pay for it (a prebuffer, prime-once) and each changed the shape of
//! the failure rather than removing it, because eager padding was still underneath. Here
//! the defect is unrepresentable: a `Take` pass has no silence in it *by construction*,
//! and every frame of invented silence is a `Backstop` pass defending the floor — counted,
//! and attributed.
//!
//! A shortfall in a backstop pass is padded at the *front* of the pass, not the back. The
//! difference is what it does to the stream: front padding pushes this input's audio later
//! — adding latency, once — where back padding would cut a hole into it. A source that
//! starts slow therefore *defers* rather than stutters. That is not hypothetical: every
//! AirPlay sender measured on #175 delivered ~20–45% of real time for its first five
//! seconds and exactly real time after, with `sender_latency_frames=77175` declaring that
//! it intends the receiver to sit 1.75 s behind. Front padding banks precisely the deficit
//! the ramp creates, which is the buffer the sender expected us to hold. (Reading the
//! declared figure and reclaiming the latency deliberately is the timing-model work, not
//! this.)
//!
//! ## What silence means: [`Supply`]
//!
//! The old loop had one `if` and one counter where there were four different states, and
//! that conflation produced most of #175's wrong diagnoses — a browser input that had
//! played a page once and gone quiet read as a source starving at 100%, sustained, for
//! every minute of every session after it (measured twice, in two runs, before it was
//! recognised). Each pass now classifies every input, and the classification is an enum
//! the compiler makes this module handle:
//!
//! - [`Supply::Surface`] — never fed. A surface that *might* make a sound; the browser
//!   holds one for the life of the panel. Owes no explanation, gets no counter.
//! - [`Supply::Flowing`] — fed, and either has audio or went empty so recently it is
//!   between packets. What it has constrains the pass; a shortfall against it is real
//!   starvation.
//! - [`Supply::Quiescent`] — fed once, empty, and nothing has arrived for
//!   [`STALE_AFTER`]. A paused phone, a page that stopped playing. Genuinely silent:
//!   contributes silence, constrains nothing, and is counted as `idle`, not `starved`.
//! - [`Supply::Tail`] — closed. What is in it is all there will ever be; it plays out at
//!   device pace and the end of it is the end of a session, not a fault.
//!
//! ## What a frame count cannot say
//!
//! Every counter above is a number of frames, and a frame of silence weighs exactly what a
//! frame of music does. A mix delivered at real time, drained without starvation and
//! consumed by a device that never ran dry reads *identically* whether the samples in it
//! are a song or zeros — which is how a page audible only at −30 dBFS spent a session
//! looking like a mixer fault (#178).
//!
//! So the report also carries the loudest sample the device was given and the factor the
//! volume applied to reach it. Signal that arrived and was then attenuated to nothing is a
//! different fault from signal that never arrived, they are told apart by one subtraction
//! in dB, and no count of frames distinguishes them at all.
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

/// The most audio the mixer keeps queued at the device.
///
/// The rest of [`LEAD`] lives in the inputs' rings. Big enough that an ordinary scheduling
/// hiccup on the mixer thread does not reach the device callback as a dropout, small
/// enough to leave most of the budget where backpressure can act on it.
const DEVICE_LEAD: Duration = Duration::from_millis(100);

/// The least audio the mixer lets the device hold while anything might play.
///
/// This is the deadline, and defending it is the *only* thing that may put silence into
/// the mix: a [`Pass::Backstop`] runs when the device's queue decays to here and nothing
/// supply-driven has topped it up. Between the floor and [`DEVICE_LEAD`] is the slack a
/// late packet spends silently — a delivery gap shorter than the difference never reaches
/// the speakers at all.
const DEVICE_FLOOR: Duration = Duration::from_millis(50);

/// The most audio one mixer pass will produce.
///
/// A pass holds each input's lock in turn, so a long one is a long time for a writer to
/// wait; a short one is a syscall per few milliseconds. This is also the granularity at
/// which a newly arrived input starts being heard.
const QUANTUM: Duration = Duration::from_millis(10);

/// How long the mixer sleeps when there is nothing to take and no floor to defend.
const IDLE_POLL: Duration = Duration::from_millis(5);

/// How long the device stays open with no inputs.
///
/// The one genuine virtue of the old per-session design was that an idle panel held no
/// sink, and it was nowhere written down. This preserves it. Long enough that skipping
/// between tracks, or between two casts, does not close and reopen the device.
const IDLE_CLOSE: Duration = Duration::from_secs(5);

/// How often the mixer retries a device it could not open, or that failed.
const REOPEN_AFTER: Duration = Duration::from_secs(1);

/// How much audio a live input may hold.
///
/// [`LEAD`] does not apply to these and never should have. It is the *media clock's*
/// invariant — `MediaClock` reports `submitted - OUTPUT_LEAD`, so a paced source may run
/// exactly that far ahead of the speakers and no further, or a cast plays its video early.
/// A live source has no media clock: `spawn_pcm`'s own comment says "the sender is the
/// clock, there is no video to synchronise". Capping it at `LEAD` applied a lip-sync
/// constraint to sources with no lips, and cost the audio instead.
///
/// What it has to hold is the sender's burst. A phone does not hand over one packet every
/// 8 ms in a tidy line; it sends ahead, in lumps, and declares how far ahead it intends to
/// run — `sender_latency_frames=77175` is 1.75 s of it. A ring holding 150 ms against that
/// alternately overflows and runs dry, which is `starved=42%` and `shed=41%` in the same
/// five seconds.
///
/// Two seconds, so the declared burst fits. The cost is latency *only if the sender
/// actually runs that far ahead*, since this is a ceiling and not a target — the ring
/// drains at real time and sits at whatever the sender banks. Reclaiming that is the
/// timing-model work (`clock_samples=0`), not this.
const LIVE_BUDGET: Duration = Duration::from_secs(2);

/// How long an empty input is still treated as between packets.
///
/// The line between [`Supply::Flowing`] and [`Supply::Quiescent`], and it has to be a
/// timescale because nothing else distinguishes them: a phone that paused and a phone
/// whose next packet is 8 ms out look identical at the instant the ring is empty. Longer
/// than any inter-packet gap a live source produces (23 ms for A2DP AAC, less for
/// everything else) plus the delivery jitter on top; short enough that one source pausing
/// does not hold the whole mix at the floor for long.
///
/// Structural rather than stateful, like `is_sounding`: nothing arms it and nothing
/// cancels it — the answer falls out of a timestamp the writer was setting anyway.
const STALE_AFTER: Duration = Duration::from_millis(250);

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
///
/// Read in exactly one place — [`MixInput::enqueue`], the write-side boundary. The drain
/// side never consults it: how an input is mixed falls out of [`Supply`], which both kinds
/// of source share.
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
    /// of its budget is going to be lost whatever this returns; all this decides is
    /// *where*. Shedding it here costs a click. Refusing it pushes the loss up into the
    /// protocol's own queue, where the unit is an encoded packet — a decoder desync rather
    /// than a gap — and where a queue that fills once never drains again, because it can
    /// only drain at real time. That queue is seconds deep, so it also becomes a permanent
    /// latency floor. See #111 and the regression it caused.
    Live,
}

/// One source's way into the mix.
///
/// Created per session from [`AudioMixer::input`], and removed from the mix when dropped.
/// Replaces the `Box<dyn AudioOut>` a session used to own: a session no longer has a
/// device, and cannot be handed one by mistake.
pub struct MixInput {
    /// What to do when this input is already at its budget.
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
}

#[derive(Debug, Default)]
struct Ring {
    /// When something last arrived, and whether anything ever has.
    ///
    /// Both questions matter and they are one field: `None` is [`Supply::Surface`] — a
    /// browser that has never made a sound — and a stale `Some` on an empty ring is
    /// [`Supply::Quiescent`] — a page that made one and stopped. Conflating those two
    /// with a starving source is, in two different ways, most of #175's wrong readings.
    last_fed: Option<Instant>,
    /// Interleaved [`CHANNELS`]-channel [`RATE`] audio waiting to be mixed.
    samples: VecDeque<f32>,
    /// The [`MixInput`] is gone. The mixer plays out what is left, then retires this.
    closed: bool,
}

/// What one input can put into a pass, decided fresh each pass.
///
/// The enum the old code kept as two booleans and an `if`, which is how a permanently
/// quiet input spent a day reading as a starving one. Every pass classifies every input
/// and the `match` is exhaustive: a new kind of silence has to say what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Supply {
    /// Never fed. A surface that might make a sound, not a source failing to.
    Surface,
    /// Fed, and live in the ordinary sense: has audio, or went empty recently enough
    /// that the next packet is presumed in flight. What it has bounds the pass.
    Flowing {
        /// Whole frames available right now. Zero between packets.
        frames: u64,
    },
    /// Fed once, empty, and quiet past [`STALE_AFTER`]: paused, muted, or done sending.
    /// Genuinely silent — mixed as silence, constraining nothing, starving nothing.
    Quiescent,
    /// Closed. What remains is all there will ever be, and the end of it is the end of a
    /// session rather than a shortfall.
    Tail {
        /// Whole frames left to play out.
        frames: u64,
    },
}

impl Ring {
    /// Classify this ring, now.
    fn supply(&self, now: Instant) -> Supply {
        let frames = self.samples.len() as u64 / u64::from(CHANNELS);
        if self.closed {
            return Supply::Tail { frames };
        }
        match self.last_fed {
            None => Supply::Surface,
            Some(at) if frames == 0 && now.saturating_duration_since(at) >= STALE_AFTER => {
                Supply::Quiescent
            }
            Some(_) => Supply::Flowing { frames },
        }
    }
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
    /// **Blocks while this input already has [`LEAD`] of audio in flight** — for a
    /// [`Backpressure::Pull`] source. That is the backpressure that paces the decoder
    /// behind it; see the module docs for why the budget spans the device's queue as well
    /// as this input's ring. A [`Backpressure::Live`] source is never made to wait.
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

    /// Make room for `stereo` inside this input's budget, then append.
    ///
    /// How the room is made is [`Backpressure`]'s whole subject: a pull source parks until
    /// the mixer has drained, a live one sheds its oldest audio and carries on. This is
    /// the one place the enum is read.
    fn enqueue(&mut self, stereo: &[f32]) {
        let budget = match self.backpressure {
            Backpressure::Pull => frames_in(LEAD),
            Backpressure::Live => frames_in(LIVE_BUDGET),
        };
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
        ring.last_fed = Some(Instant::now());
        ring.samples.extend(stereo.iter().copied());
        self.shared
            .written
            .fetch_add(stereo.len() as u64 / u64::from(CHANNELS), Ordering::Relaxed);
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
/// What the mixer has done since it started, in frames, as one consistent-enough read.
///
/// Every field is a running total for the life of the [`AudioMixer`]; the log line
/// [`report`] writes every [`REPORT_EVERY`] is a *window* over these, differenced with
/// [`MixerCounters::since`]. Totals rather than a per-window reset because a counter that
/// the reporter consumes is a counter nothing else can ever read, and the three live
/// defects these were added for (#174, #175, #177) all needed a test to read them.
///
/// The fields are not sampled under one lock, so a snapshot taken mid-pass can catch
/// `drained` incremented and `emitted` not yet. That is frames of skew on figures whose
/// subject is seconds, and paying a lock on the drain loop's hot path to remove it would
/// be the wrong trade — but it does mean the two identities below hold to within a pass,
/// not exactly:
///
/// ```text
/// written  == drained + shed + (what is still in the rings)
/// emitted  == drained + starved + idle
/// ```
///
/// Both are statements about *totals*, and neither survives [`MixerCounters::since`]: a
/// window that opens with a backlog in the rings drains more than was written into it,
/// which is ordinary and is what a warm-up produces. Read a window for rates, and the
/// totals for the identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MixerCounters {
    /// Frames of silence a backstop pass invented for an input that was mid-stream and
    /// had nothing to give. The one defect counter: structurally zero while every source
    /// keeps up.
    pub starved: u64,
    /// Frames an input with nothing to say sat out, plus a closed input's runout past its
    /// tail. Benign, and counted so the emitted identity closes.
    pub idle: u64,
    /// Frames the writers handed in.
    pub written: u64,
    /// Frames the mixer took out of the rings, summed across inputs.
    pub drained: u64,
    /// Frames dropped from a [`Backpressure::Live`] input that had run ahead of its
    /// budget.
    pub shed: u64,
    /// Frames the mixer emitted to the device, of any kind. Far from real time, this is
    /// the device's clock misbehaving — the one way this module can be wrong that no
    /// input-side counter would show.
    pub emitted: u64,
}

impl MixerCounters {
    /// What happened between `earlier` and this reading.
    ///
    /// Saturating, so a snapshot racing an increment reads as no progress rather than as
    /// most of a `u64`.
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            starved: self.starved.saturating_sub(earlier.starved),
            idle: self.idle.saturating_sub(earlier.idle),
            written: self.written.saturating_sub(earlier.written),
            drained: self.drained.saturating_sub(earlier.drained),
            shed: self.shed.saturating_sub(earlier.shed),
            emitted: self.emitted.saturating_sub(earlier.emitted),
        }
    }

    /// The share of what the device was given that the mixer made up, as a fraction.
    ///
    /// `None` when nothing was emitted at all, which is a mixer that has not run rather
    /// than one that starved: there is no denominator and the caller must not read zero
    /// as health. This is the quantity #175 is about, and the one an assertion on a
    /// *duration* could only infer.
    #[must_use]
    pub fn invented(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        (self.emitted > 0).then(|| self.starved as f64 / self.emitted as f64)
    }
}

struct Shared {
    /// Live inputs. The mixer retires an entry once its [`MixInput`] is gone and its ring
    /// has played out.
    inputs: Mutex<Vec<Arc<InputState>>>,
    /// Signalled when an input appears or disappears, so an idle mixer wakes.
    work: Condvar,
    taps: Mutex<Vec<Arc<dyn MixTap>>>,
    /// Running totals for the life of the mixer, read out as [`MixerCounters`]. What each
    /// one means is documented on that type, which is the public face of these; what is
    /// worth knowing *here* is that nothing resets them. See [`Reported`].
    ///
    /// `shed` is also kept per-[`MixInput`] for the teardown line, and here as well
    /// because a counter that only reports when a session *ends* is no use while one is
    /// going wrong.
    starved: AtomicU64,
    idle: AtomicU64,
    written: AtomicU64,
    drained: AtomicU64,
    shed: AtomicU64,
    emitted: AtomicU64,
    /// The loudest sample the device was given since the last report.
    ///
    /// Measured *after* [`Gain`], because the question it answers is what the speakers
    /// got rather than what the mix held. Read against [`Gain::factor`] on the same line:
    /// a peak far below the mix's own level says the volume ate it, and a peak of nothing
    /// at all says no source ever had anything to attenuate.
    ///
    /// Held as `f32` bits under `fetch_max`, which is exact for non-negative floats —
    /// IEEE-754 orders them identically to their bit patterns read as `u32`.
    peak: AtomicU32,
    gain: Arc<Gain>,
    /// Frames the mixer has written to the device but the device has not played.
    ///
    /// Published for the writers: it is the other half of their in-flight budget. Read
    /// with `Relaxed` because it is a pacing hint that is re-read every wait, not a
    /// synchronisation point.
    device_inflight: AtomicU64,
    shutdown: AtomicBool,
}

impl Shared {
    /// One reading of every total. See [`MixerCounters`] for what "one reading" is worth.
    fn counters(&self) -> MixerCounters {
        MixerCounters {
            starved: self.starved.load(Ordering::Relaxed),
            idle: self.idle.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            drained: self.drained.load(Ordering::Relaxed),
            shed: self.shed.load(Ordering::Relaxed),
            emitted: self.emitted.load(Ordering::Relaxed),
        }
    }
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
            idle: AtomicU64::new(0),
            shed: AtomicU64::new(0),
            written: AtomicU64::new(0),
            drained: AtomicU64::new(0),
            emitted: AtomicU64::new(0),
            peak: AtomicU32::new(0),
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

    /// What this mixer has done since it started.
    ///
    /// Totals, not a window: difference two readings with [`MixerCounters::since`] to get
    /// a rate. Reading these does not disturb the log line — that is the whole reason
    /// they are totals, and #175's counters existing only inside an `info!` is what a
    /// test could not assert on.
    #[must_use]
    pub fn counters(&self) -> MixerCounters {
        self.shared.counters()
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

/// What one iteration of the drain loop should do.
///
/// Decided by [`plan`], which is pure: the whole drain policy is a function from what the
/// inputs have and where the device's queue sits, to one of three actions — so the policy
/// is tested as arithmetic, not by timing threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Nothing to take, no floor to defend. Sleep briefly.
    Sleep,
    /// Take exactly this many frames, which every constraining input has. No silence is
    /// in a `Take` by construction — that is the point of the whole design.
    Take {
        /// Whole frames, at most a quantum.
        frames: u64,
    },
    /// The device is at the floor: emit a quantum, padding at the front whatever any
    /// input cannot supply. The only pass that invents silence.
    Backstop,
}

/// Decide the next pass.
///
/// A pass may take the *least* any [`Supply::Flowing`] or [`Supply::Tail`] input can
/// supply — never more, so a `Take` cannot pad — bounded by the device's headroom and the
/// quantum. When that is nothing and the device would decay through [`DEVICE_FLOOR`],
/// a backstop runs instead; otherwise the loop sleeps and lets the queue carry the gap.
fn plan(supplies: &[Supply], inflight: u64) -> Pass {
    let constraint = supplies
        .iter()
        .filter_map(|s| match s {
            Supply::Flowing { frames } | Supply::Tail { frames } => Some(*frames),
            Supply::Surface | Supply::Quiescent => None,
        })
        .min();
    let headroom = frames_in(DEVICE_LEAD).saturating_sub(inflight);
    let frames = constraint
        .unwrap_or(0)
        .min(headroom)
        .min(frames_in(QUANTUM));
    if frames > 0 {
        return Pass::Take { frames };
    }
    if inflight <= frames_in(DEVICE_FLOOR) {
        return Pass::Backstop;
    }
    Pass::Sleep
}

/// Where a pass reports what it did. Absent in unit tests that only assert on samples.
struct MixCounters<'a> {
    starved: &'a AtomicU64,
    idle: &'a AtomicU64,
    drained: &'a AtomicU64,
    peak: &'a AtomicU32,
}

/// Execute a pass: pull up to `frames` from every input, sum, and apply the panel's
/// volume.
///
/// Each input is classified again under its own lock — the plan's snapshot may be a
/// moment old — and contributes what it has, up to `frames`. A shortfall is padded at the
/// *front* of this input's contribution, so a source that is behind is deferred rather
/// than perforated; against a [`Pass::Take`] the shortfall is structurally zero, because
/// the plan never asks for more than the scarcest input had.
fn mix_pass(
    inputs: &[Arc<InputState>],
    frames: u64,
    now: Instant,
    gain: &Gain,
    counters: Option<&MixCounters<'_>>,
) -> Vec<f32> {
    let channels = usize::from(CHANNELS);
    let wanted = usize::try_from(frames).unwrap_or(usize::MAX) * channels;
    let mut mixed = vec![0.0f32; wanted];
    for input in inputs {
        let Ok(mut ring) = input.ring.lock() else {
            continue;
        };
        let supply = ring.supply(now);
        let available = match supply {
            // A surface owes no explanation and gets no counter.
            Supply::Surface => continue,
            // Genuinely silent: it contributes silence because it has nothing to say,
            // not because the mixer outran it.
            Supply::Quiescent => {
                if let Some(c) = counters {
                    c.idle.fetch_add(frames, Ordering::Relaxed);
                }
                continue;
            }
            Supply::Flowing { frames } | Supply::Tail { frames } => frames,
        };
        // Whole frames only: taking an odd number of samples would swap this input's
        // channels for the rest of the session, which is the kind of fault that sounds
        // like "something is subtly wrong with the stereo" and never gets found.
        let take = available.min(frames);
        let take_samples = usize::try_from(take).unwrap_or(usize::MAX) * channels;
        // The shortfall leads and the audio trails: deferral, not a hole.
        let offset = wanted - take_samples;
        for slot in mixed[offset..].iter_mut() {
            if let Some(sample) = ring.samples.pop_front() {
                *slot += sample;
            }
        }
        if let Some(c) = counters {
            c.drained.fetch_add(take, Ordering::Relaxed);
            let short = frames - take;
            if short > 0 {
                match supply {
                    // Mid-stream and had nothing to give: the defect counter.
                    Supply::Flowing { .. } => {
                        c.starved.fetch_add(short, Ordering::Relaxed);
                    }
                    // Running out past the end of a session is the session ending.
                    Supply::Tail { .. } => {
                        c.idle.fetch_add(short, Ordering::Relaxed);
                    }
                    Supply::Surface | Supply::Quiescent => {}
                }
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
    // Last, so what is measured is exactly the samples the device is about to be handed —
    // a peak taken before the gain would report the mix's ambition rather than its result.
    if let Some(c) = counters {
        let peak = mixed.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        c.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
    }
    mixed
}

/// Where the mix is going, and what has been given to it.
///
/// There is always one, even when the box has no sound card and even when the sink has
/// just vanished — the fallback is [`NullAudioOut`], which accounts for everything and
/// plays nothing. That is what makes the "no device" case need no second code path: it
/// keeps time exactly as a real device does, so the inputs keep draining in real time and
/// nothing behind them stalls. It is also what keeps a Bluetooth session alive across a
/// panel that went to sleep and took the HDMI sink with it (#55).
///
/// [`NullAudioOut`]: crate::audio_out::NullAudioOut
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

    /// Frames the mixer believes are queued at this sink.
    fn inflight(&self, now: Instant) -> u64 {
        self.submitted.saturating_sub(self.played(now))
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
    let mut last_report = Reported {
        at: Instant::now(),
        counters: MixerCounters::default(),
        underruns: 0,
    };

    while !shared.shutdown.load(Ordering::Relaxed) {
        let inputs = live_inputs(shared);
        let now = Instant::now();
        report(shared, now, &mut last_report, inputs.len(), &sink);

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

        let inflight = sink.inflight(now);
        shared.device_inflight.store(inflight, Ordering::Relaxed);

        let supplies: Vec<Supply> = inputs
            .iter()
            .map(|input| {
                input
                    .ring
                    .lock()
                    .map_or(Supply::Quiescent, |ring| ring.supply(now))
            })
            .collect();

        let frames = match plan(&supplies, inflight) {
            Pass::Sleep => {
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            Pass::Take { frames } => frames,
            Pass::Backstop => frames_in(QUANTUM),
        };

        let mixed = mix_pass(
            &inputs,
            frames,
            now,
            &shared.gain,
            Some(&MixCounters {
                starved: &shared.starved,
                idle: &shared.idle,
                drained: &shared.drained,
                peak: &shared.peak,
            }),
        );
        shared.emitted.fetch_add(frames, Ordering::Relaxed);
        publish(shared, now, &mixed);

        if let Err(e) = sink.out.write(&block_of(&mixed)) {
            warn!(error = %e, "mixer: the output device failed; carrying on in silence");
            sink = Sink::silent();
            retry_at = Some(now + REOPEN_AFTER);
        } else {
            sink.submitted += frames;
        }
        // Republished every pass, because it is half of every writer's budget: a device
        // that stops draining has to show up as a full budget, or the sources ahead of it
        // would carry on producing into a ring nobody empties.
        let inflight = sink.inflight(Instant::now());
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

/// What the last report read, so the next one can carry a window without resetting
/// anything.
///
/// The counters on [`Shared`] are running totals for the life of the mixer. They used to
/// be `swap`ped to zero here, which made the log the only thing that could ever read them
/// — a test asserting on invented silence would have been racing the reporter for the
/// figure, and whichever won would have taken it from the other. The device's underrun
/// count was already kept this way, because it comes from the backend and could not be
/// reset; every counter now works the way that one did.
struct Reported {
    at: Instant,
    counters: MixerCounters,
    /// The device's own dry-callback count. This is the far side of
    /// [`Shared::emitted`]: silence the *device* inserted because the mixer was late,
    /// which no counter on the mix side can see.
    underruns: u64,
}

/// Say what the speakers did not get, if anything, at most every [`REPORT_EVERY`].
///
/// Silence and shedding are both *normal* in small amounts — a source starting, a track
/// changing — so this reports rates against real time and stays quiet when nothing moved.
/// The figures are sums across inputs; against the usual one active source the two
/// identities on [`Shared::emitted`] read straight off the line.
fn report(shared: &Arc<Shared>, now: Instant, last: &mut Reported, inputs: usize, sink: &Sink) {
    let window = now.saturating_duration_since(last.at);
    if window < REPORT_EVERY {
        return;
    }
    let totals = shared.counters();
    let MixerCounters {
        starved,
        idle,
        shed,
        written,
        drained,
        emitted,
    } = totals.since(&last.counters);
    // Read before the early return, or a window the log skipped would fold its figures
    // into the next one and report a rate that never happened.
    let underruns = sink.out.underruns().unwrap_or(0);
    let dry = underruns.saturating_sub(last.underruns);
    // The peak is still taken rather than differenced: it is a maximum over the window,
    // not a total, and one carried into the next window would report a sound that stopped
    // as one still playing.
    let peak = f32::from_bits(shared.peak.swap(0, Ordering::Relaxed));
    *last = Reported {
        at: now,
        counters: totals,
        underruns,
    };
    // Idle and emitted alone do not wake the log: a panel holding a quiet page is not an
    // event. Anything a session did — or failed to get — is.
    if starved == 0 && shed == 0 && written == 0 && drained == 0 {
        return;
    }
    let expected = window.as_secs_f64() * f64::from(RATE);
    #[allow(clippy::cast_precision_loss)]
    let pct = |n: u64| {
        if expected > 0.0 {
            format!("{:.1}", n as f64 / expected * 100.0)
        } else {
            "0.0".to_owned()
        }
    };
    info!(
        starved_frames = starved,
        starved_pct = pct(starved),
        idle_frames = idle,
        shed_frames = shed,
        written_frames = written,
        written_pct = pct(written),
        drained_frames = drained,
        drained_pct = pct(drained),
        emitted_frames = emitted,
        emitted_pct = pct(emitted),
        device_dry = dry,
        inputs,
        peak_dbfs = dbfs(peak),
        amplitude = shared.gain.factor(),
        "mixer: audio the speakers did not get"
    );
}

/// A measured sample peak as dBFS, for a log line.
///
/// Not [`castaway_core::Volume`]: that type has no constructor from a bare amplitude and
/// should not gain one (#85). This is a measurement of what samples *were*, not a level
/// anyone set, and the two must not become interchangeable.
fn dbfs(peak: f32) -> String {
    if peak > 0.0 {
        format!("{:.1}", peak.log10() * 20.0)
    } else {
        "-inf".to_owned()
    }
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
            idle: AtomicU64::new(0),
            shed: AtomicU64::new(0),
            written: AtomicU64::new(0),
            drained: AtomicU64::new(0),
            emitted: AtomicU64::new(0),
            peak: AtomicU32::new(0),
        })
    }

    fn state_with(ring: Ring) -> Arc<InputState> {
        Arc::new(InputState {
            ring: Mutex::new(ring),
            room: Condvar::new(),
        })
    }

    /// An input mid-stream with audio banked, which is what every summing test means.
    fn filled(samples: Vec<f32>) -> Arc<InputState> {
        state_with(Ring {
            last_fed: Some(Instant::now()),
            samples: samples.into(),
            closed: false,
        })
    }

    fn counters<'a>(
        starved: &'a AtomicU64,
        idle: &'a AtomicU64,
        drained: &'a AtomicU64,
        peak: &'a AtomicU32,
    ) -> MixCounters<'a> {
        MixCounters {
            starved,
            idle,
            drained,
            peak,
        }
    }

    /// What [`mix_pass`] recorded, as an amplitude.
    fn measured(peak: &AtomicU32) -> f32 {
        f32::from_bits(peak.load(Ordering::Relaxed))
    }

    // ---- classification -----------------------------------------------------------

    #[test]
    fn an_input_is_what_its_ring_says_it_is() {
        let now = Instant::now();
        // Never fed: a surface, wherever the clock stands.
        assert_eq!(Ring::default().supply(now), Supply::Surface);
        // Fed and holding audio: flowing, however old the last write is.
        let stale = now - STALE_AFTER * 4;
        let holding = Ring {
            last_fed: Some(stale),
            samples: vec![0.5; 8].into(),
            closed: false,
        };
        assert_eq!(holding.supply(now), Supply::Flowing { frames: 4 });
        // Fed, empty, but fresh: between packets, still flowing — with nothing to give.
        let between = Ring {
            last_fed: Some(now),
            samples: VecDeque::new(),
            closed: false,
        };
        assert_eq!(between.supply(now), Supply::Flowing { frames: 0 });
        // Fed, empty, and quiet past the line: gone quiet, not starving.
        let quiet = Ring {
            last_fed: Some(stale),
            samples: VecDeque::new(),
            closed: false,
        };
        assert_eq!(quiet.supply(now), Supply::Quiescent);
        // Closed: a tail, whatever else is true.
        let tail = Ring {
            last_fed: Some(stale),
            samples: vec![0.5; 4].into(),
            closed: true,
        };
        assert_eq!(tail.supply(now), Supply::Tail { frames: 2 });
    }

    // ---- the plan ----------------------------------------------------------------

    #[test]
    fn a_pass_never_asks_for_more_than_the_scarcest_input_has() {
        let comfortable = frames_in(DEVICE_FLOOR) + 100;
        // The least-supplied flowing input bounds the take.
        assert_eq!(
            plan(
                &[
                    Supply::Flowing { frames: 100 },
                    Supply::Flowing { frames: 700 }
                ],
                comfortable
            ),
            Pass::Take { frames: 100 }
        );
        // A tail bounds it the same way: its remainder is played out, not overdrawn.
        assert_eq!(
            plan(
                &[Supply::Tail { frames: 3 }, Supply::Flowing { frames: 700 }],
                comfortable
            ),
            Pass::Take { frames: 3 }
        );
        // An input between packets supplies nothing, so nothing is taken — the device's
        // queue carries the gap. This is the line the old loop crossed: it would have
        // emitted anyway and padded, which is #174's baseline defect.
        assert_eq!(
            plan(
                &[
                    Supply::Flowing { frames: 0 },
                    Supply::Flowing { frames: 700 }
                ],
                comfortable
            ),
            Pass::Sleep
        );
    }

    #[test]
    fn surfaces_and_quiet_inputs_constrain_nothing() {
        let comfortable = frames_in(DEVICE_FLOOR) + 100;
        // A never-fed surface alongside a playing source must not hold the mix at the
        // floor — this is the browser's input during every session.
        assert_eq!(
            plan(
                &[Supply::Surface, Supply::Flowing { frames: 700 }],
                comfortable
            ),
            Pass::Take {
                frames: frames_in(QUANTUM)
            }
        );
        // Likewise one that played once and went quiet.
        assert_eq!(
            plan(
                &[Supply::Quiescent, Supply::Flowing { frames: 700 }],
                comfortable
            ),
            Pass::Take {
                frames: frames_in(QUANTUM)
            }
        );
        // Alone, they supply nothing; above the floor there is nothing to do.
        assert_eq!(plan(&[Supply::Surface], comfortable), Pass::Sleep);
    }

    #[test]
    fn silence_is_invented_only_at_the_floor() {
        // Above the floor with nothing to take: wait for audio rather than invent quiet.
        assert_eq!(
            plan(
                &[Supply::Flowing { frames: 0 }],
                frames_in(DEVICE_FLOOR) + 1
            ),
            Pass::Sleep
        );
        // At the floor the device must be fed, whatever the inputs have.
        assert_eq!(
            plan(&[Supply::Flowing { frames: 0 }], frames_in(DEVICE_FLOOR)),
            Pass::Backstop
        );
        // Even with no inputs able to speak at all: an open device is kept fed.
        assert_eq!(plan(&[Supply::Surface], 0), Pass::Backstop);
        assert_eq!(plan(&[], 0), Pass::Backstop);
    }

    #[test]
    fn the_device_ceiling_caps_the_take() {
        // A full device takes nothing, however much is banked; that is what lets the
        // write-side budget mean anything.
        let full = frames_in(DEVICE_LEAD);
        assert_eq!(plan(&[Supply::Flowing { frames: 9600 }], full), Pass::Sleep);
        // Just below it, the take is the headroom, not the bank and not the quantum.
        assert_eq!(
            plan(&[Supply::Flowing { frames: 9600 }], full - 7),
            Pass::Take { frames: 7 }
        );
    }

    // ---- executing a pass --------------------------------------------------------

    #[test]
    fn two_sources_are_summed_rather_than_one_replacing_the_other() {
        // The whole point of the change: before this, two sessions each held their own
        // device and the OS did this — invisibly, and with no way to express a policy.
        let a = filled(vec![0.25; 8]);
        let b = filled(vec![0.5; 8]);
        let mixed = mix_pass(&[a, b], 4, Instant::now(), &Gain::default(), None);
        assert_eq!(mixed, vec![0.75; 8]);
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
    fn a_sum_that_leaves_the_unit_range_is_clipped_here_rather_than_in_the_backend() {
        // Two loud sources at once is exactly the case this mixer makes possible, and an
        // out-of-range f32 is anything from a clamp to a wrap depending on the backend.
        let a = filled(vec![0.8; 4]);
        let b = filled(vec![0.8; 4]);
        let mixed = mix_pass(&[a, b], 2, Instant::now(), &Gain::default(), None);
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
        let mixed = mix_pass(&[a, b], 2, Instant::now(), &gain, None);
        for sample in mixed {
            assert!((sample - 0.25).abs() < 1e-4, "{sample}");
        }
    }

    #[test]
    fn muting_silences_the_mix_without_disturbing_the_level() {
        let gain = Gain::default();
        gain.set(Volume::from_dbfs(0.75f32.log10() * 20.0));
        gain.set_muted(true);
        let mixed = mix_pass(&[filled(vec![1.0; 4])], 2, Instant::now(), &gain, None);
        assert_eq!(mixed, vec![0.0; 4]);
        assert!(
            (gain.level() - 0.75).abs() < 1e-4,
            "the level survives a mute"
        );
    }

    #[test]
    fn a_shortfall_is_padded_in_front_so_a_late_source_defers_rather_than_stutters() {
        // A backstop pass against an input with less than the pass: the silence leads and
        // the audio trails, so the stream is pushed later — latency, once — instead of a
        // hole being cut into it. Every AirPlay sender measured on #175 under-delivers
        // for its first seconds while intending the receiver to bank a lead; this is what
        // turns that ramp into the bank it intended.
        let starved = AtomicU64::new(0);
        let idle = AtomicU64::new(0);
        let drained = AtomicU64::new(0);
        let peak = AtomicU32::new(0);
        let short = filled(vec![1.0; 4]);
        let mixed = mix_pass(
            &[short],
            4,
            Instant::now(),
            &Gain::default(),
            Some(&counters(&starved, &idle, &drained, &peak)),
        );
        assert_eq!(mixed, vec![0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!(
            starved.load(Ordering::Relaxed),
            2,
            "half the pass was invented and has to say so"
        );
        assert_eq!(drained.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn an_input_that_went_quiet_is_idle_not_starved() {
        // The reading that dominated #175: the browser's input plays a page once, goes
        // quiet, and every counter after that is ~100% "starved" for as long as the panel
        // runs — two runs measured it before it was recognised. A quiet input is not a
        // starving one, and the defect counter must not say it is.
        let starved = AtomicU64::new(0);
        let idle = AtomicU64::new(0);
        let drained = AtomicU64::new(0);
        let peak = AtomicU32::new(0);
        let quiet = state_with(Ring {
            last_fed: Some(Instant::now() - STALE_AFTER * 4),
            samples: VecDeque::new(),
            closed: false,
        });
        let playing = filled(vec![0.5; 960]);
        let mixed = mix_pass(
            &[quiet, playing],
            480,
            Instant::now(),
            &Gain::default(),
            Some(&counters(&starved, &idle, &drained, &peak)),
        );
        assert_eq!(mixed, vec![0.5; 960], "the playing source is untouched");
        assert_eq!(
            starved.load(Ordering::Relaxed),
            0,
            "a source with nothing to say is not being starved of anything"
        );
        assert_eq!(idle.load(Ordering::Relaxed), 480, "counted, as idle");
        assert_eq!(drained.load(Ordering::Relaxed), 480);
    }

    #[test]
    fn a_tails_runout_is_the_end_of_a_session_not_a_fault() {
        let starved = AtomicU64::new(0);
        let idle = AtomicU64::new(0);
        let drained = AtomicU64::new(0);
        let peak = AtomicU32::new(0);
        let state = state_with(Ring {
            last_fed: Some(Instant::now()),
            samples: vec![1.0; 8].into(),
            closed: true,
        });
        let mixed = mix_pass(
            &[state],
            8,
            Instant::now(),
            &Gain::default(),
            Some(&counters(&starved, &idle, &drained, &peak)),
        );
        // The tail plays out — at the end of the pass, deferred like any shortfall.
        assert_eq!(
            &mixed[8..],
            vec![1.0; 8],
            "the last moment reaches the speakers"
        );
        assert_eq!(
            starved.load(Ordering::Relaxed),
            0,
            "running out past the end is not starvation"
        );
        assert_eq!(idle.load(Ordering::Relaxed), 4);
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
        let _ = mix_pass(
            &[Arc::clone(&state)],
            480,
            Instant::now(),
            &Gain::default(),
            None,
        );
        assert!(
            live_inputs(&shared).is_empty(),
            "closed and drained, so it should be gone"
        );
    }

    // ---- the identities ----------------------------------------------------------

    /// What the writer gave equals what the mixer took plus what was thrown away plus
    /// what is still sitting in the ring.
    ///
    /// This test exists because its absence is the methodological fault behind a day of
    /// wrong answers on #175. Every quantity that mattered was *derived* by subtracting
    /// counters, each subtraction carried an assumption about a term nobody measured, and
    /// each assumption was eventually false. Measure the terms; do not infer them.
    #[test]
    fn what_was_written_is_what_was_drained_plus_what_is_left() {
        let starved = AtomicU64::new(0);
        let idle = AtomicU64::new(0);
        let drained = AtomicU64::new(0);
        let peak = AtomicU32::new(0);
        // No mixer thread: this asserts an identity, so a second drainer racing the
        // arithmetic would make it assert nothing.
        let state = state_with(Ring::default());
        let shared = shared_with(vec![Arc::clone(&state)]);
        let mut input = MixInput {
            backpressure: Backpressure::Pull,
            state: Arc::clone(&state),
            shared: Arc::clone(&shared),
            convert: None,
            shape: None,
            dropped: 0,
            shed: 0,
        };
        for _ in 0..4 {
            input.write(&tone(480, 0.5)).unwrap();
        }
        let written = shared.written.load(Ordering::Relaxed);
        assert_eq!(written, 4 * 480, "the writer's own count, not a deduction");

        let _ = mix_pass(
            &[Arc::clone(&state)],
            500,
            Instant::now(),
            &shared.gain,
            Some(&counters(&starved, &idle, &drained, &peak)),
        );
        let took = drained.load(Ordering::Relaxed);
        let left = state.ring.lock().unwrap().samples.len() as u64 / u64::from(CHANNELS);
        let shed = shared.shed.load(Ordering::Relaxed);
        assert_eq!(
            written,
            took + shed + left,
            "written={written} took={took} shed={shed} left={left}"
        );
    }

    /// The measurement no frame count can make (#178).
    ///
    /// Perfect flow — every frame written, drained and emitted at real time, nothing
    /// starved and nothing shed — is what a page attenuated to −30 dBFS looks like *and*
    /// what a page handing over digital silence looks like. They were the same log line
    /// for a session. Separating them is this counter's whole job, which is why it is
    /// taken after the gain and why "quiet" and "nothing" must not read alike.
    #[test]
    fn the_report_says_whether_the_speakers_got_signal_or_only_frames() {
        /// One pass over one full input, answering with the peak it recorded.
        fn pass(gain: &Gain, samples: Vec<f32>) -> f32 {
            let (starved, idle) = (AtomicU64::new(0), AtomicU64::new(0));
            let (drained, peak) = (AtomicU64::new(0), AtomicU32::new(0));
            let frames = samples.len() as u64 / u64::from(CHANNELS);
            let _ = mix_pass(
                &[filled(samples)],
                frames,
                Instant::now(),
                gain,
                Some(&counters(&starved, &idle, &drained, &peak)),
            );
            measured(&peak)
        }

        let unity = Gain::default();
        assert_eq!(pass(&unity, vec![1.0; 8]), 1.0);
        assert_eq!(dbfs(pass(&unity, vec![1.0; 8])), "0.0", "full scale");

        // Digital silence at full volume: the source has nothing, and the line has to be
        // able to say so rather than looking like a healthy 100% of real time.
        assert_eq!(pass(&unity, vec![0.0; 8]), 0.0);
        assert_eq!(dbfs(pass(&unity, vec![0.0; 8])), "-inf", "nothing at all");

        // The #178 reading, exactly: full-scale audio arriving, half a slider of a 60 dB
        // taper applied to it, and a mix that measures 30 dB down.
        let half = Gain::default();
        half.set(Volume::from_position(0.5));
        assert_eq!(
            dbfs(pass(&half, vec![1.0; 8])),
            "-30.0",
            "signal that arrived and was attenuated is not signal that never arrived"
        );

        // Mute is the same mechanism, so it reads as the same measurement.
        let muted = Gain::default();
        muted.set_muted(true);
        assert_eq!(dbfs(pass(&muted, vec![1.0; 8])), "-inf");
    }

    #[test]
    fn the_report_reads_the_counters_rather_than_taking_them() {
        // #204. Every figure on the report used to be `swap`ped to zero as it was read,
        // which made the five-second log line the only thing in the process that could
        // ever see one: a test asserting on invented silence would have been racing the
        // reporter for the number, and whichever won would have taken it from the other.
        // That is why #175's counters landed and #204 still had nothing to assert on.
        let shared = shared_with(vec![]);
        shared.written.store(1000, Ordering::Relaxed);
        shared.drained.store(990, Ordering::Relaxed);
        shared.starved.store(10, Ordering::Relaxed);
        shared.emitted.store(1000, Ordering::Relaxed);

        let start = Instant::now();
        let mut last = Reported {
            at: start,
            counters: MixerCounters::default(),
            underruns: 0,
        };
        report(&shared, start + REPORT_EVERY, &mut last, 1, &Sink::silent());

        let after = shared.counters();
        assert_eq!(
            after.written, 1000,
            "the reporter consumed the total it printed"
        );
        assert_eq!(
            last.counters, after,
            "a window moves its own floor, and nothing else"
        );

        // A second window therefore carries what happened *in* it, not the life of the
        // mixer — which is the property the swap was there to get, kept without the cost.
        shared.written.fetch_add(500, Ordering::Relaxed);
        shared.emitted.fetch_add(500, Ordering::Relaxed);
        let later = shared.counters();
        let window = later.since(&last.counters);
        assert_eq!(window.written, 500, "the window is a difference");
        assert_eq!(window.starved, 0, "and nothing starved in it");

        // And the quantity #175 is about is now a fraction anyone can read, rather than
        // something inferred from how long a fixture took to play.
        let invented = later.invented().expect("something was emitted");
        assert!(
            (invented - 10.0 / 1500.0).abs() < 1e-9,
            "invented silence read as {invented} of everything the device was given"
        );
        assert_eq!(
            MixerCounters::default().invented(),
            None,
            "a mixer that never emitted has not starved; it has not run"
        );
    }

    // ---- the whole machine, against real-time devices -----------------------------

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

    /// A producer on its own clock, delivering in packets — the shape every previous test
    /// here lacked, and the reason four of them measured 100% while a phone lost audio.
    ///
    /// A `while { write() }` producer is flow-controlled by the thing under test: it can
    /// never get ahead of the mixer and never fall behind it, so it exercises the average
    /// rate and nothing else. A phone does neither favour. It fires one packet's worth of
    /// audio on its own deadline schedule — 352 frames of 44.1 kHz every ~8 ms is AirPlay's
    /// ALAC cadence — and the mixer's passes fall where they fall between those packets.
    /// The defect this exists to catch is the mixer padding those gaps with silence:
    /// output that is real audio and holes interleaved, which no average-rate measurement
    /// can distinguish from perfect playback (#175).
    #[test]
    fn a_packet_source_on_its_own_clock_is_heard_without_holes() {
        const SOURCE_RATE: u32 = 44_100;
        /// One ALAC packet, as AirPlay sends it.
        const PACKET: usize = 352;
        /// Long enough to open the device and settle into steady state.
        const WARMUP: Duration = Duration::from_millis(1000);
        const WINDOW: Duration = Duration::from_millis(3000);

        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Live);
        input.format(SOURCE_RATE, CHANNELS).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let producer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let block = PcmBlock {
                    sample_rate: SOURCE_RATE,
                    channels: CHANNELS,
                    samples: vec![0.5; PACKET * usize::from(CHANNELS)],
                    pts: Duration::ZERO,
                };
                let period =
                    Duration::from_nanos(1_000_000_000u64 * PACKET as u64 / u64::from(SOURCE_RATE));
                let mut sent = 0u64;
                let started = Instant::now();
                let mut next = started;
                while !stop.load(Ordering::Relaxed) {
                    input.write(&block).unwrap();
                    sent += 1;
                    next += period;
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                }
                (sent, started.elapsed())
            })
        };

        std::thread::sleep(WARMUP);
        let from = device.heard.lock().unwrap().len();
        std::thread::sleep(WINDOW);
        let heard = device.heard.lock().unwrap()[from..].to_vec();
        stop.store(true, Ordering::Relaxed);
        let (sent, producing) = producer.join().unwrap();

        // The harness's own honesty first: a producer that was parked could not keep its
        // schedule, and everything below would then measure the parking, not the mixing.
        let offered = sent as f64 * PACKET as f64 / producing.as_secs_f64();
        assert!(
            offered > f64::from(SOURCE_RATE) * 0.97,
            "the producer only kept {offered:.0} frames/s of its {SOURCE_RATE} Hz schedule; \
             it was made to wait, so the measurement below means nothing"
        );

        let frames = heard.chunks_exact(usize::from(CHANNELS));
        let total = frames.len();
        let audible = frames.filter(|f| f.iter().any(|s| *s != 0.0)).count();
        #[allow(clippy::cast_precision_loss)]
        let carried = audible as f64 / total as f64;
        println!(
            "{audible}/{total} device frames carried audio ({:.1}%)",
            carried * 100.0
        );
        assert!(
            carried > 0.97,
            "only {:.1}% of the output carried audio against a source arriving in packets \
             at exactly real time; the rest is silence the mixer padded into the gaps \
             between packets",
            carried * 100.0
        );
    }

    /// A sender that starts slow is deferred, not perforated.
    ///
    /// Every AirPlay sender measured on #175 delivered ~20–45% of real time for its first
    /// five seconds and exactly real time after — four sessions, same shape — while
    /// declaring `sender_latency_frames=77175`: it *intends* the receiver to run 1.75 s
    /// behind, and the ramp is how the gap opens. A drain that pads eagerly plays that
    /// ramp as audio riddled with holes. Front-padded backstops instead convert the ramp
    /// deficit into deferral once, so the steady state that follows is continuous.
    #[test]
    fn a_sender_that_ramps_up_is_deferred_not_riddled() {
        const SOURCE_RATE: u32 = 44_100;
        const PACKET: usize = 352;
        /// The ramp: packets at a third of real time.
        const RAMP: Duration = Duration::from_millis(1500);
        const WINDOW: Duration = Duration::from_millis(3000);

        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Live);
        input.format(SOURCE_RATE, CHANNELS).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let producer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let block = PcmBlock {
                    sample_rate: SOURCE_RATE,
                    channels: CHANNELS,
                    samples: vec![0.5; PACKET * usize::from(CHANNELS)],
                    pts: Duration::ZERO,
                };
                let period =
                    Duration::from_nanos(1_000_000_000u64 * PACKET as u64 / u64::from(SOURCE_RATE));
                let started = Instant::now();
                let mut next = started;
                while !stop.load(Ordering::Relaxed) {
                    input.write(&block).unwrap();
                    // A third of real time during the ramp, real time after.
                    next += if started.elapsed() < RAMP {
                        period * 3
                    } else {
                        period
                    };
                    if let Some(wait) = next.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                }
            })
        };

        // Let the ramp finish, then measure the steady state that follows it.
        std::thread::sleep(RAMP + Duration::from_millis(500));
        let from = device.heard.lock().unwrap().len();
        std::thread::sleep(WINDOW);
        let heard = device.heard.lock().unwrap()[from..].to_vec();
        stop.store(true, Ordering::Relaxed);
        producer.join().unwrap();

        let frames = heard.chunks_exact(usize::from(CHANNELS));
        let total = frames.len();
        let audible = frames.filter(|f| f.iter().any(|s| *s != 0.0)).count();
        #[allow(clippy::cast_precision_loss)]
        let carried = audible as f64 / total as f64;
        println!(
            "post-ramp: {audible}/{total} device frames carried audio ({:.1}%)",
            carried * 100.0
        );
        assert!(
            carried > 0.97,
            "only {:.1}% of the post-ramp output carried audio; the ramp's deficit should \
             have become deferral, not recurring holes",
            carried * 100.0
        );
    }

    #[test]
    fn a_live_source_is_never_made_to_wait() {
        let device = Recorder::new();
        let mixer = mixer_with(&device);
        let mut input = mixer.input(Backpressure::Live);
        let start = Instant::now();
        // 100 blocks of 50 ms: five seconds of audio, more than twice the budget.
        for _ in 0..100 {
            input.write(&tone(2400, 0.5)).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "a live source was parked for {elapsed:?} offering five seconds of audio; \
             its own queue fills for exactly that long, and it newest-drops"
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

    /// …and when the device comes *back*, sound comes back with it.
    ///
    /// The only device-vanish test in the tree used a factory that refuses forever, so it
    /// proved the session survives and nothing at all about recovery. The `REOPEN_AFTER` /
    /// `retry_at` loop — which is the fix for the panel sleeping and taking the HDMI sink
    /// with it (#55) — had no test where the device fails and *then* succeeds, and D52
    /// admits mid-session device selection is "only exercised by construction" (#204).
    ///
    /// Three separate things go wrong if the loop is wrong, and this fails on each:
    /// never retrying (silence forever), retrying every pass (a box with no sound card
    /// spends the session looking for one — caught by the attempt count), and letting the
    /// retry stall the sources (caught by `dropped` and by the elapsed time).
    #[test]
    fn a_device_that_comes_back_is_reopened_and_heard() {
        /// Refuses until `opens_at`, then hands over the recorder.
        struct ComesBack {
            opens_at: Instant,
            attempts: Arc<AtomicU64>,
            recorder: Arc<Recorder>,
            open: bool,
        }

        impl AudioOut for ComesBack {
            fn start(&mut self, rate: u32, channels: u16) -> Result<(), PipelineError> {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                if Instant::now() < self.opens_at {
                    return Err(PipelineError::Audio("the panel is asleep".into()));
                }
                self.open = true;
                Arc::clone(&self.recorder).start(rate, channels)
            }

            fn write(&mut self, block: &PcmBlock) -> Result<(), PipelineError> {
                if !self.open {
                    return Err(PipelineError::Audio("the panel is asleep".into()));
                }
                Arc::clone(&self.recorder).write(block)
            }

            fn stop(&mut self) {}

            fn frames_played(&self) -> Option<u64> {
                self.open
                    .then(|| Arc::clone(&self.recorder).frames_played())?
            }
        }

        // Asleep for two whole retry intervals, so the loop has to come back more than
        // once — a single retry would pass a shorter outage by luck.
        let outage = REOPEN_AFTER * 2;
        let opens_at = Instant::now() + outage;
        let attempts = Arc::new(AtomicU64::new(0));
        let recorder = Recorder::new();

        let mixer = AudioMixer::new(Arc::new({
            let (attempts, recorder) = (Arc::clone(&attempts), Arc::clone(&recorder));
            move || {
                Box::new(ComesBack {
                    opens_at,
                    attempts: Arc::clone(&attempts),
                    recorder: Arc::clone(&recorder),
                    open: false,
                })
            }
        }));

        // A source writing throughout, across the outage and out the other side. Pull, so
        // the mixer paces it — which is the case where a stalled reopen would show up as
        // the writer blocking rather than as silence.
        let mut input = mixer.input(Backpressure::Pull);
        let started = Instant::now();
        let deadline = opens_at + REOPEN_AFTER + Duration::from_millis(500);
        while Instant::now() < deadline {
            input.write(&tone(240, 0.5)).unwrap();
        }

        assert!(
            recorder.peak() > 0.4,
            "the device came back and nothing was heard (peak {})",
            recorder.peak()
        );

        // Nothing was dropped *throughout* — the outage is not allowed to cost the source
        // its audio, only its audibility.
        assert_eq!(
            input.dropped, 0,
            "a source was made to wait on a device that was not there"
        );

        // And the retries were paced. Without `retry_at` this loop spins on `open()` as
        // fast as it can pass, which on a box with no sound card is a core burnt for the
        // life of the session. A couple per `REOPEN_AFTER` is the shape; dozens is not.
        let tried = attempts.load(Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let budget =
            (started.elapsed().as_secs_f64() / REOPEN_AFTER.as_secs_f64()).ceil() as u64 + 2;
        assert!(
            tried <= budget,
            "opened the device {tried} times in {:?}; REOPEN_AFTER is {REOPEN_AFTER:?}, so \
             at most {budget} were paced",
            started.elapsed()
        );
        assert!(tried >= 2, "the device was never retried after it refused");
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
        std::thread::sleep(Duration::from_millis(400));
        let heard = device.heard();
        assert!(
            !heard.is_empty(),
            "a 44.1 kHz mono source produced nothing at the device"
        );
        // Counted over the *audible* part, not over everything the device was given: the
        // mixer emits silence when it must keep the device fed with nothing banked, which
        // is what an idle panel is. 100 ms in at 44.1 kHz mono is ~100 ms out at 48 kHz
        // stereo, give or take the resampler's delay line.
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
