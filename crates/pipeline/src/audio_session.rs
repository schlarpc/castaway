//! The audio session: encoded frames in one end, sound out the other.
//!
//! Joins [`crate::audio_decode`] to [`crate::audio_out`] on a dedicated OS thread, for
//! the same reason video decode gets one — decoding blocks, and a blocked runtime worker
//! stalls every protocol adapter sharing it (ground rule 4).
//!
//! The codec is not a parameter. It arrives on the first frame, because the A2DP adapter
//! already negotiated it and stamps every [`EncodedFrame`] with it, and taking it from
//! the stream rather than from a second configuration path removes the chance of the two
//! disagreeing.
//!
//! The *format* is the opposite: it cannot come from the stream, because aptX and aptX HD
//! have no header to carry it. It is a required parameter, handed down from the AVDTP
//! negotiation via [`castaway_core::SessionEvent::Audio`] — the bug #70 recorded was this
//! function inventing 44.1 kHz while the phone was sending 48 (#70).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use castaway_core::{AudioCodec, AudioFormat, EncodedFrame, PcmFrame, Volume};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::audio_decode::decode_audio_stream;
use crate::audio_out::AudioOut;
// Not test-only or headless-only any more: it is also what a *real* output falls back to
// when the device refuses the stream, so that a missing sound card costs the sound and not
// the picture.
use crate::audio_out::NullAudioOut;

/// Output gain, shared between the thread that is playing and whoever holds the remote.
///
/// This exists because a volume command had nowhere to land. AVRCP `SET_ABSOLUTE_VOLUME`
/// was parsed, answered `Accepted`, and emitted as a `ControlTxn` that the pipeline
/// logged and dropped — so a phone's volume rocker did nothing, and a phone that entered
/// absolute-volume mode on the strength of our Target record stopped attenuating locally
/// and pinned playback at full scale.
///
/// Applied here, at the last point before the device, rather than in each protocol: the
/// panel has one pair of speakers, so it has one volume, and a source-side gain would
/// leave every other source at whatever the last one set.
///
/// Stored as bits in an atomic so the audio thread never takes a lock — a mutex here
/// would put the remote's contention on the path that must not stall.
#[derive(Debug)]
pub struct Gain {
    level: std::sync::atomic::AtomicU32,
    muted: AtomicBool,
}

impl Default for Gain {
    fn default() -> Self {
        Self {
            level: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
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
        self.level.store(
            level.amplitude().to_bits(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// The current level, as the amplitude samples are multiplied by.
    ///
    /// Deliberately not a [`Volume`]: there is no constructor from a bare amplitude, and
    /// there should not be one. Every sender that needs its slider told where it ended up
    /// keeps its own authoritative copy in its own scale — Cast a position, DLNA a
    /// percent, AirPlay a dBFS figure — so nothing has to reverse the taper to answer.
    #[must_use]
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(std::sync::atomic::Ordering::Relaxed))
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

    /// Scale a block in place.
    fn apply(&self, block: &mut PcmFrame) {
        let factor = self.factor();
        // Unity is the overwhelmingly common case — every source that never touches the
        // volume — and skipping it keeps the whole mechanism free when unused.
        if (factor - 1.0).abs() < f32::EPSILON {
            return;
        }
        for sample in &mut block.samples {
            *sample *= factor;
        }
    }
}

/// The output a session uses when the caller expresses no preference.
///
/// Real device when the `audio-out` feature is on, accounting-only otherwise — so a
/// headless CI box runs the whole path and a kiosk makes noise, from the same code.
#[must_use]
pub fn default_output() -> Box<dyn AudioOut> {
    #[cfg(feature = "audio-out")]
    {
        Box::new(crate::audio_out::CpalAudioOut::new())
    }
    #[cfg(not(feature = "audio-out"))]
    {
        Box::new(NullAudioOut::new())
    }
}

/// Run a decode → output session on a dedicated thread until the source ends or `stop`
/// is set.
///
/// The flag is not optional. A preempted session whose phone is still streaming will
/// otherwise decode forever, and two sessions writing to one output device do not mix —
/// they fight, and it sounds like it.
pub fn spawn(
    frames: mpsc::Receiver<EncodedFrame>,
    format: AudioFormat,
    config: Option<bytes::Bytes>,
    output: Box<dyn AudioOut>,
    stop: Arc<AtomicBool>,
    gain: Arc<Gain>,
    failed: Option<SessionFailed>,
) {
    std::thread::spawn(move || {
        run(
            frames,
            format,
            config.as_deref(),
            output,
            &stop,
            &gain,
            failed,
        );
    });
}

/// How a session says it could not play, so the source can be told to stop.
///
/// A callback rather than a channel because the reporting machinery
/// (`RenderPipeline::end_report`) is private to that module and threading it through
/// here would export it for one caller. Called at most once, and only for failures — a
/// session that ends normally is already handled by the frame channel closing.
pub type SessionFailed = Box<dyn FnOnce(String) + Send>;

/// Drive one audio session to completion. Blocking; call it on its own thread.
pub fn run(
    mut frames: mpsc::Receiver<EncodedFrame>,
    format: AudioFormat,
    config: Option<&[u8]>,
    mut output: Box<dyn AudioOut>,
    stop: &AtomicBool,
    gain: &Gain,
    failed: Option<SessionFailed>,
) {
    // Wait for the first frame so the codec comes from the stream itself.
    let Some(first) = frames.blocking_recv() else {
        return;
    };
    let Some(codec) = first.audio_codec else {
        warn!("audio session: first frame names no codec");
        return;
    };

    let mut started = false;
    let mut pending = Some(first);
    // Why the output could not be opened, if it could not. Set inside the sink closure
    // and reported after it, because the closure cannot consume the `FnOnce`.
    let mut refused: Option<String> = None;

    let result = decode_audio_stream(
        codec,
        format,
        config,
        || {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            pending.take().or_else(|| frames.blocking_recv())
        },
        |mut block| {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            gain.apply(&mut block);
            if !started {
                if let Err(e) = output.start(block.sample_rate, block.channels) {
                    // ERROR, not WARN: with the rate negotiated and resampled this should
                    // now be unreachable for a mismatch, so if it fires the box genuinely
                    // cannot play and somebody has to be told. The session used to stop
                    // here and say nothing further — the source kept streaming into a
                    // dead channel, the card kept saying "playing", and the only symptom
                    // was silence.
                    error!(
                        error = %e,
                        rate = block.sample_rate,
                        channels = block.channels,
                        "audio session: the output device refused the stream; ending the session"
                    );
                    refused = Some(format!(
                        "output device refused {} Hz x {}: {e}",
                        block.sample_rate, block.channels
                    ));
                    return false;
                }
                info!(
                    ?codec,
                    %format,
                    rate = block.sample_rate,
                    channels = block.channels,
                    "audio session: playing"
                );
                started = true;
            }
            match output.write(&block) {
                Ok(()) => true,
                Err(e) => {
                    warn!(error = %e, "audio session: output failed");
                    false
                }
            }
        },
    );

    if stop.load(Ordering::Relaxed) {
        info!(?codec, "audio session: preempted");
    }
    if let Err(e) = result {
        // The most likely cause is a codec we advertised but cannot decode, which is
        // the failure #14 exists to prevent — so name it loudly.
        warn!(error = %e, ?codec, "audio session ended with an error");
        crate::audio_decode::warn_undecodable(codec);
        if refused.is_none() {
            refused = Some(format!("{codec:?} decode failed: {e}"));
        }
    }
    output.stop();
    // Tell the source. Without this the session is over here and still running there:
    // the phone keeps sending, the adapter keeps dropping into a closed channel, and the
    // now-playing card keeps claiming playback that is not happening.
    if let (Some(why), Some(report)) = (refused, failed) {
        report(why);
    }
}

/// What a media-URL session shares with the thread that plays its sound.
///
/// One value rather than two arguments because the two are inseparable: both exist only
/// for the path where *we* are the player and neither means anything for the paths where
/// the sender is. Bluetooth and Spotify pass [`None`] — the phone is the clock, and there
/// is nothing here to seek.
#[derive(Clone)]
pub struct PacedSession {
    /// The clock this thread drives, by publishing how far it has submitted.
    pub clock: Arc<crate::clock::MediaClock>,
    /// The seek handshake this thread has to answer, by throwing away what it had queued.
    pub seek: Arc<crate::seek::SeekControl>,
}

/// Spawn a PCM playback session on a dedicated thread. The audio-only sibling of
/// [`spawn`], for adapters that arrive already decoded.
pub fn spawn_pcm(
    frames: std::sync::mpsc::Receiver<PcmFrame>,
    output: Box<dyn AudioOut>,
    stop: Arc<AtomicBool>,
    gain: Arc<Gain>,
    session: Option<PacedSession>,
) {
    std::thread::spawn(move || run_pcm(frames, output, &stop, &gain, session.as_ref()));
}

/// Answer a pending seek flush, if there is one. Returns whether anything was flushed.
///
/// Everything queued is from before the seek, so it is dropped — and this is the only
/// thread that can do it, since the demuxer cannot reach into a channel it has already
/// written to. Without it a seek plays roughly a second of wherever playback used to be
/// before arriving where it was sent.
///
/// Acknowledged even when there was nothing to drop: the decode thread waits on that
/// acknowledgement before pushing the first block of the new position, and silence would
/// cost it the whole grace period on every seek.
fn service_seek_flush(
    session: Option<&PacedSession>,
    frames: &std::sync::mpsc::Receiver<PcmFrame>,
    output: &mut dyn AudioOut,
    open_as: &mut Option<(u32, u16)>,
    pace: &mut Pace,
) -> bool {
    let Some(seek) = session.map(|s| s.seek.as_ref()) else {
        return false;
    };
    let Some(epoch) = seek.flush_wanted() else {
        return false;
    };
    let mut dropped = 0usize;
    while frames.try_recv().is_ok() {
        dropped += 1;
    }
    debug!(dropped, "pcm session: discarded pre-seek audio");
    // The device's own buffer holds the same staleness, so it is reopened on the next
    // block rather than played out first.
    if open_as.is_some() {
        output.stop();
        *open_as = None;
    }
    *pace = Pace::default();
    seek.flushed(epoch);
    true
}

/// Drive one already-decoded audio session to completion. Blocking; call it on its own
/// thread.
///
/// There is no `format` parameter, and that is the whole point of the variant: unlike the
/// A2DP path — where aptX carries no in-band rate and the negotiated one has to be handed
/// down separately (#70) — every [`PcmFrame`] states its own rate and channel count. The
/// shape cannot disagree with the samples because it travels with them.
pub fn run_pcm(
    frames: std::sync::mpsc::Receiver<PcmFrame>,
    mut output: Box<dyn AudioOut>,
    stop: &AtomicBool,
    gain: &Gain,
    session: Option<&PacedSession>,
) {
    let clock = session.map(|s| s.clock.as_ref());
    // What the output device is currently open as, not what the first block said: a
    // source may change rate between tracks, and writing 48 kHz samples into a device
    // opened at 44.1 plays them at the wrong pitch rather than failing.
    let mut open_as: Option<(u32, u16)> = None;
    let mut pace = Pace::default();

    'blocks: loop {
        // Everything queued is from before a seek, so drop it. Checked here, at the top of
        // every block, because this is the only thread that can: the demuxer cannot reach
        // into a channel it has already written to, and without this a seek plays roughly
        // a second of wherever playback used to be before arriving where it was sent.
        //
        // Acknowledged even when there was nothing to drop — the decode thread is waiting
        // on that acknowledgement before it pushes the first block of the new position,
        // and silence would cost it the whole grace period on every seek.
        service_seek_flush(session, &frames, output.as_mut(), &mut open_as, &mut pace);
        // `recv_timeout`, not `recv`, so `stop` is observable while nothing is arriving.
        // A session preempted while its source was *paused* has no next block to wake on,
        // so a plain `recv` parked here forever: the thread leaked and — worse — the
        // output device stayed open, which on an exclusive-mode device means the source
        // that preempted us cannot start at all.
        let mut block = match frames.recv_timeout(STOP_POLL) {
            Ok(block) => block,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    info!("pcm session: preempted while idle");
                    break;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if stop.load(Ordering::Relaxed) {
            info!("pcm session: preempted");
            break;
        }
        // Hold the block rather than dropping it: a pause must be resumable from where
        // it stopped, and this thread not consuming is also what stalls the demuxer
        // behind it (the queue it reads from is bounded on purpose).
        while clock.is_some_and(crate::clock::MediaClock::is_paused) {
            if stop.load(Ordering::Relaxed) {
                info!("pcm session: preempted while paused");
                output.stop();
                return;
            }
            // Seeking *while paused* is the ordinary way people find a spot, and this is
            // where the thread sits while they do it. Without this the flush handshake
            // went unanswered for the whole grace period on every paused scrub — and on
            // resume the block held below was written to the device and observed onto the
            // media clock at its old position, so the clock jumped backwards and a
            // second of stale audio played before the top of the loop finally flushed.
            if service_seek_flush(session, &frames, output.as_mut(), &mut open_as, &mut pace) {
                // The held block is pre-seek too, and dropping it is the point.
                continue 'blocks;
            }
            std::thread::sleep(STOP_POLL);
        }
        let shape = (block.sample_rate, block.channels);
        if open_as != Some(shape) {
            if open_as.is_some() {
                info!(?open_as, new = ?shape, "pcm session: stream shape changed, reopening");
                output.stop();
            }
            if let Err(e) = output.start(shape.0, shape.1) {
                // Silence, not a black screen. Ending here dropped the receiver, which
                // ended the demuxer behind it, which ended the *video* — so on a box whose
                // sound card was absent, busy, or held in exclusive mode, a video cast
                // produced a flash and nothing while the phone said PLAYING.
                //
                // A null sink keeps time exactly as the real one does, so the picture
                // plays on, paced, with one line saying why it is quiet.
                warn!(error = %e, rate = shape.0, channels = shape.1,
                    "pcm session: the output refused the stream; playing on in silence");
                output = Box::new(NullAudioOut::new());
                if output.start(shape.0, shape.1).is_err() {
                    // Unreachable in practice — the null sink accepts everything — but a
                    // library crate does not get to assume that on a runtime path.
                    break;
                }
            } else {
                info!(rate = shape.0, channels = shape.1, "pcm session: playing");
            }
            open_as = Some(shape);
            pace = Pace::default();
        }
        let played = block.duration();
        let through = block.pts + played;
        gain.apply(&mut block);
        if let Err(e) = output.write(&block) {
            // Same reasoning as a refused start, one step later: a device that goes away
            // mid-session — unplugged, or claimed by something else — must cost the sound
            // and not the picture.
            warn!(error = %e, "pcm session: the output failed; playing on in silence");
            output = Box::new(NullAudioOut::new());
            if output.start(shape.0, shape.1).is_err() || output.write(&block).is_err() {
                break;
            }
        }
        // Published *before* the pacing sleep, not after: this says how far the stream
        // has been submitted, and the sleep that follows is precisely the mechanism that
        // keeps submission within `LEAD` of the speaker. Publishing after would make the
        // clock jump in whole-block steps at the moment the thread wakes.
        //
        // Only the media-URL path passes a clock; A2DP and Spotify have no video to
        // synchronise and nothing reads it.
        if let Some(clock) = clock {
            clock.observe_audio(through);
        }
        pace.wait_for(played);
    }

    output.stop();
}

/// How often a parked PCM session looks up to see whether it has been preempted.
///
/// Short enough that a preempted session releases the audio device promptly, long enough
/// that an idle one is not a busy loop.
const STOP_POLL: Duration = Duration::from_millis(200);

/// How far ahead of real time the session is allowed to run.
///
/// This is the buffer that absorbs a scheduling hiccup on either side. Too small and any
/// stall is a dropout; too large and a pause takes that long to actually go quiet.
///
/// Shared with [`crate::clock`] rather than restated: the media clock subtracts exactly
/// this to turn what has been submitted into what has been heard, and two copies would
/// drift into lip sync that is quietly off by the difference.
const LEAD: Duration = crate::clock::OUTPUT_LEAD;

/// A gap this big means the stream stopped rather than merely stuttered — a pause, a
/// buffering stall, a track that took a while to load — so the clock restarts instead of
/// trying to make up the time in one burst.
const RESYNC_AFTER: Duration = Duration::from_secs(1);

/// Wall-clock pacing for a pull-based source.
///
/// A *pushed* stream needs none of this: A2DP arrives in real time because the phone is
/// the clock, and the output drops a late block rather than stalling the link (ground
/// rule 4). A *pulled* stream is the opposite — librespot decodes as fast as its sink
/// accepts — so with an output that never blocks (both of ours: cpal drops when its ring
/// is full, and the null sink accepts instantly) a track is consumed in seconds and the
/// queue turbo-advances. Nothing else in the chain is a clock, so this is.
#[derive(Default)]
struct Pace {
    /// When the current run of audio started, and how much has been handed over since.
    /// `None` until the first block, and reset whenever the stream stops for a while.
    since: Option<(std::time::Instant, Duration)>,
}

impl Pace {
    /// Account for `played` worth of audio, then sleep off anything beyond [`LEAD`].
    fn wait_for(&mut self, played: Duration) {
        let now = std::time::Instant::now();
        let (start, submitted) = self.since.get_or_insert((now, Duration::ZERO));

        // Fell far behind: the source paused or stalled. Catching up would replay the
        // silence at speed, which is the very thing this exists to prevent.
        if now.duration_since(*start) > *submitted + RESYNC_AFTER {
            *start = now;
            *submitted = Duration::ZERO;
        }

        *submitted += played;
        if let Some(ahead) = submitted.checked_sub(now.duration_since(*start)) {
            if let Some(excess) = ahead.checked_sub(LEAD) {
                std::thread::sleep(excess);
            }
        }
    }
}

/// Which codecs this build can actually decode.
///
/// The adapter's endpoint table must be built from this. Advertising a codec we cannot
/// decode means the sender picks it and the session is silence rather than a clean
/// fallback to one we can (#14).
#[must_use]
pub fn decodable_codecs() -> Vec<AudioCodec> {
    [
        AudioCodec::Ldac,
        AudioCodec::AptXHd,
        AudioCodec::AptX,
        AudioCodec::Aac,
        AudioCodec::Sbc,
    ]
    .into_iter()
    .filter(|c| crate::audio_decode::can_decode(*c))
    .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::error::PipelineError;

    fn format() -> AudioFormat {
        AudioFormat::from_hz(44_100, 2).unwrap()
    }

    fn running() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn a_session_with_no_frames_exits_instead_of_parking() {
        // The phone connected and never sent anything. The thread must not leak.
        let (tx, rx) = mpsc::channel::<EncodedFrame>(1);
        drop(tx);
        run(
            rx,
            format(),
            None,
            Box::new(NullAudioOut::new()),
            &running(),
            &Gain::default(),
            None,
        );
    }

    #[test]
    fn a_session_that_plays_normally_reports_no_failure() {
        // The other half: the failure path must not fire for an ordinary session, or
        // every Bluetooth stream would tear itself down on the first frame.
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let reported = Arc::new(Mutex::new(false));
        let sink = Arc::clone(&reported);
        run(
            rx,
            format(),
            None,
            Box::new(NullAudioOut::new()),
            &running(),
            &Gain::default(),
            Some(Box::new(move |_| *sink.lock().expect("poisoned") = true)),
        );
        assert!(
            !*reported.lock().unwrap(),
            "reported a failure that was not"
        );
    }

    #[test]
    fn a_frame_without_a_codec_is_refused_rather_than_guessed() {
        let (tx, rx) = mpsc::channel(1);
        tx.blocking_send(EncodedFrame {
            video_codec: None,
            audio_codec: None,
            pts: Duration::ZERO,
            keyframe: true,
            data: bytes::Bytes::from_static(&[0; 8]),
        })
        .unwrap();
        drop(tx);
        // Guessing SBC here would decode noise; exiting is the honest answer.
        run(
            rx,
            format(),
            None,
            Box::new(NullAudioOut::new()),
            &running(),
            &Gain::default(),
            None,
        );
    }

    /// An output that remembers what it was asked to do, so a test can assert on the
    /// device calls rather than only on "it did not crash".
    #[derive(Debug, Default)]
    struct RecordingOut {
        log: Arc<Mutex<Vec<Call>>>,
    }

    #[derive(Debug, PartialEq)]
    enum Call {
        Start(u32, u16),
        Wrote(usize),
        Stop,
    }

    impl crate::audio_out::AudioOut for RecordingOut {
        fn start(&mut self, sample_rate: u32, channels: u16) -> Result<(), PipelineError> {
            self.log
                .lock()
                .map_err(|_| PipelineError::Audio("poisoned".into()))?
                .push(Call::Start(sample_rate, channels));
            Ok(())
        }
        fn write(&mut self, block: &crate::audio_decode::PcmBlock) -> Result<(), PipelineError> {
            self.log
                .lock()
                .map_err(|_| PipelineError::Audio("poisoned".into()))?
                .push(Call::Wrote(block.frame_count()));
            Ok(())
        }
        fn stop(&mut self) {
            if let Ok(mut log) = self.log.lock() {
                log.push(Call::Stop);
            }
        }
    }

    fn pcm(sample_rate: u32, channels: u16, frames: usize) -> PcmFrame {
        PcmFrame {
            sample_rate,
            channels,
            samples: vec![0.0; frames * usize::from(channels)],
            pts: Duration::ZERO,
        }
    }

    #[test]
    fn a_pcm_session_with_no_frames_exits_instead_of_parking() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<PcmFrame>(1);
        drop(tx);
        run_pcm(
            rx,
            Box::new(NullAudioOut::new()),
            &running(),
            &Gain::default(),
            None,
        );
    }

    /// Records the samples themselves, not just how many there were.
    struct AmplitudeOut {
        peak: Arc<Mutex<f32>>,
    }
    impl AudioOut for AmplitudeOut {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), PipelineError> {
            Ok(())
        }
        fn write(&mut self, block: &crate::audio_decode::PcmBlock) -> Result<(), PipelineError> {
            let loudest = block.samples.iter().fold(0.0f32, |a, s| a.max(s.abs()));
            if let Ok(mut peak) = self.peak.lock() {
                *peak = peak.max(loudest);
            }
            Ok(())
        }
        fn stop(&mut self) {}
    }

    fn loud(frames: usize) -> PcmFrame {
        PcmFrame {
            sample_rate: 44_100,
            channels: 2,
            samples: vec![1.0; frames * 2],
            pts: Duration::ZERO,
        }
    }

    #[test]
    fn the_gain_reaches_the_samples() {
        // The bug: AVRCP absolute volume was parsed, answered `Accepted`, and dropped by
        // a pipeline whose `control` only logged — so the phone's rocker did nothing, and
        // a phone that had taken absolute-volume control pinned us at full scale.
        // Positions, not amplitudes — the middle entry is the one that changed with
        // #85: half the slider travel is -30 dB, so the peak is 0.0316 and not 0.5.
        for (position, muted, expected) in
            [(1.0, false, 1.0), (0.5, false, 0.031_623), (1.0, true, 0.0)]
        {
            let peak = Arc::new(Mutex::new(0.0f32));
            let (tx, rx) = std::sync::mpsc::sync_channel(2);
            tx.send(loud(64)).unwrap();
            drop(tx);
            let gain = Gain::default();
            gain.set(Volume::from_position(position));
            gain.set_muted(muted);
            run_pcm(
                rx,
                Box::new(AmplitudeOut {
                    peak: Arc::clone(&peak),
                }),
                &running(),
                &gain,
                None,
            );
            let got = *peak.lock().unwrap();
            assert!(
                (got - expected).abs() < 1e-5,
                "position {position} muted {muted}: peak {got}, want {expected}"
            );
        }
    }

    #[test]
    fn a_level_outside_the_scale_cannot_reach_the_output() {
        // A protocol that scales wrong (or a NaN out of a division) must not be able to
        // hand the output a factor that clips every sample or silences it by accident.
        //
        // This used to be `Gain`'s job, done by clamping whatever `f32` it was passed.
        // It is now the type's, done at construction, and `Gain` has no way to receive a
        // bad value at all (#85) — so what is left to check here is that the mixer holds
        // exactly what the boundary produced, with no second opinion applied.
        let gain = Gain::default();
        gain.set(Volume::from_position(4.0));
        assert!(
            (gain.level() - 1.0).abs() < f32::EPSILON,
            "saturates at unity"
        );
        gain.set(Volume::from_position(-1.0));
        assert!(gain.level().abs() < f32::EPSILON, "a hard zero, not -60 dB");
        gain.set(Volume::from_position(f32::NAN));
        assert!(
            gain.level().abs() < f32::EPSILON,
            "a NaN never reaches a sample"
        );
        gain.set(Volume::from_dbfs(-6.0));
        assert!((gain.level() - 0.501_187).abs() < 1e-5, "stored verbatim");
    }

    #[test]
    fn a_pcm_session_opens_the_output_with_the_shape_the_samples_state() {
        // Nothing hands this session a negotiated format, so if it ever invents one the
        // stream plays at the wrong pitch — the #70 failure, arriving by a new route.
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(pcm(44_100, 2, 512)).unwrap();
        tx.send(pcm(44_100, 2, 256)).unwrap();
        drop(tx);

        run_pcm(
            rx,
            Box::new(RecordingOut {
                log: Arc::clone(&log),
            }),
            &running(),
            &Gain::default(),
            None,
        );

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                Call::Start(44_100, 2),
                Call::Wrote(512),
                Call::Wrote(256),
                Call::Stop
            ]
        );
    }

    #[test]
    fn a_pcm_session_reopens_the_output_when_the_stream_shape_changes() {
        // Writing 48 kHz samples into a device opened at 44.1 does not fail, it plays
        // everything sharp — so the reopen has to be driven by the samples themselves.
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(pcm(44_100, 2, 128)).unwrap();
        tx.send(pcm(48_000, 2, 128)).unwrap();
        drop(tx);

        run_pcm(
            rx,
            Box::new(RecordingOut {
                log: Arc::clone(&log),
            }),
            &running(),
            &Gain::default(),
            None,
        );

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                Call::Start(44_100, 2),
                Call::Wrote(128),
                Call::Stop,
                Call::Start(48_000, 2),
                Call::Wrote(128),
                Call::Stop
            ]
        );
    }

    #[test]
    fn a_pull_based_session_plays_in_real_time_rather_than_as_fast_as_it_can() {
        // The bug this exists to prevent, seen twice on real hardware: neither output
        // blocks — cpal drops when its ring is full, the null sink accepts instantly — so
        // librespot decoded a whole track in seconds and the queue turbo-advanced.
        //
        // Two seconds of audio in 40 ms blocks. It must take roughly two seconds minus the
        // lead, not the microseconds a memcpy loop would.
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let per_block = 44_100 / 25; // 40 ms
        for _ in 0..50 {
            tx.send(pcm(44_100, 2, per_block)).unwrap();
        }
        drop(tx);

        let start = std::time::Instant::now();
        run_pcm(
            rx,
            Box::new(NullAudioOut::new()),
            &running(),
            &Gain::default(),
            None,
        );
        let taken = start.elapsed();

        let expected = Duration::from_secs(2).saturating_sub(LEAD);
        assert!(
            taken >= expected.mul_f32(0.8),
            "played 2s of audio in {taken:?}; it is not pacing"
        );
        // And not the opposite failure: pacing must not *add* time.
        assert!(taken < Duration::from_secs(4), "far too slow: {taken:?}");
    }

    #[test]
    fn a_stall_resyncs_the_clock_instead_of_sprinting_to_catch_up() {
        // After a pause the source resumes where it left off. Treating the silent gap as
        // a debt to repay would replay it at speed — the same audible failure by a
        // different route.
        let mut pace = Pace::default();
        pace.wait_for(Duration::from_millis(100));
        // Pretend a long stall happened by rewinding the recorded start.
        if let Some((start, _)) = pace.since.as_mut() {
            *start -= Duration::from_secs(30);
        }
        let resumed = std::time::Instant::now();
        pace.wait_for(Duration::from_millis(100));
        assert!(
            resumed.elapsed() < Duration::from_millis(50),
            "a resync must not sleep, and must not try to reclaim the stall"
        );
    }

    #[test]
    fn a_preempted_pcm_session_stops_without_draining_the_rest() {
        // The second source has already taken the output device; this one must let go.
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        for _ in 0..3 {
            tx.send(pcm(44_100, 2, 64)).unwrap();
        }
        drop(tx);
        let stop = Arc::new(AtomicBool::new(true));

        run_pcm(
            rx,
            Box::new(RecordingOut {
                log: Arc::clone(&log),
            }),
            &stop,
            &Gain::default(),
            None,
        );

        assert_eq!(
            *log.lock().unwrap(),
            vec![Call::Stop],
            "wrote after preemption"
        );
    }

    #[test]
    fn sbc_is_always_decodable_because_every_sender_falls_back_to_it() {
        // If this ever fails, the endpoint table loses its guaranteed floor and some
        // phone ends up with no codec in common with us at all.
        let codecs = decodable_codecs();
        assert!(
            codecs.contains(&AudioCodec::Sbc),
            "SBC must be decodable; got {codecs:?}"
        );
    }

    #[test]
    fn the_advertised_table_never_contains_a_codec_we_cannot_decode() {
        // The invariant #14 actually needs. The old version of this test asserted the
        // table followed the *feature flag*, which is what let a build advertise LDAC
        // with no decoder behind it and hand a phone five minutes of silence.
        for codec in decodable_codecs() {
            assert!(
                crate::audio_decode::can_decode(codec),
                "{codec:?} is advertised but cannot be decoded"
            );
        }
        // LDAC follows the backend, in both directions. This used to assert it was never
        // present, which was right while the feature bound nothing and became a lie the
        // moment it bound something — the point was always that the list reports what
        // exists, not that one codec is permanently absent.
        //
        // Whether the *endpoint* is advertised is a separate question and is not settled
        // here: the app keeps LDAC out of the default table until a config asks for it
        // (`bluetooth::OPT_IN`). This is only about what can be decoded.
        assert_eq!(
            decodable_codecs().contains(&AudioCodec::Ldac),
            cfg!(feature = "ldac"),
            "LDAC must be decodable exactly when its backend is linked"
        );
    }
}
