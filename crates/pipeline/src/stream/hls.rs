//! Cutting a run of coded pictures into HLS segments, and saying what is on offer.
//!
//! Two pure pieces. [`Segmenter`] decides where one segment ends and the next begins —
//! always on a keyframe, because a segment a player cannot start on is a segment it
//! cannot join at — and [`LiveWindow`] holds the last few and renders the media playlist
//! that points at them.
//!
//! Nothing here touches a socket or a clock; the segment boundary is derived from sample
//! durations, so the whole thing is deterministic and testable without an encoder.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use super::fmp4::{self, Sample, TIMESCALE};

/// One finished media segment, ready to be handed to whoever asks for it.
#[derive(Debug, Clone)]
pub struct Segment {
    /// Its number, which is both the `moof` sequence number and its place in the
    /// playlist's media sequence. One counter for both, so a playlist can never name a
    /// segment whose fragment header disagrees.
    pub sequence: u32,
    /// Where it starts on the track timeline, in [`TIMESCALE`] ticks.
    pub base_decode_time: u64,
    /// How long it runs, in [`TIMESCALE`] ticks.
    pub duration_ticks: u64,
    /// The `moof` + `mdat` bytes. Shared rather than cloned: several viewers fetch the
    /// same segment, and the window drops its copy on its own schedule.
    pub bytes: Arc<[u8]>,
}

impl Segment {
    /// Its duration as a `Duration`, for the `EXTINF` line.
    #[must_use]
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.duration_ticks as f64 / f64::from(TIMESCALE))
    }
}

/// A duration in [`TIMESCALE`] ticks. Integer throughout: a segment boundary derived from
/// a float is one that lands a tick either side of where the playlist says it does.
fn ticks(duration: Duration) -> u64 {
    u64::try_from(
        duration
            .as_nanos()
            .saturating_mul(u128::from(TIMESCALE))
            .saturating_div(1_000_000_000),
    )
    .unwrap_or(u64::MAX)
}

/// Accumulates samples and cuts a segment when one is due.
///
/// Both tracks go through one segmenter and come out in one segment, because a segment
/// carrying both is what lets the player be a `<video>` and one `SourceBuffer` rather than
/// two renditions somebody has to keep in step.
#[derive(Debug)]
pub struct Segmenter {
    /// The length a segment aims for, in the *video* track's timescale. Actual lengths
    /// land on the first keyframe at or after it, which with a fixed GOP means "exactly",
    /// and with a GOP the encoder decided to change means "close enough that the
    /// playlist's target duration still holds".
    target_ticks: u64,
    video: Pending,
    /// Absent until the first audio sample, and absent for the whole run in a build or on
    /// a box with no AAC encoder — which is a video-only stream rather than an error.
    audio: Option<Pending>,
    next_sequence: u32,
}

/// One track's accumulated samples and where they sit on its own timeline.
#[derive(Debug, Default)]
struct Pending {
    samples: Vec<Sample>,
    /// Ticks accumulated in the *pending* segment.
    ticks: u64,
    /// Where the pending segment starts on this track's timeline.
    base: u64,
}

impl Pending {
    fn push(&mut self, sample: Sample) {
        self.ticks += u64::from(sample.duration);
        self.samples.push(sample);
    }

    /// Close what is pending, returning the fragment and advancing the base.
    fn cut(&mut self, track_id: u32) -> Option<fmp4::Fragment> {
        if self.samples.is_empty() {
            return None;
        }
        let base_decode_time = self.base;
        self.base += std::mem::take(&mut self.ticks);
        Some(fmp4::Fragment {
            track_id,
            base_decode_time,
            samples: std::mem::take(&mut self.samples),
        })
    }
}

impl Segmenter {
    /// A segmenter aiming for `target`-long segments.
    #[must_use]
    pub fn new(target: Duration) -> Self {
        Self {
            target_ticks: ticks(target),
            video: Pending::default(),
            audio: None,
            next_sequence: 1,
        }
    }

    /// Offer one coded picture. Returns the segment this sample *closed*, if any.
    ///
    /// The cut happens before the sample is taken, not after: a segment must begin with a
    /// keyframe, so the frame that triggers the boundary belongs to the next one. Only
    /// video can close a segment — an audio frame cannot, because every one of them is a
    /// sync sample and cutting on one would produce a segment a player cannot start.
    pub fn push_video(&mut self, sample: Sample) -> Option<Segment> {
        let cut = sample.keyframe && self.video.ticks >= self.target_ticks;
        let finished = cut.then(|| self.flush()).flatten();
        self.video.push(sample);
        finished
    }

    /// Offer one coded audio frame. Never closes a segment.
    pub fn push_audio(&mut self, sample: Sample) {
        self.audio.get_or_insert_with(Pending::default).push(sample);
    }

    /// Close whatever is pending, whether or not it reached the target.
    ///
    /// Used when the stream stops: the last GOP is still worth serving, and a viewer who
    /// joined late would otherwise sit on a playlist whose final segment never appears.
    pub fn flush(&mut self) -> Option<Segment> {
        let duration_ticks = self.video.ticks;
        let base_decode_time = self.video.base;
        let video = self.video.cut(fmp4::VIDEO_TRACK)?;
        // Audio's fragment is emitted only when it has something in it. A `trun` with no
        // samples is legal and pointless, and the next fragment's `tfdt` says where the
        // track actually is regardless — so a starved audio track leaves a gap the player
        // waits out rather than a run of empty fragments it has to parse.
        let fragments: Vec<fmp4::Fragment> = std::iter::once(video)
            .chain(self.audio.as_mut().and_then(|a| a.cut(fmp4::AUDIO_TRACK)))
            .collect();

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        Some(Segment {
            sequence,
            base_decode_time,
            duration_ticks,
            bytes: fmp4::media_segment(sequence, &fragments).into(),
        })
    }
}

/// The last few segments, and the playlist that names them.
///
/// A live window rather than a growing list: the panel can be streamed for hours and the
/// bytes have to go somewhere. Old segments fall off the back, and `EXT-X-MEDIA-SEQUENCE`
/// is how a player that is mid-playlist notices.
#[derive(Debug)]
pub struct LiveWindow {
    capacity: usize,
    segments: VecDeque<Segment>,
    /// The target segment length in [`TIMESCALE`] ticks, so the playlist's target
    /// duration has a floor even before the first segment lands.
    target_ticks: u64,
}

impl LiveWindow {
    /// A window holding `capacity` segments of roughly `target` each.
    #[must_use]
    pub fn new(capacity: usize, target: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            segments: VecDeque::new(),
            target_ticks: ticks(target),
        }
    }

    /// Add a segment, retiring the oldest if the window is full.
    pub fn push(&mut self, segment: Segment) {
        self.segments.push_back(segment);
        while self.segments.len() > self.capacity {
            self.segments.pop_front();
        }
    }

    /// Look a segment up by number. `None` once it has fallen out of the window, which is
    /// a 404 rather than an error: a player that asks for one that old has fallen behind.
    #[must_use]
    pub fn get(&self, sequence: u32) -> Option<&Segment> {
        self.segments.iter().find(|s| s.sequence == sequence)
    }

    /// How many segments are on offer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Whether nothing has been published yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// The media playlist.
    ///
    /// `init_uri` and `segment_uri` are given rather than baked so the routes and the
    /// playlist cannot drift apart: the caller that mounts `/stream/seg-{n}.m4s` is the
    /// caller that says how to spell it.
    #[must_use]
    pub fn playlist(&self, init_uri: &str, segment_uri: &dyn Fn(u32) -> String) -> String {
        // The spec requires this be an upper bound on every EXTINF present, and players
        // treat a playlist that violates it as broken rather than rounding it off. Taken
        // from what is actually in the window, not from the target the segmenter aimed
        // at, because a GOP the encoder stretched would otherwise exceed it.
        // Whole seconds, rounded up, and derived from the tick counts rather than from
        // the float durations: `EXTINF` is what a player checks this against, and a
        // target one hundredth of a second short of a segment is a rejected playlist.
        let ticks_per_second = u64::from(TIMESCALE);
        let target_secs = self
            .segments
            .iter()
            .map(|s| s.duration_ticks)
            .fold(self.target_ticks, u64::max)
            .div_ceil(ticks_per_second)
            .max(1);
        let media_sequence = self.segments.front().map_or(1, |s| s.sequence);
        let mut out = String::with_capacity(128 + self.segments.len() * 32);
        out.push_str("#EXTM3U\n");
        // Version 7 is the floor for fMP4 segments (`EXT-X-MAP` with an fMP4 payload).
        out.push_str("#EXT-X-VERSION:7\n");
        out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
        out.push_str(&format!("#EXT-X-TARGETDURATION:{target_secs}\n"));
        out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{media_sequence}\n"));
        out.push_str(&format!("#EXT-X-MAP:URI=\"{init_uri}\"\n"));
        for segment in &self.segments {
            out.push_str(&format!(
                "#EXTINF:{:.3},\n",
                segment.duration().as_secs_f64()
            ));
            out.push_str(&segment_uri(segment.sequence));
            out.push('\n');
        }
        // No `EXT-X-ENDLIST`: the panel is still on, and a player that sees one stops
        // reloading and reports the stream as ended.
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A sample of one frame at 30 fps.
    fn frame(keyframe: bool) -> Sample {
        Sample {
            data: vec![0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }],
            duration: TIMESCALE / 30,
            keyframe,
        }
    }

    /// 30 fps, one-second segments.
    fn segmenter() -> Segmenter {
        Segmenter::new(Duration::from_secs(1))
    }

    #[test]
    fn a_segment_is_cut_at_the_first_keyframe_past_the_target() {
        let mut seg = segmenter();
        // A second's worth, then the keyframe that opens the next segment.
        for _ in 0..30 {
            assert!(seg.push_video(frame(false)).is_none());
        }
        let finished = seg.push_video(frame(true)).expect("the second is up");
        assert_eq!(finished.sequence, 1);
        assert_eq!(finished.duration_ticks, u64::from(TIMESCALE));
        assert_eq!(finished.base_decode_time, 0);
    }

    #[test]
    fn a_keyframe_before_the_target_does_not_cut() {
        // Otherwise an encoder that emits an extra IDR — a scene change, a rate control
        // decision — produces a playlist full of 100 ms segments, and every player that
        // reads it buffers for the target duration it was told to expect.
        let mut seg = segmenter();
        seg.push_video(frame(true));
        for _ in 0..5 {
            assert!(seg.push_video(frame(true)).is_none(), "too early to cut");
        }
    }

    #[test]
    fn every_segment_begins_with_the_keyframe_that_closed_the_last_one() {
        // The frame that triggers the boundary belongs to the *next* segment. Putting it
        // in the one it closed leaves the next segment starting on a delta frame, which
        // no player can join.
        let mut seg = segmenter();
        for _ in 0..30 {
            seg.push_video(frame(false));
        }
        let first = seg.push_video(frame(true)).unwrap();
        for _ in 0..30 {
            seg.push_video(frame(false));
        }
        let second = seg.push_video(frame(true)).unwrap();
        assert_eq!(second.base_decode_time, first.duration_ticks);
        // 30 delta frames plus the keyframe that opened it.
        assert_eq!(second.duration_ticks, u64::from(TIMESCALE) + 3000);
    }

    #[test]
    fn the_timeline_is_contiguous_across_segments() {
        // A gap here is a stall in every player, and a gap is exactly what happens if the
        // base decode time is recomputed from a wall clock rather than accumulated.
        let mut seg = segmenter();
        let mut got = Vec::new();
        for i in 0..200 {
            if let Some(s) = seg.push_video(frame(i % 30 == 0)) {
                got.push(s);
            }
        }
        assert!(got.len() > 3);
        for pair in got.windows(2) {
            assert_eq!(
                pair[1].base_decode_time,
                pair[0].base_decode_time + pair[0].duration_ticks
            );
        }
    }

    #[test]
    fn flushing_publishes_a_short_final_segment() {
        let mut seg = segmenter();
        seg.push_video(frame(true));
        seg.push_video(frame(false));
        let last = seg.flush().unwrap();
        assert_eq!(last.duration_ticks, 6000);
        assert!(seg.flush().is_none(), "nothing left to publish");
    }

    #[test]
    fn the_window_forgets_the_oldest_and_says_so() {
        let mut window = LiveWindow::new(3, Duration::from_secs(1));
        let mut seg = segmenter();
        let mut published = 0;
        for i in 0..300 {
            if let Some(s) = seg.push_video(frame(i % 30 == 0)) {
                window.push(s);
                published += 1;
            }
        }
        assert!(published > 3);
        assert_eq!(window.len(), 3);
        assert!(window.get(1).is_none(), "long gone");

        let text = window.playlist("init.mp4", &|n| format!("seg-{n}.m4s"));
        let sequence = text
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .unwrap();
        assert_eq!(
            sequence.parse::<u32>().unwrap(),
            published - 2,
            "the media sequence names the oldest segment still on offer"
        );
    }

    #[test]
    fn the_playlist_names_every_segment_it_holds_and_no_others() {
        let mut window = LiveWindow::new(4, Duration::from_secs(1));
        let mut seg = segmenter();
        for i in 0..150 {
            if let Some(s) = seg.push_video(frame(i % 30 == 0)) {
                window.push(s);
            }
        }
        let text = window.playlist("init.mp4", &|n| format!("seg-{n}.m4s"));
        let named: Vec<&str> = text.lines().filter(|l| l.ends_with(".m4s")).collect();
        assert_eq!(named.len(), window.len());
        for line in named {
            let n: u32 = line
                .trim_start_matches("seg-")
                .trim_end_matches(".m4s")
                .parse()
                .unwrap();
            assert!(window.get(n).is_some(), "{line} is not on offer");
        }
        assert!(text.contains("#EXT-X-MAP:URI=\"init.mp4\""));
        assert!(
            !text.contains("ENDLIST"),
            "the panel has not stopped showing anything"
        );
    }

    #[test]
    fn the_target_duration_is_never_shorter_than_a_segment_in_the_window() {
        // A GOP the encoder stretched past the target produces an EXTINF above it, and a
        // playlist whose TARGETDURATION is exceeded is one players reject outright.
        let mut window = LiveWindow::new(4, Duration::from_secs(1));
        let mut seg = Segmenter::new(Duration::from_secs(1));
        for _ in 0..100 {
            seg.push_video(frame(false));
        }
        window.push(seg.push_video(frame(true)).unwrap());
        let text = window.playlist("init.mp4", &|n| format!("seg-{n}.m4s"));
        let target: f64 = text
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-TARGETDURATION:"))
            .unwrap()
            .parse()
            .unwrap();
        let longest = text
            .lines()
            .filter_map(|l| l.strip_prefix("#EXTINF:"))
            .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
            .fold(0.0, f64::max);
        assert!(target >= longest, "target {target} < segment {longest}");
    }
}
