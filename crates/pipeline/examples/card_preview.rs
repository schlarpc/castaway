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
}
