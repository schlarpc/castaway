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
use std::time::{Duration, Instant};

use super::aac::AacEncoder;
use super::audio::{StreamAudio, CHANNELS, RATE};
use super::cadence::Cadence;
use super::encoder::H264Encoder;
use super::feed::EncodedAudio;
use super::fmp4::{AacConfig, Media, Track, AUDIO_TRACK, TIMESCALE, VIDEO_TRACK};
use super::hls::Segmenter;
use super::opus::{Chunker, OpusEncoder};
use super::timeline::Timeline;
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
    /// Slots filled with a repeat of the previous picture, total for this presentation.
    ///
    /// [`Publish::duplicates`](super::cadence::Publish) was consumed per job and summed
    /// nowhere, so a stream limping at 4 real fps out of 30 read identically to one
    /// duplicating a genuinely static panel (#233). This and the two below are the
    /// stream's `starved`/`idle` split, reported by the teardown line.
    duplicated: u64,
    /// Of `duplicated`, the slots owed to the *encoder*: a due slot went unfilled while
    /// the depth check was refusing frames. The stream's `starved` — structurally zero
    /// while the encoder keeps up, however still the panel is.
    stalled: u64,
    /// How many times the grid gave up on wall-clock agreement and rebased.
    resyncs: u64,
    /// Whether the encoder has refused a due slot since the last publish, so the
    /// duplicates that publish carries are attributed to it rather than to the panel.
    encoder_behind: bool,
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
    /// `audio` is the panel's sound, if this build and this box have any. Without it the
    /// stream is video-only — which is a whole, playable stream, not a degraded one.
    #[must_use]
    pub fn new(
        state: Arc<LiveStream>,
        config: StreamConfig,
        width: u32,
        height: u32,
        audio: Option<Arc<StreamAudio>>,
    ) -> Self {
        let (jobs, rx) = sync_channel(DEPTH);
        let queued = Arc::new(AtomicUsize::new(0));
        // A new presentation: a fresh origin, and nothing left in the mix from the last
        // one. Done here rather than on the encode thread so the cadence below and the
        // audio tee agree from the first frame.
        if let Some(audio) = audio.as_ref() {
            audio.restart();
        }
        let timeline = audio
            .as_ref()
            .map_or_else(|| Arc::new(Timeline::new()), |a| a.timeline());
        {
            let owned = Arc::clone(&state);
            let queued = Arc::clone(&queued);
            let audio = audio.clone();
            // Named, because a stuck thread in a `perf` capture that says
            // `castaway-stream` is a diagnosis and one that says `thread-14` is a hunt.
            let spawned = std::thread::Builder::new()
                .name("castaway-stream".into())
                .spawn(move || {
                    encode_loop(
                        &rx,
                        &owned,
                        config,
                        width,
                        height,
                        &queued,
                        audio.as_deref(),
                    );
                });
            if let Err(e) = spawned {
                state.failed(format!("could not start the encode thread: {e}"));
            }
        }
        Self {
            state,
            config,
            cadence: Cadence::new(config.rate, config.max_gap, timeline),
            width,
            height,
            jobs: Some(jobs),
            queued,
            asked_at: None,
            duplicated: 0,
            stalled: 0,
            resyncs: 0,
            encoder_behind: false,
        }
    }
}

impl OutputTap for StreamTap {
    fn wants_frame(&mut self, now: Instant) -> Option<FrameWant> {
        self.asked_at = None;
        self.jobs.as_ref()?;
        if !self.cadence.due(now) {
            return None;
        }
        // The encode thread is behind. Skipping here costs the stream a frame and the
        // panel nothing at all, which is the right way round — but the slot that goes by
        // becomes a duplicate, and *whose fault* that was is worth remembering: asked
        // after `due`, so a refusal here is always a due slot the encoder could not take.
        if self.queued.load(Ordering::Acquire) >= DEPTH {
            self.encoder_behind = true;
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
        // Attribute this publish's duplicates before totalling them: the flag's window is
        // "since the last publish", so it resets here whether or not it was read.
        let behind = std::mem::take(&mut self.encoder_behind);
        if publish.duplicates > 0 {
            self.duplicated += u64::from(publish.duplicates);
            if behind {
                self.stalled += u64::from(publish.duplicates);
            }
        }
        if publish.resynced {
            self.resyncs += 1;
            tracing::warn!(
                duplicates = publish.duplicates,
                resyncs = self.resyncs,
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
                // encoder will see it as a duplicate on the next frame through — and that
                // duplicate is the encoder's, not the panel's.
                self.encoder_behind = true;
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
        // The presentation's teardown line, as [`crate::mixer::MixInput`] does it. A few
        // duplicates are the design working; `stalled` in bulk says the box cannot encode
        // this stream at this rate, which is worth seeing without debug logging.
        if self.duplicated > 0 {
            tracing::info!(
                duplicated = self.duplicated,
                stalled = self.stalled,
                resyncs = self.resyncs,
                "stream: slots filled with a repeated picture; `stalled` of them because \
                 the encoder was behind"
            );
        }
    }
}

/// Open the encoders, then turn frames and sound into published segments until the tap
/// goes away.
fn encode_loop(
    jobs: &Receiver<Job>,
    state: &Arc<LiveStream>,
    config: StreamConfig,
    width: u32,
    height: u32,
    queued: &AtomicUsize,
    audio: Option<&StreamAudio>,
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
    let mut video = match H264Encoder::open(width, height, config.rate, config.bitrate, gop.max(1))
    {
        Ok(encoder) => encoder,
        Err(e) => {
            tracing::warn!(error = %e, "the output stream has no encoder");
            state.failed(e.to_string());
            return;
        }
    };

    // Audio is optional twice over: this build may have no audio path at all, and a box
    // may have no AAC encoder. Either way the video track still goes out — a stream with
    // no sound beats no stream.
    let mut sound = audio.and_then(|audio| match AacEncoder::open(RATE, config.audio_bitrate) {
        Ok(encoder) => {
            // The remote's Opus beside the HLS track's AAC (#259): same mix, same
            // draw, second codec, because WebRTC does not take AAC. Its absence
            // costs only the remote's sound, so it degrades separately.
            let opus = match OpusEncoder::open(RATE, config.audio_bitrate) {
                Ok(opus) => Some(opus),
                Err(e) => {
                    tracing::warn!(error = %e, "the remote track will be silent");
                    None
                }
            };
            Some(Sound::new(encoder, audio, opus))
        }
        Err(e) => {
            tracing::warn!(error = %e, "the output stream will be silent");
            None
        }
    });

    let mut segmenter = Segmenter::new(config.segment);
    let mut previous: Option<Nv12Planes> = None;
    // Whether the init segment has been published. Deliberately not "have the encoders
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
        // A peer that just joined, or one whose decoder lost sync, cannot use anything
        // until an IDR arrives — and at a keyframe a second, waiting one out is most of a
        // second of black. Asked for here, once per job, so a burst of requests inside one
        // GOP costs one keyframe rather than one each.
        if state.feed().take_keyframe_request() {
            video.request_keyframe();
        }
        for planes in frames {
            match video.encode(planes) {
                Ok(samples) => {
                    // The init segment is written from the first frame's parameter sets,
                    // not from the encoder's `extradata`, and this is where the difference
                    // lands: `h264_vaapi` publishes an `extradata` PPS that disagrees with
                    // the one its bitstream actually uses, and describing the track with
                    // the wrong one produces segments that are structurally perfect and
                    // decode to grey (`AvcConfig::absorb`).
                    if !samples.is_empty() && !live {
                        live = true;
                        go_live(state, &mut video, sound.as_ref(), width, height);
                    }
                    for sample in samples {
                        // Both shapes of the same picture, from one encode: Annex-B to
                        // whoever is watching live, AVCC into the segment being cut.
                        state.feed().publish(&sample, video.config());
                        if let Some(segment) = segmenter.push_video(sample) {
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

        // Then whatever sound has settled. Pulled by absolute position on the shared
        // timeline rather than by "whatever arrived", which is what keeps the audio track
        // from drifting against the video one however long the stream runs.
        if let Some(sound) = sound.as_mut() {
            sound.drain(&mut segmenter, state, config.audio_settle);
        }
    }

    // The tap has gone. Whatever the encoders are still holding is worth publishing: a
    // viewer who joined a second ago would otherwise sit on a playlist whose last segment
    // never appears.
    for sample in video.flush() {
        state.feed().publish(&sample, video.config());
        if let Some(segment) = segmenter.push_video(sample) {
            state.publish(segment);
        }
    }
    if let Some(sound) = sound.as_mut() {
        for sample in sound.encoder.flush() {
            segmenter.push_audio(sample);
        }
    }
    if let Some(segment) = segmenter.flush() {
        state.publish(segment);
    }
    tracing::info!("output stream stopped; nobody is watching");
    state.stopped();
}

/// Describe both tracks and publish the init segment.
fn go_live(
    state: &Arc<LiveStream>,
    video: &mut H264Encoder,
    sound: Option<&Sound>,
    width: u32,
    height: u32,
) {
    let mut tracks = vec![Track {
        id: VIDEO_TRACK,
        timescale: TIMESCALE,
        media: Media::Avc {
            config: video.describe().clone(),
            width,
            height,
        },
    }];
    if let Some(sound) = sound {
        tracks.push(Track {
            id: AUDIO_TRACK,
            // The audio track's own sample rate, so an AAC frame is exactly 1024 ticks.
            timescale: RATE,
            media: Media::Aac {
                config: sound.config.clone(),
                sample_rate: RATE,
                channels: CHANNELS,
            },
        });
    }
    let codec = fmp4::codec_string(&tracks);
    state.go_live(
        fmp4::init_segment(&tracks),
        StreamStatus::Live {
            encoder: match sound {
                Some(sound) => format!("{} + {}", video.name(), sound.name),
                None => video.name().to_string(),
            },
            width,
            height,
            codec,
        },
    );
}

/// The audio half of the encode loop.
struct Sound {
    encoder: AacEncoder,
    mix: Arc<super::audio::AudioMix>,
    config: AacConfig,
    name: String,
    /// Samples of the mix still to be thrown away before the first coded frame.
    ///
    /// An encoder does not answer the first frame until it has seen a few: its output lags
    /// its input by `initial_padding` samples. Discarding exactly that much of the mix up
    /// front means coded frame *k* carries mix position `k * frame_size` again — the
    /// encoder's own delay cancelled by not feeding it the samples it would have delayed.
    /// The alternative is an `elst` edit list, which is more boxes and which several
    /// players ignore.
    prime: usize,
    /// The remote's Opus, fed the same draws (#259). `None` when no encoder opened —
    /// the HLS track is unaffected either way.
    opus: Option<RemoteAudio>,
}

/// The Opus re-encode of the mix, for WebRTC peers (#259).
struct RemoteAudio {
    encoder: OpusEncoder,
    /// Regroups the AAC-sized draws (1024 samples) into Opus's 20 ms frames (960).
    pending: Chunker,
}

impl Sound {
    fn new(encoder: AacEncoder, audio: &StreamAudio, opus: Option<OpusEncoder>) -> Self {
        Self {
            mix: audio.mix(),
            config: encoder.config().clone(),
            name: encoder.name().to_string(),
            prime: encoder.initial_padding(),
            encoder,
            opus: opus.map(|encoder| RemoteAudio {
                pending: Chunker::new(encoder.frame_size()),
                encoder,
            }),
        }
    }

    /// Encode every frame of sound that has settled, and hand the samples to `segmenter`.
    fn drain(&mut self, segmenter: &mut Segmenter, state: &Arc<LiveStream>, settle: Duration) {
        let frame_size = self.encoder.frame_size();
        loop {
            let now = Instant::now();
            if self.prime > 0 {
                let Some(_) = self.mix.take(now, self.prime, settle) else {
                    return;
                };
                self.prime = 0;
                continue;
            }
            let Some(frames) = self.mix.take(now, frame_size, settle) else {
                return;
            };
            // The remote's copy first, so an AAC error cannot silence the peers along
            // with the playlist. Gated on a listener existing — the check is the cheap
            // half, the encode the expensive one — and the chunker is emptied when
            // nobody is left, so the held remainder cannot front-run the next
            // listener's sound (#259).
            let mut opus_stopped = false;
            if let Some(opus) = self.opus.as_mut() {
                if state.feed().audio_watched() {
                    opus.pending.push(&frames);
                    while let Some(frame) = opus.pending.take() {
                        match opus.encoder.encode(&frame) {
                            Ok(coded) => {
                                for packet in coded {
                                    state.feed().publish_audio(EncodedAudio {
                                        data: packet.data.into(),
                                        duration: Duration::from_nanos(
                                            u64::from(packet.samples) * 1_000_000_000
                                                / u64::from(RATE),
                                        ),
                                    });
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "the remote track's audio stopped");
                                opus_stopped = true;
                                break;
                            }
                        }
                    }
                } else {
                    opus.pending.clear();
                }
            }
            if opus_stopped {
                self.opus = None;
            }
            match self.encoder.encode(&frames) {
                Ok(samples) => {
                    for sample in samples {
                        segmenter.push_audio(sample);
                    }
                }
                Err(e) => {
                    // The video track keeps going. An audio encoder that stopped mid-run
                    // leaves a track that ends where it stopped, which players handle;
                    // taking the whole stream down for it would not be a trade worth making.
                    tracing::warn!(error = %e, "the output stream's audio stopped");
                    return;
                }
            }
        }
    }
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
                cadence: Cadence::new(config.rate, config.max_gap, Arc::new(Timeline::new())),
                width: 64,
                height: 32,
                jobs: Some(jobs),
                queued: Arc::new(AtomicUsize::new(0)),
                asked_at: None,
                duplicated: 0,
                stalled: 0,
                resyncs: 0,
                encoder_behind: false,
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
    fn duplicates_the_encoder_caused_are_told_apart_from_a_still_panel() {
        // #233: per-job `duplicates` summed to nowhere, so a stream limping at 4 real
        // fps because the encoder was behind read identically to one repeating a
        // genuinely static panel. The totals draw the mixer's `starved`/`idle` line.
        let (mut tap, rx) = detached();
        let t0 = Instant::now();
        let frame = planes(64, 32);

        assert!(tap.wants_frame(t0).is_some());
        tap.on_frame(&TappedFrame::Nv12(&frame));
        let _ = rx.try_recv();

        // The encode thread is full, and a due slot is refused for it: the fault is
        // remembered until the next publish carries the duplicates it caused.
        tap.queued.store(DEPTH, Ordering::Release);
        assert!(
            tap.wants_frame(t0 + Duration::from_millis(50)).is_none(),
            "the depth check refuses a due slot"
        );
        // The encoder catches up 300 ms in — slot nine — so slots one through eight come
        // back as duplicates, every one of them the encoder's.
        tap.queued.store(0, Ordering::Release);
        assert!(tap.wants_frame(t0 + Duration::from_millis(300)).is_some());
        tap.on_frame(&TappedFrame::Nv12(&frame));
        assert_eq!(rx.try_recv().unwrap().duplicates, 8);
        assert_eq!(tap.duplicated, 8, "the total accumulates");
        assert_eq!(tap.stalled, 8, "and every one of them is the encoder's");

        // A panel that simply presented nothing for the same stretch: the duplicates
        // total grows, and `stalled` must not move — a still panel is not a defect.
        tap.queued.store(0, Ordering::Release);
        assert!(tap.wants_frame(t0 + Duration::from_millis(600)).is_some());
        tap.on_frame(&TappedFrame::Nv12(&frame));
        assert_eq!(rx.try_recv().unwrap().duplicates, 8);
        assert_eq!(tap.duplicated, 16);
        assert_eq!(
            tap.stalled, 8,
            "a quiet panel's repeats are not the encoder's"
        );
    }

    #[test]
    fn a_stream_kept_on_schedule_counts_nothing() {
        // The other direction: every slot filled on time leaves every total at zero, or
        // the teardown line stops meaning anything.
        let (mut tap, rx) = detached();
        let t0 = Instant::now();
        let frame = planes(64, 32);
        for i in 0..30u64 {
            let now = t0 + Duration::from_nanos(i * 1_000_000_000 / 30);
            assert!(tap.wants_frame(now).is_some());
            tap.on_frame(&TappedFrame::Nv12(&frame));
            let _ = rx.try_recv();
            tap.queued.store(0, Ordering::Release);
        }
        assert_eq!((tap.duplicated, tap.stalled, tap.resyncs), (0, 0, 0));
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
