//! The `/info` response: a binary plist describing the receiver's capabilities that a
//! sender fetches before starting a session.

use plist::{Dictionary, Value};

use crate::advert::AirPlayIdentity;
use crate::error::AirPlayError;

/// Build the `/info` binary plist for this receiver.
///
/// # Errors
/// [`AirPlayError::Plist`] if serialization fails (not expected for this fixed shape).
pub fn info_plist(ident: &AirPlayIdentity) -> Result<Vec<u8>, AirPlayError> {
    let mut dict = Dictionary::new();
    dict.insert("deviceid".into(), Value::String(ident.device_id.clone()));
    dict.insert("name".into(), Value::String(ident.name.clone()));
    dict.insert("model".into(), Value::String("castaway1,1".into()));
    dict.insert("manufacturer".into(), Value::String("castaway".into()));
    dict.insert("sourceVersion".into(), Value::String("377.40.00".into()));
    // Feature bitmask (video + audio + mirroring). Split hi/lo as senders expect.
    dict.insert("features".into(), Value::Integer(0x445F_8A00i64.into()));
    dict.insert("statusFlags".into(), Value::Integer(0x4i64.into()));
    dict.insert("pi".into(), Value::String(ident.device_id.clone()));
    dict.insert("vv".into(), Value::Integer(2i64.into()));
    // A minimal display block advertising 3840x2160@60 (the C6522QT panel).
    let mut display = Dictionary::new();
    display.insert("width".into(), Value::Integer(3840i64.into()));
    display.insert("height".into(), Value::Integer(2160i64.into()));
    display.insert("refreshRate".into(), Value::Integer(60i64.into()));
    dict.insert(
        "displays".into(),
        Value::Array(vec![Value::Dictionary(display)]),
    );

    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &Value::Dictionary(dict))
        .map_err(|e| AirPlayError::Plist(e.to_string()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn info_plist_roundtrips() {
        let ident = AirPlayIdentity {
            name: "Hackerspace TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
        };
        let bytes = info_plist(&ident).unwrap();
        assert!(bytes.starts_with(b"bplist00"), "should be a binary plist");
        // Re-parse and check a field survives.
        let val: Value = plist::from_bytes(&bytes).unwrap();
        let dict = val.as_dictionary().unwrap();
        assert_eq!(
            dict.get("name").unwrap().as_string().unwrap(),
            "Hackerspace TV"
        );
    }
}
