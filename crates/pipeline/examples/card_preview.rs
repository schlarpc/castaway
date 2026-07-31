//! Render the now-playing card to a PNG so a human can look at it.
#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
use castaway_core::{
    Artwork, ControlCapabilities, ImageFormat, NowPlaying, PlaybackState, QueueItem, RepeatMode,
    SourceDescription,
};
use pipeline::attract::to_png;
use pipeline::nowplaying_card::{background_span, render, NowPlayingCard};
use pipeline::transport;

/// What a Bluetooth phone advertises: AVRCP passthrough and nothing else. No shuffle, no
/// repeat, no absolute seek — so the preview shows the four-button strip a phone gets.
fn bluetooth_controls() -> ControlCapabilities {
    ControlCapabilities::TRANSPORT
}

/// What a Spotify Connect session advertises, which is everything the strip can draw.
fn spotify_controls() -> ControlCapabilities {
    ControlCapabilities::TRANSPORT
        | ControlCapabilities::SEEK
        | ControlCapabilities::SHUFFLE
        | ControlCapabilities::REPEAT
}

/// Render the card *and* its transport strip into one image, the way the compositor
/// stacks them. The preview exists so a person can look at the result; a preview that
/// omitted the controls would be checking a screen nobody is going to see.
fn compose(card: &NowPlayingCard, w: u32, h: u32) -> Vec<u8> {
    let mut base = render(card, w, h).unwrap();
    let model = card.transport();
    if model.is_empty() {
        return base;
    }
    let (x, y, sw, sh) = transport::placement(w, h);
    let (pw, ph) = (sw.round() as u32, sh.round() as u32);
    let (top, bottom) = background_span(y / h as f32, 1.0);
    let strip = transport::render(&model, pw, ph, top, bottom).unwrap();
    let (x0, y0) = (x.round() as u32, y.round() as u32);
    for row in 0..ph {
        for col in 0..pw {
            let (dx, dy) = (x0 + col, y0 + row);
            if dx >= w || dy >= h {
                continue;
            }
            let si = ((row * pw + col) * 4) as usize;
            let di = ((dy * w + dx) * 4) as usize;
            base[di..di + 4].copy_from_slice(&strip[si..si + 4]);
        }
    }
    base
}

fn main() {
    let out = std::env::args().nth(1).unwrap();
    // The exact data last night's iPhone session produced.
    let card = NowPlayingCard {
        track: NowPlaying::new(PlaybackState::Playing)
            .with_title("Dadvocate")
            .with_artist("Childish Gambino")
            .with_album("Bando Stone and The New World"),
        source: SourceDescription::new()
            .with_display_name("iPhone")
            .with_address("6C:3A:FF:D1:69:3E")
            .with_link("AAC · 44.1 kHz · stereo · \u{2264}256 kbps"),
        up_next: Vec::new(),
        controls: bluetooth_controls(),
    };
    let (w, h) = (1920, 1080);
    let rgba = compose(&card, w, h);
    std::fs::write(&out, to_png(w, h, &rgba).unwrap()).unwrap();
    println!("wrote {out}");

    // The other state the card actually lands in: a source that publishes no AVRCP
    // metadata at all, which is every non-phone sender.
    let bare = NowPlayingCard {
        track: NowPlaying::default(),
        source: SourceDescription::new()
            .with_display_name("bagel")
            .with_address("E0:D4:E8:A3:C0:8B")
            .with_link("aptX HD · 48 kHz · stereo · 576 kbps"),
        up_next: Vec::new(),
        controls: bluetooth_controls(),
    };
    let out2 = out.replace(".png", "-bare.png");
    let rgba = compose(&bare, w, h);
    std::fs::write(&out2, to_png(w, h, &rgba).unwrap()).unwrap();
    println!("wrote {out2}");

    // A Spotify Connect session, with exactly the fields `proto-spotify` emits. Worth a
    // case of its own because it is the one source with rich text and *no* artwork —
    // librespot hands over cover URLs rather than bytes, so the art panel is empty while
    // the metadata is complete (#50). It is also the only source whose
    // description carries no address, because there is no device on the other end to have
    // one: the peer is Spotify's cloud.
    let spotify = NowPlayingCard {
        track: NowPlaying::new(PlaybackState::Playing)
            .with_title("Windowlicker")
            .with_artist("Aphex Twin")
            .with_album("Windowlicker"),
        source: SourceDescription::new()
            .with_display_name("schlarpc")
            .with_link("Spotify Connect · 44100 Hz · stereo"),
        up_next: Vec::new(),
        controls: spotify_controls(),
    };
    let out3 = out.replace(".png", "-spotify.png");
    let rgba = compose(&spotify, w, h);
    std::fs::write(&out3, to_png(w, h, &rgba).unwrap()).unwrap();
    println!("wrote {out3}");

    // The same session once the cover has been fetched. Pass a PNG/JPEG path as the
    // second argument to see real art in the panel.
    if let Some(cover_path) = std::env::args().nth(2) {
        let bytes = std::fs::read(&cover_path).unwrap();
        let with_art = NowPlayingCard {
            // Mid-track, shuffling, repeating the playlist: the state that exercises
            // every glyph and puts the scrubber somewhere other than the ends.
            track: spotify
                .track
                .clone()
                .with_artwork(Artwork::new(ImageFormat::Png, bytes.into()))
                .with_shuffle(true)
                .with_repeat(RepeatMode::Context),
            source: spotify.source.clone(),
            // A queue with more in it than the card shows, so the "and N more" tail is
            // exercised rather than assumed.
            up_next: vec![
                QueueItem::new("Come to Daddy").with_artist("Aphex Twin"),
                QueueItem::new("Roygbiv").with_artist("Boards of Canada"),
                QueueItem::new("Xtal").with_artist("Aphex Twin"),
                QueueItem::new("Alberto Balsalm").with_artist("Aphex Twin"),
                QueueItem::new("Rhubarb").with_artist("Aphex Twin"),
            ],
            controls: spotify_controls(),
        };
        let mut with_art = with_art;
        with_art.track.position = Some(std::time::Duration::from_secs(97));
        with_art.track.duration = Some(std::time::Duration::from_secs(366));
        let out4 = out.replace(".png", "-spotify-art.png");
        let rgba = compose(&with_art, w, h);
        std::fs::write(&out4, to_png(w, h, &rgba).unwrap()).unwrap();
        println!("wrote {out4}");
    }
}
