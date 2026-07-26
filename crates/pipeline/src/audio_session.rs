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
//! negotiation via [`castaway_core::SessionEvent::Audio`] — the bug Q25 recorded was this
//! function inventing 44.1 kHz while the phone was sending 48 (OPEN-QUESTIONS Q25).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use castaway_core::{AudioCodec, AudioFormat, EncodedFrame, PcmFrame};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::audio_decode::decode_audio_stream;
use crate::audio_out::AudioOut;
#[cfg(any(test, not(feature = "audio-out")))]
use crate::audio_out::NullAudioOut;

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
    output: Box<dyn AudioOut>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || run(frames, format, output, &stop));
}

/// Drive one audio session to completion. Blocking; call it on its own thread.
pub fn run(
    mut frames: mpsc::Receiver<EncodedFrame>,
    format: AudioFormat,
    mut output: Box<dyn AudioOut>,
    stop: &AtomicBool,
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

    let result = decode_audio_stream(
        codec,
        format,
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
                if let Err(e) = output.start(block.sample_rate, block.channels) {
                    warn!(error = %e, "audio session: output refused the stream");
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
        // the failure Q22 exists to prevent — so name it loudly.
        warn!(error = %e, ?codec, "audio session ended with an error");
        crate::audio_decode::warn_undecodable(codec);
    }
    output.stop();
}

/// Spawn a PCM playback session on a dedicated thread. The audio-only sibling of
/// [`spawn`], for adapters that arrive already decoded.
pub fn spawn_pcm(
    frames: mpsc::Receiver<PcmFrame>,
    output: Box<dyn AudioOut>,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || run_pcm(frames, output, &stop));
}

/// Drive one already-decoded audio session to completion. Blocking; call it on its own
/// thread.
///
/// There is no `format` parameter, and that is the whole point of the variant: unlike the
/// A2DP path — where aptX carries no in-band rate and the negotiated one has to be handed
/// down separately (Q25) — every [`PcmFrame`] states its own rate and channel count. The
/// shape cannot disagree with the samples because it travels with them.
pub fn run_pcm(
    mut frames: mpsc::Receiver<PcmFrame>,
    mut output: Box<dyn AudioOut>,
    stop: &AtomicBool,
) {
    // What the output device is currently open as, not what the first block said: a
    // source may change rate between tracks, and writing 48 kHz samples into a device
    // opened at 44.1 plays them at the wrong pitch rather than failing.
    let mut open_as: Option<(u32, u16)> = None;

    while let Some(block) = frames.blocking_recv() {
        if stop.load(Ordering::Relaxed) {
            info!("pcm session: preempted");
            break;
        }
        let shape = (block.sample_rate, block.channels);
        if open_as != Some(shape) {
            if open_as.is_some() {
                info!(?open_as, new = ?shape, "pcm session: stream shape changed, reopening");
                output.stop();
            }
            if let Err(e) = output.start(shape.0, shape.1) {
                warn!(error = %e, rate = shape.0, channels = shape.1,
                    "pcm session: output refused the stream");
                break;
            }
            info!(rate = shape.0, channels = shape.1, "pcm session: playing");
            open_as = Some(shape);
        }
        if let Err(e) = output.write(&block) {
            warn!(error = %e, "pcm session: output failed");
            break;
        }
    }

    output.stop();
}

/// Which codecs this build can actually decode.
///
/// The adapter's endpoint table must be built from this. Advertising a codec we cannot
/// decode means the sender picks it and the session is silence rather than a clean
/// fallback to one we can (Q22).
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
        run(rx, format(), Box::new(NullAudioOut::new()), &running());
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
        run(rx, format(), Box::new(NullAudioOut::new()), &running());
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
        let (tx, rx) = mpsc::channel::<PcmFrame>(1);
        drop(tx);
        run_pcm(rx, Box::new(NullAudioOut::new()), &running());
    }

    #[test]
    fn a_pcm_session_opens_the_output_with_the_shape_the_samples_state() {
        // Nothing hands this session a negotiated format, so if it ever invents one the
        // stream plays at the wrong pitch — the Q25 failure, arriving by a new route.
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(4);
        tx.blocking_send(pcm(44_100, 2, 512)).unwrap();
        tx.blocking_send(pcm(44_100, 2, 256)).unwrap();
        drop(tx);

        run_pcm(
            rx,
            Box::new(RecordingOut {
                log: Arc::clone(&log),
            }),
            &running(),
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
        let (tx, rx) = mpsc::channel(4);
        tx.blocking_send(pcm(44_100, 2, 128)).unwrap();
        tx.blocking_send(pcm(48_000, 2, 128)).unwrap();
        drop(tx);

        run_pcm(
            rx,
            Box::new(RecordingOut {
                log: Arc::clone(&log),
            }),
            &running(),
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
    fn a_preempted_pcm_session_stops_without_draining_the_rest() {
        // The second source has already taken the output device; this one must let go.
        let log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel(4);
        for _ in 0..3 {
            tx.blocking_send(pcm(44_100, 2, 64)).unwrap();
        }
        drop(tx);
        let stop = Arc::new(AtomicBool::new(true));

        run_pcm(
            rx,
            Box::new(RecordingOut {
                log: Arc::clone(&log),
            }),
            &stop,
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
        // The invariant Q22 actually needs. The old version of this test asserted the
        // table followed the *feature flag*, which is what let a build advertise LDAC
        // with no decoder behind it and hand a phone five minutes of silence.
        for codec in decodable_codecs() {
            assert!(
                crate::audio_decode::can_decode(codec),
                "{codec:?} is advertised but cannot be decoded"
            );
        }
        assert!(
            !decodable_codecs().contains(&AudioCodec::Ldac),
            "no LDAC decoder is bound yet, so it must not be advertised"
        );
    }
}
