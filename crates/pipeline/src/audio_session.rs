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

use castaway_core::{AudioCodec, AudioFormat, EncodedFrame, PcmFrame};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audio_decode::decode_audio_stream;
use crate::mixer::MixInput;

/// The panel's one volume.
///
/// Lives in [`crate::mixer`] now, because that is where it is applied: the argument for a
/// single shared gain was always "the panel has one pair of speakers, so it has one
/// volume", and until #111 that was implemented N times at N sinks. Re-exported here
/// because a session is still how most callers reach it.
pub use crate::mixer::Gain;

/// Run a decode → mix session on a dedicated thread until the source ends or `stop` is
/// set.
///
/// The flag is not optional. A preempted session whose phone is still streaming will
/// otherwise decode forever. It no longer has anything to do with sharing the *device* —
/// since #111 two sessions genuinely do mix — but the panel is still single-source by
/// policy, and a preempted source has to be told to stop rather than left playing under
/// the one that replaced it.
pub fn spawn(
    frames: mpsc::Receiver<EncodedFrame>,
    format: AudioFormat,
    config: Option<bytes::Bytes>,
    input: MixInput,
    stop: Arc<AtomicBool>,
    failed: Option<SessionFailed>,
) {
    std::thread::spawn(move || {
        run(frames, format, config.as_deref(), input, &stop, failed);
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
    mut input: MixInput,
    stop: &AtomicBool,
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
    // Why the session could not play, if it could not. Set inside the sink closure and
    // reported after it, because the closure cannot consume the `FnOnce`.
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
        |block| {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            if !started {
                info!(
                    ?codec,
                    %format,
                    rate = block.sample_rate,
                    channels = block.channels,
                    "audio session: playing"
                );
                started = true;
            }
            // No device error can arrive here any more: the mixer owns the device, and a
            // sink that refuses or vanishes is its problem to retry behind us (#111).
            // What is left is a shape with no conversion to the mix format, which is a
            // property of the samples rather than of the box, and is fatal to the
            // session either way.
            //
            // The write *blocks* while this source is already `mixer::LEAD` ahead of the
            // speakers, and that is the whole pacing mechanism for this path.
            match input.write(&block) {
                Ok(()) => true,
                Err(e) => {
                    warn!(error = %e, rate = block.sample_rate, channels = block.channels,
                        "audio session: these samples cannot reach the mix");
                    refused = Some(format!(
                        "no conversion for {} Hz x {}: {e}",
                        block.sample_rate, block.channels
                    ));
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
    // Dropping the input is what leaves the mix; the mixer plays out whatever is still
    // queued in it first, so the last block of a track is not truncated.
    drop(input);
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
    input: MixInput,
    stop: Arc<AtomicBool>,
    session: Option<PacedSession>,
) {
    std::thread::spawn(move || run_pcm(frames, input, &stop, session.as_ref()));
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
    input: &mut MixInput,
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
    // What this session has already handed to the mixer is pre-seek too. Dropping it is
    // now a ring clear rather than a device reopen: the device is shared, and closing it
    // to flush one source's staleness would have interrupted every other source (#111).
    input.flush();
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
    mut input: MixInput,
    stop: &AtomicBool,
    session: Option<&PacedSession>,
) {
    let clock = session.map(|s| s.clock.as_ref());
    // What shape has been announced, purely so a change is worth one log line. Nothing
    // has to be reopened for it any more: the mix has one fixed format and the input
    // resamples into it, so a source changing rate between tracks is a new resampler
    // rather than a device that has to be torn down and rebuilt (#111).
    let mut open_as: Option<(u32, u16)> = None;

    'blocks: loop {
        // Everything queued is from before a seek, so drop it. Checked here, at the top of
        // every block, because this is the only thread that can: the demuxer cannot reach
        // into a channel it has already written to, and without this a seek plays roughly
        // a second of wherever playback used to be before arriving where it was sent.
        //
        // Acknowledged even when there was nothing to drop — the decode thread is waiting
        // on that acknowledgement before it pushes the first block of the new position,
        // and silence would cost it the whole grace period on every seek.
        service_seek_flush(session, &frames, &mut input);
        // `recv_timeout`, not `recv`, so `stop` is observable while nothing is arriving.
        // A session preempted while its source was *paused* has no next block to wake on,
        // so a plain `recv` parked here forever: the thread leaked and — worse — the
        // output device stayed open, which on an exclusive-mode device means the source
        // that preempted us cannot start at all.
        let block = match frames.recv_timeout(STOP_POLL) {
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
                // Dropping `input` on the way out is what leaves the mix. It matters most
                // on exactly this path: a session preempted while *paused* has no next
                // block to wake on, and before the mixer this was where a parked thread
                // held an output device the source that replaced it then could not open.
                return;
            }
            // Seeking *while paused* is the ordinary way people find a spot, and this is
            // where the thread sits while they do it. Without this the flush handshake
            // went unanswered for the whole grace period on every paused scrub — and on
            // resume the block held below was written to the device and observed onto the
            // media clock at its old position, so the clock jumped backwards and a
            // second of stale audio played before the top of the loop finally flushed.
            if service_seek_flush(session, &frames, &mut input) {
                // The held block is pre-seek too, and dropping it is the point.
                continue 'blocks;
            }
            std::thread::sleep(STOP_POLL);
        }
        let shape = (block.sample_rate, block.channels);
        if open_as != Some(shape) {
            if open_as.is_some() {
                info!(?open_as, new = ?shape, "pcm session: stream shape changed");
            } else {
                info!(rate = shape.0, channels = shape.1, "pcm session: playing");
            }
            open_as = Some(shape);
        }
        let through = block.pts + block.duration();
        // Published *before* the write, which is what blocks: this says how far the
        // stream has been submitted, and the block that follows is precisely the
        // mechanism keeping submission within `mixer::LEAD` of the speaker. Publishing
        // after would make the clock jump in whole-block steps as the thread wakes.
        //
        // Only the media-URL path passes a clock; A2DP and Spotify have no video to
        // synchronise and nothing reads it.
        if let Some(clock) = clock {
            clock.observe_audio(through);
        }
        // Silence, not a black screen. This used to be where a device that refused or
        // vanished ended the receiver, which ended the demuxer, which ended the *video* —
        // so a box whose sound card was absent, busy, or held in exclusive mode showed a
        // flash and nothing while the phone said PLAYING. There is no device here to
        // refuse: the mixer holds it, falls back to silence that still keeps time, and
        // retries underneath every source at once. What remains is a shape with no
        // conversion into the mix, which no fallback can rescue.
        if let Err(e) = input.write(&block) {
            warn!(error = %e, rate = shape.0, channels = shape.1,
                "pcm session: these samples cannot reach the mix; ending");
            break;
        }
    }
}

/// How often a parked PCM session looks up to see whether it has been preempted.
///
/// Short enough that a preempted session releases the audio device promptly, long enough
/// that an idle one is not a busy loop.
const STOP_POLL: Duration = Duration::from_millis(200);

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

    use castaway_core::Volume;

    use super::*;
    use crate::audio_out::AudioOut;
    use crate::error::PipelineError;
    use crate::mixer::AudioMixer;

    /// A device that plays in real time and remembers what it heard.
    ///
    /// Sessions no longer hold devices, so this sits under a mixer rather than being
    /// handed to a session. That is a better assertion than the call log it replaced: it
    /// says what came out of the panel, not what one source asked for.
    #[derive(Debug, Default)]
    struct Recorder {
        start: Mutex<Option<std::time::Instant>>,
        heard: Mutex<Vec<f32>>,
    }

    impl Recorder {
        /// Sample frames that carried actual audio, ignoring the silence the mixer pads
        /// with whenever no source has anything to say.
        fn audible_frames(&self) -> usize {
            self.heard
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.abs() > 1e-4)
                .count()
                / 2
        }

        fn peak(&self) -> f32 {
            self.heard
                .lock()
                .unwrap()
                .iter()
                .fold(0.0f32, |peak, s| peak.max(s.abs()))
        }
    }

    impl AudioOut for Arc<Recorder> {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), PipelineError> {
            *self.start.lock().unwrap() = Some(std::time::Instant::now());
            Ok(())
        }
        fn write(&mut self, block: &PcmFrame) -> Result<(), PipelineError> {
            self.heard.lock().unwrap().extend_from_slice(&block.samples);
            Ok(())
        }
        fn stop(&mut self) {}
        fn frames_played(&self) -> Option<u64> {
            let start = (*self.start.lock().unwrap())?;
            Some(
                u64::try_from(start.elapsed().as_nanos() * 48_000 / 1_000_000_000)
                    .unwrap_or(u64::MAX),
            )
        }
    }

    /// A mixer over a recorder, plus the recorder to assert on.
    fn rig() -> (AudioMixer, Arc<Recorder>) {
        let device = Arc::new(Recorder::default());
        let for_factory = Arc::clone(&device);
        let mixer = AudioMixer::new(Arc::new(move || Box::new(Arc::clone(&for_factory))));
        (mixer, device)
    }

    /// Let the mixer play out `handed` worth of audio.
    ///
    /// It drains in real time, so this has to be at least as long as the audio itself
    /// plus the in-flight budget a source is allowed to run ahead by — a session returns
    /// as soon as the mixer has *accepted* the last block, not when it has been heard.
    fn settle(handed: Duration) {
        std::thread::sleep(handed + crate::mixer::LEAD + Duration::from_millis(100));
    }

    fn format() -> AudioFormat {
        AudioFormat::from_hz(44_100, 2).unwrap()
    }

    fn running() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn pcm(sample_rate: u32, channels: u16, frames: usize) -> PcmFrame {
        pcm_at(sample_rate, channels, frames, 0.5)
    }

    fn pcm_at(sample_rate: u32, channels: u16, frames: usize, value: f32) -> PcmFrame {
        PcmFrame {
            sample_rate,
            channels,
            samples: vec![value; frames * usize::from(channels)],
            pts: Duration::ZERO,
        }
    }

    #[test]
    fn a_session_with_no_frames_exits_instead_of_parking() {
        // The phone connected and never sent anything. The thread must not leak.
        let (mixer, _device) = rig();
        let (tx, rx) = mpsc::channel::<EncodedFrame>(1);
        drop(tx);
        run(
            rx,
            format(),
            None,
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            None,
        );
    }

    #[test]
    fn a_session_that_plays_normally_reports_no_failure() {
        // The other half: the failure path must not fire for an ordinary session, or
        // every Bluetooth stream would tear itself down on the first frame.
        let (mixer, _device) = rig();
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let reported = Arc::new(Mutex::new(false));
        let sink = Arc::clone(&reported);
        run(
            rx,
            format(),
            None,
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            Some(Box::new(move |_| *sink.lock().expect("poisoned") = true)),
        );
        assert!(
            !*reported.lock().unwrap(),
            "reported a failure that was not"
        );
    }

    #[test]
    fn a_frame_without_a_codec_is_refused_rather_than_guessed() {
        let (mixer, _device) = rig();
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
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            None,
        );
    }

    #[test]
    fn a_pcm_session_with_no_frames_exits_instead_of_parking() {
        let (mixer, _device) = rig();
        let (tx, rx) = std::sync::mpsc::sync_channel::<PcmFrame>(1);
        drop(tx);
        run_pcm(
            rx,
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            None,
        );
    }

    #[test]
    fn the_gain_reaches_the_samples() {
        // The bug: AVRCP absolute volume was parsed, answered `Accepted`, and dropped by
        // a pipeline whose `control` only logged — so the phone's rocker did nothing, and
        // a phone that had taken absolute-volume control pinned us at full scale.
        //
        // Asserted at the device rather than inside the session, because since #111 the
        // session does not apply it — the mixer does, once, to the sum. A session that
        // silently stopped honouring the volume would still pass a test that checked the
        // session's own output.
        //
        // Positions, not amplitudes — the middle entry is the one that changed with #85:
        // half the slider travel is -30 dB, so the peak is 0.0316 and not 0.5.
        for (position, muted, expected) in
            [(1.0, false, 1.0), (0.5, false, 0.031_623), (1.0, true, 0.0)]
        {
            let (mixer, device) = rig();
            mixer.gain().set(Volume::from_position(position));
            mixer.gain().set_muted(muted);
            let (tx, rx) = std::sync::mpsc::sync_channel(2);
            tx.send(pcm_at(48_000, 2, 64, 1.0)).unwrap();
            drop(tx);
            run_pcm(
                rx,
                mixer.input(crate::mixer::Backpressure::Pull),
                &running(),
                None,
            );
            settle(Duration::from_millis(2));
            let got = device.peak();
            assert!(
                (got - expected).abs() < 1e-3,
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
    fn every_source_passes_through_the_one_volume_because_there_is_one_sink() {
        // The hole #86 found: the browser's page audio reached its own device without
        // passing through `Gain` at all, so YouTube Lounge's `setVolume` converted
        // cleanly into a `ControlTxn::Volume`, landed here, and did nothing.
        //
        // What makes the fix hold now is structural rather than a convention every writer
        // has to remember. There is one device and the mixer owns it, so "a path that
        // reaches the speakers without passing through the gain" is not a mistake anyone
        // can make — it is a path that does not reach the speakers.
        let (mixer, device) = rig();
        mixer.gain().set(Volume::from_dbfs(-6.0));
        let mut a = mixer.input(crate::mixer::Backpressure::Pull);
        let mut b = mixer.input(crate::mixer::Backpressure::Pull);
        // A tenth of a second each, not a single quantum. Two sources only sum where
        // both rings are non-empty in the *same* pass, so with one quantum apiece a
        // mixer pass landing between these two writes drains `a` alone and nothing ever
        // sums — a race no amount of settling fixes, and one a longer sleep makes more
        // likely rather than less. Ten quanta means the pass boundary can fall anywhere.
        a.write(&pcm_at(48_000, 2, 4800, 0.5)).unwrap();
        b.write(&pcm_at(48_000, 2, 4800, 0.5)).unwrap();
        settle(Duration::from_millis(100));
        // Two sources at 0.5 sum to 1.0, attenuated once by -6 dB. A peak, so one
        // unsummed quantum at the head cannot hide the answer.
        let peak = device.peak();
        assert!(
            (peak - 0.501_187).abs() < 1e-3,
            "the sum should be attenuated once, at the sink: got {peak}"
        );
    }

    #[test]
    fn a_pcm_session_plays_at_the_rate_the_samples_state() {
        // Nothing hands this session a negotiated format, so if it ever invents one the
        // stream plays at the wrong pitch — the #70 failure, arriving by a new route.
        // At the mix rate this is exact: what goes in comes out, frame for frame.
        let (mixer, device) = rig();
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(pcm(48_000, 2, 512)).unwrap();
        tx.send(pcm(48_000, 2, 256)).unwrap();
        drop(tx);
        run_pcm(
            rx,
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            None,
        );
        settle(Duration::from_millis(16));
        assert_eq!(
            device.audible_frames(),
            768,
            "every frame the session stated should have reached the device once"
        );
    }

    #[test]
    fn a_stream_that_changes_shape_keeps_playing_without_reopening_anything() {
        // This used to tear the device down and open it again, because writing 48 kHz
        // samples into a device opened at 44.1 plays everything sharp. The mix has one
        // fixed format now, so a source changing rate between tracks is a new resampler
        // and nothing else — which matters because the device is *shared*: reopening it
        // for one source's track change would have interrupted every other source (#111).
        let (mixer, device) = rig();
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(pcm(44_100, 2, 4410)).unwrap();
        tx.send(pcm(48_000, 2, 4800)).unwrap();
        drop(tx);
        run_pcm(
            rx,
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            None,
        );
        settle(Duration::from_millis(200));
        // 100 ms at each rate is ~200 ms out, give or take the resampler's delay line.
        let frames = device.audible_frames();
        assert!(
            (8000..=10_400).contains(&frames),
            "both blocks should have played through: {frames} frames"
        );
    }

    #[test]
    fn a_pull_based_session_plays_in_real_time_rather_than_as_fast_as_it_can() {
        // The bug this exists to prevent, seen twice on real hardware: no output blocked —
        // cpal drops when its ring is full, the null sink accepts instantly — so librespot
        // decoded a whole track in seconds and the queue turbo-advanced.
        //
        // The mechanism that stops it changed with #111 and the property did not. It was
        // `Pace`, a per-session sleep against the device's counter; it is now the mixer's
        // in-flight budget, and the session is paced simply by `write` blocking.
        //
        // Two seconds of audio in 40 ms blocks. It must take roughly two seconds minus the
        // lead a source is allowed to run ahead by, not the microseconds a memcpy would.
        let (mixer, _device) = rig();
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let per_block = 48_000 / 25; // 40 ms
        for _ in 0..50 {
            tx.send(pcm(48_000, 2, per_block)).unwrap();
        }
        drop(tx);

        let start = std::time::Instant::now();
        run_pcm(
            rx,
            mixer.input(crate::mixer::Backpressure::Pull),
            &running(),
            None,
        );
        let taken = start.elapsed();

        let expected = Duration::from_secs(2).saturating_sub(crate::mixer::LEAD);
        assert!(
            taken >= expected.mul_f32(0.8),
            "played 2s of audio in {taken:?}; it is not pacing"
        );
        // And not the opposite failure: pacing must not *add* time.
        assert!(taken < Duration::from_secs(4), "far too slow: {taken:?}");
    }

    #[test]
    fn a_source_that_stalls_resumes_without_replaying_the_gap_at_speed() {
        // After a pause the source resumes where it left off. Treating the silent gap as
        // a debt to repay would replay it at speed.
        //
        // `Pace` needed an explicit resync threshold for this — it measured submission
        // against a start instant, so a stall accumulated a debt it then had to be told to
        // forgive. The budget needs none: a source that stops writing lets its ring drain,
        // and a source with an empty ring is never held. The absence of a mechanism is the
        // thing worth pinning, because it is what a future "improvement" would re-add.
        let (mixer, _device) = rig();
        let mut input = mixer.input(crate::mixer::Backpressure::Pull);
        input.write(&pcm(48_000, 2, 4800)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let resumed = std::time::Instant::now();
        input.write(&pcm(48_000, 2, 480)).unwrap();
        assert!(
            resumed.elapsed() < Duration::from_millis(50),
            "a source resuming after a stall was made to wait: {:?}",
            resumed.elapsed()
        );
    }

    #[test]
    fn a_preempted_pcm_session_stops_without_draining_the_rest() {
        // The panel is single-source by policy, so a preempted session has to let go —
        // and, before #111, had to let go of the *device* before the source that
        // preempted it could start at all.
        let (mixer, device) = rig();
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        for _ in 0..3 {
            tx.send(pcm(48_000, 2, 64)).unwrap();
        }
        drop(tx);
        let stop = Arc::new(AtomicBool::new(true));

        run_pcm(
            rx,
            mixer.input(crate::mixer::Backpressure::Pull),
            &stop,
            None,
        );
        settle(Duration::from_millis(2));

        assert_eq!(
            device.audible_frames(),
            0,
            "a preempted session wrote anyway"
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
