//! The CKS backend wire format — request derivation and response decoding.
//!
//! Sans-I/O (ground rule 3): everything here is `bytes -> types`. [`crate::provider`]
//! owns the socket.
//!
//! The endpoint answers with the **current** 2-day window only — it is not an
//! oracle for arbitrary dates. What it is, is a source that keeps working after
//! the checked-in table runs out, for as long as the backend runs.
//!
//! ## The request
//!
//! ```text
//! GET https://cast.remotetogo.com/api/v1/cks?ts=<ts>&sig=<sig>
//!     User-Agent: AirReceiver/1.0.0 CrKey/1.0
//!     x-api-key: ***REMOVED-CKS-API-KEY-PROVENANCE-S2***
//!
//!     ts  = decimal seconds
//!     sig = lowercase_hex(MD5(SECRET || ts))
//! ```
//!
//! `sig` is a bare unkeyed digest of a constant and a timestamp, and the backend
//! does not check that the timestamp is recent — a request dated 30 days in the
//! past is answered identically. So it is a static credential wearing a
//! timestamp, not a challenge-response. (The endpoint that would have made it one
//! is present in the binary and unreferenced.)
//!
//! ## The response
//!
//! JSON. Every certificate/key/signature value is base64 over AES-128-CTR under a
//! fixed key *and a fixed counter block* — the same keystream for every field of
//! every response, which makes it an encoding rather than encryption. Decoding it
//! is [`decode_response`].

use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use base64::Engine as _;
use md5::{Digest as _, Md5};
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _};
use rsa::RsaPrivateKey;
use serde::Deserialize;

use crate::window::Window;
use crate::ReplayError;

/// The endpoint, as a format string over `ts` and `sig`.
const URL: &str = "https://cast.remotetogo.com/api/v1/cks";

/// The User-Agent the reference client sends. Sent verbatim: the backend is a
/// third party's, and looking like an unfamiliar client is the one thing likeliest
/// to get this path turned off.
pub const USER_AGENT: &str = "AirReceiver/1.0.0 CrKey/1.0";

/// The API key header name.
pub const API_KEY_HEADER: &str = "x-api-key";

/// The API key value.
const API_KEY: &str = "***REMOVED-CKS-API-KEY-PROVENANCE-S2***";

/// The constant `sig` is derived from. Prefixed to `ts`, not keyed over it.
const SIG_SECRET: &str = "***REMOVED-CKS-SIG-SECRET-PROVENANCE-S1***";

/// The fixed AES-128-CTR key every response field is encoded under.
const FIELD_KEY: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // ***REMOVED: PROVENANCE S3***
];

/// The fixed counter block. Reused for every field of every response.
const FIELD_IV: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // ***REMOVED: PROVENANCE S4***
];

type FieldCipher = ctr::Ctr128BE<aes::Aes128>;

/// A request to the CKS backend: everything the transport needs, and nothing it
/// has to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CksRequest {
    /// The full URL, query included.
    pub url: String,
    /// The `User-Agent` to send.
    pub user_agent: &'static str,
    /// The API key header name and value.
    pub api_key: (&'static str, &'static str),
}

/// Derive the request for timestamp `ts`.
#[must_use]
pub fn request(ts: i64) -> CksRequest {
    CksRequest {
        url: format!("{URL}?ts={ts}&sig={}", sig(ts)),
        user_agent: USER_AGENT,
        api_key: (API_KEY_HEADER, API_KEY),
    }
}

/// The `sig` query parameter for `ts`: `MD5(SECRET || ts)`, lowercase hex.
#[must_use]
pub fn sig(ts: i64) -> String {
    let mut h = Md5::new();
    h.update(SIG_SECRET.as_bytes());
    h.update(ts.to_string().as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The response body, as it arrives.
#[derive(Debug, Deserialize)]
struct RawResponse {
    ica: String,
    cpu: String,
    r#pub: String,
    pri: String,
    sha1: String,
    sha256: String,
    now: i64,
}

/// A decoded CKS response.
#[derive(Debug, Clone)]
pub struct CksResponse {
    /// Intermediate certificate, DER.
    pub ica: Vec<u8>,
    /// The Google device ("client auth") certificate, DER.
    pub device_cert: Vec<u8>,
    /// The peer certificate, DER. Already carries the current window's validity —
    /// unlike the static table's, it needs no re-issuing.
    pub peer_cert: Vec<u8>,
    /// The peer RSA private key, PEM. The one field the backend sends as PEM.
    pub peer_key_pem: String,
    /// The precomputed SHA-1 signature.
    pub sha1: Vec<u8>,
    /// The precomputed SHA-256 signature.
    pub sha256: Vec<u8>,
    /// The backend's clock. Used in preference to the local one for window
    /// arithmetic, so a panel with a wrong clock still picks the right window.
    pub now: i64,
}

/// Decode a response body.
///
/// # Errors
/// [`ReplayError::Response`] if the body is not JSON, a required key is absent, a
/// value is not base64, or a signature is not 256 bytes.
pub fn decode_response(body: &[u8]) -> Result<CksResponse, ReplayError> {
    let raw: RawResponse = serde_json::from_slice(body)
        .map_err(|e| ReplayError::Response(format!("body is not the expected JSON object: {e}")))?;

    let peer_key = unwrap_field(&raw.pri, "pri")?;
    let response = CksResponse {
        ica: unwrap_field(&raw.ica, "ica")?,
        device_cert: unwrap_field(&raw.cpu, "cpu")?,
        peer_cert: unwrap_field(&raw.r#pub, "pub")?,
        peer_key_pem: String::from_utf8(peer_key)
            .map_err(|e| ReplayError::Response(format!("pri is not text: {e}")))?,
        sha1: unwrap_field(&raw.sha1, "sha1")?,
        sha256: unwrap_field(&raw.sha256, "sha256")?,
        now: raw.now,
    };
    for (what, sig) in [("sha1", &response.sha1), ("sha256", &response.sha256)] {
        if sig.len() != 256 {
            return Err(ReplayError::Response(format!(
                "{what} is {} bytes; an RSA-2048 signature is 256",
                sig.len()
            )));
        }
    }
    Ok(response)
}

impl CksResponse {
    /// Turn a decoded response into a credential.
    ///
    /// The window comes from the peer certificate's own validity rather than from
    /// the response's `nb`/`na`: the certificate is what the sender bounds, and
    /// the reference client reads the window the same way. (Live responses carry
    /// `nb`/`na` too, and the client ignores them.)
    ///
    /// # Errors
    /// [`ReplayError::InvalidKey`] if `pri` is not a usable RSA key,
    /// [`ReplayError::Response`] if the peer certificate cannot be parsed or its
    /// validity is not ordered.
    pub fn into_credential(self) -> Result<crate::CastCredential, ReplayError> {
        // The backend sends PKCS#8 in practice; the reference client feeds `pri`
        // to `PEM_read_bio_RSAPrivateKey`, which takes either, so accept both.
        let key = RsaPrivateKey::from_pkcs8_pem(&self.peer_key_pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&self.peer_key_pem))
            .map_err(|e| ReplayError::InvalidKey(format!("CKS peer key: {e}")))?;
        let key_pkcs8 = key
            .to_pkcs8_der()
            .map_err(|e| ReplayError::InvalidKey(format!("re-encoding the CKS peer key: {e}")))?
            .as_bytes()
            .to_vec();

        let (_, cert) = x509_parser::parse_x509_certificate(&self.peer_cert)
            .map_err(|e| ReplayError::Response(format!("peer certificate is not X.509: {e}")))?;
        let window = Window::new(
            cert.validity().not_before.timestamp(),
            cert.validity().not_after.timestamp(),
        )?;

        crate::CastCredential::new(
            self.device_cert,
            vec![self.ica],
            self.peer_cert,
            key_pkcs8,
            self.sha1,
            self.sha256,
            window,
            crate::CredentialOrigin::Network,
        )
    }
}

/// base64, then AES-128-CTR.
fn unwrap_field(value: &str, name: &str) -> Result<Vec<u8>, ReplayError> {
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .map_err(|e| ReplayError::Response(format!("{name} is not base64: {e}")))?;
    FieldCipher::new(&FIELD_KEY.into(), &FIELD_IV.into()).apply_keystream(&mut bytes);
    Ok(bytes)
}

/// The inverse of [`unwrap_field`]. Only used to build test fixtures — the
/// keystream is fixed, so encoding and decoding are the same operation.
#[cfg(test)]
fn wrap_field(plain: &[u8]) -> String {
    let mut bytes = plain.to_vec();
    FieldCipher::new(&FIELD_KEY.into(), &FIELD_IV.into()).apply_keystream(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Both vectors are from the reference client, one of which was confirmed
    /// against the live endpoint (HTTP 200 on the first try).
    #[test]
    fn sig_matches_the_reference_derivation() {
        assert_eq!(sig(1_700_000_000), "6f12c3bf54f1ddd603e7d0a4478c378b");
        assert_eq!(
            request(1_700_000_000).url,
            "https://cast.remotetogo.com/api/v1/cks\
             ?ts=1700000000&sig=6f12c3bf54f1ddd603e7d0a4478c378b"
        );
    }

    #[test]
    fn the_request_carries_the_headers_the_backend_expects() {
        let r = request(1);
        assert_eq!(r.user_agent, "AirReceiver/1.0.0 CrKey/1.0");
        assert_eq!(r.api_key, ("x-api-key", "***REMOVED-CKS-API-KEY-PROVENANCE-S2***"));
    }

    fn body(sha1_len: usize) -> Vec<u8> {
        format!(
            r#"{{"ica":"{}","cpu":"{}","pub":"{}","pri":"{}",
                 "sha1":"{}","sha256":"{}","now":1785339945,"nb":1,"na":2}}"#,
            wrap_field(b"ica-der"),
            wrap_field(b"cpu-der"),
            wrap_field(b"pub-der"),
            wrap_field(b"-----BEGIN PRIVATE KEY-----\n"),
            wrap_field(&vec![0xAA; sha1_len]),
            wrap_field(&vec![0xBB; 256]),
        )
        .into_bytes()
    }

    #[test]
    fn decodes_every_field_through_the_fixed_keystream() {
        let r = decode_response(&body(256)).unwrap();
        assert_eq!(r.ica, b"ica-der");
        assert_eq!(r.device_cert, b"cpu-der");
        assert_eq!(r.peer_cert, b"pub-der");
        assert!(r.peer_key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(r.sha1, vec![0xAA; 256]);
        assert_eq!(r.sha256, vec![0xBB; 256]);
        assert_eq!(r.now, 1_785_339_945);
    }

    /// Unknown keys are ignored rather than fatal: `nb`/`na` are present in live
    /// responses and the reference client does not read them, so the backend is
    /// free to add more.
    #[test]
    fn unknown_keys_do_not_break_decoding() {
        assert!(decode_response(&body(256)).is_ok());
    }

    #[test]
    fn a_truncated_signature_is_rejected() {
        assert!(matches!(
            decode_response(&body(128)),
            Err(ReplayError::Response(_))
        ));
    }

    #[test]
    fn a_missing_key_is_rejected() {
        let body = br#"{"ica":"","cpu":"","pub":"","pri":"","sha1":""}"#;
        assert!(matches!(
            decode_response(body),
            Err(ReplayError::Response(_))
        ));
    }

    #[test]
    fn a_non_json_body_is_rejected() {
        assert!(matches!(
            decode_response(b"<html>502 Bad Gateway</html>"),
            Err(ReplayError::Response(_))
        ));
    }
}
