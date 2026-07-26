//! Video ids, categories, and the segments the API returns.

use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::error::SponsorBlockError;

/// A YouTube video id, as it arrives from the Lounge's `nowPlaying`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VideoId(String);

impl VideoId {
    /// Parse a video id. YouTube's are 11 characters of URL-safe base64.
    ///
    /// # Errors
    /// [`SponsorBlockError::NotAVideoId`] if it is not that shape. Worth rejecting rather
    /// than passing through: it is hashed into a URL, and a playlist id or a stray empty
    /// string would silently look up a bucket nothing is in.
    pub fn parse(raw: &str) -> Result<Self, SponsorBlockError> {
        if raw.len() != 11 {
            return Err(SponsorBlockError::NotAVideoId("not 11 characters"));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(SponsorBlockError::NotAVideoId("unexpected characters"));
        }
        Ok(Self(raw.to_string()))
    }

    /// The id itself.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The full hex `sha256` of the id.
    #[must_use]
    pub fn hash_hex(&self) -> String {
        let digest = Sha256::digest(self.0.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The first four hex characters of the hash — all the server is told.
    #[must_use]
    pub fn hash_prefix(&self) -> String {
        self.hash_hex()[..4].to_string()
    }
}

/// A segment's UUID, used to remember what has already been skipped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub struct SegmentUuid(String);

impl SegmentUuid {
    /// The uuid as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a segment is.
///
/// `Unknown` is deliberate: the category list is the API's to grow, and a new one must not
/// fail the parse of every other segment in the response. Unknown categories are never
/// skipped, so the failure mode of falling behind the API is "we skip less", not "we skip
/// something the viewer wanted".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[non_exhaustive]
pub enum Category {
    /// Paid promotion.
    #[serde(rename = "sponsor")]
    Sponsor,
    /// The creator promoting themselves — merch, Patreon, their other channel.
    #[serde(rename = "selfpromo")]
    SelfPromo,
    /// "Like and subscribe."
    #[serde(rename = "interaction")]
    Interaction,
    /// Title cards and intro animations.
    #[serde(rename = "intro")]
    Intro,
    /// End cards.
    #[serde(rename = "outro")]
    Outro,
    /// A recap or a preview of what is coming.
    #[serde(rename = "preview")]
    Preview,
    /// Music videos: the non-music talking around the song.
    #[serde(rename = "music_offtopic")]
    MusicOfftopic,
    /// Tangents and filler.
    #[serde(rename = "filler")]
    Filler,
    /// The whole video is a paid promotion.
    #[serde(rename = "exclusive_access")]
    ExclusiveAccess,
    /// Anything this build has not been taught. Never skipped.
    #[serde(other)]
    Unknown,
}

impl Category {
    /// The name the API uses, or `None` for [`Category::Unknown`], which is ours.
    #[must_use]
    pub fn as_api_str(self) -> Option<&'static str> {
        Some(match self {
            Self::Sponsor => "sponsor",
            Self::SelfPromo => "selfpromo",
            Self::Interaction => "interaction",
            Self::Intro => "intro",
            Self::Outro => "outro",
            Self::Preview => "preview",
            Self::MusicOfftopic => "music_offtopic",
            Self::Filler => "filler",
            Self::ExclusiveAccess => "exclusive_access",
            Self::Unknown => return None,
        })
    }

    /// A word for the on-screen toast: "Skipped a sponsor".
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Sponsor => "sponsor",
            Self::SelfPromo => "self-promo",
            Self::Interaction => "reminder",
            Self::Intro => "intro",
            Self::Outro => "outro",
            Self::Preview => "recap",
            Self::MusicOfftopic => "non-music",
            Self::Filler => "filler",
            Self::ExclusiveAccess => "promotion",
            Self::Unknown => "segment",
        }
    }
}

/// What the submitter wants done with a segment. We can only act on `Skip` — muting would
/// mean driving volume over the Lounge and restoring it exactly, and poi/chapter are
/// player UI we do not control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[non_exhaustive]
pub enum ActionType {
    /// Seek past it.
    #[serde(rename = "skip")]
    Skip,
    /// Silence it in place.
    #[serde(rename = "mute")]
    Mute,
    /// The entire video is this category.
    #[serde(rename = "full")]
    Full,
    /// A point of interest, not a range.
    #[serde(rename = "poi")]
    Poi,
    /// A chapter marker.
    #[serde(rename = "chapter")]
    Chapter,
    /// Anything newer than this build.
    #[serde(other)]
    Unknown,
}

/// One segment to skip.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Where it starts.
    pub start: Duration,
    /// Where it ends — the position to seek to.
    pub end: Duration,
    /// What it is.
    pub category: Category,
    /// Which submission this is, so a skip happens once.
    pub uuid: SegmentUuid,
}

impl Segment {
    /// How long it runs.
    #[must_use]
    pub fn len(&self) -> Duration {
        self.end.saturating_sub(self.start)
    }

    /// Whether it covers no time at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == Duration::ZERO
    }
}

// --- the wire shapes ---

#[derive(Deserialize)]
struct ApiVideo {
    #[serde(rename = "videoID")]
    video_id: String,
    segments: Vec<ApiSegment>,
}

#[derive(Deserialize)]
struct ApiSegment {
    segment: [f64; 2],
    #[serde(rename = "UUID")]
    uuid: SegmentUuid,
    category: Category,
    #[serde(rename = "actionType")]
    action_type: ActionType,
}

/// Pull our video's skippable segments out of a hash-prefix response.
///
/// The response covers every video sharing the prefix, so the filtering by id here is not
/// a nicety — it is the half of the privacy trade that happens on our side.
///
/// Segments shorter than `minimum` are dropped: a sub-second skip is a visible stutter
/// that buys nothing. Overlapping segments are merged, so two submissions covering the
/// same break become one seek instead of two.
///
/// # Errors
/// [`SponsorBlockError::Malformed`] if the body is not the documented JSON.
pub fn parse_response(
    body: &str,
    video: &VideoId,
    categories: &[Category],
    minimum: Duration,
) -> Result<Vec<Segment>, SponsorBlockError> {
    let videos: Vec<ApiVideo> =
        serde_json::from_str(body).map_err(|e| SponsorBlockError::Malformed(e.to_string()))?;

    let mut segments: Vec<Segment> = videos
        .into_iter()
        .filter(|v| v.video_id == video.as_str())
        .flat_map(|v| v.segments)
        .filter(|s| s.action_type == ActionType::Skip)
        .filter(|s| categories.contains(&s.category))
        .filter_map(|s| {
            let [start, end] = s.segment;
            // NaN, negatives, and end-before-start are all "not a range we can seek".
            if !start.is_finite() || !end.is_finite() || end <= start || start < 0.0 {
                return None;
            }
            Some(Segment {
                start: Duration::from_secs_f64(start),
                end: Duration::from_secs_f64(end),
                category: s.category,
                uuid: s.uuid,
            })
        })
        .filter(|s| s.len() >= minimum)
        .collect();

    segments.sort_by_key(|s| s.start);
    Ok(merge_overlapping(segments))
}

/// Fold overlapping or touching segments together, keeping the first one's identity.
fn merge_overlapping(sorted: Vec<Segment>) -> Vec<Segment> {
    let mut merged: Vec<Segment> = Vec::with_capacity(sorted.len());
    for segment in sorted {
        match merged.last_mut() {
            Some(previous) if segment.start <= previous.end => {
                previous.end = previous.end.max(segment.end);
            }
            _ => merged.push(segment),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const SPONSOR: &[Category] = &[Category::Sponsor, Category::SelfPromo];

    fn body() -> String {
        // The shape the hash-prefix endpoint really answers with: several videos sharing
        // a prefix, each with its own segments.
        serde_json::json!([
            {
                "videoID": "aaaaaaaaaaa",
                "segments": [
                    {"segment": [0.0, 30.0], "UUID": "other-video", "category": "sponsor",
                     "actionType": "skip", "videoDuration": 100.0}
                ]
            },
            {
                "videoID": "dQw4w9WgXcQ",
                "segments": [
                    {"segment": [10.5, 25.0], "UUID": "u-sponsor", "category": "sponsor",
                     "actionType": "skip", "videoDuration": 212.0},
                    {"segment": [60.0, 70.0], "UUID": "u-mute", "category": "sponsor",
                     "actionType": "mute", "videoDuration": 212.0},
                    {"segment": [90.0, 95.0], "UUID": "u-intro", "category": "intro",
                     "actionType": "skip", "videoDuration": 212.0},
                    {"segment": [120.0, 120.2], "UUID": "u-tiny", "category": "sponsor",
                     "actionType": "skip", "videoDuration": 212.0},
                    {"segment": [150.0, 160.0], "UUID": "u-promo", "category": "selfpromo",
                     "actionType": "skip", "videoDuration": 212.0}
                ]
            }
        ])
        .to_string()
    }

    #[test]
    fn takes_only_our_video_and_only_what_we_can_act_on() {
        let video = VideoId::parse("dQw4w9WgXcQ").unwrap();
        let segments = parse_response(&body(), &video, SPONSOR, Duration::from_secs(1)).unwrap();
        let uuids: Vec<&str> = segments.iter().map(|s| s.uuid.as_str()).collect();
        assert_eq!(
            uuids,
            vec!["u-sponsor", "u-promo"],
            "another video's segments, a mute, an unselected category, and a \
             sub-second stutter all have to be dropped"
        );
        assert_eq!(segments[0].start, Duration::from_secs_f64(10.5));
        assert_eq!(segments[0].end, Duration::from_secs(25));
    }

    #[test]
    fn an_unknown_category_does_not_poison_the_rest() {
        // The API is free to add categories; a build that has not learned one must still
        // skip the sponsors it does understand.
        let raw = serde_json::json!([{
            "videoID": "dQw4w9WgXcQ",
            "segments": [
                {"segment": [5.0, 15.0], "UUID": "u-new", "category": "brand_new_thing",
                 "actionType": "skip"},
                {"segment": [20.0, 30.0], "UUID": "u-known", "category": "sponsor",
                 "actionType": "skip"}
            ]
        }])
        .to_string();
        let video = VideoId::parse("dQw4w9WgXcQ").unwrap();
        let segments = parse_response(&raw, &video, SPONSOR, Duration::from_secs(1)).unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].uuid.as_str(), "u-known");
    }

    #[test]
    fn overlapping_submissions_become_one_seek() {
        let raw = serde_json::json!([{
            "videoID": "dQw4w9WgXcQ",
            "segments": [
                {"segment": [10.0, 25.0], "UUID": "a", "category": "sponsor", "actionType": "skip"},
                {"segment": [20.0, 40.0], "UUID": "b", "category": "sponsor", "actionType": "skip"},
                {"segment": [80.0, 90.0], "UUID": "c", "category": "sponsor", "actionType": "skip"}
            ]
        }])
        .to_string();
        let video = VideoId::parse("dQw4w9WgXcQ").unwrap();
        let segments = parse_response(&raw, &video, SPONSOR, Duration::from_secs(1)).unwrap();
        assert_eq!(segments.len(), 2, "10-25 and 20-40 are one break, not two");
        assert_eq!(segments[0].start, Duration::from_secs(10));
        assert_eq!(segments[0].end, Duration::from_secs(40));
    }

    #[test]
    fn nonsense_ranges_are_dropped_rather_than_seeked_to() {
        let raw = serde_json::json!([{
            "videoID": "dQw4w9WgXcQ",
            "segments": [
                {"segment": [30.0, 10.0], "UUID": "backwards", "category": "sponsor", "actionType": "skip"},
                {"segment": [-5.0, 10.0], "UUID": "negative", "category": "sponsor", "actionType": "skip"},
                {"segment": [10.0, 10.0], "UUID": "empty", "category": "sponsor", "actionType": "skip"}
            ]
        }])
        .to_string();
        let video = VideoId::parse("dQw4w9WgXcQ").unwrap();
        assert!(parse_response(&raw, &video, SPONSOR, Duration::ZERO)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn video_ids_are_parsed() {
        assert!(VideoId::parse("dQw4w9WgXcQ").is_ok());
        assert!(VideoId::parse("").is_err());
        assert!(VideoId::parse("too-short").is_err());
        assert!(
            VideoId::parse("PLplaylistid").is_err(),
            "12 chars is not a video"
        );
        assert!(VideoId::parse("bad/chars!!!").is_err());
    }
}
