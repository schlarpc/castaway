//! librespot's audio output, wired to our pipeline.
//!
//! librespot owns the decode (Vorbis, plus normalisation and dithering), so what comes
//! out the far end is interleaved PCM rather than a bitstream. That is why
//! [`FrameSource::Pcm`] exists — see DECISION-LOG D31.
//!
//! [`FrameSource::Pcm`]: castaway_core::FrameSource::Pcm

use std::time::Duration;

use castaway_core::PcmFrame;
use librespot_playback::audio_backend::{Sink, SinkError, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::{NUM_CHANNELS, SAMPLE_RATE};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// How many blocks may sit between librespot's decoder and our output device.
///
/// Small on purpose. This queue is pure latency: every block in it is audio that has been
/// decoded but not heard, and it has to be re-buffered anyway by the output device. What
/// it buys is tolerance for a scheduling hiccup on the output thread, nothing more.
const QUEUE_BLOCKS: usize = 8;

/// The [`Sink`] librespot writes into. Forwards blocks to a [`PcmFrame`] channel that the
/// pipeline's PCM session drains.
pub struct PcmSink {
    tx: mpsc::Sender<PcmFrame>,
    /// Sample frames handed over so far, that is, the presentation time of the *next*
    /// block. Counted rather than taken from a clock so a paused stream does not
    /// accumulate position while nothing is playing.
    frames_sent: u64,
}

impl PcmSink {
    /// Build a sink feeding `tx`.
    #[must_use]
    pub const fn new(tx: mpsc::Sender<PcmFrame>) -> Self {
        Self { tx, frames_sent: 0 }
    }

    /// A channel pair sized for this sink: the sender for [`PcmSink::new`], the receiver
    /// for [`castaway_core::FrameSource::Pcm`].
    #[must_use]
    pub fn channel() -> (mpsc::Sender<PcmFrame>, mpsc::Receiver<PcmFrame>) {
        mpsc::channel(QUEUE_BLOCKS)
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
        debug!("spotify sink: started");
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
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

        // Blocking is correct here, and deliberate. This runs on librespot's own player
        // thread (never a runtime worker), and a full queue means the speaker has not
        // caught up — so stalling the decoder is exactly what a real audio device does.
        // Dropping instead would turn a slow output into a stream of clicks.
        self.tx.blocking_send(block).map_err(|_| {
            // The session was torn down under us; say so once and let librespot stop.
            warn!("spotify sink: pipeline went away");
            SinkError::NotConnected("pcm session ended".into())
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn converter() -> Converter {
        Converter::new(None)
    }

    #[test]
    fn samples_are_forwarded_with_the_shape_librespot_decodes_to() {
        let (tx, mut rx) = PcmSink::channel();
        let mut sink = PcmSink::new(tx);
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
        let (tx, mut rx) = PcmSink::channel();
        let mut sink = PcmSink::new(tx);
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
        let (tx, _rx) = PcmSink::channel();
        let mut sink = PcmSink::new(tx);
        assert!(sink
            .write(AudioPacket::Raw(vec![0; 16]), &mut converter())
            .is_err());
    }

    #[test]
    fn a_dropped_pipeline_ends_the_sink_instead_of_blocking_forever() {
        // The session was preempted by another source. librespot must be told, or its
        // player thread parks on a channel nobody is draining.
        let (tx, rx) = PcmSink::channel();
        drop(rx);
        let mut sink = PcmSink::new(tx);
        assert!(sink
            .write(AudioPacket::Samples(vec![0.0; 4]), &mut converter())
            .is_err());
    }
}
