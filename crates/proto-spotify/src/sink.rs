//! librespot's audio output, wired to our pipeline.
//!
//! librespot owns the decode (Vorbis, plus normalisation and dithering), so what comes
//! out the far end is interleaved PCM rather than a bitstream. That is why
//! [`FrameSource::Pcm`] exists — see DECISION-LOG D30.
//!
//! [`FrameSource::Pcm`]: castaway_core::FrameSource::Pcm

use std::time::Duration;

use castaway_core::PcmFrame;
use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use std::sync::mpsc;
use tracing::{debug, warn};

/// How many blocks may sit between librespot's decoder and our output device.
///
/// Small on purpose. This queue is pure latency: every block in it is audio that has been
/// decoded but not heard, and it has to be re-buffered anyway by the output device. What
/// it buys is tolerance for a scheduling hiccup on the output thread, nothing more.
const QUEUE_BLOCKS: usize = 8;

/// How long librespot's `start()` will wait for the pipeline to hand back a channel.
///
/// Generous, because what happens in that window is a whole session hand-off — the event
/// crosses to the session manager, preempts whoever holds the panel, and comes back as a
/// new PCM session on its own thread. Bounded, because a receiver whose pipeline has gone
/// away entirely must fail rather than park librespot's player thread forever.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(3);

/// The channel between librespot's player thread and the pipeline, and the means to
/// rebuild it.
///
/// The rebuilding is the point. A `FrameSource::Pcm` used to be emitted exactly once, when
/// the session started, and `Player::new`'s sink builder is `FnOnce` — so when another
/// source preempted Spotify, the pipeline dropped the receiver and there was no way to
/// ever give the player a new one. The device stayed in the picker, went on accepting
/// play/pause, went on updating the phone's UI, and produced silence for the rest of its
/// life. Ten seconds of Bluetooth was enough, and it looked exactly like working.
///
/// So the sink holds a slot rather than a sender. Preemption empties it, and the next
/// `start()` — which is what librespot calls when the user presses play again — asks for a
/// fresh one and waits for it. Taking the panel back is then the same gesture as starting
/// playback in the first place, which is what someone standing in front of it expects.
#[derive(Debug)]
pub struct PcmLink {
    /// The live sender, or `None` between sessions.
    slot: std::sync::Mutex<Option<mpsc::SyncSender<PcmFrame>>>,
    /// Signalled when [`PcmLink::attach`] fills the slot.
    ready: std::sync::Condvar,
    /// Sink → runner: "I have audio and nowhere to put it."
    ///
    /// A *tokio* unbounded sender because its `send` is synchronous and never blocks, so
    /// it is safe to call from librespot's player thread — which is inside a runtime and
    /// therefore cannot block on one (see the note on `write` below).
    wants: tokio::sync::mpsc::UnboundedSender<()>,
}

impl PcmLink {
    /// Build a link and the request stream the session runner should serve.
    #[must_use]
    pub fn new() -> (
        std::sync::Arc<Self>,
        tokio::sync::mpsc::UnboundedReceiver<()>,
    ) {
        let (wants, requests) = tokio::sync::mpsc::unbounded_channel();
        (
            std::sync::Arc::new(Self {
                slot: std::sync::Mutex::new(None),
                ready: std::sync::Condvar::new(),
                wants,
            }),
            requests,
        )
    }

    /// Install a fresh channel, returning the receiver for `FrameSource::Pcm`.
    ///
    /// Replaces whatever was there: the old receiver is gone by definition — that is why
    /// we are here — and a stale sender would silently swallow blocks.
    pub fn attach(&self) -> mpsc::Receiver<PcmFrame> {
        let (tx, rx) = mpsc::sync_channel(QUEUE_BLOCKS);
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(tx);
        }
        self.ready.notify_all();
        rx
    }

    /// Ask the runner for a channel. Coalescing: several requests before one is served
    /// cost nothing, because serving one satisfies all of them.
    fn request(&self) {
        let _ = self.wants.send(());
    }

    /// The current sender, if the pipeline is listening.
    fn sender(&self) -> Option<mpsc::SyncSender<PcmFrame>> {
        self.slot.lock().ok()?.clone()
    }

    /// Forget the current channel, so the next `start()` asks for a new one.
    fn detach(&self) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = None;
        }
    }

    /// Block until a channel is installed, or `timeout` passes.
    fn wait_for_channel(&self, timeout: Duration) -> bool {
        let Ok(slot) = self.slot.lock() else {
            return false;
        };
        let Ok((slot, _)) = self
            .ready
            .wait_timeout_while(slot, timeout, |slot| slot.is_none())
        else {
            return false;
        };
        slot.is_some()
    }
}

/// The [`Sink`] librespot writes into. Forwards blocks to a [`PcmFrame`] channel that the
/// pipeline's PCM session drains.
pub struct PcmSink {
    link: std::sync::Arc<PcmLink>,
    /// Sample frames handed over so far, that is, the presentation time of the *next*
    /// block. Counted rather than taken from a clock so a paused stream does not
    /// accumulate position while nothing is playing.
    frames_sent: u64,
}

impl PcmSink {
    /// Build a sink over `link`.
    #[must_use]
    pub const fn new(link: std::sync::Arc<PcmLink>) -> Self {
        Self {
            link,
            frames_sent: 0,
        }
    }

    /// The shape librespot always decodes to.
    ///
    /// Not a guess and not negotiable: librespot resamples everything to these constants
    /// before it reaches a backend, and they travel on every [`PcmFrame`] so the output
    /// stage never has to be told separately.
    #[must_use]
    pub const fn format() -> (u32, u16) {
        (SAMPLE_RATE, NUM_CHANNELS as u16)
    }
}

impl Sink for PcmSink {
    fn start(&mut self) -> SinkResult<()> {
        // This is the hook that makes preemption survivable. librespot calls it from
        // `ensure_sink_running`, which is exactly "the user pressed play" — so if the
        // pipeline took our channel away, this is where we ask for it back and wait for
        // the session hand-off to complete.
        if self.link.sender().is_some() {
            debug!("spotify sink: started");
            return Ok(());
        }
        debug!("spotify sink: no pipeline channel; asking for one");
        self.link.request();
        if self.link.wait_for_channel(ATTACH_TIMEOUT) {
            debug!("spotify sink: reattached to the pipeline");
            Ok(())
        } else {
            // Refuse rather than start into a void: librespot pauses, the phone's UI says
            // paused, and pressing play again retries. Silent "playing" is the failure
            // this whole mechanism exists to end.
            warn!("spotify sink: the pipeline did not offer a channel");
            Err(SinkError::NotConnected("no pcm session".into()))
        }
    }

    fn stop(&mut self) -> SinkResult<()> {
        // Deliberately *not* detaching. librespot calls this on every pause, and tearing
        // the channel down here would make each unpause a fresh session hand-off — a new
        // "Now casting from…" banner and a pipeline rebuild every time someone pauses to
        // talk. The channel only goes away when the pipeline takes it.
        debug!(frames = self.frames_sent, "spotify sink: stopped");
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _converter: &mut Converter) -> SinkResult<()> {
        let samples = match packet {
            AudioPacket::Samples(samples) => samples,
            // Only the `passthrough-decoder` feature produces these, and we do not enable
            // it — we want librespot's decode, not its bitstream. Refuse rather than play
            // the bytes as if they were samples.
            AudioPacket::Raw(_) => {
                return Err(SinkError::InvalidParams(
                    "spotify sink received a raw packet; passthrough is not enabled".into(),
                ));
            }
        };

        let (sample_rate, channels) = Self::format();
        #[allow(clippy::cast_possible_truncation)]
        let block = PcmFrame {
            sample_rate,
            channels,
            // librespot works in f64 end to end; the output stage speaks f32, and the
            // precision beyond it is well below what any DAC here resolves.
            samples: samples.iter().map(|s| *s as f32).collect(),
            pts: Duration::from_nanos(
                self.frames_sent
                    .saturating_mul(1_000_000_000)
                    .checked_div(u64::from(sample_rate))
                    .unwrap_or(0),
            ),
        };
        self.frames_sent += block.frame_count() as u64;

        // Blocking is correct here, and deliberate: a full queue means the speaker has
        // not caught up, so stalling the decoder is what a real audio device does, and
        // dropping instead would turn a slow output into a stream of clicks.
        //
        // It has to be a *std* channel to do it. This runs on librespot's player thread,
        // which builds its own tokio runtime and blocks on it (player.rs), so tokio's
        // `blocking_send` panics here with "Cannot block the current thread from within a
        // runtime" — on the very first block of audio, killing the sink thread and taking
        // playback with it. Blocking librespot's own runtime is fine; asking tokio to let
        // us do it is not.
        let Some(tx) = self.link.sender() else {
            // Preempted between blocks. Ask for a channel and let librespot pause; the
            // `start()` that follows the next play will wait for the answer.
            self.link.request();
            return Err(SinkError::NotConnected("no pcm session".into()));
        };
        tx.send(block).map_err(|_| {
            // The session was torn down under us. Forget the channel so `start()` knows
            // to ask for a new one rather than writing into a dead one forever.
            warn!("spotify sink: pipeline went away");
            self.link.detach();
            self.link.request();
            SinkError::NotConnected("pcm session ended".into())
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::Arc;

    use super::*;

    fn converter() -> Converter {
        Converter::new(None)
    }

    #[test]
    fn samples_are_forwarded_with_the_shape_librespot_decodes_to() {
        let (link, _requests) = PcmLink::new();
        let rx = link.attach();
        let mut sink = PcmSink::new(link);
        // Four stereo sample frames.
        sink.write(AudioPacket::Samples(vec![0.5; 8]), &mut converter())
            .unwrap();

        let block = rx.try_recv().unwrap();
        assert_eq!(block.sample_rate, SAMPLE_RATE);
        assert_eq!(block.channels, 2);
        assert_eq!(block.frame_count(), 4);
        assert!(block
            .samples
            .iter()
            .all(|s| (*s - 0.5).abs() < f32::EPSILON));
    }

    #[test]
    fn presentation_time_accumulates_across_blocks() {
        // A card that shows position needs this to be the *audio* clock. Taking it from a
        // wall clock would drift the moment playback paused.
        let (link, _requests) = PcmLink::new();
        let rx = link.attach();
        let mut sink = PcmSink::new(link);
        let one_second = vec![0.0; (SAMPLE_RATE as usize) * 2];
        sink.write(AudioPacket::Samples(one_second.clone()), &mut converter())
            .unwrap();
        sink.write(AudioPacket::Samples(one_second), &mut converter())
            .unwrap();

        assert_eq!(rx.try_recv().unwrap().pts, Duration::ZERO);
        assert_eq!(rx.try_recv().unwrap().pts, Duration::from_secs(1));
    }

    #[test]
    fn a_raw_packet_is_refused_rather_than_played_as_samples() {
        let (link, _requests) = PcmLink::new();
        let _rx = link.attach();
        let mut sink = PcmSink::new(link);
        assert!(sink
            .write(AudioPacket::Raw(vec![0; 16]), &mut converter())
            .is_err());
    }

    #[tokio::test]
    async fn a_write_from_inside_a_runtime_does_not_panic() {
        // The bug this exists to prevent, and it was not theoretical — it killed playback
        // on the first block of audio of the first real session.
        //
        // librespot's player thread builds its own tokio runtime and blocks on it
        // (playback/src/player.rs), so `write` runs *inside* a runtime context even though
        // it is not a runtime worker. tokio's `blocking_send` panics there unconditionally
        // — "Cannot block the current thread from within a runtime" — taking the sink
        // thread down and leaving librespot with a closed command channel.
        //
        // `#[tokio::test]` puts this test body in exactly that position. A std channel has
        // no such rule, which is why the PCM path uses one.
        let (link, _requests) = PcmLink::new();
        let rx = link.attach();
        let mut sink = PcmSink::new(link);
        sink.write(AudioPacket::Samples(vec![0.0; 8]), &mut converter())
            .expect("writing from a runtime context must not panic or fail");
        assert_eq!(rx.try_recv().unwrap().frame_count(), 4);
    }

    #[test]
    fn a_dropped_pipeline_ends_the_sink_instead_of_blocking_forever() {
        // The session was preempted by another source. librespot must be told, or its
        // player thread parks on a channel nobody is draining.
        let (link, _requests) = PcmLink::new();
        let rx = link.attach();
        drop(rx);
        let mut sink = PcmSink::new(link);
        assert!(sink
            .write(AudioPacket::Samples(vec![0.0; 4]), &mut converter())
            .is_err());
    }

    #[test]
    fn a_preempted_sink_asks_for_a_channel_and_plays_again_once_it_has_one() {
        // The bug this closes was the worst one in the crate, because it looked like it
        // worked: after any preemption the device stayed in the picker, went on accepting
        // play/pause, went on updating the phone's UI — and was silent forever. The PCM
        // source was emitted once at session start and `Player::new`'s sink builder is
        // `FnOnce`, so there was no way to hand the player a new channel.
        let (link, mut requests) = PcmLink::new();
        let rx = link.attach();
        let mut sink = PcmSink::new(Arc::clone(&link));

        // Preemption: the pipeline drops the receiver.
        drop(rx);
        assert!(sink
            .write(AudioPacket::Samples(vec![0.0; 4]), &mut converter())
            .is_err());
        assert!(
            requests.try_recv().is_ok(),
            "the sink must ask for a channel"
        );

        // Someone presses play. librespot calls `start()`, which waits for the hand-off.
        let waiting = std::thread::spawn(move || sink.start().map(|()| sink));
        let fresh = link.attach();
        let mut sink = waiting
            .join()
            .expect("the start thread must not panic")
            .expect("start must succeed once a channel is offered");

        sink.write(AudioPacket::Samples(vec![0.25; 4]), &mut converter())
            .expect("playback resumes on the new channel");
        assert_eq!(fresh.try_recv().unwrap().frame_count(), 2);
    }

    #[test]
    fn a_start_with_no_pipeline_behind_it_gives_up_rather_than_parking_the_player() {
        // If nothing answers, librespot must be told so it pauses and the phone shows
        // paused. Parking its player thread forever is how a receiver becomes a device
        // that claims to be playing and is not.
        let (link, _requests) = PcmLink::new();
        let mut sink = PcmSink::new(link);
        let started = std::time::Instant::now();
        assert!(sink.start().is_err());
        assert!(
            started.elapsed() >= ATTACH_TIMEOUT,
            "it should have waited for the hand-off before giving up"
        );
    }
}
