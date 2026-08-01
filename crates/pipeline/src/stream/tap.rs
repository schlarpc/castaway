//! The [`OutputTap`] that turns composited frames into HLS segments.
//!
//! Two threads and one channel. On the render thread the tap does as little as possible —
//! ask the cadence whether a slot is due, take the converted planes, hand them on — because
//! anything slow here stalls the panel. On the encode thread everything else happens:
//! opening the encoder, boxing the samples, cutting segments, publishing them.
//!
//! ## Backpressure, and why it is not a queue
//!
//! If the encoder falls behind, [`StreamTap::wants_frame`] simply stops asking. That skips
//! the readback entirely — the expensive part — and the slots that go by become duplicates
//! on the next frame that does get through ([`super::cadence`]). So a slow encoder
//! degrades the stream's frame rate and never the panel's, and the timeline stays
//! contiguous either way. A queue would have inverted that: buffered frames are latency,
//! and dropping from the back of one is the hole in the timeline this whole design exists
//! to avoid.
//!
//! ## Retiring
//!
//! A tap holds the render loop at display rate for as long as it is attached
//! (`RenderLoop::demand`), so this one retires when nothing has fetched a segment for
//! [`StreamConfig::idle_timeout`]. Closing the browser tab stops costing the panel
//! anything within ten seconds, and the next request starts a fresh tap.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Instant;

use super::cadence::Cadence;
use super::encoder::H264Encoder;
use super::hls::Segmenter;
use super::{fmp4, LiveStream, StreamConfig, StreamStatus};
use crate::nv12::Nv12Planes;
use crate::tap::{FrameWant, OutputTap, TappedFrame};

/// How many frames may be in flight to the encode thread.
///
/// Two: one being encoded and one waiting. Deeper would be latency the viewer pays for
/// and nothing else — see the module docs on why this is not a queue.
const DEPTH: usize = 2;

/// One unit of work for the encode thread.
struct Job {
    planes: Nv12Planes,
    /// Repeats of the *previous* frame to emit first, filling slots the panel did not
    /// present into. Sent as a count rather than as frames because the encode thread
    /// already has the picture.
    duplicates: u32,
}

/// Feeds the composited output to an encoder and publishes the segments.
pub struct StreamTap {
    state: Arc<LiveStream>,
    config: StreamConfig,
    cadence: Cadence,
    width: u32,
    height: u32,
    jobs: Option<SyncSender<Job>>,
    /// How many jobs the encode thread has not finished. Read before the readback, which
    /// is what makes the backpressure free.
    queued: Arc<AtomicUsize>,
    /// The instant [`Self::wants_frame`] said yes at, so the frame is timed by when it was
    /// *asked for* rather than by when it came back — a readback takes a millisecond or
    /// two and the grid should not absorb that.
    asked_at: Option<Instant>,
}

impl std::fmt::Debug for StreamTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTap")
            .field("size", &(self.width, self.height))
            .field("queued", &self.queued.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl StreamTap {
    /// Start encoding `width`×`height` into `state`.
    ///
    /// The encoder is opened on the thread this spawns, not here: probing five candidates
    /// against a driver can take tens of milliseconds, and this is called from the render
    /// loop while it is applying commands.
    #[must_use]
    pub fn new(state: Arc<LiveStream>, config: StreamConfig, width: u32, height: u32) -> Self {
        let (jobs, rx) = sync_channel(DEPTH);
        let queued = Arc::new(AtomicUsize::new(0));
        {
            let owned = Arc::clone(&state);
            let queued = Arc::clone(&queued);
            // Named, because a stuck thread in a `perf` capture that says
            // `castaway-stream` is a diagnosis and one that says `thread-14` is a hunt.
            let spawned = std::thread::Builder::new()
                .name("castaway-stream".into())
                .spawn(move || encode_loop(&rx, &owned, config, width, height, &queued));
            if let Err(e) = spawned {
                state.failed(format!("could not start the encode thread: {e}"));
            }
        }
        Self {
            state,
            config,
            cadence: Cadence::new(config.rate, config.max_gap),
            width,
            height,
            jobs: Some(jobs),
            queued,
            asked_at: None,
        }
    }
}

impl OutputTap for StreamTap {
    fn wants_frame(&mut self, now: Instant) -> Option<FrameWant> {
        self.asked_at = None;
        self.jobs.as_ref()?;
        // The encode thread is behind. Skipping here costs the stream a frame and the
        // panel nothing at all, which is the right way round.
        if self.queued.load(Ordering::Acquire) >= DEPTH {
            return None;
        }
        if !self.cadence.due(now) {
            return None;
        }
        self.asked_at = Some(now);
        Some(FrameWant::Nv12 {
            width: self.width,
            height: self.height,
        })
    }

    fn on_frame(&mut self, frame: &TappedFrame<'_>) {
        let TappedFrame::Nv12(planes) = frame else {
            // Unreachable: this tap only ever asks for NV12. Taking no slot means the
            // cadence is untouched, so a shape mismatch costs a frame rather than
            // desynchronising the timeline.
            return;
        };
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        let now = self.asked_at.take().unwrap_or_else(Instant::now);
        let publish = self.cadence.take(now);
        if publish.resynced {
            tracing::warn!(
                duplicates = publish.duplicates,
                "the panel stopped presenting long enough that the stream rebased its clock"
            );
        }
        self.queued.fetch_add(1, Ordering::AcqRel);
        // The clone is the one copy this seam costs. The alternative is handing the
        // readback buffer over, which would mean a tap could take ownership of a frame
        // several taps are being served from — a much worse trade than 3 MB at 1080p.
        let job = Job {
            planes: (*planes).clone(),
            duplicates: publish.duplicates,
        };
        match jobs.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // `wants_frame` checks the depth first, so this is a race rather than a
                // state: the count is one frame stale. The slot is already taken, so the
                // encoder will see it as a duplicate on the next frame through.
                self.queued.fetch_sub(1, Ordering::AcqRel);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.queued.fetch_sub(1, Ordering::AcqRel);
                self.jobs = None;
            }
        }
    }

    fn finished(&self) -> bool {
        self.jobs.is_none() || !self.state.wanted(Instant::now(), self.config.idle_timeout)
    }
}

impl Drop for StreamTap {
    fn drop(&mut self) {
        // Dropping the sender is the stop signal: the encode thread's `recv` fails, it
        // flushes the encoder, publishes the last partial segment and releases the claim.
        self.jobs = None;
    }
}

/// Open the encoder, then turn frames into published segments until the tap goes away.
fn encode_loop(
    jobs: &Receiver<Job>,
    state: &Arc<LiveStream>,
    config: StreamConfig,
    width: u32,
    height: u32,
    queued: &AtomicUsize,
) {
    // The keyframe interval *is* the segment length: a segment has to start on one, so a
    // GOP that disagreed would give the segmenter nowhere to cut at the target.
    let gop = u32::try_from(
        config
            .segment
            .as_millis()
            .saturating_mul(u128::from(config.rate.get()))
            / 1000,
    )
    .unwrap_or(u32::MAX);
    let mut encoder =
        match H264Encoder::open(width, height, config.rate, config.bitrate, gop.max(1)) {
            Ok(encoder) => encoder,
            Err(e) => {
                tracing::warn!(error = %e, "the output stream has no encoder");
                state.failed(e.to_string());
                return;
            }
        };
    let mut segmenter = Segmenter::new(config.segment);
    let mut previous: Option<Nv12Planes> = None;
    // Whether the init segment has been published. Deliberately not "has the encoder
    // opened": the track cannot be described until a frame has come out of it.
    let mut live = false;
    while let Ok(job) = jobs.recv() {
        // The duplicates go first: they are the slots between the last published frame and
        // this one, and an encoder codes a repeated picture as almost nothing.
        let repeats = previous
            .as_ref()
            .map(|planes| std::iter::repeat_n(planes, job.duplicates as usize));
        let frames = repeats
            .into_iter()
            .flatten()
            .chain(std::iter::once(&job.planes));
        let mut failed = None;
        for planes in frames {
            match encoder.encode(planes) {
                Ok(samples) => {
                    // The init segment is written from the first frame's parameter sets,
                    // not from the encoder's `extradata`, and this is where the difference
                    // lands: `h264_vaapi` publishes an `extradata` PPS that disagrees with
                    // the one its bitstream actually uses, and describing the track with
                    // the wrong one produces segments that are structurally perfect and
                    // decode to grey (`AvcConfig::absorb`).
                    if !samples.is_empty() && !live {
                        live = true;
                        let config = encoder.describe().clone();
                        state.go_live(
                            fmp4::init_segment(&config, width, height),
                            StreamStatus::Live {
                                encoder: encoder.name().to_string(),
                                width,
                                height,
                                codec: config.codec_string(),
                            },
                        );
                    }
                    for sample in samples {
                        if let Some(segment) = segmenter.push(sample) {
                            state.publish(segment);
                        }
                    }
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        queued.fetch_sub(1, Ordering::AcqRel);
        if let Some(e) = failed {
            tracing::warn!(error = %e, "the output stream encoder stopped");
            state.failed(e.to_string());
            return;
        }
        previous = Some(job.planes);
    }

    // The tap has gone. Whatever the encoder is still holding is worth publishing: a
    // viewer who joined a second ago would otherwise sit on a playlist whose last segment
    // never appears.
    for sample in encoder.flush() {
        if let Some(segment) = segmenter.push(sample) {
            state.publish(segment);
        }
    }
    if let Some(segment) = segmenter.flush() {
        state.publish(segment);
    }
    tracing::info!("output stream stopped; nobody is watching");
    state.stopped();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::time::Duration;

    fn planes(width: u32, height: u32) -> Nv12Planes {
        let stride = width.div_ceil(256) * 256;
        let uv_offset = (stride * height) as usize;
        Nv12Planes {
            width,
            height,
            data: vec![128; uv_offset + (stride * height / 2) as usize],
            y_stride: stride,
            uv_offset,
            uv_stride: stride,
        }
    }

    /// A tap with nothing on the other end of the channel, so the render-thread half can
    /// be driven without an encoder.
    fn detached() -> (StreamTap, Receiver<Job>) {
        let (jobs, rx) = sync_channel(DEPTH);
        let config = StreamConfig::default();
        let state = Arc::new(LiveStream::new(&config));
        (
            StreamTap {
                state,
                config,
                cadence: Cadence::new(config.rate, config.max_gap),
                width: 64,
                height: 32,
                jobs: Some(jobs),
                queued: Arc::new(AtomicUsize::new(0)),
                asked_at: None,
            },
            rx,
        )
    }

    #[test]
    fn the_tap_asks_at_the_streams_rate_not_the_panels() {
        // The reason `wants_frame` is asked before the readback: at 60 Hz into a 30 fps
        // stream, half the presented frames cost nothing at all.
        let (mut tap, rx) = detached();
        let t0 = Instant::now();
        let frame = planes(64, 32);
        let mut asked = 0;
        for i in 0..60u64 {
            let now = t0 + Duration::from_nanos(i * 1_000_000_000 / 60);
            if tap.wants_frame(now).is_some() {
                asked += 1;
                tap.on_frame(&TappedFrame::Nv12(&frame));
                // Stand in for the encode thread, so the depth check is not what limits it.
                let _ = rx.try_recv();
                tap.queued.fetch_sub(1, Ordering::AcqRel);
            }
        }
        assert_eq!(asked, 30, "one second of a 30 fps stream");
    }

    #[test]
    fn a_slow_encoder_stops_the_readback_rather_than_the_panel() {
        // Nothing drains `rx`, so the depth fills and stays full. What must *not* happen
        // is the tap continuing to ask: each ask is a full conversion and readback, and
        // the frame it produced would have nowhere to go.
        let (mut tap, _rx) = detached();
        let t0 = Instant::now();
        let frame = planes(64, 32);
        let mut asked = 0;
        for i in 0..60u64 {
            let now = t0 + Duration::from_nanos(i * 1_000_000_000 / 60);
            if tap.wants_frame(now).is_some() {
                asked += 1;
                tap.on_frame(&TappedFrame::Nv12(&frame));
            }
        }
        assert_eq!(
            asked, DEPTH,
            "it stopped asking once the encoder fell behind"
        );
    }

    #[test]
    fn the_slots_a_slow_encoder_missed_come_back_as_duplicates() {
        // The other half of the same story: skipping the readback must not shorten the
        // timeline, or the stream runs slow and the viewer drifts behind.
        let (mut tap, rx) = detached();
        let t0 = Instant::now();
        let frame = planes(64, 32);

        assert!(tap.wants_frame(t0).is_some());
        tap.on_frame(&TappedFrame::Nv12(&frame));
        assert_eq!(rx.recv().unwrap().duplicates, 0);
        tap.queued.fetch_sub(1, Ordering::AcqRel);

        // …three hundred milliseconds later, which at 30 fps is slot nine: slots one
        // through eight went by unfilled and are owed.
        let later = t0 + Duration::from_millis(300);
        assert!(tap.wants_frame(later).is_some());
        tap.on_frame(&TappedFrame::Nv12(&frame));
        assert_eq!(rx.recv().unwrap().duplicates, 8);
    }

    #[test]
    fn a_tap_nobody_is_watching_retires() {
        // A tap holds the render loop at display rate. One that outlived its viewers would
        // keep an unattended panel converting and reading back forever.
        let (tap, _rx) = detached();
        assert!(
            !tap.finished(),
            "nobody has asked yet, so it is still wanted"
        );
        tap.state.touch(Instant::now() - Duration::from_secs(60));
        assert!(tap.finished());

        // …and a fresh request revives it, so a player that reloads a playlist after a
        // pause does not have to wait for a whole new encoder.
        tap.state.touch(Instant::now());
        assert!(!tap.finished());
    }

    #[test]
    fn a_dead_encode_thread_retires_the_tap_rather_than_looping() {
        let (mut tap, rx) = detached();
        drop(rx);
        let frame = planes(64, 32);
        assert!(tap.wants_frame(Instant::now()).is_some());
        tap.on_frame(&TappedFrame::Nv12(&frame));
        assert!(tap.finished(), "there is nothing on the other end any more");
        assert!(tap.wants_frame(Instant::now()).is_none());
    }
}
