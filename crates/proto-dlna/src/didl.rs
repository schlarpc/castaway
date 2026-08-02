//! DIDL-Lite: the metadata a control point hands over with the URI.
//!
//! `SetAVTransportURI` carries two things — a URL and a `CurrentURIMetaData` blob — and
//! this receiver used to store the second one and echo it back verbatim without ever
//! looking inside. That is enough to satisfy a control point reading `GetMediaInfo` and
//! useless for the panel: a DLNA cast put a title on nobody's screen, so music arrived as
//! sound with a blank wall behind it while Bluetooth and Spotify both drew a full card.
//!
//! What is parsed is what the card can show, and no more. DIDL-Lite is a large, extensible
//! schema and the overwhelming majority of it describes *browsing* a media server, which
//! this receiver does not do.
//!
//! Parsing is deliberately forgiving about namespaces. The spec says `dc:title` and
//! `upnp:artist`, and real control points variously bind those prefixes differently, omit
//! them, or invent their own — so elements are matched on local name. The failure mode of
//! being strict is a blank card for a file that told us its title, which is worse than the
//! failure mode of being loose (matching a `title` that meant something else, in a document
//! whose only purpose is to describe the item we are about to play).

use std::time::Duration;

use castaway_core::NowPlaying;

/// What kind of item the control point says it sent.
///
/// From `upnp:class`, which is the only place a control point states its *intent*. Worth
/// having separately from what the container turns out to hold: an audio-only URL is
/// music either way, but a control point that says `videoItem` and hands over something
/// with no video stream is a broken link rather than a music session, and the two deserve
/// different words on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemKind {
    /// `object.item.audioItem…` — music.
    Audio,
    /// `object.item.videoItem…`.
    Video,
    /// `object.item.imageItem…` — a still.
    Image,
    /// No `upnp:class`, or one we do not recognise.
    #[default]
    Unknown,
}

impl ItemKind {
    fn parse(class: &str) -> Self {
        let class = class.to_ascii_lowercase();
        if class.contains("audioitem") {
            Self::Audio
        } else if class.contains("videoitem") {
            Self::Video
        } else if class.contains("imageitem") {
            Self::Image
        } else {
            Self::Unknown
        }
    }
}

/// The parts of a DIDL-Lite item this receiver has a use for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Didl {
    /// `dc:title`.
    pub title: Option<String>,
    /// `upnp:artist`, falling back to `dc:creator`.
    pub artist: Option<String>,
    /// `upnp:album`.
    pub album: Option<String>,
    /// `upnp:albumArtURI`. A URL, not bytes — fetching it is the caller's business.
    pub art_url: Option<String>,
    /// `res@duration`, when the control point stated one.
    pub duration: Option<Duration>,
    /// What `upnp:class` said this is.
    pub kind: ItemKind,
}

impl Didl {
    /// Whether anything at all was found worth showing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.artist.is_none() && self.album.is_none()
    }

    /// Fold into a now-playing snapshot.
    ///
    /// Only overwrites what DIDL actually supplied, so a later snapshot carrying the
    /// container's own tags — or the artwork, which arrives from a separate fetch — is not
    /// erased by a control point that sent a title and nothing else.
    #[must_use]
    pub fn apply_to(&self, mut track: NowPlaying) -> NowPlaying {
        if self.title.is_some() {
            track.title = self.title.clone();
        }
        if self.artist.is_some() {
            track.artist = self.artist.clone();
        }
        if self.album.is_some() {
            track.album = self.album.clone();
        }
        if self.duration.is_some() {
            track.duration = self.duration;
        }
        track
    }
}

/// Parse a `CurrentURIMetaData` blob.
///
/// Never fails: this is metadata for a decoration, and a control point that sends
/// malformed XML should still get its media played. An unparseable blob yields an empty
/// [`Didl`], which the card renders as "no track information" — the same as a sender that
/// supplied nothing, which is the honest description of both.
#[must_use]
pub fn parse(blob: &str) -> Didl {
    use quick_xml::events::Event;

    let mut out = Didl::default();
    if blob.trim().is_empty() {
        return out;
    }
    let mut reader = quick_xml::Reader::from_str(blob);
    // A bare `&` in a title is malformed XML that control points nonetheless send, and
    // quick-xml 0.38 began failing the *read* on one rather than only the unescape. Since
    // a read error ends the scan, refusing it would drop every field after the offending
    // one — the opposite of what the "never fails" contract above promises.
    reader.config_mut().allow_dangling_amp = true;

    // The element whose text we are currently collecting, by local name.
    let mut field: Option<String> = None;
    // Its text so far: character data arrives in fragments split around entity
    // references (see `xmlref`), so the value is only complete at the closing tag.
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "res" {
                    // Duration is an attribute, so it is read here rather than from text.
                    if let Some(d) = attr(&e, "duration").and_then(|v| parse_duration(&v)) {
                        out.duration = Some(d);
                    }
                }
                field = Some(name);
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "res" {
                    if let Some(d) = attr(&e, "duration").and_then(|v| parse_duration(&v)) {
                        out.duration = Some(d);
                    }
                }
                field = None;
            }
            Ok(Event::Text(t)) if field.is_some() => {
                if let Ok(v) = t.xml10_content() {
                    text.push_str(&v);
                }
            }
            // An entity reference is its own event now. One this parser cannot resolve is
            // dropped rather than failing the field: `&nbsp;` in a title should cost the
            // space, not the title.
            Ok(Event::GeneralRef(r)) if field.is_some() => {
                if let Some(v) = crate::xmlref::resolve(&r) {
                    text.push_str(&v);
                }
            }
            // Same reason as the SOAP layer: a title wrapped in CDATA is legal, and the
            // old comment here claimed to tolerate it while the code dropped it.
            Ok(Event::CData(c)) if field.is_some() => {
                text.push_str(&String::from_utf8_lossy(c.as_ref()));
            }
            Ok(Event::End(_)) => {
                // The text is complete only here, so this is where the field is recorded.
                if let Some(name) = field.take() {
                    let value = text.trim();
                    if !value.is_empty() {
                        assign(&mut out, &name, value);
                    }
                }
                text.clear();
            }
            Ok(Event::Eof) | Err(_) => break,
            // Comments, CDATA, the declaration and the rest carry nothing this reads, but
            // must not end the scan: a control point that wraps a title in CDATA or leads
            // with an XML declaration is not malformed, and stopping there would lose the
            // whole item.
            Ok(_) => {}
        }
    }
    out
}

/// Record one element's text against the field it names.
fn assign(out: &mut Didl, name: &str, value: &str) {
    match name {
        "title" => out.title = Some(value.to_owned()),
        // `upnp:artist` is the specific one; `dc:creator` is what a control point falls
        // back to, so it must not overwrite a real artist.
        "artist" => out.artist = Some(value.to_owned()),
        "creator" if out.artist.is_none() => out.artist = Some(value.to_owned()),
        "album" => out.album = Some(value.to_owned()),
        "albumArtURI" => out.art_url = Some(value.to_owned()),
        "class" => out.kind = ItemKind::parse(value),
        _ => {}
    }
}

/// The part of `prefix:name` after the colon.
fn local_name(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    text.rsplit(':').next().unwrap_or(&text).to_owned()
}

fn attr(e: &quick_xml::events::BytesStart<'_>, want: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        (local_name(a.key.as_ref()) == want)
            .then(|| String::from_utf8_lossy(a.value.as_ref()).into_owned())
    })
}

/// Parse a DIDL `res@duration`.
///
/// The grammar is normative and machine-readable — `av:duration.cds1` in
/// `http://www.upnp.org/schemas/av/av.xsd`, which is the union of two patterns:
///
/// ```text
/// [-+]?[0-9]+(:[0-5][0-9]){2}(\.[0-9]+)?
/// [-+]?[0-9]+(:[0-5][0-9]){2}(\.[0-9]+/[0-9]+)?
/// ```
///
/// Three consequences that are easy to get wrong, and that this got wrong first time:
///
/// - **The sign is part of the spec**, not a broken server being tolerated. CDS:4 spells
///   it out in prose too. Rejecting `+0:03:45` was a conformance bug of ours.
/// - **Hours are unbounded** (`[0-9]+`) while minutes and seconds are exactly `[0-5][0-9]`.
/// - **`F0/F1` is a rational fraction**, not a frame count: `0:03:25.7/10` is 25.7 seconds.
///   The prose requires `F0 < F1`, which the pattern cannot express, so it is checked here.
///
/// Separate from `state.rs`'s AVTransport time parser even though the grammars overlap:
/// these are genuinely different productions, and a third will be needed for RFC 2326 NPT
/// when `TimeSeekRange` lands (where a bare-seconds form is legal).
///
/// Whole seconds are all the card needs, so a valid fraction is parsed for *validity* and
/// then dropped — but an invalid one makes the whole value `None`, because a scrubber
/// drawn against a misread duration is worse than one not drawn at all.
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    // `NOT_IMPLEMENTED` is the spec's own answer for a live stream with no end, and the
    // sign is legal — a negative duration is nonsense for playback, so it is read and
    // refused rather than silently made positive.
    let (negative, rest) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if negative {
        return None;
    }

    let mut parts = rest.split(':');
    let h: u64 = parts.next()?.parse().ok()?;
    let minutes = parts.next()?;
    let seconds = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    // Exactly two digits each, per the pattern — `0:3:45` is not conformant, and accepting
    // it would mean accepting `0:345:00` too.
    if minutes.len() != 2 || seconds.split('.').next()?.len() != 2 {
        return None;
    }
    let m: u64 = minutes.parse().ok()?;

    let (whole, fraction) = match seconds.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (seconds, None),
    };
    let sec: u64 = whole.parse().ok()?;
    if m > 59 || sec > 59 {
        return None;
    }
    if let Some(f) = fraction {
        match f.split_once('/') {
            // The rational form: both halves must be numbers, and the prose requires the
            // numerator to be the smaller of the two.
            Some((num, denom)) => {
                let (num, denom): (u64, u64) = (num.parse().ok()?, denom.parse().ok()?);
                if denom == 0 || num >= denom {
                    return None;
                }
            }
            None => {
                if f.is_empty() || !f.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
            }
        }
    }
    Some(Duration::from_secs(h * 3600 + m * 60 + sec))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What BubbleUPnP, foobar2000 and Windows "Cast to device" all send, modulo
    /// whitespace: namespaced elements inside a single `<item>`.
    const REAL: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
        xmlns:dc="http://purl.org/dc/elements/1.1/"
        xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
        <item id="1" parentID="0" restricted="1">
          <dc:title>Windowlicker</dc:title>
          <upnp:artist>Aphex Twin</upnp:artist>
          <upnp:album>Windowlicker</upnp:album>
          <upnp:class>object.item.audioItem.musicTrack</upnp:class>
          <upnp:albumArtURI>http://server/art.jpg</upnp:albumArtURI>
          <res protocolInfo="http-get:*:audio/mpeg:*" duration="0:06:06.000">http://server/a.mp3</res>
        </item></DIDL-Lite>"#;

    #[test]
    fn a_real_control_points_metadata_reaches_the_card() {
        let d = parse(REAL);
        assert_eq!(d.title.as_deref(), Some("Windowlicker"));
        assert_eq!(d.artist.as_deref(), Some("Aphex Twin"));
        assert_eq!(d.album.as_deref(), Some("Windowlicker"));
        assert_eq!(d.art_url.as_deref(), Some("http://server/art.jpg"));
        assert_eq!(d.duration, Some(Duration::from_secs(366)));
        assert_eq!(d.kind, ItemKind::Audio);
    }

    /// quick-xml 0.38 began delivering entity references as their own events, splitting a
    /// text run into fragments. Taking only the first one silently truncates every value
    /// at its first entity — "Simon & Garfunkel" would reach the card as "Simon", which
    /// looks like correct metadata rather than a parser bug.
    #[test]
    fn a_title_is_not_truncated_at_its_first_entity() {
        let d = parse(
            r#"<DIDL-Lite><item><dc:title>Tom &amp; Jerry &lt;1940&gt;</dc:title>
               <upnp:artist>AC&#47;DC</upnp:artist>
               <upnp:albumArtURI>http://s/art?w=1&amp;h=2</upnp:albumArtURI>
               </item></DIDL-Lite>"#,
        );
        assert_eq!(d.title.as_deref(), Some("Tom & Jerry <1940>"));
        assert_eq!(d.artist.as_deref(), Some("AC/DC"));
        assert_eq!(d.art_url.as_deref(), Some("http://s/art?w=1&h=2"));
    }

    /// The contract above says parsing never fails. quick-xml 0.38 made a bare `&` fail
    /// the *read*, and a read error ends the scan — so one sloppy title would have cost
    /// every field after it, not just its own.
    #[test]
    fn a_bare_ampersand_does_not_discard_the_rest_of_the_item() {
        let d = parse(
            r#"<DIDL-Lite><item><dc:title>AT&T Bare</dc:title>
               <upnp:album>After</upnp:album></item></DIDL-Lite>"#,
        );
        assert_eq!(d.album.as_deref(), Some("After"));
    }

    /// An undeclared entity has no defined expansion. Dropping it keeps the rest of the
    /// title, which is the forgiving half of the contract; failing the field would trade
    /// a whole title for one character.
    #[test]
    fn an_unresolvable_entity_costs_only_itself() {
        let d = parse(r#"<item><dc:title>Hard&nbsp;Rock</dc:title></item>"#);
        assert_eq!(d.title.as_deref(), Some("HardRock"));
    }

    /// Prefixes are matched on local name because control points disagree about them —
    /// and being strict costs a blank card for an item that told us its title.
    #[test]
    fn elements_are_matched_without_their_prefixes() {
        let d = parse(
            r#"<DIDL-Lite><item><title>Bare</title><artist>Nobody</artist>
               <class>object.item.videoItem</class></item></DIDL-Lite>"#,
        );
        assert_eq!(d.title.as_deref(), Some("Bare"));
        assert_eq!(d.artist.as_deref(), Some("Nobody"));
        assert_eq!(d.kind, ItemKind::Video);
    }

    /// `dc:creator` is the fallback, not a competitor: a blob with both must keep the
    /// artist, whichever order they arrive in.
    #[test]
    fn creator_never_displaces_a_real_artist() {
        let with_both = r#"<item><dc:creator>Some Uploader</dc:creator>
            <upnp:artist>Aphex Twin</upnp:artist></item>"#;
        assert_eq!(parse(with_both).artist.as_deref(), Some("Aphex Twin"));

        let reversed = r#"<item><upnp:artist>Aphex Twin</upnp:artist>
            <dc:creator>Some Uploader</dc:creator></item>"#;
        assert_eq!(parse(reversed).artist.as_deref(), Some("Aphex Twin"));

        let only_creator = r"<item><dc:creator>Some Uploader</dc:creator></item>";
        assert_eq!(parse(only_creator).artist.as_deref(), Some("Some Uploader"));
    }

    /// Malformed metadata must not stop the media playing: this is a decoration, and the
    /// control point has already told us the URL it wants.
    /// A control point that wraps the title in CDATA is not malformed — it is doing the
    /// natural thing for embedding XML in XML. This used to be dropped on the floor.
    #[test]
    fn cdata_content_is_read_not_discarded() {
        let d = parse(r"<item><dc:title><![CDATA[Rock & Roll]]></dc:title></item>");
        assert_eq!(d.title.as_deref(), Some("Rock & Roll"));
    }

    #[test]
    fn broken_metadata_is_empty_rather_than_fatal() {
        for blob in ["", "   ", "not xml at all", "<item><title>unclosed"] {
            let d = parse(blob);
            assert!(
                d.title.is_none() || d.title.as_deref() == Some("unclosed"),
                "{blob:?}"
            );
        }
    }

    #[test]
    fn durations_come_off_the_res_element_in_every_form_the_spec_allows() {
        for (raw, want) in [
            ("0:06:06.000", 366),
            ("0:03:45", 225),
            ("1:00:00", 3600),
            ("0:00:30.5", 30),
            // The spec's fractional form, which some servers really do emit.
            // The rational form: 2.15/25 of a second past 0:01:02, not 15 frames.
            ("0:01:02.15/25", 62),
            // The sign is part of the normative grammar, not a broken server.
            ("+0:03:45", 225),
            // Hours are unbounded.
            ("100:00:00", 360_000),
        ] {
            let blob = format!(r#"<item><res duration="{raw}">u</res></item>"#);
            assert_eq!(
                parse(&blob).duration,
                Some(Duration::from_secs(want)),
                "{raw}"
            );
        }
    }

    /// A live stream's `res` carries no duration, or a nonsense one. Either way the card
    /// must not draw a scrubber against it.
    #[test]
    fn a_stream_with_no_duration_reports_none() {
        assert_eq!(parse(r#"<item><res>u</res></item>"#).duration, None);
        assert_eq!(
            parse(r#"<item><res duration="NOT_IMPLEMENTED">u</res></item>"#).duration,
            None
        );
        assert_eq!(
            parse(r#"<item><res duration="0:99:00">u</res></item>"#).duration,
            None
        );
    }

    #[test]
    fn applying_to_a_snapshot_only_overwrites_what_was_supplied() {
        let existing = NowPlaying::default()
            .with_title("From the container")
            .with_artist("Container Artist");
        let didl = Didl {
            title: Some("From DIDL".into()),
            ..Didl::default()
        };
        let merged = didl.apply_to(existing);
        assert_eq!(merged.title.as_deref(), Some("From DIDL"));
        assert_eq!(
            merged.artist.as_deref(),
            Some("Container Artist"),
            "a title-only blob must not erase an artist we already had"
        );
    }
}
