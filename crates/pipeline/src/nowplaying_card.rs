//! The now-playing surface: what the panel shows while something is casting audio.
//!
//! Sits between the attract scene and the video layer. An audio session has no pixels of
//! its own — a Bluetooth phone sends sound and nothing else — so without this the screen
//! keeps showing the idle card while music plays, which reads as "it didn't work".
//! A video cast covers this in turn, because then the sender *does* have pixels and they
//! are the point.
//!
//! Rendered on the kiosk thread from the metadata itself rather than handed over as
//! pixels. A 4K RGBA buffer is 33 MB and the metadata is a few hundred bytes; pushing the
//! former through the render channel on every track change would be absurd when the
//! latter reproduces it exactly.

use castaway_core::{NowPlaying, PlaybackState, QueueItem, SourceDescription};
use tracing::debug;

use crate::error::PipelineError;
use crate::text::{self, Rgba};

/// Everything the card draws, as the pipeline knows it.
///
/// Both halves together, because they arrive separately and each is meaningless alone: a
/// track with no device says nothing about *where* the sound is coming from, and a device
/// with no track is a connection nobody can see the effect of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NowPlayingCard {
    /// What is playing.
    pub track: NowPlaying,
    /// Who is connected, and over what.
    pub source: SourceDescription,
    /// What is queued behind it, nearest first. Empty when the queue is empty *or* when
    /// the source cannot see one — the card cannot tell those apart and does not try, it
    /// simply shows nothing.
    pub up_next: Vec<QueueItem>,
}

/// One laid-out line of the card.
///
/// Owns its text: some lines are composed rather than borrowed, and a card is laid out a
/// handful of times per track, so the allocations are beneath notice.
struct Line {
    text: String,
    px: f32,
    color: Rgba,
    bold: bool,
    /// Space below this line before the next.
    gap: f32,
}

/// The design is laid out against this height and scaled; see [`render`].
const DESIGN_HEIGHT: f32 = 720.0;

struct Palette {
    bg_top: Rgba,
    bg_bottom: Rgba,
    title: Rgba,
    artist: Rgba,
    album: Rgba,
    source: Rgba,
    state: Rgba,
    art_edge: Rgba,
    art_bg: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            // Deliberately the attract scene's background, so moving between idle and
            // playing does not flash a different colour across the whole panel.
            bg_top: [0x0d, 0x14, 0x28, 0xff],
            bg_bottom: [0x03, 0x05, 0x0b, 0xff],
            title: [0xff, 0xff, 0xff, 0xff],
            artist: [0x4f, 0xd1, 0xc5, 0xff],
            album: [0x9a, 0xa4, 0xb8, 0xff],
            source: [0x9a, 0xa4, 0xb8, 0xff],
            state: [0xe8, 0xec, 0xf4, 0xff],
            art_edge: [0x1b, 0x27, 0x44, 0xff],
            art_bg: [0x11, 0x1a, 0x30, 0xff],
        }
    }
}

/// Shrink `px` until `text` fits `avail`, so a long title lays out instead of overrunning.
fn fit_px(font: &ab_glyph::FontRef<'static>, s: &str, px: f32, avail: f32) -> f32 {
    let mut size = px;
    while size > 8.0 && text::measure(font, s, size) > avail {
        size *= 0.94;
    }
    size
}

/// What to print for the transport state.
///
/// `Stopped` prints nothing: a card that says "Stopped" under a track title is a
/// contradiction the viewer has to resolve, and the common cause is a sender that never
/// reported a state at all.
const fn state_label(state: PlaybackState) -> Option<&'static str> {
    match state {
        PlaybackState::Playing => Some("Playing"),
        PlaybackState::Paused => Some("Paused"),
        PlaybackState::SeekingForward => Some("Seeking \u{25b6}"),
        PlaybackState::SeekingBackward => Some("\u{25c0} Seeking"),
        PlaybackState::Error => Some("Playback error"),
        PlaybackState::Stopped => None,
        // `PlaybackState` is non-exhaustive: a state we do not know how to name is better
        // left blank than guessed at on a two-metre screen.
        _ => None,
    }
}

/// Decide what the card says and how, in reading order.
///
/// Separate from drawing so it can be tested for *content*: pixels cannot be asserted on
/// usefully, and content vanishing is exactly what a layout refactor breaks — a card that
/// draws nothing wrong is not the same as one that draws the right thing.
fn build_lines(
    card: &NowPlayingCard,
    f: &text::Fonts,
    pal: &Palette,
    s: f32,
    avail: f32,
) -> Vec<Line> {
    // What the card leads with depends on what it knows. With a track, the device is a
    // caption above it. Without one — a source that publishes no AVRCP metadata at all,
    // which is most non-phone senders — a small caption captioning nothing floats alone
    // in the middle of a two-metre screen and reads as broken rather than as sparse. So
    // the device becomes the headline and says what it is waiting for.
    let has_track = card.track.title.is_some();
    let device = card
        .source
        .display_name
        .clone()
        .or_else(|| card.source.address.clone());
    let state = state_label(card.track.state);
    let mut lines: Vec<Line> = Vec::with_capacity(5);

    let source_line = card.source.to_string();
    let waiting = "Connected \u{2014} no track information";

    if has_track {
        if !source_line.is_empty() {
            lines.push(Line {
                text: source_line.clone(),
                px: fit_px(&f.regular, &source_line, 26.0 * s, avail),
                color: pal.source,
                bold: false,
                gap: 46.0 * s,
            });
        }
    } else if let Some(device) = &device {
        // The device, typeset as the headline it now is.
        lines.push(Line {
            text: device.clone(),
            px: fit_px(&f.bold, device, 72.0 * s, avail),
            color: pal.title,
            bold: true,
            gap: 20.0 * s,
        });
        if let Some(link) = &card.source.link {
            lines.push(Line {
                text: link.clone(),
                px: fit_px(&f.regular, link, 36.0 * s, avail),
                color: pal.artist,
                bold: false,
                gap: 34.0 * s,
            });
        }
        lines.push(Line {
            text: waiting.to_string(),
            px: 28.0 * s,
            color: pal.album,
            bold: false,
            gap: 0.0,
        });
    }

    if let Some(title) = &card.track.title {
        lines.push(Line {
            text: title.clone(),
            px: fit_px(&f.bold, title, 72.0 * s, avail),
            color: pal.title,
            bold: true,
            gap: 20.0 * s,
        });
    }
    if let Some(artist) = &card.track.artist {
        lines.push(Line {
            text: artist.clone(),
            px: fit_px(&f.regular, artist, 44.0 * s, avail),
            color: pal.artist,
            bold: false,
            gap: 14.0 * s,
        });
    }
    if let Some(album) = &card.track.album {
        lines.push(Line {
            text: album.clone(),
            px: fit_px(&f.regular, album, 32.0 * s, avail),
            color: pal.album,
            bold: false,
            gap: 34.0 * s,
        });
    }
    if let Some(state) = state {
        lines.push(Line {
            text: state.to_string(),
            px: 28.0 * s,
            // Zero when this is the last line, a real gap when the queue follows. Each
            // line owns the space *below* it, so a trailing gap would push the centred
            // block visibly off-axis — but leaving it zero when something follows
            // overlaps the two, which is what this originally did.
            gap: if card.up_next.is_empty() {
                0.0
            } else {
                40.0 * s
            },
            color: pal.state,
            bold: false,
        });
    }

    // "Up next", below the transport state. On a shared screen this is the question
    // people actually ask — whose song is on after this one — so it earns space even
    // though it is nobody's idea of essential playback metadata.
    //
    // Capped rather than scrolled. The block is vertically centred against the art square,
    // so an unbounded list would push the title off-centre and eventually off the panel;
    // three is what fits beside the art at 720p without crowding it.
    const MAX_UP_NEXT: usize = 3;
    if !card.up_next.is_empty() {
        lines.push(Line {
            text: "Up next".to_string(),
            px: 22.0 * s,
            color: pal.source,
            bold: true,
            gap: 12.0 * s,
        });
        for item in card.up_next.iter().take(MAX_UP_NEXT) {
            let text = item.to_string();
            lines.push(Line {
                text: text.clone(),
                px: fit_px(&f.regular, &text, 26.0 * s, avail),
                color: pal.album,
                bold: false,
                gap: 8.0 * s,
            });
        }
        // Say how many were not shown rather than truncating in silence — "and 12 more"
        // is the difference between a short queue and a hidden one.
        let hidden = card.up_next.len().saturating_sub(MAX_UP_NEXT);
        if hidden > 0 {
            lines.push(Line {
                text: format!("and {hidden} more"),
                px: 22.0 * s,
                color: pal.source,
                bold: false,
                gap: 0.0,
            });
        }
    }

    lines
}

/// A decoded cover, square-cropped and ready to scale into the art panel.
struct Cover {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Decode cover bytes into RGBA.
///
/// The declared [`ImageFormat`] is a hint, not a promise — it comes from a MIME type a
/// peer supplied — so the bytes are sniffed instead. A JPEG labelled PNG should still
/// appear on the wall.
fn decode_cover(artwork: &castaway_core::Artwork) -> Result<Cover, PipelineError> {
    let decoded = image::load_from_memory(&artwork.data)
        .map_err(|e| PipelineError::Decode(format!("cover art: {e}")))?
        .to_rgba8();
    Ok(Cover {
        width: decoded.width(),
        height: decoded.height(),
        rgba: decoded.into_raw(),
    })
}

/// Draw `cover` into the square at `(x, y)` of side `side`, centre-cropped.
///
/// Nearest-neighbour on purpose: the source is typically 300–640px scaling into a panel
/// of a similar order, the result is looked at from across a room, and a good resampler
/// is a dependency and a millisecond this does not need. Centre-crop rather than stretch
/// because a non-square cover stretched to a square is instantly, distractingly wrong.
fn draw_cover(buf: &mut [u8], width: u32, height: u32, cover: &Cover, x: f32, y: f32, side: f32) {
    if cover.width == 0 || cover.height == 0 || side <= 0.0 {
        return;
    }
    let crop = cover.width.min(cover.height);
    let crop_x = (cover.width - crop) / 2;
    let crop_y = (cover.height - crop) / 2;

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x0, y0, span) = (x.round() as i64, y.round() as i64, side.round() as i64);
    for dy in 0..span {
        let py = y0 + dy;
        if py < 0 || py >= i64::from(height) {
            continue;
        }
        for dx in 0..span {
            let px = x0 + dx;
            if px < 0 || px >= i64::from(width) {
                continue;
            }
            // Map destination pixel back into the cropped source square.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let sx = crop_x + ((dx as u64 * u64::from(crop)) / span.max(1) as u64) as u32;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let sy = crop_y + ((dy as u64 * u64::from(crop)) / span.max(1) as u64) as u32;
            let si = ((sy.min(cover.height - 1) as usize) * cover.width as usize
                + sx.min(cover.width - 1) as usize)
                * 4;
            // `try_from` rather than `as`: both are already proven in range by the guards
            // above, and saying so with a fallible conversion keeps that proof local
            // instead of resting on a lint allowance.
            let (Ok(py), Ok(px)) = (usize::try_from(py), usize::try_from(px)) else {
                continue;
            };
            let di = (py * width as usize + px) * 4;
            if let (Some(src), Some(dst)) = (cover.rgba.get(si..si + 4), buf.get_mut(di..di + 4)) {
                dst.copy_from_slice(src);
            }
        }
    }
}

/// Draw the card at `width` × `height`, returning RGBA8.
///
/// # Errors
/// [`PipelineError`] if the bundled fonts cannot be parsed.
pub fn render(card: &NowPlayingCard, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let f = text::fonts()?;
    let pal = Palette::default();

    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
    text::fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom);

    let w = width as f32;
    let h = height as f32;
    // Scale relative to a 720p design so the layout holds at any panel resolution.
    let s = h / DESIGN_HEIGHT;
    let margin = 90.0 * s;

    // The artwork square, on the left. Drawn as a framed panel even when empty: the card
    // should not reflow when art arrives a second after the text, because a layout that
    // jumps is worse than one with a gap in it.
    let art = (h - margin * 2.0).min(w * 0.34);
    let art_x = margin;
    let art_y = (h - art) / 2.0;
    let edge = (3.0 * s).max(1.0);
    text::fill_rect(
        &mut buf,
        width,
        height,
        art_x - edge,
        art_y - edge,
        art + edge * 2.0,
        art + edge * 2.0,
        pal.art_edge,
    );
    text::fill_rect(&mut buf, width, height, art_x, art_y, art, art, pal.art_bg);

    // Paint the cover over the panel, if we have one that decodes. A cover that fails to
    // decode leaves the empty panel rather than taking the whole card down with it —
    // artwork is the least important thing here and the most likely to be malformed,
    // since it is whatever bytes a phone or a CDN chose to send.
    if let Some(artwork) = &card.track.artwork {
        match decode_cover(artwork) {
            Ok(cover) => draw_cover(&mut buf, width, height, &cover, art_x, art_y, art),
            Err(e) => debug!(error = %e, "now-playing card: cover art did not decode"),
        }
    }

    // The text column, right of the art. Laid out as a block and centred against the
    // art square rather than flowed from the top: a card with no album, or no artist,
    // otherwise leaves a hole where that line would have been and the whole thing drifts
    // upward as metadata arrives piecemeal.
    let text_x = art_x + art + 70.0 * s;
    let avail = (w - text_x - margin).max(1.0);

    let lines = build_lines(card, &f, &pal, s, avail);

    // Total height, so the block can be centred on the art square's midline.
    let total: f32 = lines
        .iter()
        .map(|l| {
            let font = if l.bold { &f.bold } else { &f.regular };
            text::ascent(font, l.px) + l.gap
        })
        .sum();
    let mut y = art_y + (art - total) / 2.0;

    for line in &lines {
        let font = if line.bold { &f.bold } else { &f.regular };
        y += text::ascent(font, line.px);
        text::draw_text(
            &mut buf, width, height, text_x, y, &line.text, line.px, line.color, font,
        );
        y += line.gap;
    }

    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn card() -> NowPlayingCard {
        NowPlayingCard {
            track: NowPlaying::default()
                .with_title("Derezzed")
                .with_artist("Daft Punk"),
            source: SourceDescription::new().with_display_name("iPhone"),
            up_next: Vec::new(),
        }
    }

    /// The text of every laid-out line, for asserting on content rather than pixels.
    fn line_texts(card: &NowPlayingCard) -> Vec<String> {
        let f = text::fonts().unwrap();
        build_lines(card, &f, &Palette::default(), 1.0, 800.0)
            .into_iter()
            .map(|l| l.text)
            .collect()
    }

    #[test]
    fn the_queue_is_listed_nearest_first_under_a_heading() {
        let mut c = card();
        c.track.state = PlaybackState::Playing;
        c.up_next = vec![
            QueueItem::new("Aerodynamic").with_artist("Daft Punk"),
            QueueItem::new("Veridis Quo"),
        ];
        let texts = line_texts(&c);
        assert!(texts.contains(&"Up next".to_string()), "{texts:?}");
        let first = texts.iter().position(|t| t.starts_with("Aerodynamic"));
        let second = texts.iter().position(|t| t.starts_with("Veridis Quo"));
        assert!(first < second, "queue order lost: {texts:?}");
        // An item with no artist must not render a dangling separator.
        assert!(texts.contains(&"Veridis Quo".to_string()), "{texts:?}");
    }

    #[test]
    fn a_long_queue_says_how_much_it_is_hiding() {
        // Silent truncation would read as a three-song queue, which is a different and
        // much more annoying claim than "there are more".
        let mut c = card();
        c.up_next = (0..7)
            .map(|i| QueueItem::new(format!("Track {i}")))
            .collect();
        let texts = line_texts(&c);
        assert!(texts.contains(&"and 4 more".to_string()), "{texts:?}");
    }

    #[test]
    fn an_empty_queue_adds_no_heading() {
        // A bare "Up next" over nothing looks like the queue failed to load.
        let texts = line_texts(&card());
        assert!(!texts.contains(&"Up next".to_string()), "{texts:?}");
    }

    #[test]
    fn the_state_line_makes_room_only_when_the_queue_follows_it() {
        // Regression: `state` owned a zero gap because it used to be last, so the queue
        // heading was drawn on top of it.
        let f = text::fonts().unwrap();
        let mut c = card();
        c.track.state = PlaybackState::Playing;

        let gap_of = |card: &NowPlayingCard| {
            build_lines(card, &f, &Palette::default(), 1.0, 800.0)
                .into_iter()
                .find(|l| l.text == "Playing")
                .map(|l| l.gap)
                .unwrap()
        };
        assert_eq!(gap_of(&c), 0.0, "a trailing line must not pad the block");

        c.up_next = vec![QueueItem::new("Something")];
        assert!(gap_of(&c) > 0.0, "the queue would overlap the state line");
    }

    #[test]
    fn a_cover_that_does_not_decode_leaves_the_panel_empty_rather_than_failing() {
        // Cover art is the least important thing on the card and the most likely to be
        // malformed — it is whatever bytes a phone or a CDN chose to send.
        let mut c = card();
        c.track = c.track.clone().with_artwork(castaway_core::Artwork::new(
            castaway_core::ImageFormat::Jpeg,
            bytes::Bytes::from_static(b"not an image"),
        ));
        assert_eq!(render(&c, 640, 360).unwrap().len(), 640 * 360 * 4);
    }

    #[test]
    fn the_card_is_the_size_it_was_asked_for() {
        let rgba = render(&card(), 640, 360).unwrap();
        assert_eq!(rgba.len(), 640 * 360 * 4);
    }

    #[test]
    fn an_empty_card_still_renders() {
        // A session can open before any metadata arrives — a phone that streams and
        // reports nothing — and the screen must show *something* rather than panic or
        // leave the idle scene up.
        let rgba = render(&NowPlayingCard::default(), 320, 180).unwrap();
        assert_eq!(rgba.len(), 320 * 180 * 4);
        assert!(
            rgba.iter().any(|b| *b != 0),
            "the background should be drawn"
        );
    }

    #[test]
    fn a_very_long_title_does_not_escape_the_surface() {
        // `draw_text` clips, so the real risk is the fitter looping forever or the buffer
        // being written past; both would show up here.
        let mut c = card();
        c.track = c.track.with_title("x".repeat(400));
        let rgba = render(&c, 640, 360).unwrap();
        assert_eq!(rgba.len(), 640 * 360 * 4);
    }

    /// The card's content, in reading order — straight out of the real builder.
    fn lines_of(card: &NowPlayingCard) -> Vec<String> {
        let f = text::fonts().unwrap();
        build_lines(card, &f, &Palette::default(), 1.0, 1000.0)
            .into_iter()
            .map(|l| l.text)
            .collect()
    }

    #[test]
    fn a_card_with_a_track_shows_the_track() {
        // Regression: a refactor to improve the no-metadata case silently deleted the
        // title, artist and album, and `render` still returned a correctly-sized buffer.
        // A card that draws nothing *wrong* is not the same as one that draws the right
        // thing, which is why this asserts content and not pixels.
        let mut c = card();
        c.track = c.track.clone().with_album("TRON");
        let lines = lines_of(&c);
        assert!(lines.iter().any(|l| l == "Derezzed"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "Daft Punk"), "{lines:?}");
        assert!(lines.iter().any(|l| l == "TRON"), "{lines:?}");
        assert!(
            lines[0].contains("iPhone"),
            "the device captions the track: {lines:?}"
        );
    }

    #[test]
    fn a_card_with_no_track_leads_with_the_device() {
        // What every non-phone sender produces — BlueZ as a source publishes no AVRCP
        // metadata at all. A small caption captioning nothing, floating mid-screen, reads
        // as broken rather than sparse, so the device becomes the headline and says why.
        let bare = NowPlayingCard {
            track: NowPlaying::default(),
            source: SourceDescription::new()
                .with_display_name("bagel")
                .with_link("aptX HD · 48 kHz"),
            up_next: Vec::new(),
        };
        let lines = lines_of(&bare);
        assert_eq!(lines[0], "bagel", "the device leads: {lines:?}");
        assert!(lines.iter().any(|l| l.contains("aptX HD")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("no track information")),
            "and says why it is sparse: {lines:?}"
        );
        assert_eq!(render(&bare, 320, 180).unwrap().len(), 320 * 180 * 4);
    }

    #[test]
    fn stopped_prints_no_state_at_all() {
        // A sender that never reports a state leaves it at the default, and "Stopped"
        // under a playing track is a contradiction the viewer has to resolve.
        assert_eq!(state_label(PlaybackState::Stopped), None);
        assert_eq!(state_label(PlaybackState::Playing), Some("Playing"));
    }
}
