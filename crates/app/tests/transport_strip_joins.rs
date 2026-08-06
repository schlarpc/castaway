//! What the strip actually offers, for the capability sets the protocols really publish.
//!
//! `TransportModel::from_now_playing` is a *join*: the card says what is playing, the
//! session's [`castaway_core::RemoteControl`] says what may be offered, and the strip is
//! what falls out. Both halves are well tested on their own and the join was tested only
//! against sets invented in the test — `pipeline`'s own tests build a
//! `ControlCapabilities` by hand, and `proto-spotify`'s session tests use a stub whose set
//! is `NONE` (#199). So "this protocol's real set draws these buttons" was asserted
//! nowhere, which is the shape where a button that does nothing, or a missing button that
//! should be there, survives every check in the repo.
//!
//! This crate is where the join belongs: `app` is the one that depends on both sides, and
//! a `proto-*` crate reaching for `pipeline` would be the dependency direction backwards
//! (ground rule 2).

use castaway_core::{ControlCapabilities, NowPlaying, PlaybackState, RepeatMode};
use pipeline::transport::{TransportControl, TransportModel};

/// A card mid-track, with everything a well-behaved sender reports.
fn playing() -> NowPlaying {
    let mut card = NowPlaying::new(PlaybackState::Playing);
    card.title = Some("Windowlicker".to_owned());
    card.duration = Some(std::time::Duration::from_secs(366));
    card.position = Some(std::time::Duration::from_secs(61));
    card
}

#[test]
fn a_spotify_session_offers_the_whole_strip_once_the_phone_has_reported_itself() {
    let mut card = playing();
    card.shuffle = Some(false);
    card.repeat = Some(RepeatMode::Off);

    let strip = TransportModel::from_now_playing(
        &card,
        proto_spotify::control::SpotifyRemote::capabilities(),
    );
    assert_eq!(
        strip.controls(),
        [
            TransportControl::Shuffle,
            TransportControl::Previous,
            TransportControl::PlayPause,
            TransportControl::Next,
            TransportControl::Repeat,
        ],
        "Connect drives all five, so all five should be on the glass"
    );
    // And the scrub track is live, because Spotify advertises SEEK and the track has a
    // length. A drawn-but-dead scrubber is the failure this pairing prevents: it moves
    // under the finger and snaps back.
    assert!(strip.is_seekable());
    assert!(strip.scrub_fraction().is_some());
}

#[test]
fn spotify_offers_no_shuffle_button_until_the_phone_has_said_which_way_it_is_set() {
    // The capability is advertised from the first moment of the session, but the *state*
    // arrives on a player event that may never come — a session transferred mid-playlist
    // reports the track before it reports the mode. A button drawn from the capability
    // alone would show a guessed glyph, and pressing it would toggle from an unknown
    // starting point into whichever mode the room did not ask for.
    let strip = TransportModel::from_now_playing(
        &playing(),
        proto_spotify::control::SpotifyRemote::capabilities(),
    );
    assert_eq!(
        strip.controls(),
        [
            TransportControl::Previous,
            TransportControl::PlayPause,
            TransportControl::Next,
        ],
        "shuffle and repeat need the state as well as the capability"
    );
}

#[test]
fn a_cast_or_dlna_session_gets_a_scrubber_and_no_skip_buttons() {
    // Both drive one item at a time: there is no queue on our side of either protocol, so
    // a next/previous button would have nothing to move to. Asserted for the contrast —
    // the same join, the same card, a different set, a visibly different strip.
    for caps in [
        proto_cast::control::CastRemote::capabilities(),
        proto_dlna::control::DlnaRemote::capabilities(),
    ] {
        let strip = TransportModel::from_now_playing(&playing(), caps);
        assert_eq!(strip.controls(), [TransportControl::PlayPause]);
        assert!(strip.is_seekable());
        assert!(
            !caps.contains(ControlCapabilities::NEXT),
            "neither has a queue to skip through"
        );
    }
}

#[test]
fn a_session_that_advertises_nothing_draws_no_strip_at_all() {
    // An empty bar across the bottom of a two-metre screen reads as controls that failed
    // to load. The `NONE` set is what every session has before its control surface
    // arrives, which for Bluetooth is a second L2CAP channel that routinely connects after
    // audio is already flowing.
    let mut card = playing();
    card.duration = None;
    card.position = None;
    assert!(TransportModel::from_now_playing(&card, ControlCapabilities::NONE).is_empty());
}
