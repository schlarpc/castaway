//! A duplicate of the panel's output, as a web stream (#101 phase 2).
//!
//! Phase 1 landed [`crate::tap::OutputTap`] and a CPU-readback screenshot. This is the
//! other half of the same want: not "what is the panel showing" but "show me the panel",
//! continuously, from a browser on the other side of the building.
//!
//! ## The shape of it
//!
//! ```text
//!  compositor ──▶ RGBA→NV12 wgpu pass ──▶ readback ──▶ ffmpeg H.264 ──▶ fMP4 ──▶ HLS
//!      (glass)      crate::nv12          (1.5 B/px)     encoder      fmp4     hls
//! ```
//!
//! Four things about that chain are decisions rather than steps:
//!
//! - **The colour conversion is on the GPU.** Encoders want NV12; the compositor renders
//!   RGBA. Converting on the CPU through swscale would spend more time than the encode
//!   does, so it is a render pass ([`crate::nv12`]) — the inverse of the YUV→RGB the
//!   compositor already does for hardware-decoded video, and it also halves the readback
//!   because NV12 is 1.5 bytes a pixel where RGBA is 4.
//! - **The stream is not the panel's resolution.** The conversion pass writes its planes
//!   at whatever size the stream wants, so downscaling 4K to 1080p is free — the sampler
//!   was going to filter anyway — and it takes the readback from 12 MB a frame to 3.
//! - **A stream has its own clock** ([`cadence`]), and ground rule 4 inverts on it: the
//!   glass drops late frames, a stream cannot, so it duplicates.
//! - **Nothing runs until somebody asks.** A tap pins the render loop at display rate
//!   (see `RenderLoop::demand`), so the stream tap retires itself once no one has fetched
//!   a segment for [`StreamConfig::idle_timeout`]. An unattended panel with nobody
//!   watching costs exactly nothing.

pub mod cadence;
pub mod feed;
pub mod fmp4;
pub mod hls;
pub mod timeline;

/// The panel's sound, tapped at the one seam every session's audio passes through.
/// Needs the decode path for its PCM types and libswresample for the rate conversion.
#[cfg(feature = "stream")]
pub mod aac;
#[cfg(feature = "stream")]
pub mod audio;

#[cfg(feature = "stream")]
mod encoder;
#[cfg(feature = "stream")]
mod tap;

#[cfg(feature = "stream")]
pub use aac::AacEncoder;
#[cfg(feature = "stream")]
pub use audio::{AudioMix, StreamAudio};
#[cfg(feature = "stream")]
pub use encoder::{EncoderChoice, H264Encoder};
#[cfg(feature = "stream")]
pub use tap::StreamTap;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cadence::FrameRate;
use feed::LiveFeed;
use hls::{LiveWindow, Segment};

/// How the duplicate is produced. Every field here is a trade between what the stream
/// costs the panel and what it looks like at the other end.
///
/// Not `#[non_exhaustive]`: that would stop a caller writing `StreamConfig { rate, ..
/// Default::default() }`, which is exactly the idiom this is for, and the struct already
/// has a `Default` that absorbs a new field for anyone who does not care about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Frames a second the stream publishes.
    pub rate: FrameRate,
    /// The tallest the stream will be. A 4K panel streams at 1080p: the conversion pass
    /// scales for free, and the difference is 12 MB a frame read back versus 3.
    pub max_height: u32,
    /// Target video bitrate in bits per second.
    pub bitrate: u32,
    /// Target audio bitrate in bits per second.
    pub audio_bitrate: u32,
    /// How long a session's block gets to arrive late and still land where it belongs on
    /// the timeline (see [`audio::AudioMix::take`]). The one knob trading the stream's
    /// audio latency against how much of a slow session's block is clipped off its front.
    pub audio_settle: Duration,
    /// How long each segment aims to be, which is also the keyframe interval — a segment
    /// has to start on one.
    pub segment: Duration,
    /// How many segments stay fetchable. The window is the whole of what a player can
    /// seek back through, and it is also the memory this costs.
    pub window: usize,
    /// How long after the last fetch the tap retires. Long enough to survive a player
    /// reloading a playlist, short enough that a closed browser tab stops costing the
    /// panel a readback a frame.
    pub idle_timeout: Duration,
    /// The longest gap [`cadence::Cadence`] papers over with duplicates before rebasing.
    pub max_gap: Duration,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            rate: FrameRate::DEFAULT,
            max_height: 1080,
            // Enough for text on a shell screen to stay readable at 1080p, which is what
            // this is for — somebody checking a layout without walking to the panel.
            bitrate: 6_000_000,
            // AAC-LC stereo at 128 kbit/s is transparent enough for a panel duplicate and
            // is a rounding error next to the video.
            audio_bitrate: 128_000,
            // Comfortably more than any output queue in this box, and small next to the
            // seconds of latency HLS has already spent by the time a player starts.
            audio_settle: Duration::from_millis(150),
            // One-second segments: a player buffers a few of them before it starts, so
            // this is most of the end-to-end latency. Shorter costs keyframes, and a
            // keyframe a second is already the expensive end of a screen-content stream.
            segment: Duration::from_secs(1),
            window: 8,
            idle_timeout: Duration::from_secs(10),
            max_gap: Duration::from_secs(2),
        }
    }
}

/// The dimensions the stream encodes at, given what the panel is.
///
/// Both are forced even, because 4:2:0 chroma is subsampled by two in each direction and
/// an odd dimension has no way to express its last row. Aspect is preserved, and a panel
/// already at or below the cap is left alone rather than resampled for nothing.
#[must_use]
pub fn stream_size(panel: (u32, u32), max_height: u32) -> (u32, u32) {
    let (w, h) = (panel.0.max(2), panel.1.max(2));
    let (w, h) = if h <= max_height {
        (w, h)
    } else {
        // Round to nearest rather than truncating: at 4K→1080p the exact answer is
        // integral, and where it is not, a half-pixel of aspect error is invisible while
        // a truncated one accumulates on odd panel sizes.
        let scaled = (u64::from(w) * u64::from(max_height)).div_ceil(u64::from(h));
        (u32::try_from(scaled).unwrap_or(w), max_height)
    };
    (w & !1, h & !1)
}

/// What the stream is doing, for anything that has to answer a request about it.
///
/// Deliberately *not* `#[non_exhaustive]`: the only consumer is the app's `/stream/*`
/// routes, and a fifth state that the status endpoint silently reported as one of the
/// other four would be worse than a build failure. Ground rule 1 — if the match could go
/// stale tomorrow, make the compiler say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStatus {
    /// Nothing is being encoded. The next request starts it.
    Idle,
    /// A tap is attached; the first segment has not been published yet.
    Starting,
    /// Publishing.
    Live {
        /// The libavcodec encoder that opened — `h264_vaapi`, `libx264`, and so on. Worth
        /// surfacing: "the stream works" and "the stream works and is not melting a core"
        /// are different answers.
        encoder: String,
        /// Coded width.
        width: u32,
        /// Coded height.
        height: u32,
        /// The RFC 6381 codec string, for an MSE `SourceBuffer`.
        codec: String,
    },
    /// The encoder could not be opened, or stopped. Kept until something asks again, so
    /// the reason reaches whoever fetched the playlist instead of only the log.
    Failed(String),
}

/// What is on offer, shared between whatever is producing segments and whatever is
/// serving them.
///
/// Always compiled, even in a build with no encoder in it: the HTTP routes then exist and
/// say *why* there is no stream, which — exactly as with `/screenshot.png` — is a
/// different problem from "no such endpoint" and the only one of the two worth chasing.
#[derive(Debug)]
pub struct LiveStream {
    inner: Mutex<Inner>,
    /// Whether a tap is attached. Separate from the status behind the mutex so
    /// [`Self::claim`] is a single atomic and a burst of simultaneous first requests
    /// installs exactly one tap.
    running: AtomicBool,
    /// The window's shape, kept so [`Self::go_live`] can build a fresh one.
    capacity: usize,
    target: Duration,
    /// The same pictures, fanned out to real-time subscribers (#18). Beside the window
    /// rather than behind the mutex: a WebRTC peer reads it from its own task and must
    /// not contend with a playlist being rendered.
    feed: Arc<LiveFeed>,
}

#[derive(Debug)]
struct Inner {
    status: StreamStatus,
    #[allow(clippy::struct_field_names)]
    init: Option<Arc<[u8]>>,
    window: LiveWindow,
    /// When something last fetched anything. The tap reads this to decide whether anyone
    /// is still watching.
    last_request: Option<Instant>,
}

impl LiveStream {
    /// An idle stream holding a window of `config.window` segments.
    #[must_use]
    pub fn new(config: &StreamConfig) -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: StreamStatus::Idle,
                init: None,
                window: LiveWindow::new(config.window, config.segment),
                last_request: None,
            }),
            running: AtomicBool::new(false),
            capacity: config.window,
            target: config.segment,
            feed: Arc::new(LiveFeed::new()),
        }
    }

    /// The real-time fan-out of the same encoded pictures the segments are built from.
    #[must_use]
    pub fn feed(&self) -> &Arc<LiveFeed> {
        &self.feed
    }

    /// Take responsibility for starting the stream, if nobody else already has.
    ///
    /// Exactly one caller gets `true`, and that caller installs the tap. Released by
    /// [`Self::stopped`] when the tap retires.
    ///
    /// Claiming also retires the previous presentation's segments, and that is not
    /// housekeeping — it is the fix for a race that made every restarted stream unplayable.
    /// The tap takes about a second to publish its first segment, and until [`Self::go_live`]
    /// the window still held the *last* run's. A player asking in that gap got a playlist
    /// naming segments that `go_live` was about to delete, fetched one, got 404, and gave
    /// up. ffmpeg reported "Segment N failed too many times, skipping" and exited; the tap
    /// then retired ten seconds later having served nobody.
    ///
    /// So the moment a new presentation is claimed, the old one stops being on offer. The
    /// playlist 503s for the second it takes to have something real to say, which is a
    /// state every player already retries through — it is what the first request of all
    /// gets.
    pub fn claim(&self) -> bool {
        let claimed = self
            .running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if claimed {
            if let Ok(mut inner) = self.inner.lock() {
                inner.window = LiveWindow::new(self.capacity, self.target);
                inner.init = None;
                inner.status = StreamStatus::Starting;
            }
        }
        claimed
    }

    /// Whether a tap is attached.
    pub fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Note that something asked for the stream, which is what keeps it alive.
    pub fn touch(&self, now: Instant) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.last_request = Some(now);
        }
    }

    /// Whether anything has asked within `timeout`.
    ///
    /// A stream that has never been asked for counts as wanted: the tap is installed by
    /// the first request and the segment it is about to produce is that request's answer.
    pub fn wanted(&self, now: Instant, timeout: Duration) -> bool {
        // A connected WebRTC peer is a viewer that never fetches anything, so the
        // request clock would retire the tap out from under it after ten seconds of a
        // perfectly healthy session. Being attached *is* the request (#18).
        if self.feed.watched() {
            return true;
        }
        self.inner.lock().is_ok_and(|inner| {
            inner
                .last_request
                .is_none_or(|at| now.saturating_duration_since(at) < timeout)
        })
    }

    /// Publish the init segment and go live.
    ///
    /// This is the moment the track's identity changes — new encoder, new parameter sets,
    /// and a segmenter counting from one again — so whatever the previous run left in the
    /// window goes with it. Keeping it would put two different tracks' segment number 1 in
    /// the same playlist, which is the kind of wrong a player renders as a glitch rather
    /// than an error.
    pub fn go_live(&self, init: Vec<u8>, status: StreamStatus) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.window = LiveWindow::new(self.capacity, self.target);
            inner.init = Some(init.into());
            inner.status = status;
        }
    }

    /// Publish a finished segment.
    pub fn publish(&self, segment: Segment) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.window.push(segment);
        }
    }

    /// Record that the encoder could not run. Also releases the claim: the next request
    /// is entitled to try again, because the reason may have been a GPU that was busy.
    pub fn failed(&self, why: String) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.status = StreamStatus::Failed(why);
        }
        self.stopped();
    }

    /// Release the claim, so the next request starts a fresh tap.
    ///
    /// The window keeps what it has until the next [`Self::claim`] retires it, so a player
    /// that was mid-fetch when the stream stopped still gets an answer instead of a 404 it
    /// has to guess the meaning of.
    pub fn stopped(&self) {
        self.running.store(false, Ordering::Release);
        if let Ok(mut inner) = self.inner.lock() {
            if matches!(
                inner.status,
                StreamStatus::Live { .. } | StreamStatus::Starting
            ) {
                inner.status = StreamStatus::Idle;
            }
        }
    }

    /// What the stream is doing.
    #[must_use]
    pub fn status(&self) -> StreamStatus {
        self.inner
            .lock()
            .map_or(StreamStatus::Idle, |inner| inner.status.clone())
    }

    /// The init segment, once there is one.
    #[must_use]
    pub fn init_segment(&self) -> Option<Arc<[u8]>> {
        self.inner.lock().ok()?.init.clone()
    }

    /// One media segment, if it is still in the window.
    #[must_use]
    pub fn segment(&self, sequence: u32) -> Option<Arc<[u8]>> {
        Some(self.inner.lock().ok()?.window.get(sequence)?.bytes.clone())
    }

    /// The media playlist, or `None` while there is nothing to play.
    #[must_use]
    pub fn playlist(&self, init_uri: &str, segment_uri: &dyn Fn(u32) -> String) -> Option<String> {
        let inner = self.inner.lock().ok()?;
        if inner.init.is_none() || inner.window.is_empty() {
            return None;
        }
        Some(inner.window.playlist(init_uri, segment_uri))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_4k_panel_streams_at_1080p_with_its_aspect_intact() {
        assert_eq!(stream_size((3840, 2160), 1080), (1920, 1080));
        assert_eq!(stream_size((2560, 1440), 1080), (1920, 1080));
    }

    #[test]
    fn a_panel_already_small_enough_is_left_alone() {
        // Upscaling to the cap would cost encode bandwidth for pixels that were never
        // drawn.
        assert_eq!(stream_size((1280, 720), 1080), (1280, 720));
    }

    #[test]
    fn the_coded_size_is_always_even() {
        // 4:2:0 subsamples chroma by two in each direction; an odd dimension has no way to
        // express its last row and every encoder rejects it — at open time on some,
        // per-frame on others.
        for panel in [(1919, 1079), (1365, 767), (3, 3)] {
            let (w, h) = stream_size(panel, 1080);
            assert_eq!((w % 2, h % 2), (0, 0), "{panel:?} → {w}x{h}");
        }
        let (w, h) = stream_size((1921, 1441), 1080);
        assert_eq!((w % 2, h % 2), (0, 0), "{w}x{h}");
    }

    #[test]
    fn exactly_one_caller_starts_the_stream() {
        // Several requests land at once when a page with a player on it loads, and each
        // installing its own tap would mean several encoders fighting over one window.
        let stream = LiveStream::new(&StreamConfig::default());
        assert!(stream.claim());
        assert!(!stream.claim());
        stream.stopped();
        assert!(stream.claim(), "and a fresh one may start after");
    }

    #[test]
    fn a_stream_nobody_has_asked_for_yet_still_counts_as_wanted() {
        // The tap is installed by the first request, and it must survive long enough to
        // produce the segment that request is waiting for.
        let stream = LiveStream::new(&StreamConfig::default());
        let now = Instant::now();
        assert!(stream.wanted(now, Duration::from_secs(10)));
        stream.touch(now);
        assert!(stream.wanted(now + Duration::from_secs(9), Duration::from_secs(10)));
        assert!(!stream.wanted(now + Duration::from_secs(11), Duration::from_secs(10)));
    }

    #[test]
    fn claiming_the_stream_takes_the_previous_run_off_offer() {
        // The bug this exists for: a restarted stream served the *old* presentation's
        // playlist for the second it took the new tap to publish anything. A player
        // fetched a segment named there, `go_live` deleted it, and the 404 made ffmpeg
        // give up on the stream entirely.
        let stream = LiveStream::new(&StreamConfig::default());
        assert!(stream.claim());
        stream.go_live(
            vec![1, 2, 3],
            StreamStatus::Live {
                encoder: "test".into(),
                width: 16,
                height: 16,
                codec: "avc1.42000a".into(),
            },
        );
        stream.publish(Segment {
            sequence: 1,
            base_decode_time: 0,
            duration_ticks: 90_000,
            bytes: vec![4, 5, 6].into(),
        });
        assert!(stream
            .playlist("init.mp4", &|n| format!("seg-{n}.m4s"))
            .is_some());

        // The tap retires. What it left is still fetchable — a player mid-request gets an
        // answer rather than a 404 it has to interpret.
        stream.stopped();
        assert!(stream.segment(1).is_some());

        // …and the moment somebody starts it again, none of it is on offer.
        assert!(stream.claim());
        assert!(stream.segment(1).is_none());
        assert!(stream.init_segment().is_none());
        assert!(stream
            .playlist("init.mp4", &|n| format!("seg-{n}.m4s"))
            .is_none());
        assert_eq!(stream.status(), StreamStatus::Starting);
    }

    #[test]
    fn there_is_no_playlist_until_there_is_something_to_play() {
        // A playlist with an `EXT-X-MAP` and no segments makes a player retry forever
        // without saying why; 404 makes it retry and say so.
        let stream = LiveStream::new(&StreamConfig::default());
        assert!(stream
            .playlist("init.mp4", &|n| format!("seg-{n}.m4s"))
            .is_none());
        stream.go_live(vec![1, 2, 3], StreamStatus::Starting);
        assert!(stream
            .playlist("init.mp4", &|n| format!("seg-{n}.m4s"))
            .is_none());
        stream.publish(Segment {
            sequence: 1,
            base_decode_time: 0,
            duration_ticks: 90_000,
            bytes: vec![4, 5, 6].into(),
        });
        let text = stream
            .playlist("init.mp4", &|n| format!("seg-{n}.m4s"))
            .unwrap();
        assert!(text.contains("seg-1.m4s"));
    }

    #[test]
    fn a_failed_encoder_releases_the_claim_so_the_next_request_may_retry() {
        // The reason is usually transient — a GPU busy with something else, a render node
        // that was not ready yet — and a stream that latched failed forever would need a
        // restart of the whole panel to clear.
        let stream = LiveStream::new(&StreamConfig::default());
        assert!(stream.claim());
        stream.failed("no encoder".into());
        assert_eq!(stream.status(), StreamStatus::Failed("no encoder".into()));
        assert!(stream.claim());
    }
}
