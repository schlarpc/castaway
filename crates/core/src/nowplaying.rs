//! Track metadata for the "now playing" surface.
//!
//! Every protocol here has a version of this — Cast's `MediaStatus`, DLNA's
//! `AVTransportURIMetaData`, Spotify's player state, AVRCP's `GetElementAttributes` — so
//! it belongs in `core` rather than in whichever adapter got here first. Bluetooth is
//! simply the first source rich enough to justify building it, because an audio-only
//! session has nothing to put on a 65" panel *except* this.
//!
//! A [`NowPlaying`] is a **full snapshot**, not a delta. Adapters own their protocol
//! state and re-emit a complete picture whenever any part of it changes; the renderer
//! just draws the latest one it was handed. That matters for artwork in particular: cover
//! art arrives over a separate fetch that completes well after the track change, so it
//! shows up as a second snapshot with [`NowPlaying::artwork`] populated rather than
//! delaying the text.

use std::fmt;
use std::time::Duration;

/// What the sender is currently doing with the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PlaybackState {
    /// Nothing loaded, or playback finished.
    #[default]
    Stopped,
    /// Actively playing.
    Playing,
    /// Paused mid-item, position retained.
    Paused,
    /// Scrubbing forward.
    SeekingForward,
    /// Scrubbing backward.
    SeekingBackward,
    /// The sender reported an error state for the current item.
    Error,
}

impl PlaybackState {
    /// Whether audio should be expected to be flowing right now. Used by the renderer to
    /// decide between the playing and paused treatments.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Playing | Self::SeekingForward | Self::SeekingBackward
        )
    }
}

/// Image encodings a peer may hand us for cover art.
///
/// A closed enum rather than a MIME string: the decoder has to match on it anyway, and
/// parsing at the boundary means an unrecognised format is refused where it arrives
/// instead of failing inside an image decoder three layers down (ground rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageFormat {
    /// JPEG. What AVRCP cover art overwhelmingly returns.
    Jpeg,
    /// PNG.
    Png,
    /// GIF.
    Gif,
    /// Windows bitmap. Permitted by BIP; rare in practice.
    Bmp,
}

impl ImageFormat {
    /// Parse an image format from a MIME type or a BIP `encoding` token.
    ///
    /// Accepts both spellings because the two live side by side in this stack: OBEX/BIP
    /// image descriptors carry bare tokens (`JPEG`), while everything else says
    /// `image/jpeg`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let token = s.rsplit('/').next().unwrap_or(s).trim();
        match token.to_ascii_lowercase().as_str() {
            "jpeg" | "jpg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "gif" => Some(Self::Gif),
            "bmp" => Some(Self::Bmp),
            _ => None,
        }
    }

    /// The canonical MIME type.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Bmp => "image/bmp",
        }
    }
}

/// Cover art bytes, tagged with a format the decoder actually supports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    /// Encoding of `data`.
    pub format: ImageFormat,
    /// The encoded image, exactly as the peer sent it. Decoding happens in `pipeline`.
    pub data: bytes::Bytes,
}

impl Artwork {
    /// Wrap encoded image bytes.
    #[must_use]
    pub const fn new(format: ImageFormat, data: bytes::Bytes) -> Self {
        Self { format, data }
    }

    /// Size of the encoded image in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the artwork payload is empty (a peer that answered with nothing).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// One item waiting behind the current track.
///
/// Deliberately thin — a title and who it is by. This exists to answer "whose song is
/// next" on a shared screen, not to mirror the sender's whole queue model, and anything
/// richer would have to be invented for the protocols that do not supply it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueItem {
    /// Track title, or the best label the source could give.
    pub title: String,
    /// Performing artist, when the source says.
    pub artist: Option<String>,
}

impl QueueItem {
    /// An item with just a title.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            artist: None,
        }
    }

    /// Builder-style artist setter.
    #[must_use]
    pub fn with_artist(mut self, artist: impl Into<String>) -> Self {
        self.artist = Some(artist.into());
        self
    }
}

impl fmt::Display for QueueItem {
    /// `Title — Artist`, or just the title when nobody said who it is by.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.title)?;
        if let Some(artist) = &self.artist {
            write!(f, " \u{2014} {artist}")?;
        }
        Ok(())
    }
}

/// A complete snapshot of what the active source is playing.
///
/// Every field is optional because every field genuinely is: AVRCP peers routinely
/// return a title and nothing else, and a receiver that renders "Unknown Artist" over a
/// blank string looks broken in a way that an absent field does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct NowPlaying {
    /// Track title.
    pub title: Option<String>,
    /// Performing artist.
    pub artist: Option<String>,
    /// Album name.
    pub album: Option<String>,
    /// Genre, when the peer bothers.
    pub genre: Option<String>,
    /// Track number within the album, and the album's total if known.
    pub track: Option<(u32, Option<u32>)>,
    /// Total length of the item.
    pub duration: Option<Duration>,
    /// Playback position within the item, as of when this snapshot was taken.
    pub position: Option<Duration>,
    /// What the sender is doing.
    pub state: PlaybackState,
    /// Cover art, once fetched. Absent on the first snapshot for a track.
    pub artwork: Option<Artwork>,
}

impl NowPlaying {
    /// An empty snapshot in the given state.
    #[must_use]
    pub fn new(state: PlaybackState) -> Self {
        Self {
            state,
            ..Self::default()
        }
    }

    /// Whether this snapshot has any text worth rendering. A snapshot with no text and
    /// no art is a state change only, and shouldn't replace a populated card on screen.
    #[must_use]
    pub fn has_text(&self) -> bool {
        self.title.is_some() || self.artist.is_some() || self.album.is_some()
    }

    /// Whether this snapshot describes a different *item* than `other`.
    ///
    /// Compares only the identifying text, deliberately ignoring position, state and
    /// artwork — it answers "should the card change?", which is what drives both the
    /// re-render and the decision to go fetch new cover art.
    #[must_use]
    pub fn is_same_item(&self, other: &Self) -> bool {
        self.title == other.title && self.artist == other.artist && self.album == other.album
    }

    /// Builder-style title setter.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Builder-style artist setter.
    #[must_use]
    pub fn with_artist(mut self, artist: impl Into<String>) -> Self {
        self.artist = Some(artist.into());
        self
    }

    /// Builder-style album setter.
    #[must_use]
    pub fn with_album(mut self, album: impl Into<String>) -> Self {
        self.album = Some(album.into());
        self
    }

    /// Builder-style artwork setter.
    #[must_use]
    pub fn with_artwork(mut self, artwork: Artwork) -> Self {
        self.artwork = Some(artwork);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_format_parses_both_mime_and_bip_spellings() {
        // BIP image descriptors say `JPEG`; everything else says `image/jpeg`. Both
        // reach this parser, so both have to land on the same variant.
        assert_eq!(ImageFormat::parse("image/jpeg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("JPEG"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::parse("image/png"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::parse("image/webp"), None);
    }

    #[test]
    fn artwork_arriving_late_does_not_change_the_item() {
        // The load-bearing behaviour for cover art: the text snapshot lands first and
        // the image lands seconds later. The renderer must treat the second snapshot as
        // the *same* card gaining art, not as a track change.
        let text = NowPlaying::default()
            .with_title("Bloom")
            .with_artist("Beach House");
        let with_art = text.clone().with_artwork(Artwork::new(
            ImageFormat::Jpeg,
            bytes::Bytes::from_static(&[1, 2, 3]),
        ));
        assert!(text.is_same_item(&with_art));
        assert!(with_art.artwork.is_some());
    }

    #[test]
    fn position_and_state_do_not_make_it_a_new_item() {
        let a = NowPlaying::default().with_title("Bloom");
        let mut b = a.clone();
        b.position = Some(Duration::from_secs(30));
        b.state = PlaybackState::Paused;
        assert!(a.is_same_item(&b));
    }

    #[test]
    fn a_different_track_is_a_different_item() {
        let a = NowPlaying::default().with_title("Bloom");
        let b = NowPlaying::default().with_title("Myth");
        assert!(!a.is_same_item(&b));
    }

    #[test]
    fn a_bare_state_change_carries_no_text() {
        assert!(!NowPlaying::new(PlaybackState::Paused).has_text());
        assert!(NowPlaying::default().with_title("x").has_text());
    }

    #[test]
    fn seeking_counts_as_active_playback() {
        assert!(PlaybackState::Playing.is_active());
        assert!(PlaybackState::SeekingForward.is_active());
        assert!(!PlaybackState::Paused.is_active());
        assert!(!PlaybackState::Stopped.is_active());
    }
}
