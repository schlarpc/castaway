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

use castaway_core::{ControlCapabilities, NowPlaying, PlaybackState, QueueItem, SourceDescription};
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
    /// What the active session will honour, which decides whether a transport strip is
    /// drawn along the bottom and therefore how much room the card has.
    pub controls: ControlCapabilities,
}

impl NowPlayingCard {
    /// The transport strip this card implies, if any.
    #[must_use]
    pub fn transport(&self) -> crate::transport::TransportModel {
        crate::transport::TransportModel::from_now_playing(&self.track, self.controls)
    }

    /// Whether a transport strip will be drawn under this card.
    #[must_use]
    pub fn has_transport(&self) -> bool {
        !self.transport().is_empty()
    }

    /// The state word this card actually draws, if any.
    ///
    /// `None` whenever a strip is up: the strip's play/pause glyph already says exactly
    /// this over the same card, and the word underneath read as clutter ("playing" under
    /// a pause button). See [`build_lines`].
    fn drawn_state(&self) -> Option<&'static str> {
        if self.has_transport() {
            None
        } else {
            state_label(self.track.state)
        }
    }

    /// Whether this card rasterizes to the same pixels as `other`.
    ///
    /// Every field that does not reach the pixels is excluded, and the ones that reach
    /// them only *sometimes* are compared as what is drawn rather than as what they are.
    /// The card is a full-surface raster — tens of megabytes at 4K — so a field compared
    /// here that the card never draws costs a re-raster of identical pixels, which on the
    /// glass is a visible flash (#83).
    ///
    /// - **Position** the card never draws; that is the strip's job. This is what lets a
    ///   source republish position once a second for free.
    /// - **Shuffle and repeat** likewise: they are strip glyphs and nothing else.
    /// - **State** the card draws only when there is no strip, so it is compared through
    ///   [`Self::drawn_state`]. Comparing it directly is what made *pausing* flash the
    ///   whole screen — the commonest thing anyone does to a card, on the surface most
    ///   likely to be up.
    ///
    /// `has_transport` stays in the comparison, and has to: it decides the space the card
    /// lays out against, so a card that gains or loses its strip really is a different
    /// picture.
    #[must_use]
    pub fn visual_eq(&self, other: &Self) -> bool {
        let key = |c: &Self| {
            let mut flat = c.clone();
            flat.track.position = None;
            flat.track.state = castaway_core::PlaybackState::Stopped;
            flat.track.shuffle = None;
            flat.track.repeat = None;
            (flat, c.drawn_state(), c.has_transport())
        };
        key(self) == key(other)
    }
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

/// The card's background colours over a vertical slice of the panel, `0.0..=1.0`.
///
/// Exists so a layer drawn *over* part of the card — the transport strip — can continue
/// the same gradient instead of painting a band across it. The card owns the palette, so
/// it owns this answer.
#[must_use]
pub fn background_span(from: f32, to: f32) -> (crate::text::Rgba, crate::text::Rgba) {
    let pal = Palette::default();
    let at = |t: f32| -> crate::text::Rgba {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                (f32::from(a) * (1.0 - t) + f32::from(b) * t).round() as u8
            }
        };
        [
            mix(pal.bg_top[0], pal.bg_bottom[0]),
            mix(pal.bg_top[1], pal.bg_bottom[1]),
            mix(pal.bg_top[2], pal.bg_bottom[2]),
            0xff,
        ]
    };
    (at(from), at(to))
}

/// Shrink `px` until `text` fits `avail`, so a long title lays out instead of overrunning.
fn fit_px(font: &text::Face, s: &str, px: f32, avail: f32) -> f32 {
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
    // The state line only earns its place when there is no transport strip: the strip's
    // play/pause glyph already says exactly this, drawn over the same card, and the
    // word underneath it read as clutter ("playing" under a pause button). A card with
    // no strip — a source that advertises no controls — keeps the word, because then
    // nothing else on the screen says whether sound should be coming out.
    let state = if card.has_transport() {
        None
    } else {
        state_label(card.track.state)
    };
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
            // Same rule as the state line: a line owns the space below it, so this is
            // zero when nothing follows and a real gap when the queue does. Getting this
            // wrong drew "Up next" on top of the line above it.
            gap: if card.up_next.is_empty() {
                0.0
            } else {
                40.0 * s
            },
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
        // No "and N more" footer. It said how many were hidden, which nobody standing
        // at the panel can do anything with — the phone that owns the queue already
        // shows the whole thing. The rows shown are the queue as far as this card is
        // concerned.
    }

    lines
}

/// A decoded cover, already centre-cropped square and resampled to the art panel's
/// exact size.
///
/// The size is on the type on purpose: a cover that exists is a cover that fits, so the
/// blit has no scaling decision left to make — or get wrong. All resampling policy lives
/// in [`decode_cover`], the boundary where a peer's bytes become our pixels.
struct Cover {
    side: u32,
    rgba: Vec<u8>,
}

/// Decode cover bytes into a card-ready `side` × `side` RGBA square.
///
/// The declared [`ImageFormat`] is a hint, not a promise — it comes from a MIME type a
/// peer supplied — so the bytes are sniffed instead. A JPEG labelled PNG should still
/// appear on the wall.
///
/// Centre-crop rather than stretch, because a non-square cover stretched square is
/// instantly, distractingly wrong. The resample filter is chosen by direction: art
/// arrives at whatever size the source felt like — Bluetooth's linked thumbnail is a
/// fixed 200×200 (#75), a CDN might send 640 — while the panel square can be several
/// times that. Upscales get Catmull-Rom, which stays sharp at those factors without
/// Lanczos's ringing on the hard edges cover art is full of; downscales get Lanczos3,
/// where the sharpness is the virtue and minification swallows the ringing.
fn decode_cover(artwork: &castaway_core::Artwork, side: u32) -> Result<Cover, PipelineError> {
    let decoded = image::load_from_memory(&artwork.data)
        .map_err(|e| PipelineError::Decode(format!("cover art: {e}")))?
        .to_rgba8();
    let (w, h) = decoded.dimensions();
    let crop = w.min(h);
    if crop == 0 || side == 0 {
        return Err(PipelineError::Decode(
            "cover art: nothing to draw (empty image or zero-size panel)".into(),
        ));
    }
    let square = image::imageops::crop_imm(&decoded, (w - crop) / 2, (h - crop) / 2, crop, crop);
    let rgba = if side == crop {
        square.to_image().into_raw()
    } else {
        let filter = if side > crop {
            image::imageops::FilterType::CatmullRom
        } else {
            image::imageops::FilterType::Lanczos3
        };
        image::imageops::resize(&square.to_image(), side, side, filter).into_raw()
    };
    Ok(Cover { side, rgba })
}

/// Copy `cover` into place at `(x, y)`, clipped to the surface.
///
/// A row copy and nothing more — see [`Cover`] for why there is no scaling here.
fn draw_cover(buf: &mut [u8], width: u32, height: u32, cover: &Cover, x: f32, y: f32) {
    let side = i64::from(cover.side);
    #[allow(clippy::cast_possible_truncation)]
    let (x0, y0) = (x.round() as i64, y.round() as i64);
    // The horizontal overlap with the surface, as cover-local columns. Vertical clipping
    // is per row below; horizontal is the same for every row, so it is decided once.
    let col0 = (-x0).clamp(0, side);
    let col1 = (i64::from(width) - x0).clamp(0, side);
    // `try_from` rather than `as`: all are already proven in range by the clamps above
    // (and `x0 + col0 >= 0` because `col0 >= -x0`), and saying so with a fallible
    // conversion keeps that proof local instead of resting on a lint allowance.
    let (Ok(col0), Ok(col1), Ok(px)) = (
        usize::try_from(col0),
        usize::try_from(col1),
        usize::try_from(x0 + col0),
    ) else {
        return;
    };
    if col0 >= col1 {
        return;
    }
    let len = (col1 - col0) * 4;
    for dy in 0..side {
        let py = y0 + dy;
        if py < 0 || py >= i64::from(height) {
            continue;
        }
        let (Ok(py), Ok(dy)) = (usize::try_from(py), usize::try_from(dy)) else {
            continue;
        };
        let si = (dy * cover.side as usize + col0) * 4;
        let di = (py * width as usize + px) * 4;
        if let (Some(src), Some(dst)) = (cover.rgba.get(si..si + len), buf.get_mut(di..di + len)) {
            dst.copy_from_slice(src);
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
    // The gradient still spans the *whole* surface, strip included: the strip paints its
    // own slice of the same ramp, so the two layers meet without a seam.
    text::fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom);

    let w = width as f32;
    // The card lays out against the space the transport strip is *not* using. Without
    // this the vertically-centred block runs underneath the controls, which reads as a
    // layout bug rather than as an overlay — and the two are drawn on separate layers, so
    // nothing else would ever notice the collision.
    let reserved = if card.has_transport() {
        height as f32 * crate::transport::STRIP_HEIGHT_FRACTION
    } else {
        0.0
    };
    let h = height as f32 - reserved;
    // Scale relative to a 720p design so the layout holds at any panel resolution.
    let s = h / DESIGN_HEIGHT;
    let margin = 90.0 * s;

    // The artwork square, on the left. Drawn as a framed panel even when *empty*, so the
    // card does not reflow when art arrives a second after the text — a layout that jumps
    // is worse than one with a gap in it.
    //
    // But only when there is a track for art to belong to. With no track, no art is
    // coming, and a large empty square beside "connected, nothing playing" is not a
    // placeholder, it is just a hole in the middle of a two-metre screen.
    let expects_art = card.track.title.is_some() || card.track.artwork.is_some();
    let art = if expects_art {
        (h - margin * 2.0).min(w * 0.34)
    } else {
        0.0
    };
    let art_x = margin;
    let art_y = (h - art) / 2.0;
    if expects_art {
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
    }

    // Paint the cover over the panel, if we have one that decodes. A cover that fails to
    // decode leaves the empty panel rather than taking the whole card down with it —
    // artwork is the least important thing here and the most likely to be malformed,
    // since it is whatever bytes a phone or a CDN chose to send.
    if let Some(artwork) = &card.track.artwork {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        match decode_cover(artwork, art.max(0.0).round() as u32) {
            Ok(cover) => draw_cover(&mut buf, width, height, &cover, art_x, art_y),
            Err(e) => debug!(error = %e, "now-playing card: cover art did not decode"),
        }
    }

    // The text column, right of the art. Laid out as a block and centred against the
    // art square rather than flowed from the top: a card with no album, or no artist,
    // otherwise leaves a hole where that line would have been and the whole thing drifts
    // upward as metadata arrives piecemeal.
    let text_x = if expects_art {
        art_x + art + 70.0 * s
    } else {
        // No art panel, so the text starts at the margin and gets the whole width.
        margin
    };
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
            controls: ControlCapabilities::NONE,
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

    /// The same card, with controls, so a transport strip is drawn under it.
    fn card_with_strip() -> NowPlayingCard {
        let mut c = card();
        c.controls = ControlCapabilities::PLAY | ControlCapabilities::PAUSE;
        c.track.state = PlaybackState::Playing;
        c.track.duration = Some(std::time::Duration::from_secs(200));
        c.track.position = Some(std::time::Duration::ZERO);
        c
    }

    #[test]
    fn pausing_a_card_that_has_a_strip_is_not_a_new_picture() {
        // #83, and the commonest thing anyone does to a card. The card draws no state word
        // while a strip is up — the strip's glyph says it instead — so pause and play
        // rasterize identically. `visual_eq` compared `state` anyway, so every pause threw
        // away a full-surface texture and uploaded the same pixels again: tens of
        // megabytes at 4K, which on the glass is a flash of the whole screen.
        let playing = card_with_strip();
        let mut paused = playing.clone();
        paused.track.state = PlaybackState::Paused;
        assert!(playing.has_transport(), "the premise: a strip is up");
        assert!(
            playing.visual_eq(&paused),
            "pause and play draw the same card when the strip draws the glyph"
        );
    }

    #[test]
    fn pausing_a_card_with_no_strip_is_a_new_picture() {
        // The other half, and why the state cannot simply be masked. With no strip the
        // card is the only thing on the panel that says whether sound should be coming
        // out, so it prints the word — and then the word really does change.
        let mut playing = card();
        playing.track.state = PlaybackState::Playing;
        let mut paused = playing.clone();
        paused.track.state = PlaybackState::Paused;
        assert!(!playing.has_transport(), "the premise: no strip");
        assert!(
            !playing.visual_eq(&paused),
            "the card prints the word itself"
        );
    }

    #[test]
    fn shuffle_and_repeat_never_reach_the_card() {
        // Strip glyphs, both of them. A phone toggling shuffle mid-track must not cost a
        // full-panel raster.
        let base = card_with_strip();
        let mut shuffled = base.clone();
        shuffled.track.shuffle = Some(true);
        shuffled.track.repeat = Some(castaway_core::RepeatMode::Context);
        assert!(base.visual_eq(&shuffled));
    }

    #[test]
    fn position_never_reaches_the_card() {
        // The original reason `visual_eq` exists: position republishes once a second.
        let base = card_with_strip();
        let mut later = base.clone();
        later.track.position = Some(std::time::Duration::from_secs(97));
        assert!(base.visual_eq(&later));
    }

    #[test]
    fn gaining_a_strip_is_a_new_picture() {
        // `has_transport` decides the space the card lays out against, so a card that
        // gains or loses its strip really does rasterize differently — this is the one
        // thing about the strip the card must keep noticing.
        let bare = card();
        let with_strip = card_with_strip();
        assert_ne!(bare.has_transport(), with_strip.has_transport());
        assert!(!bare.visual_eq(&with_strip));
    }

    #[test]
    fn the_things_the_card_does_draw_are_still_noticed() {
        let base = card_with_strip();
        for (what, mutate) in [
            (
                "title",
                (|c: &mut NowPlayingCard| c.track.title = Some("Other".into()))
                    as fn(&mut NowPlayingCard),
            ),
            ("artist", |c: &mut NowPlayingCard| {
                c.track.artist = Some("Someone".into())
            }),
            ("album", |c: &mut NowPlayingCard| {
                c.track.album = Some("An album".into())
            }),
            ("queue", |c: &mut NowPlayingCard| {
                c.up_next = vec![QueueItem::new("Next")];
            }),
            ("source", |c: &mut NowPlayingCard| {
                c.source = SourceDescription::new().with_display_name("Pixel");
            }),
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert!(
                !base.visual_eq(&changed),
                "a changed {what} must re-rasterize the card"
            );
        }
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
    fn a_long_queue_shows_its_head_and_no_counting_footer() {
        // "and N more" told someone at the panel a number they could do nothing with;
        // the phone that owns the queue already shows the whole thing. The card shows
        // the next few and stops.
        let mut c = card();
        c.up_next = (0..7)
            .map(|i| QueueItem::new(format!("Track {i}")))
            .collect();
        let texts = line_texts(&c);
        assert!(texts.contains(&"Track 0".to_string()), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("more")),
            "no counting footer: {texts:?}"
        );
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
    fn the_waiting_line_makes_room_only_when_the_queue_follows_it() {
        // The state line's bug, in the other branch of the layout: a connected source
        // with no track drew "Up next" on top of "Connected — no track information".
        let f = text::fonts().unwrap();
        let mut bare = NowPlayingCard {
            track: NowPlaying::default(),
            source: SourceDescription::new().with_display_name("schlarpc"),
            up_next: Vec::new(),
            controls: ControlCapabilities::NONE,
        };
        let gap_of = |card: &NowPlayingCard| {
            build_lines(card, &f, &Palette::default(), 1.0, 800.0)
                .into_iter()
                .find(|l| l.text.starts_with("Connected"))
                .map(|l| l.gap)
                .unwrap()
        };
        assert_eq!(gap_of(&bare), 0.0);
        bare.up_next = vec![QueueItem::new("Something")];
        assert!(gap_of(&bare) > 0.0, "the queue would overlap the notice");
    }

    #[test]
    fn a_card_with_no_track_draws_no_art_panel() {
        // No track means no art is coming, and a large empty square next to "connected,
        // nothing playing" is a hole in the screen rather than a placeholder.
        let (w, h) = (640u32, 360u32);
        let bare = NowPlayingCard {
            track: NowPlaying::default(),
            source: SourceDescription::new().with_display_name("schlarpc"),
            up_next: vec![QueueItem::new("Something")],
            controls: ControlCapabilities::NONE,
        };
        let playing = NowPlayingCard {
            track: NowPlaying::default().with_title("Alkatraz"),
            ..bare.clone()
        };

        // Sample where the art panel's interior would be, left of any text.
        let probe = |img: &[u8]| {
            let (x, y) = (60usize, (h / 2) as usize);
            let i = (y * w as usize + x) * 4;
            [img[i], img[i + 1], img[i + 2]]
        };
        let bare_px = probe(&render(&bare, w, h).unwrap());
        let playing_px = probe(&render(&playing, w, h).unwrap());
        assert_ne!(
            bare_px, playing_px,
            "the art panel should be absent without a track and present with one"
        );
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
    fn every_format_the_boundary_accepts_is_one_this_build_can_decode() {
        // `ImageFormat`'s docstring says it exists so an unrecognised format is refused
        // where it arrives "instead of failing inside an image decoder three layers down".
        // That only holds while the two lists agree, and they had not: this crate built
        // `image` with jpeg+png while the enum accepted gif and bmp, so a GIF thumbnail
        // parsed as valid and then failed exactly where the docstring promised it could
        // not (#87).
        //
        // Exhaustive over `ImageFormat::ALL` rather than a handful of samples, and asking
        // the decoder itself rather than reading the feature list, so a format added to
        // core or a feature dropped from Cargo.toml fails here rather than on the wall.
        for format in castaway_core::ImageFormat::ALL {
            let decoder = image::ImageFormat::from_mime_type(format.mime()).unwrap_or_else(|| {
                panic!(
                    "{format:?}: `image` does not know the MIME type {}",
                    format.mime()
                )
            });
            assert!(
                decoder.reading_enabled(),
                "{format:?} parses at the boundary but this build has no decoder for it — \
                 add its feature to `image` in pipeline/Cargo.toml, or drop the variant"
            );
        }
    }

    #[test]
    fn a_format_the_boundary_accepts_actually_decodes_bytes() {
        // The companion to the exhaustive check above, which trusts `image`'s own view of
        // which features are on. This one puts real bytes of a format that used to be
        // rejected through the whole path.
        let mut gif = b"GIF87a".to_vec();
        gif.extend_from_slice(&[1, 0, 1, 0, 0x80, 0, 0]); // 1x1, global table of 2
        gif.extend_from_slice(&[0, 0, 0, 0xFF, 0xFF, 0xFF]); // black, white
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0]); // image descriptor
        gif.extend_from_slice(&[0x02, 0x02, 0x44, 0x01, 0x00]); // LZW data
        gif.push(0x3B); // trailer

        let artwork =
            castaway_core::Artwork::new(castaway_core::ImageFormat::Gif, bytes::Bytes::from(gif));
        let cover = decode_cover(&artwork, 32).expect("a GIF cover must reach the card");
        assert_eq!(cover.side, 32);
        assert_eq!(cover.rgba.len(), 32 * 32 * 4);
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
            controls: ControlCapabilities::NONE,
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
