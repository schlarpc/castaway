//! The `/info` response: a binary plist describing the receiver's capabilities that a
//! sender fetches before starting a session.
//!
//! Two things about this endpoint are easy to get wrong and were, until this was
//! checked against three real captured `/info` bodies (an Apple TV 4K, a Denon
//! AVR-X3500H and AirServer, all in `openairplay/airplay-spec`):
//!
//! 1. **The key names are not the TXT key names.** `/info` uses `deviceID`,
//!    `sourceVersion`, `protocolVersion` and `statusFlags` where the mDNS record uses
//!    `deviceid`, `srcvers`, `protovers` and `flags`. Same data, different spelling.
//! 2. **`features` is the whole 64-bit value**, not the low word the TXT record shows
//!    first. A sender reads both, and a mismatch between them is a contradiction it can
//!    see.
//!
//! It is a *binary* plist. The XML form with `text/x-apple-plist+xml` belongs to the
//! different, legacy `GET /server-info` endpoint.
//!
//! 3. **The request has a body, and it changes what the answer is.** An iOS sender's
//!    *first* request of every session is `GET /info` carrying a plist
//!    `{qualifier: ["txtAirPlay"]}`, which asks for one thing: the receiver's mDNS TXT
//!    record, byte for byte, read back over the connection it is already on. The answer
//!    is that record and nothing else — see [`InfoQuery`].

use plist::{Dictionary, Value};
use tracing::{debug, warn};

use crate::advert::{AirPlayIdentity, MODEL, SOURCE_VERSION};
use crate::error::AirPlayError;

/// Which mDNS TXT record a `/info` qualifier is asking for.
///
/// An enum rather than the string off the wire because the answer is a *different
/// advertisement* per variant, and a name we don't serve has to be distinguishable from
/// one we do — a sender asking for an unknown record gets a reply without it, not a
/// reply with the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxtQualifier {
    /// `txtAirPlay`: the `_airplay._tcp` record.
    AirPlay,
    /// `txtRAOP`: the `_raop._tcp` record.
    Raop,
}

impl TxtQualifier {
    /// The qualifier name, which is also the reply key it is answered under.
    const fn key(self) -> &'static str {
        match self {
            Self::AirPlay => "txtAirPlay",
            Self::Raop => "txtRAOP",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "txtAirPlay" => Some(Self::AirPlay),
            "txtRAOP" => Some(Self::Raop),
            other => {
                debug!(qualifier = %other, "/info asks for a record we do not advertise");
                None
            }
        }
    }
}

/// What a `GET /info` is asking for.
///
/// The two are not variations on one answer, they are different answers: a qualified
/// request is answered with the named TXT records **and nothing else** (which is what
/// UxPlay does, and it is what iOS is served by every receiver it mirrors to), while an
/// unqualified one gets the capability dictionary. Modelling it as an enum is what stops
/// the two from being merged into a single "everything" reply — which is what this used
/// to send, and it left the `txtAirPlay` the sender asked for absent from the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoQuery {
    /// No parsable qualifier: the full capability dictionary.
    Full,
    /// The TXT records named by the request's `qualifier` array, in order.
    Txt(Vec<TxtQualifier>),
}

impl InfoQuery {
    /// Read the request body. Anything that is not a plist with a `qualifier` array —
    /// including the empty body of a plain `GET /info` — is [`Self::Full`].
    #[must_use]
    pub fn parse(body: &[u8]) -> Self {
        let Ok(value) = plist::Value::from_reader(std::io::Cursor::new(body)) else {
            return Self::Full;
        };
        let Some(qualifier) = value
            .as_dictionary()
            .and_then(|d| d.get("qualifier"))
            .and_then(Value::as_array)
        else {
            return Self::Full;
        };
        Self::Txt(
            qualifier
                .iter()
                .filter_map(Value::as_string)
                .filter_map(TxtQualifier::parse)
                .collect(),
        )
    }
}

/// Encode a TXT record as it goes on the wire: each `key=value` string prefixed with its
/// own length in one octet (RFC 6763 §6.1).
///
/// The entries come from the same [`AirPlayIdentity`] the responder advertises from, so
/// the record read back here and the record on the wire cannot disagree — which is the
/// only reason a sender asks for it over a connection it has already made.
fn txt_record(entries: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in entries {
        let pair = format!("{key}={value}");
        match u8::try_from(pair.len()) {
            Ok(len) => {
                out.push(len);
                out.extend_from_slice(pair.as_bytes());
            }
            // A single TXT string cannot exceed 255 octets; one that does was never
            // advertised either, so omitting it keeps the two records identical.
            Err(_) => warn!(%key, "TXT entry too long for one record string; omitted"),
        }
    }
    out
}

/// Serialize a reply dictionary as the binary plist `/info` answers with.
fn finish(dict: &Dictionary) -> Result<Vec<u8>, AirPlayError> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, dict).map_err(|e| AirPlayError::Plist(e.to_string()))?;
    Ok(buf)
}

/// Build the `/info` binary plist answering `query`.
///
/// # Errors
/// [`AirPlayError::Plist`] if serialization fails (not expected for this fixed shape).
pub fn info_plist(ident: &AirPlayIdentity, query: &InfoQuery) -> Result<Vec<u8>, AirPlayError> {
    if let InfoQuery::Txt(qualifiers) = query {
        let mut dict = Dictionary::new();
        for qualifier in qualifiers {
            let service = match qualifier {
                TxtQualifier::AirPlay => ident.airplay_service(),
                TxtQualifier::Raop => ident.raop_service(),
            };
            dict.insert(
                qualifier.key().into(),
                Value::Data(txt_record(&service.txt)),
            );
        }
        return finish(&dict);
    }

    let mut dict = Dictionary::new();
    dict.insert("deviceID".into(), Value::String(ident.device_id.clone()));
    dict.insert("macAddress".into(), Value::String(ident.device_id.clone()));
    dict.insert("name".into(), Value::String(ident.name.clone()));
    dict.insert("model".into(), Value::String(MODEL.into()));
    dict.insert("manufacturer".into(), Value::String("castaway".into()));
    dict.insert("sourceVersion".into(), Value::String(SOURCE_VERSION.into()));
    dict.insert("protocolVersion".into(), Value::String("1.1".into()));
    // The full 64-bit mask, from the same value the TXT record encodes as
    // `0x<low>,0x<high>`. Both come from `AirPlayIdentity::features()` precisely so
    // they cannot disagree.
    dict.insert(
        "features".into(),
        Value::Integer(ident.features().as_u64().into()),
    );
    // Status flag bit 2, "audio cable attached" — the same field the TXT calls `flags`.
    dict.insert("statusFlags".into(), Value::Integer(4i64.into()));
    // `pi` is a stable UUID, not the device id. These were the same string here once,
    // which is the Roku/Samsung behaviour rather than the Apple one.
    dict.insert("pi".into(), Value::String(ident.pairing_id.clone()));
    // The same `pk` the TXT records carry; the three encodings of this identity must
    // not disagree, so all come from `public_key_hex`.
    dict.insert("pk".into(), Value::String(ident.public_key_hex()));
    dict.insert("vv".into(), Value::Integer(2i64.into()));

    // The mirroring surface. A sender reads *height* to choose an encode format and
    // adjusts width to however the device is being held — orientation is not negotiated,
    // it is reported back in the type-1 packet's header floats — so this advertises a
    // height budget rather than a fixed geometry. 1080 with feature bit 42 clear keeps
    // the sender on H.264, which is the only codec decoded here.
    let mut display = Dictionary::new();
    display.insert("uuid".into(), Value::String(ident.pairing_id.clone()));
    // 16:9 against the advertised height. Width is nominal — the sender adjusts it to
    // however the device is being held and reports what it chose in the stream header.
    let height = i64::from(ident.mirror_height);
    let width = height * 16 / 9;
    display.insert("widthPixels".into(), Value::Integer(width.into()));
    display.insert("heightPixels".into(), Value::Integer(height.into()));
    display.insert("width".into(), Value::Integer(width.into()));
    display.insert("height".into(), Value::Integer(height.into()));
    // Millimetres, and 0 means "unknown" — which is what every real receiver reports.
    display.insert("widthPhysical".into(), Value::Integer(0i64.into()));
    display.insert("heightPhysical".into(), Value::Integer(0i64.into()));
    // `refreshRate` is a frame **period**, not a rate: 1/60, as a real. The integer 60
    // that used to be the whole of this block reads as a sixty-second frame.
    display.insert("refreshRate".into(), Value::Real(1.0 / 60.0));
    display.insert("maxFPS".into(), Value::Integer(30i64.into()));
    // Overscan would inset the picture for a CRT-era TV and waste resolution on a flat
    // panel, and rotation must agree with feature bit 8, which is clear.
    display.insert("overscanned".into(), Value::Boolean(false));
    display.insert("rotation".into(), Value::Boolean(false));
    // A separate, small bitmask from the device features; nobody has decoded it, and
    // every implementation reports 14 or 30.
    display.insert("features".into(), Value::Integer(14i64.into()));
    dict.insert(
        "displays".into(),
        Value::Array(vec![Value::Dictionary(display)]),
    );

    finish(&dict)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ident() -> AirPlayIdentity {
        AirPlayIdentity {
            name: "Hackerspace TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
            pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
            offer_hevc: false,
            mirror_height: 1080,
        }
    }

    fn parsed(ident: &AirPlayIdentity) -> Dictionary {
        answer(ident, &InfoQuery::Full)
    }

    fn answer(ident: &AirPlayIdentity, query: &InfoQuery) -> Dictionary {
        let bytes = info_plist(ident, query).unwrap();
        assert!(bytes.starts_with(b"bplist00"), "should be a binary plist");
        let val: Value = plist::from_bytes(&bytes).unwrap();
        val.as_dictionary().unwrap().clone()
    }

    /// A qualifier request body, as an iOS sender's first request carries it.
    fn qualifier_body(name: &str) -> Vec<u8> {
        let mut dict = Dictionary::new();
        dict.insert(
            "qualifier".into(),
            Value::Array(vec![Value::String(name.into())]),
        );
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, &dict).unwrap();
        buf
    }

    /// Decode a wire TXT record back into its pairs.
    fn decode_txt(mut bytes: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Some((&len, rest)) = bytes.split_first() {
            let (entry, tail) = rest.split_at(usize::from(len));
            let text = String::from_utf8(entry.to_vec()).unwrap();
            let (k, v) = text.split_once('=').unwrap();
            out.push((k.to_string(), v.to_string()));
            bytes = tail;
        }
        out
    }

    #[test]
    fn an_empty_body_is_the_full_capability_dictionary() {
        assert_eq!(InfoQuery::parse(&[]), InfoQuery::Full);
        assert!(parsed(&ident()).contains_key("displays"));
    }

    #[test]
    fn a_qualified_request_is_answered_with_that_txt_record_and_nothing_else() {
        // The defect this replaces: every `GET /info` was answered with the capability
        // dictionary, so the one thing an iOS sender's *first* request asks for — the
        // `txtAirPlay` record — was absent from every answer we ever sent.
        let id = ident();
        let query = InfoQuery::parse(&qualifier_body("txtAirPlay"));
        assert_eq!(query, InfoQuery::Txt(vec![TxtQualifier::AirPlay]));
        let dict = answer(&id, &query);
        assert_eq!(dict.len(), 1, "only the record asked for: {dict:?}");
        let txt = dict.get("txtAirPlay").unwrap().as_data().unwrap();
        // Byte-for-byte the record the responder advertises: a sender that reads both
        // can see a difference.
        assert_eq!(decode_txt(txt), id.airplay_service().txt);
    }

    #[test]
    fn the_raop_record_can_be_asked_for_by_its_own_name() {
        let id = ident();
        let dict = answer(&id, &InfoQuery::parse(&qualifier_body("txtRAOP")));
        let txt = dict.get("txtRAOP").unwrap().as_data().unwrap();
        assert_eq!(decode_txt(txt), id.raop_service().txt);
    }

    #[test]
    fn a_qualifier_we_do_not_serve_leaves_the_answer_empty_rather_than_wrong() {
        let dict = answer(&ident(), &InfoQuery::parse(&qualifier_body("txtSomething")));
        assert!(dict.is_empty(), "{dict:?}");
    }

    #[test]
    fn info_plist_roundtrips() {
        let dict = parsed(&ident());
        assert_eq!(
            dict.get("name").unwrap().as_string().unwrap(),
            "Hackerspace TV"
        );
    }

    #[test]
    fn keys_use_the_info_spellings_not_the_txt_ones() {
        // Three real captured `/info` bodies agree on these; the TXT spellings here
        // meant a sender looking for `deviceID` found nothing.
        let dict = parsed(&ident());
        for key in [
            "deviceID",
            "sourceVersion",
            "protocolVersion",
            "statusFlags",
        ] {
            assert!(dict.contains_key(key), "missing `{key}`");
        }
        for key in ["deviceid", "srcvers", "protovers", "flags"] {
            assert!(!dict.contains_key(key), "`{key}` is the TXT spelling");
        }
    }

    #[test]
    fn features_is_the_full_64_bit_value_the_txt_record_encodes() {
        // The defect this replaces: `/info` carried only the low 32 bits, so it
        // advertised a *different, smaller* capability set than the mDNS record.
        let id = ident();
        let dict = parsed(&id);
        let from_info = dict.get("features").unwrap().as_unsigned_integer().unwrap();
        assert_eq!(from_info, id.features().as_u64());
    }

    #[test]
    fn status_flags_match_the_advertised_flags() {
        let id = ident();
        let dict = parsed(&id);
        let flags = dict
            .get("statusFlags")
            .unwrap()
            .as_unsigned_integer()
            .unwrap();
        let advertised = id
            .airplay_service()
            .txt
            .iter()
            .find(|(k, _)| k == "flags")
            .map(|(_, v)| v.clone())
            .unwrap();
        // Compare the values, not the spellings: `0x4` and `0X04` are the same field.
        let advertised = u64::from_str_radix(advertised.trim_start_matches("0x"), 16).unwrap();
        assert_eq!(flags, advertised);
    }

    #[test]
    fn the_display_block_describes_a_surface_a_sender_can_address() {
        let dict = parsed(&ident());
        let displays = dict.get("displays").unwrap().as_array().unwrap();
        let d = displays[0].as_dictionary().unwrap();
        // Without a uuid it is not a display the sender can name.
        assert!(d.get("uuid").unwrap().as_string().is_some());
        assert_eq!(
            d.get("heightPixels").unwrap().as_unsigned_integer(),
            Some(1080)
        );
        for key in ["widthPixels", "maxFPS", "overscanned", "widthPhysical"] {
            assert!(d.contains_key(key), "missing `{key}`");
        }
    }

    #[test]
    fn refresh_rate_is_a_period_not_a_rate() {
        // The defect this replaces: an integer 60 here reads as a sixty-second frame.
        let dict = parsed(&ident());
        let d = dict.get("displays").unwrap().as_array().unwrap()[0]
            .as_dictionary()
            .unwrap();
        let rate = d.get("refreshRate").unwrap();
        assert!(rate.as_real().is_some(), "must be a Real, got {rate:?}");
        assert!((rate.as_real().unwrap() - 1.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn the_info_pk_is_the_pairing_identity() {
        // `/info`, the TXT records, and `/pair-setup` are three encodings of one
        // identity; a sender reads them all and can see a contradiction. All three
        // must come from `PairingIdentity` — `/info` used to carry a SHA-256-derived
        // stand-in the pairing layer never presented.
        let id = ident();
        let dict = parsed(&id);
        let pairing = crate::pairing::PairingIdentity::from_seed(&id.pairing_id);
        assert_eq!(
            dict.get("pk").unwrap().as_string().unwrap(),
            pairing.public_key_hex()
        );
    }

    #[test]
    fn the_pairing_identifier_is_not_the_device_id() {
        let dict = parsed(&ident());
        assert_ne!(
            dict.get("pi").unwrap().as_string(),
            dict.get("deviceID").unwrap().as_string()
        );
    }
}
