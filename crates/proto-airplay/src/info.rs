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

use plist::{Dictionary, Value};

use crate::advert::{AirPlayIdentity, MODEL, SOURCE_VERSION};
use crate::error::AirPlayError;

/// Build the `/info` binary plist for this receiver.
///
/// # Errors
/// [`AirPlayError::Plist`] if serialization fails (not expected for this fixed shape).
pub fn info_plist(ident: &AirPlayIdentity) -> Result<Vec<u8>, AirPlayError> {
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
    dict.insert("vv".into(), Value::Integer(2i64.into()));

    // No `displays` array. It describes a mirroring surface, and nothing here serves
    // one; advertising a screen we will not accept a stream for is the same class of
    // lie as the FairPlay bits in the TXT record. When mirroring lands this returns,
    // and `refreshRate` is a `Real` holding the frame *period* (1/60), not the integer
    // `60` that used to sit here — 60 reads as a sixty-second frame.

    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &Value::Dictionary(dict))
        .map_err(|e| AirPlayError::Plist(e.to_string()))?;
    Ok(buf)
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
        }
    }

    fn parsed(ident: &AirPlayIdentity) -> Dictionary {
        let bytes = info_plist(ident).unwrap();
        assert!(bytes.starts_with(b"bplist00"), "should be a binary plist");
        let val: Value = plist::from_bytes(&bytes).unwrap();
        val.as_dictionary().unwrap().clone()
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
    fn no_display_is_offered_while_there_is_no_mirroring() {
        assert!(!parsed(&ident()).contains_key("displays"));
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
