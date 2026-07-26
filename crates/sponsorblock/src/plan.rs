//! When to skip: a state machine fed playback positions, answering with what to do next.
//!
//! Deliberately sans-I/O and synchronous (ground rule 3). The actor supplies positions as
//! the Lounge reports them and performs whatever comes back; every decision about *when*
//! is testable here with no clock, no socket, and no YouTube.

use std::collections::HashSet;
use std::time::Duration;

use crate::segment::{Segment, SegmentUuid, VideoId};

/// What to do at a given playback position.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Seek now — the position is inside this segment.
    Skip {
        /// Where to seek to (the segment's end).
        to: Duration,
        /// The segment being skipped, for the toast and the bookkeeping.
        segment: Segment,
    },
    /// Nothing to do until playback reaches this position.
    WaitUntil(Duration),
    /// Nothing ahead worth waiting for.
    Idle,
}

/// Tracks the segments for the current video and what has already been skipped.
#[derive(Debug, Default)]
pub struct Planner {
    video: Option<VideoId>,
    segments: Vec<Segment>,
    skipped: HashSet<SegmentUuid>,
    /// The furthest position seen for this video, used to tell a genuine rewind from a
    /// stale report arriving just after our own seek.
    high_water: Duration,
}

/// How far past a segment's start still counts as "inside it".
///
/// Positions arrive quantised and a little stale, so a strict `start <= position` misses
/// a segment whose start fell between two reports and leaves it unskipped forever.
const LATE_TOLERANCE: Duration = Duration::from_millis(250);

/// How far back playback must jump before a skipped segment is due again.
///
/// Set by the hazard on the other side, not by taste. Our own seek lands the player past
/// a segment, and the screen may still have a *stale* position in flight from before it —
/// re-arming on that would skip the same segment again, and again, forever. So a rewind
/// only counts once playback has demonstrably got this far past where it now claims to
/// be, which a stale report never satisfies and a replay always does.
const REWIND_SETTLE: Duration = Duration::from_secs(5);

impl Planner {
    /// A planner with nothing loaded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the segments for a video, forgetting the previous one entirely.
    ///
    /// The skipped set is per-video: coming back to a video later should skip its
    /// sponsors again, because the player is at the start again.
    pub fn load(&mut self, video: VideoId, segments: Vec<Segment>) {
        self.video = Some(video);
        self.segments = segments;
        self.skipped.clear();
    }

    /// The video these segments belong to.
    #[must_use]
    pub fn video(&self) -> Option<&VideoId> {
        self.video.as_ref()
    }

    /// How many segments are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Whether there is nothing to skip in this video.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Decide what to do at `position`.
    ///
    /// A `Skip` is only ever returned once per segment. That is not an optimisation: the
    /// seek lands the player *at* the segment's end, and the next position report will be
    /// a hair before it if the clock rounds down — so a planner without this memory
    /// re-issues the same skip forever and pins the video in place.
    pub fn decide(&mut self, position: Duration) -> Decision {
        self.rearm_if_rewound(position);
        self.high_water = self.high_water.max(position);
        if let Some(segment) = self.covering(position) {
            let segment = segment.clone();
            self.skipped.insert(segment.uuid.clone());
            return Decision::Skip {
                to: segment.end,
                segment,
            };
        }
        match self.next_start(position) {
            Some(start) => Decision::WaitUntil(start),
            None => Decision::Idle,
        }
    }

    /// Mark a segment skipped without acting on it — for a caller that decided not to.
    pub fn mark_skipped(&mut self, uuid: &SegmentUuid) {
        self.skipped.insert(uuid.clone());
    }

    /// Playback moved backwards past segments we already skipped — a replay, or someone
    /// seeking back over one. Those are due again; the player is in front of them now.
    fn rearm_if_rewound(&mut self, position: Duration) {
        if self.high_water < position + REWIND_SETTLE {
            return;
        }
        let segments = &self.segments;
        self.skipped.retain(|uuid| {
            segments
                .iter()
                .find(|s| &s.uuid == uuid)
                // Keep it skipped only where playback is still past the whole segment.
                // Anywhere before its end, the player is going to run into it again.
                .is_none_or(|s| position >= s.end)
        });
        self.high_water = position;
    }

    fn covering(&self, position: Duration) -> Option<&Segment> {
        self.segments.iter().find(|s| {
            !self.skipped.contains(&s.uuid)
                && position + LATE_TOLERANCE >= s.start
                // The end is exclusive: a position exactly at the end is past it, which
                // is precisely where our own seek leaves the player.
                && position < s.end
        })
    }

    fn next_start(&self, position: Duration) -> Option<Duration> {
        self.segments
            .iter()
            .filter(|s| !self.skipped.contains(&s.uuid))
            .map(|s| s.start)
            .filter(|start| *start > position)
            .min()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::segment::Category;

    fn seg(start: u64, end: u64, uuid: &str) -> Segment {
        Segment {
            start: Duration::from_secs(start),
            end: Duration::from_secs(end),
            category: Category::Sponsor,
            uuid: serde_json::from_str(&format!("\"{uuid}\"")).unwrap(),
        }
    }

    fn planner(segments: Vec<Segment>) -> Planner {
        let mut p = Planner::new();
        p.load(VideoId::parse("dQw4w9WgXcQ").unwrap(), segments);
        p
    }

    #[test]
    fn waits_for_the_next_segment_then_skips_it() {
        let mut p = planner(vec![seg(30, 45, "a"), seg(90, 100, "b")]);
        assert_eq!(
            p.decide(Duration::from_secs(5)),
            Decision::WaitUntil(Duration::from_secs(30))
        );
        match p.decide(Duration::from_secs(30)) {
            Decision::Skip { to, segment } => {
                assert_eq!(to, Duration::from_secs(45));
                assert_eq!(segment.uuid.as_str(), "a");
            }
            other => panic!("expected a skip, got {other:?}"),
        }
        // Having skipped the first, the next thing to wait for is the second.
        assert_eq!(
            p.decide(Duration::from_secs(45)),
            Decision::WaitUntil(Duration::from_secs(90))
        );
    }

    #[test]
    fn does_not_fight_its_own_seek() {
        // The seek lands at the segment's end, and the clock may report a hair under it.
        // Re-issuing the skip there would pin the video in place forever.
        let mut p = planner(vec![seg(30, 45, "a")]);
        assert!(matches!(
            p.decide(Duration::from_secs(30)),
            Decision::Skip { .. }
        ));
        assert_eq!(p.decide(Duration::from_secs(45)), Decision::Idle);
        assert_eq!(
            p.decide(Duration::from_secs_f64(44.9)),
            Decision::Idle,
            "a position that rounds back inside the segment must not re-skip it"
        );
    }

    #[test]
    fn catches_a_segment_whose_start_fell_between_two_reports() {
        // Positions arrive quantised; a strict start<=position leaves this unskipped.
        let mut p = planner(vec![seg(30, 45, "a")]);
        assert!(matches!(
            p.decide(Duration::from_secs_f64(29.9)),
            Decision::Skip { .. }
        ));
    }

    #[test]
    fn a_position_well_before_a_segment_is_not_inside_it() {
        let mut p = planner(vec![seg(30, 45, "a")]);
        assert_eq!(
            p.decide(Duration::from_secs(29)),
            Decision::WaitUntil(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_new_video_forgets_what_was_skipped() {
        let mut p = planner(vec![seg(30, 45, "a")]);
        assert!(matches!(
            p.decide(Duration::from_secs(30)),
            Decision::Skip { .. }
        ));
        // Same segments, played again: they are due again, because so is the video.
        p.load(
            VideoId::parse("aqz-KE-bpKQ").unwrap(),
            vec![seg(30, 45, "a")],
        );
        assert!(matches!(
            p.decide(Duration::from_secs(30)),
            Decision::Skip { .. }
        ));
    }

    #[test]
    fn replaying_the_same_video_skips_it_again() {
        // Queueing the same video again does not change the video id, so nothing reloads
        // — but the player is back at the start and the sponsor is ahead of it once more.
        let mut p = planner(vec![seg(0, 4, "a")]);
        assert!(matches!(p.decide(Duration::ZERO), Decision::Skip { .. }));
        p.decide(Duration::from_secs(30)); // watched a while
        assert!(
            matches!(p.decide(Duration::ZERO), Decision::Skip { .. }),
            "a replay has to skip the segment again"
        );
    }

    #[test]
    fn a_stale_position_arriving_after_our_seek_does_not_re_skip() {
        // The hazard the settle window exists for: we skip 0-4, and a report from before
        // the seek lands afterwards. Re-arming on that is an infinite skip loop.
        let mut p = planner(vec![seg(0, 4, "a")]);
        assert!(matches!(p.decide(Duration::ZERO), Decision::Skip { .. }));
        assert_eq!(
            p.decide(Duration::from_secs_f64(0.5)),
            Decision::Idle,
            "a stale report is not a rewind"
        );
    }

    #[test]
    fn seeking_back_over_a_skipped_segment_arms_it_again() {
        let mut p = planner(vec![seg(30, 45, "a")]);
        assert!(matches!(
            p.decide(Duration::from_secs(30)),
            Decision::Skip { .. }
        ));
        p.decide(Duration::from_secs(60));
        // Someone rewound to before the sponsor.
        assert_eq!(
            p.decide(Duration::from_secs(20)),
            Decision::WaitUntil(Duration::from_secs(30))
        );
    }

    #[test]
    fn a_video_with_nothing_to_skip_is_idle() {
        let mut p = planner(vec![]);
        assert_eq!(p.decide(Duration::from_secs(10)), Decision::Idle);
        assert!(p.is_empty());
    }
}
