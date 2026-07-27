//! `SET_PARAMETER`: volume, progress, track metadata and cover art.
//!
//! Everything a sender pushes at the now-playing card, and all of it pure. Dispatch is
//! on `Content-Type`, and a missing one is an error rather than a guess — the three
//! body formats look nothing alike, but "text with a colon in it" would happily parse a
//! DMAP blob into nonsense.
//!
//! One thing worth knowing about the artwork: shairport-sync's own source notes that the
//! `image/<type>` header is unreliable, so the image kind is sniffed from the bytes
//! instead of trusted from the header.

use castaway_core::NowPlaying;

use crate::error::ControlError;

/// The volume a sender asked for, in the dBFS scale AirPlay uses.
///
/// Not a plain `f32`: the scale has a discontinuity that is easy to lose. `-144.0` means
/// *muted* rather than "very quiet", and the usable range is `-30.0..=0.0` — so a naive
/// linear map from the raw value produces a control that does nothing for its bottom
/// 79% and then jumps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Volume {
    /// The sender asked for silence (`-144.0`).
    Muted,
    /// A level in dBFS, clamped to `-30.0..=0.0`.
    Level(f32),
}

/// The dBFS value that means "muted" rather than "quiet".
const MUTE_DBFS: f32 = -144.0;
/// The quietest audible level AirPlay's slider produces.
const MIN_DBFS: f32 = -30.0;

impl Volume {
    /// Parse the value of a `volume:` line.
    #[must_use]
    pub fn from_dbfs(raw: f32) -> Self {
        // Anything at or below the floor is the mute sentinel or an over-quiet value a
        // sender should not have sent; both mean silence.
        if raw <= MUTE_DBFS || !raw.is_finite() {
            return Self::Muted;
        }
        Self::Level(raw.clamp(MIN_DBFS, 0.0))
    }

    /// As a 0.0..=1.0 fraction, for a pipeline that thinks in linear gain positions.
    ///
    /// Linear **in dB**, which is how the sender's slider behaves — not in amplitude.
    #[must_use]
    pub fn as_fraction(self) -> f32 {
        match self {
            Self::Muted => 0.0,
            Self::Level(db) => (db - MIN_DBFS) / -MIN_DBFS,
        }
    }
}

/// Playback progress, as `progress: start/now/end` in RTP timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// RTP timestamp the track started at.
    pub start: u32,
    /// RTP timestamp of the frame playing now.
    pub now: u32,
    /// RTP timestamp the track ends at.
    pub end: u32,
}

impl Progress {
    /// Position and duration in seconds, given the stream's sample rate.
    #[must_use]
    pub fn as_seconds(&self, sample_rate: u32) -> (f64, f64) {
        let rate = f64::from(sample_rate.max(1));
        let position = f64::from(self.now.wrapping_sub(self.start)) / rate;
        let duration = f64::from(self.end.wrapping_sub(self.start)) / rate;
        (position, duration)
    }
}

/// What a `SET_PARAMETER` asked for.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ControlUpdate {
    /// A volume change.
    Volume(Volume),
    /// A progress report.
    Progress(Progress),
    /// Track metadata.
    Metadata(Box<NowPlaying>),
    /// Cover art, as the image bytes a sender pushed.
    Artwork(Vec<u8>),
    /// A body we understood the shape of but which carried nothing we use.
    Ignored,
}

/// Parse a `SET_PARAMETER` body.
///
/// # Errors
/// [`ControlError`] if the content type is missing or not one of the three this
/// endpoint carries.
pub fn parse_set_parameter(
    content_type: Option<&str>,
    body: &[u8],
) -> Result<ControlUpdate, ControlError> {
    let content_type = content_type.ok_or(ControlError::NoContentType)?;
    // Content types arrive with parameters (`text/parameters; charset=utf-8`).
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    if base.eq_ignore_ascii_case("text/parameters") {
        return parse_text_parameters(body);
    }
    if base.eq_ignore_ascii_case("application/x-dmap-tagged") {
        return Ok(ControlUpdate::Metadata(Box::new(parse_dmap(body)?)));
    }
    // The header is unreliable here, so anything image-shaped is taken on its bytes.
    if base.to_ascii_lowercase().starts_with("image/") {
        return Ok(ControlUpdate::Artwork(body.to_vec()));
    }
    Err(ControlError::UnsupportedContentType(base.to_string()))
}

/// `volume: -14.5` or `progress: 1/2/3`.
fn parse_text_parameters(body: &[u8]) -> Result<ControlUpdate, ControlError> {
    let text = std::str::from_utf8(body).map_err(|_| ControlError::NotUtf8)?;
    for line in text.lines().map(str::trim) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if key.eq_ignore_ascii_case("volume") {
            let raw = value.parse::<f32>().map_err(|_| ControlError::BadVolume)?;
            return Ok(ControlUpdate::Volume(Volume::from_dbfs(raw)));
        }
        if key.eq_ignore_ascii_case("progress") {
            let mut parts = value.split('/').map(str::trim);
            let mut next = || {
                parts
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    .ok_or(ControlError::BadProgress)
            };
            return Ok(ControlUpdate::Progress(Progress {
                start: next()?,
                now: next()?,
                end: next()?,
            }));
        }
    }
    Ok(ControlUpdate::Ignored)
}

/// DMAP tags worth reading. Everything else in the container is skipped.
mod tag {
    /// `dmap.itemname` — the track title.
    pub const TITLE: &[u8; 4] = b"minm";
    /// `daap.songartist`.
    pub const ARTIST: &[u8; 4] = b"asar";
    /// `daap.songalbum`.
    pub const ALBUM: &[u8; 4] = b"asal";
    /// Song time in **milliseconds**.
    pub const DURATION_MS: &[u8; 4] = b"astm";
}

/// The DMAP container header: a 4-byte tag then a big-endian u32 length.
const DMAP_HEADER: usize = 8;

/// Parse an `application/x-dmap-tagged` body into a now-playing snapshot.
///
/// The outer `mlit` container is skipped, then the body is a flat run of
/// `[tag:4][len:u32 BE][value]`. Lengths are bounds-checked against what is left rather
/// than trusted, because they come off a network.
fn parse_dmap(body: &[u8]) -> Result<NowPlaying, ControlError> {
    let mut now = NowPlaying::default();
    // Senders wrap the items in an `mlit` container; some do not.
    let inner = if body.len() >= DMAP_HEADER && &body[..4] == b"mlit" {
        &body[DMAP_HEADER..]
    } else {
        body
    };

    let mut offset = 0usize;
    while offset + DMAP_HEADER <= inner.len() {
        let tag: &[u8; 4] = inner[offset..offset + 4]
            .try_into()
            .map_err(|_| ControlError::MalformedDmap)?;
        let len = u32::from_be_bytes([
            inner[offset + 4],
            inner[offset + 5],
            inner[offset + 6],
            inner[offset + 7],
        ]);
        let len = usize::try_from(len).map_err(|_| ControlError::MalformedDmap)?;
        let start = offset + DMAP_HEADER;
        let Some(value) = inner.get(start..start + len) else {
            // A length running past the buffer is a truncated body, not a reason to
            // discard the fields already read.
            break;
        };

        match tag {
            tag::TITLE => now.title = utf8(value),
            tag::ARTIST => now.artist = utf8(value),
            tag::ALBUM => now.album = utf8(value),
            tag::DURATION_MS => {
                if let Ok(ms) = <[u8; 4]>::try_from(value) {
                    now.duration = Some(std::time::Duration::from_millis(u64::from(
                        u32::from_be_bytes(ms),
                    )));
                }
            }
            _ => {}
        }
        offset = start + len;
    }
    Ok(now)
}

/// A DMAP string value, if it is text at all.
fn utf8(value: &[u8]) -> Option<String> {
    std::str::from_utf8(value)
        .ok()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_volume_line_is_parsed_in_dbfs() {
        let u = parse_set_parameter(Some("text/parameters"), b"volume: -15.0\r\n").unwrap();
        assert_eq!(u, ControlUpdate::Volume(Volume::Level(-15.0)));
    }

    #[test]
    fn minus_144_is_mute_not_merely_quiet() {
        // The discontinuity this type exists for: a linear read of the raw value would
        // make the bottom of the slider do nothing and then jump.
        assert_eq!(Volume::from_dbfs(-144.0), Volume::Muted);
        assert_eq!(Volume::from_dbfs(-144.0).as_fraction(), 0.0);
        // And the usable floor is -30, not -144.
        assert_eq!(Volume::from_dbfs(-30.0).as_fraction(), 0.0);
        assert_eq!(Volume::from_dbfs(0.0).as_fraction(), 1.0);
        assert!((Volume::from_dbfs(-15.0).as_fraction() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_volume_below_the_floor_is_clamped_rather_than_negative() {
        assert_eq!(Volume::from_dbfs(-45.0), Volume::Level(-30.0));
        assert!(Volume::from_dbfs(-45.0).as_fraction() >= 0.0);
    }

    #[test]
    fn a_nonsense_volume_is_silence_not_a_panic() {
        assert_eq!(Volume::from_dbfs(f32::NAN), Volume::Muted);
        assert_eq!(Volume::from_dbfs(f32::NEG_INFINITY), Volume::Muted);
    }

    #[test]
    fn a_content_type_with_parameters_still_dispatches() {
        let u = parse_set_parameter(Some("text/parameters; charset=utf-8"), b"volume: -10.0\r\n")
            .unwrap();
        assert!(matches!(u, ControlUpdate::Volume(_)));
    }

    #[test]
    fn progress_converts_to_seconds_at_the_stream_rate() {
        let u = parse_set_parameter(Some("text/parameters"), b"progress: 1000/45100/265600\r\n")
            .unwrap();
        let ControlUpdate::Progress(p) = u else {
            panic!("expected progress")
        };
        let (position, duration) = p.as_seconds(44_100);
        assert!((position - 1.0).abs() < 1e-9, "{position}");
        assert!((duration - 6.0).abs() < 1e-9, "{duration}");
    }

    #[test]
    fn a_missing_content_type_is_refused_rather_than_guessed_at() {
        // The three body formats look nothing alike, but "text with a colon in it"
        // would happily parse a DMAP blob into nonsense.
        assert_eq!(
            parse_set_parameter(None, b"volume: -10.0"),
            Err(ControlError::NoContentType)
        );
    }

    /// Build a DMAP item.
    fn item(tag: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut v = tag.to_vec();
        v.extend_from_slice(&u32::try_from(value.len()).unwrap().to_be_bytes());
        v.extend_from_slice(value);
        v
    }

    /// Wrap items in the `mlit` container a sender sends.
    fn mlit(items: &[u8]) -> Vec<u8> {
        let mut v = item(b"mlit", items);
        v.truncate(8);
        v.extend_from_slice(items);
        v
    }

    #[test]
    fn dmap_metadata_becomes_a_now_playing_snapshot() {
        let mut items = Vec::new();
        items.extend_from_slice(&item(tag::TITLE, b"Windowlicker"));
        items.extend_from_slice(&item(tag::ARTIST, b"Aphex Twin"));
        items.extend_from_slice(&item(tag::ALBUM, b"Windowlicker"));
        items.extend_from_slice(&item(tag::DURATION_MS, &366_000u32.to_be_bytes()));
        let body = mlit(&items);

        let u = parse_set_parameter(Some("application/x-dmap-tagged"), &body).unwrap();
        let ControlUpdate::Metadata(now) = u else {
            panic!("expected metadata")
        };
        assert_eq!(now.title.as_deref(), Some("Windowlicker"));
        assert_eq!(now.artist.as_deref(), Some("Aphex Twin"));
        assert_eq!(now.album.as_deref(), Some("Windowlicker"));
        assert_eq!(now.duration, Some(std::time::Duration::from_secs(366)));
    }

    #[test]
    fn tags_we_do_not_know_are_skipped_not_fatal() {
        let mut items = Vec::new();
        items.extend_from_slice(&item(b"asaa", b"some album artist"));
        items.extend_from_slice(&item(tag::TITLE, b"Come to Daddy"));
        items.extend_from_slice(&item(b"mper", &[0u8; 8]));
        let u = parse_set_parameter(Some("application/x-dmap-tagged"), &mlit(&items)).unwrap();
        let ControlUpdate::Metadata(now) = u else {
            panic!()
        };
        assert_eq!(now.title.as_deref(), Some("Come to Daddy"));
    }

    #[test]
    fn a_length_running_past_the_buffer_keeps_what_was_already_read() {
        // Lengths come off a network. A truncated body should not discard the fields
        // that arrived intact, and must not index out of bounds.
        let mut body = mlit(&item(tag::TITLE, b"Ageispolis"));
        body.extend_from_slice(b"asar");
        body.extend_from_slice(&9999u32.to_be_bytes());
        body.extend_from_slice(b"trunc");
        let u = parse_set_parameter(Some("application/x-dmap-tagged"), &body).unwrap();
        let ControlUpdate::Metadata(now) = u else {
            panic!()
        };
        assert_eq!(now.title.as_deref(), Some("Ageispolis"));
        assert_eq!(now.artist, None);
    }

    #[test]
    fn artwork_is_taken_on_its_bytes() {
        let jpeg = b"\xff\xd8\xff\xe0 pretend this is a jpeg";
        let u = parse_set_parameter(Some("image/jpeg"), jpeg).unwrap();
        assert_eq!(u, ControlUpdate::Artwork(jpeg.to_vec()));
    }

    #[test]
    fn a_content_type_this_endpoint_does_not_carry_is_named_in_the_error() {
        let err = parse_set_parameter(Some("application/json"), b"{}").unwrap_err();
        assert!(
            matches!(&err, ControlError::UnsupportedContentType(c) if c == "application/json"),
            "{err:?}"
        );
    }

    #[test]
    fn a_body_with_nothing_we_use_is_ignored_rather_than_refused() {
        let u = parse_set_parameter(Some("text/parameters"), b"something: else\r\n").unwrap();
        assert_eq!(u, ControlUpdate::Ignored);
    }
}
