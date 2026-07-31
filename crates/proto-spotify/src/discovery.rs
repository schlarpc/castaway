//! The zeroconf onboarding HTTP surface. A Spotify app on the LAN finds
//! `_spotify-connect._tcp`, calls `getInfo` (we return our DH public key), then
//! `addUser` with an encrypted credentials blob we decrypt with the shared secret.
//!
//! Playback after onboarding (the "dealer" WebSocket + audio pull from the CDN) is
//! deferred — see #47. This module gets us discoverable + paired.

use base64::Engine as _;
use serde::Deserialize;

use crate::crypto::DhKeys;
use crate::error::SpotifyError;

/// Static-ish device identity surfaced in `getInfo`.
pub struct DeviceInfo {
    /// Friendly name shown in the Spotify device picker.
    pub remote_name: String,
    /// Stable device id (hex).
    pub device_id: String,
}

/// The parsed `addUser` form (application/x-www-form-urlencoded).
#[derive(Debug, Deserialize)]
pub struct AddUser {
    /// The Spotify username being added.
    #[serde(rename = "userName")]
    pub user_name: String,
    /// Base64 encrypted credentials blob.
    pub blob: String,
    /// Base64 sender DH public key.
    #[serde(rename = "clientKey")]
    pub client_key: String,
}

/// Render the `getInfo` JSON response, embedding our DH public key.
#[must_use]
pub fn get_info(info: &DeviceInfo, keys: &DhKeys, active_user: &str) -> String {
    let public_key = base64::engine::general_purpose::STANDARD.encode(keys.public_key());
    serde_json::json!({
        "status": 101,
        "statusString": "OK",
        "spotifyError": 0,
        "version": "2.7.1",
        "deviceID": info.device_id,
        "deviceType": "SPEAKER",
        "remoteName": info.remote_name,
        "publicKey": public_key,
        "brandDisplayName": "castaway",
        "modelDisplayName": "castaway",
        "libraryVersion": "0.1.0",
        "resolverVersion": "1",
        "groupStatus": "NONE",
        "tokenType": "default",
        "clientID": "",
        "productID": 0,
        "scope": "streaming",
        "availability": "",
        "accountReq": "PREMIUM",
        "activeUser": active_user,
    })
    .to_string()
}

/// The decrypted credentials produced by `addUser`.
pub struct DecryptedCredentials {
    /// The Spotify username.
    pub user_name: String,
    /// The decrypted inner blob (further parsed into an auth blob by the AP-login step,
    /// which is deferred).
    pub blob: Vec<u8>,
}

/// Process an `addUser` request: derive the shared secret and decrypt the blob.
///
/// # Errors
/// [`SpotifyError`] on missing/invalid base64 fields or a crypto failure.
pub fn add_user(req: &AddUser, keys: &DhKeys) -> Result<DecryptedCredentials, SpotifyError> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let client_key = b64
        .decode(req.client_key.as_bytes())
        .map_err(|_| SpotifyError::Base64("clientKey"))?;
    let blob = b64
        .decode(req.blob.as_bytes())
        .map_err(|_| SpotifyError::Base64("blob"))?;
    let shared = keys.shared_secret(&client_key);
    let decrypted = crate::crypto::decrypt_blob(&blob, &shared)?;
    Ok(DecryptedCredentials {
        user_name: req.user_name.clone(),
        blob: decrypted,
    })
}

/// Build the `addUser` success acknowledgement JSON.
#[must_use]
pub fn add_user_ok() -> String {
    serde_json::json!({ "status": 101, "statusString": "OK", "spotifyError": 0 }).to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::crypto::encrypt_blob;

    #[test]
    fn get_info_embeds_public_key_and_name() {
        let keys = DhKeys::from_private_bytes(&[5u8; 95]);
        let info = DeviceInfo {
            remote_name: "Hackerspace TV".into(),
            device_id: "deadbeef".into(),
        };
        let json = get_info(&info, &keys, "");
        assert!(json.contains("\"remoteName\":\"Hackerspace TV\""));
        assert!(json.contains("\"publicKey\":\""));
        assert!(json.contains("\"accountReq\":\"PREMIUM\""));
    }

    #[test]
    fn add_user_decrypts_blob_from_a_peer() {
        let b64 = base64::engine::general_purpose::STANDARD;
        // Simulate a sender: its own keypair, shared secret with our public key.
        let ours = DhKeys::from_private_bytes(&[2u8; 95]);
        let sender = DhKeys::from_private_bytes(&[8u8; 95]);
        let shared = sender.shared_secret(&ours.public_key());
        let blob = encrypt_blob(b"the-credentials", &shared, &[3u8; 16]).unwrap();

        let req = AddUser {
            user_name: "alice".into(),
            blob: b64.encode(&blob),
            client_key: b64.encode(sender.public_key()),
        };
        let creds = add_user(&req, &ours).unwrap();
        assert_eq!(creds.user_name, "alice");
        assert_eq!(creds.blob, b"the-credentials");
    }

    #[test]
    fn add_user_rejects_bad_base64() {
        let keys = DhKeys::from_private_bytes(&[1u8; 95]);
        let req = AddUser {
            user_name: "x".into(),
            blob: "!!!not-base64!!!".into(),
            client_key: "also bad".into(),
        };
        assert!(add_user(&req, &keys).is_err());
    }
}
