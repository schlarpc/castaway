//! SponsorBlock errors.

use thiserror::Error;

/// Failures parsing SponsorBlock data.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SponsorBlockError {
    /// A video id was not the 11-character YouTube form.
    #[error("not a youtube video id: {0}")]
    NotAVideoId(&'static str),

    /// The API response was not the JSON shape documented.
    #[error("malformed skipSegments response: {0}")]
    Malformed(String),
}
