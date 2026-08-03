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

/// How the sender repeats when the current item ends.
///
/// An enum rather than a boolean because the two repeat modes are genuinely different
/// answers to "what happens next", and every sender that has repeat at all has both:
/// repeating one track forever and repeating the album are different requests, and a
/// `bool` would force whoever renders the button to guess which one is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RepeatMode {
    /// Play to the end of the queue and stop.
    #[default]
    Off,
    /// Repeat the current item.
    Track,
    /// Repeat the whole queue or context.
    Context,
}

impl RepeatMode {
    /// The next mode when someone presses the repeat button.
    ///
    /// Off → Context → Track → Off, which is the order every music app uses: the common
    /// want is "keep this playlist going", and repeat-one is the deliberate extra press.
    #[must_use]
    pub const fn cycled(self) -> Self {
        match self {
            Self::Off => Self::Context,
            Self::Context => Self::Track,
            // Including the catch-all: this enum is non-exhaustive, and a mode we do not
            // know about should still get the user back to a state they recognise.
            _ => Self::Off,
        }
    }

    /// Whether repeat is on at all, in any mode.
    #[must_use]
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Image encodings a peer may hand us for cover art.
///
/// A closed enum rather than a MIME string: the decoder has to match on it anyway, and
/// parsing at the boundary means an unrecognised format is refused where it arrives
/// instead of failing inside an image decoder three layers down (ground rule 1).
///
/// **The list is deliberately everything `pipeline` can decode, not a curated subset.**
/// Cover art is whatever bytes a phone, a CDN or somebody's DLNA server chose to send, and
/// a blank square because the sender picked WebP is a worse outcome than a few more
/// decoders in the binary. `pipeline` has a test that fails if the two lists drift, which
/// is how they are kept honest — they had drifted, and #87 is what that cost.
///
/// AVIF is the one absence, and it is a real constraint rather than a preference: the
/// `image` crate decodes it only through dav1d, a C library, and every codec in this tree
/// is pure Rust so that the Windows build stays a cross-build.
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
    /// WebP. Common from CDNs, and the likeliest of these to turn up in real use.
    WebP,
    /// TIFF.
    Tiff,
    /// Windows icon.
    Ico,
    /// Truevision TGA.
    Tga,
    /// Radiance HDR.
    Hdr,
    /// OpenEXR.
    OpenExr,
    /// Netpbm — PBM, PGM, PPM and PAM.
    Pnm,
    /// QOI.
    Qoi,
}

impl ImageFormat {
    /// Every format, so a caller can check this list against a decoder's.
    ///
    /// Exists because the enum is `#[non_exhaustive]`: without it, the test that keeps
    /// this in step with `pipeline`'s decoder set could not enumerate what to check, and
    /// a variant added here would silently go unverified.
    pub const ALL: [Self; 12] = [
        Self::Jpeg,
        Self::Png,
        Self::Gif,
        Self::Bmp,
        Self::WebP,
        Self::Tiff,
        Self::Ico,
        Self::Tga,
        Self::Hdr,
        Self::OpenExr,
        Self::Pnm,
        Self::Qoi,
    ];

    /// Parse an image format from a MIME type or a BIP `encoding` token.
    ///
    /// Accepts both spellings because the two live side by side in this stack: OBEX/BIP
    /// image descriptors carry bare tokens (`JPEG`), while everything else says
    /// `image/jpeg`. The aliases are the ones that actually appear on a wire — `jpg` from
    /// senders that spell it after the file extension, `x-targa` and `x-icon` because
    /// those are the registered types, and the individual Netpbm spellings because a
    /// server that has a PPM says `image/x-portable-pixmap` rather than the general form.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let token = s.rsplit('/').next().unwrap_or(s).trim();
        match token.to_ascii_lowercase().as_str() {
            "jpeg" | "jpg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "gif" => Some(Self::Gif),
            "bmp" | "x-bmp" | "x-ms-bmp" => Some(Self::Bmp),
            "webp" => Some(Self::WebP),
            "tiff" | "tif" => Some(Self::Tiff),
            "ico" | "x-icon" | "vnd.microsoft.icon" => Some(Self::Ico),
            "tga" | "targa" | "x-targa" | "x-tga" => Some(Self::Tga),
            "hdr" | "vnd.radiance" => Some(Self::Hdr),
            "exr" | "x-exr" => Some(Self::OpenExr),
            "pnm" | "x-portable-anymap" | "x-portable-bitmap" | "x-portable-graymap"
            | "x-portable-pixmap" => Some(Self::Pnm),
            "qoi" | "x-qoi" => Some(Self::Qoi),
            _ => None,
        }
    }

    /// The canonical MIME type.
    ///
    /// These match what the `image` crate calls each format, deliberately: the test that
    /// checks this enum against the decoder set looks the format up *by this string*, so
    /// a spelling that disagrees is a spelling that fails the build rather than one that
    /// quietly mislabels a picture.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Bmp => "image/bmp",
            Self::WebP => "image/webp",
            Self::Tiff => "image/tiff",
            Self::Ico => "image/x-icon",
            Self::Tga => "image/x-targa",
            Self::Hdr => "image/vnd.radiance",
            Self::OpenExr => "image/x-exr",
            Self::Pnm => "image/x-portable-anymap",
            Self::Qoi => "image/x-qoi",
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
    /// Whether the sender is shuffling, when it says so.
    ///
    /// `None` and `Some(false)` are different facts and the panel treats them
    /// differently: a source that never reports shuffle gets no shuffle button, while one
    /// that reports it off gets a dimmed one. Rendering "off" for "unknown" would offer a
    /// control that does nothing.
    pub shuffle: Option<bool>,
    /// How the sender repeats, when it says so. `None` as for [`NowPlaying::shuffle`].
    pub repeat: Option<RepeatMode>,
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

    /// Builder-style shuffle setter.
    #[must_use]
    pub fn with_shuffle(mut self, shuffle: bool) -> Self {
        self.shuffle = Some(shuffle);
        self
    }

    /// Builder-style repeat setter.
    #[must_use]
    pub fn with_repeat(mut self, repeat: RepeatMode) -> Self {
        self.repeat = Some(repeat);
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
        // WebP used to be refused here, which was a gate with nothing behind it: the
        // decoder can read it, and a CDN is more likely to send it than half the formats
        // that were accepted (#87).
        assert_eq!(ImageFormat::parse("image/webp"), Some(ImageFormat::WebP));
        // The registered spellings, not just the tidy ones.
        assert_eq!(ImageFormat::parse("image/x-icon"), Some(ImageFormat::Ico));
        assert_eq!(
            ImageFormat::parse("image/x-portable-pixmap"),
            Some(ImageFormat::Pnm)
        );
        // …and something genuinely absent still is. AVIF decoding needs dav1d, which is C.
        assert_eq!(ImageFormat::parse("image/avif"), None);
        assert_eq!(ImageFormat::parse("application/pdf"), None);
    }

    #[test]
    fn every_format_round_trips_through_its_own_mime_type() {
        // `mime()` is what `pipeline`'s drift test looks each format up by, so a variant
        // whose MIME type does not parse back would silently drop out of that check.
        for format in ImageFormat::ALL {
            assert_eq!(
                ImageFormat::parse(format.mime()),
                Some(format),
                "{format:?} does not parse back from {}",
                format.mime()
            );
        }
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
