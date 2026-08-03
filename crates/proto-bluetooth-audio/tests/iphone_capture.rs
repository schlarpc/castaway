//! The parsers, against bytes an actual iPhone sent.
//!
//! Every other test in this crate judges our frames with our own decoder, which cannot
//! catch a shared misreading of a spec — the same gap `examples/ertm_echo.rs` closes for
//! ERTM by borrowing the Linux kernel's L2CAP. This closes it for the metadata and
//! cover-art paths with the one thing no reference implementation can supply: what the
//! peer we actually care about really says.
//!
//! Captured 2026-08-02 from an iPhone (`6c:3a:ff:d1:69:3e`) playing YouTube Music into
//! `examples/phone_bench.rs`, with the whole HCI conversation preserved beside these
//! extracts at `fixtures/iphone-avrcp-bip-2026-08-02.btsnoop` — 1952 packets, readable in
//! Wireshark. These files are the parameter bytes of individual responses, lifted out so
//! the sans-I/O cores can be fed them directly.
//!
//! This is the capture #74, #75 and #76 all asked for, and it answered all three.

use std::time::Duration;

use proto_bluetooth_audio::avrcp::{
    self, RepeatSetting, SettingAttribute, SettingValue, ShuffleSetting,
};
use proto_bluetooth_audio::obex::{Encoding, ImageProperties, PixelSize, VariantKind};

fn fixture(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/");
    std::fs::read(format!("{path}{name}")).expect("fixture")
}

#[test]
fn an_iphone_exposes_repeat_and_shuffle_and_nothing_else() {
    // #76's question, answered: *which* player application settings does an iPhone expose?
    // Both of the two the panel can draw, and no others — not equalizer, not scan.
    let attributes = avrcp::parse_setting_attributes(&fixture("iphone-setting-attributes.bin"))
        .expect("the listing an iPhone actually sent");
    assert_eq!(
        attributes.known,
        vec![SettingAttribute::Repeat, SettingAttribute::Shuffle]
    );
    assert!(
        attributes.unknown.is_empty(),
        "nothing outside the four the spec defines"
    );

    // …so the panel offers exactly those two buttons.
    let settings = avrcp::PlayerSettings {
        attributes,
        ..avrcp::PlayerSettings::default()
    };
    let caps = settings.capabilities();
    assert!(caps.supports(&castaway_core::ControlTxn::Shuffle(true)));
    assert!(caps.supports(&castaway_core::ControlTxn::Repeat(
        castaway_core::RepeatMode::Context
    )));
}

#[test]
fn an_iphone_offers_no_group_values_which_is_what_the_preference_list_assumed() {
    // The write path picks a value the peer listed rather than assuming the usual one.
    // This is the evidence for what "the usual one" is: repeat off/single/all, shuffle
    // off/all, and neither of the `group` values that need a browsing channel to mean
    // anything. #76 predicted they were never sent; they are never sent.
    let repeat = avrcp::parse_setting_values(
        SettingAttribute::Repeat,
        &fixture("iphone-setting-values-repeat.bin"),
    )
    .expect("repeat values");
    assert_eq!(
        repeat,
        vec![
            SettingValue::Repeat(RepeatSetting::Off),
            SettingValue::Repeat(RepeatSetting::SingleTrack),
            SettingValue::Repeat(RepeatSetting::AllTracks),
        ]
    );
    let shuffle = avrcp::parse_setting_values(
        SettingAttribute::Shuffle,
        &fixture("iphone-setting-values-shuffle.bin"),
    )
    .expect("shuffle values");
    assert_eq!(
        shuffle,
        vec![
            SettingValue::Shuffle(ShuffleSetting::Off),
            SettingValue::Shuffle(ShuffleSetting::AllTracks),
        ]
    );

    // And a repeat press therefore writes `AllTracks`, the first candidate that is on the
    // list — the group fallback exists for players that are not this one.
    let mut settings = avrcp::PlayerSettings::default();
    settings.attributes.known = vec![SettingAttribute::Repeat, SettingAttribute::Shuffle];
    settings.record_values(&repeat);
    settings.record_values(&shuffle);
    assert_eq!(
        settings.value_for(&castaway_core::ControlTxn::Repeat(
            castaway_core::RepeatMode::Context
        )),
        Some(SettingValue::Repeat(RepeatSetting::AllTracks))
    );
}

#[test]
fn the_current_settings_reach_the_card() {
    let values = avrcp::parse_current_settings(&fixture("iphone-current-settings.bin"))
        .expect("current settings");
    let mut now = castaway_core::NowPlaying::default();
    assert!(avrcp::apply_settings(&mut now, &values));
    assert_eq!(now.repeat, Some(castaway_core::RepeatMode::Off));
    assert_eq!(now.shuffle, Some(false));
}

#[test]
fn a_real_metadata_response_carries_the_cover_art_handle() {
    // #74's actual question. Attribute 8 is present here and absent from the same phone's
    // earlier responses in the same capture, and the difference between them is that the
    // BIP session had come up in between — which is the ordering the whole cover-art path
    // is built around, now confirmed against a phone rather than inferred from AOSP.
    let parsed = avrcp::parse_element_attributes(&fixture("iphone-element-attributes.bin"))
        .expect("the metadata an iPhone actually sent");
    assert_eq!(parsed.now_playing.title.as_deref(), Some("Sunrise"));
    assert_eq!(
        parsed.now_playing.artist.as_deref(),
        Some("Childish Gambino")
    );
    assert_eq!(parsed.now_playing.album.as_deref(), Some("Camp"));
    assert_eq!(
        parsed.now_playing.duration,
        Some(Duration::from_millis(219_861))
    );
    assert_eq!(parsed.cover_art_handle.as_deref(), Some("1000003"));

    // The empty attributes — track number, total tracks, genre — are sent as zero-length
    // values rather than omitted, and must not become empty strings on the card.
    assert_eq!(parsed.now_playing.track, None);
    assert_eq!(parsed.now_playing.genre, None);
}

#[test]
fn an_iphone_holds_its_cover_art_at_200x200() {
    // #75, and the measurement the whole issue turns on: is 200×200 the ceiling, or just
    // the form we ask for? It is very nearly the ceiling. The native image *is* the
    // linked thumbnail's fixed 200×200, and the single variant on offer is a 1.4×
    // upscale of it — no larger original exists to go and fetch.
    let props = ImageProperties::parse(&fixture("iphone-image-properties.xml"))
        .expect("the listing an iPhone actually sent");
    assert_eq!(props.handle.as_deref(), Some("1000001"));
    assert_eq!(props.variants.len(), 2);

    let native = &props.variants[0];
    assert_eq!(native.kind, VariantKind::Native);
    assert_eq!(
        native.encoding,
        Encoding::Known(castaway_core::ImageFormat::Jpeg)
    );
    assert_eq!(
        native.pixel,
        Some(PixelSize::Fixed {
            width: 200,
            height: 200
        })
    );

    assert_eq!(props.variants[1].kind, VariantKind::Variant);
    assert_eq!(
        props.variants[1].pixel,
        Some(PixelSize::Fixed {
            width: 280,
            height: 280
        })
    );

    // The number #75 asks for. 280×280 against the 200×200 we already fetch is not worth
    // the `GetImage` descriptor negotiation the issue's step 3 would have cost.
    assert_eq!(
        props.largest_decodable().map(|(_, size)| size),
        Some((280, 280))
    );
}

#[test]
fn the_real_document_uses_spellings_the_strict_parser_has_to_accept() {
    // Two things this file does that a hand-written fixture might not have: a space
    // before the self-closing `/>`, and no `size` or `maxsize` anywhere. Both are legal
    // and both would break a parser tuned to the spec's example rather than to a phone.
    let raw = fixture("iphone-image-properties.xml");
    let text = std::str::from_utf8(&raw).expect("utf-8");
    assert!(text.contains(" />"), "self-closing with a space");
    assert!(!text.contains("size="), "no byte figures at all");

    let props = ImageProperties::parse(&raw).expect("still parses");
    assert!(props.variants.iter().all(|v| v.size.is_none()));
}
