//! Render the now-playing card to a PNG so a human can look at it.
#![allow(clippy::unwrap_used)]
use castaway_core::{NowPlaying, PlaybackState, SourceDescription};
use pipeline::attract::to_png;
use pipeline::nowplaying_card::{render, NowPlayingCard};

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
    };
    let (w, h) = (1920, 1080);
    let rgba = render(&card, w, h).unwrap();
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
    };
    let out2 = out.replace(".png", "-bare.png");
    let rgba = render(&bare, w, h).unwrap();
    std::fs::write(&out2, to_png(w, h, &rgba).unwrap()).unwrap();
    println!("wrote {out2}");

    // A Spotify Connect session, with exactly the fields `proto-spotify` emits. Worth a
    // case of its own because it is the one source with rich text and *no* artwork —
    // librespot hands over cover URLs rather than bytes, so the art panel is empty while
    // the metadata is complete (OPEN-QUESTIONS Q39). It is also the only source whose
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
    };
    let out3 = out.replace(".png", "-spotify.png");
    let rgba = render(&spotify, w, h).unwrap();
    std::fs::write(&out3, to_png(w, h, &rgba).unwrap()).unwrap();
    println!("wrote {out3}");
}
