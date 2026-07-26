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

use castaway_core::{NowPlaying, PlaybackState, SourceDescription};

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

    // The text column, right of the art.
    let text_x = art_x + art + 70.0 * s;
    let avail = (w - text_x - margin).max(1.0);

    // Who is connected, above the track: small, because it changes once per session while
    // everything below it changes per song.
    let source = card.source.to_string();
    let mut y = art_y + text::ascent(&f.regular, 26.0 * s);
    if !source.is_empty() {
        let px = fit_px(&f.regular, &source, 26.0 * s, avail);
        text::draw_text(
            &mut buf, width, height, text_x, y, &source, px, pal.source, &f.regular,
        );
    }

    // Title, artist, album — in the order someone reads them.
    y += 78.0 * s;
    if let Some(title) = &card.track.title {
        let px = fit_px(&f.bold, title, 72.0 * s, avail);
        y += text::ascent(&f.bold, px);
        text::draw_text(
            &mut buf, width, height, text_x, y, title, px, pal.title, &f.bold,
        );
        y += 24.0 * s;
    }
    if let Some(artist) = &card.track.artist {
        let px = fit_px(&f.regular, artist, 44.0 * s, avail);
        y += text::ascent(&f.regular, px);
        text::draw_text(
            &mut buf, width, height, text_x, y, artist, px, pal.artist, &f.regular,
        );
        y += 18.0 * s;
    }
    if let Some(album) = &card.track.album {
        let px = fit_px(&f.regular, album, 32.0 * s, avail);
        y += text::ascent(&f.regular, px);
        text::draw_text(
            &mut buf, width, height, text_x, y, album, px, pal.album, &f.regular,
        );
    }

    // Transport state, bottom of the column.
    if let Some(label) = state_label(card.track.state) {
        let px = 28.0 * s;
        let baseline = art_y + art - (10.0 * s);
        text::draw_text(
            &mut buf, width, height, text_x, baseline, label, px, pal.state, &f.regular,
        );
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
        }
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

    #[test]
    fn stopped_prints_no_state_at_all() {
        // A sender that never reports a state leaves it at the default, and "Stopped"
        // under a playing track is a contradiction the viewer has to resolve.
        assert_eq!(state_label(PlaybackState::Stopped), None);
        assert_eq!(state_label(PlaybackState::Playing), Some("Playing"));
    }
}
