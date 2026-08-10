//! The FCast connection URL (#248): `fcast://r/<base64url(JSON)>`, the payload a
//! panel renders as a QR code so a sender can connect — and pin the receiver's
//! `fp` — without trusting mDNS.
//!
//! The QR channel is the point: the `fp` fingerprint learned from an mDNS TXT
//! record only resists a passive eavesdropper, but one read off the glass cannot
//! be tampered with by a network attacker, and it tells the sender in advance
//! that the receiver speaks v4 so it refuses to fall back to plaintext. Pure —
//! this builds the string; `pipeline::qr` draws it.

use base64::Engine as _;
use serde::Serialize;

/// One service a receiver exposes, in the connection document.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectService {
    /// TCP port.
    pub port: u16,
    /// Service type. `0` is the FCast TCP service.
    #[serde(rename = "type")]
    pub kind: i32,
}

/// The connection document, before base64url encoding.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    /// Human-readable receiver name.
    pub name: String,
    /// IP addresses the receiver is reachable on.
    pub addresses: Vec<String>,
    /// The services it exposes.
    pub services: Vec<ConnectService>,
    /// The mDNS TXT records (`v`, `fp`), so the QR carries the fingerprint.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub txt: std::collections::BTreeMap<String, String>,
}

impl ConnectionInfo {
    /// A v4 connection document: the FCast TCP service on `port`, reachable at
    /// `addresses`, carrying `v=4` and the pinned `fp`.
    #[must_use]
    pub fn v4(
        name: impl Into<String>,
        addresses: Vec<String>,
        port: u16,
        fingerprint: &str,
    ) -> Self {
        let mut txt = std::collections::BTreeMap::new();
        txt.insert("v".to_string(), "4".to_string());
        txt.insert("fp".to_string(), fingerprint.to_string());
        Self {
            name: name.into(),
            addresses,
            services: vec![ConnectService { port, kind: 0 }],
            txt,
        }
    }

    /// The `fcast://r/<base64url>` URL, or `None` if the document cannot be
    /// serialized (it always can — every field is a plain value — so `None` is a
    /// defensive dead branch, not a runtime path).
    #[must_use]
    pub fn to_url(&self) -> Option<String> {
        let json = serde_json::to_vec(self).ok()?;
        // base64url; the spec's decoders accept padding either way, and the
        // reference encoder pads, so pad.
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(json);
        Some(format!("fcast://r/{encoded}"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The URL round-trips through a base64url decode back to the JSON the spec
    /// documents, fingerprint and all.
    #[test]
    fn the_url_is_decodable_json_carrying_the_fingerprint() {
        let info = ConnectionInfo::v4(
            "Living Room",
            vec!["192.168.1.42".to_string()],
            46899,
            "QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc=",
        );
        let url = info.to_url().unwrap();
        let encoded = url.strip_prefix("fcast://r/").expect("the scheme prefix");
        let json = base64::engine::general_purpose::URL_SAFE
            .decode(encoded)
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["name"], "Living Room");
        assert_eq!(value["services"][0]["port"], 46899);
        assert_eq!(value["services"][0]["type"], 0);
        assert_eq!(value["txt"]["v"], "4");
        assert_eq!(
            value["txt"]["fp"],
            "QvrqvvBvKimMvIvJElsiQeiviSXvefqpiZYVxKXZOWc="
        );
    }

    /// The reference sender's `FCastNetworkConfig::parse_url` requires a service
    /// of type 0 and parseable addresses; our document satisfies both.
    #[test]
    fn the_document_matches_the_senders_parser_shape() {
        let info = ConnectionInfo::v4(
            "Panel",
            vec!["10.0.0.5".to_string(), "fe80::1".to_string()],
            46899,
            "fp",
        );
        let value = serde_json::to_value(&info).unwrap();
        assert!(value["addresses"].as_array().unwrap().len() == 2);
        assert_eq!(value["services"][0]["type"], 0);
    }
}
