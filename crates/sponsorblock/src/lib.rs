//! # sponsorblock
//!
//! Segment lookup and skip planning against the [SponsorBlock] database — the pure half.
//! Hashing, URL building, response parsing, filtering, and the decision of *when* to skip
//! all live here as `fn(state, bytes) -> decision` (ground rule 3); the HTTP call and the
//! Lounge command that performs the skip live in `app`.
//!
//! Lookups use the **hash-prefix** endpoint, never the plain `?videoID=` one: the client
//! sends only the first four hex characters of `sha256(videoId)` and filters the answer
//! locally, so the server never learns which video is playing. That is a privacy property
//! worth keeping even on a display in a room full of people who can see the screen
//! anyway — it costs nothing and it is the endpoint the project asks clients to use.
//!
//! **The database is CC BY-NC-SA 4.0.** Non-commercial use is fine (a hackerspace display
//! is squarely that) and attribution is required, which is why [`ATTRIBUTION`] exists and
//! is shown on screen. Do not persist segments to disk or ship them with the binary:
//! redistributing the database pulls ShareAlike in. An in-memory cache for the session is
//! the intended shape, and is what this supports.
//!
//! [SponsorBlock]: https://sponsor.ajay.app/
#![forbid(unsafe_code)]

pub mod error;
pub mod plan;
pub mod segment;

pub use error::SponsorBlockError;
pub use plan::{Decision, Planner};
pub use segment::{ActionType, Category, Segment, SegmentUuid, VideoId};

/// Credit the database, on screen. Required by CC BY-NC-SA 4.0.
pub const ATTRIBUTION: &str = "Segments by SponsorBlock (sponsor.ajay.app), CC BY-NC-SA 4.0";

/// The public API host.
pub const API: &str = "https://sponsor.ajay.app";

/// Build the hash-prefix lookup URL for a video.
///
/// The prefix is the first four hex characters of `sha256(videoId)` — the length the API
/// documents as the recommended trade of privacy against response size. The server
/// answers with *every* video sharing that prefix; [`segment::parse_response`] picks ours
/// out, which is what keeps the lookup private.
#[must_use]
pub fn lookup_url(video: &VideoId, categories: &[Category]) -> String {
    use std::fmt::Write as _;

    let mut url = format!("{API}/api/skipSegments/{}", video.hash_prefix());
    let mut sep = '?';
    for category in categories {
        // Unknown is a parse-time fallback, not something to ask the server for.
        if let Some(name) = category.as_api_str() {
            let _ = write!(url, "{sep}category={name}");
            sep = '&';
        }
    }
    // Only `skip` is actionable over the Lounge: muting would need volume juggling, and
    // poi/chapter are player UI we do not drive.
    let _ = write!(url, "{sep}actionType=skip&service=youtube");
    url
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lookup_url_sends_a_prefix_and_never_the_video_id() {
        let video = VideoId::parse("dQw4w9WgXcQ").unwrap();
        let url = lookup_url(&video, &[Category::Sponsor, Category::SelfPromo]);
        assert!(
            !url.contains("dQw4w9WgXcQ"),
            "the whole point is that the id does not leave the box: {url}"
        );
        assert!(url.starts_with("https://sponsor.ajay.app/api/skipSegments/"));
        assert!(url.contains("category=sponsor"));
        assert!(url.contains("category=selfpromo"));
        assert!(url.contains("actionType=skip"));
    }

    #[test]
    fn the_prefix_is_four_hex_characters_of_the_sha256() {
        // sha256("dQw4w9WgXcQ") begins with these; a wrong hash means the server answers
        // with a bucket our video is not in, and nothing ever skips.
        let video = VideoId::parse("dQw4w9WgXcQ").unwrap();
        let prefix = video.hash_prefix();
        assert_eq!(prefix.len(), 4);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(prefix, &video.hash_hex()[..4]);
    }
}
