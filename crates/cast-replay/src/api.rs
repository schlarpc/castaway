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
//!     x-api-key: <API_KEY>
//!
//!     ts  = decimal seconds
//!     sig = lowercase_hex(MD5(SIG_SECRET || ts))
//! ```
//!
//! `API_KEY` and `SIG_SECRET` are SoftMedia's, and are not written down here — see
//! [`CksCredentials`]. PROVENANCE §2 records their values and how they were first
//! recovered; this crate obtains them from the build-time carve.
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

/// SoftMedia's four backend constants: the `x-api-key` value, the secret `sig` is
/// derived from (prefixed to `ts`, not keyed over it), and the fixed AES-128-CTR key
/// and counter block every response field is encoded under.
///
/// **Not literals here.** These belong to someone else, so they are carved out of
/// `libAirReceiver.so` at build time (`nix/airreceiver-carve.nix`) and reach the crate
/// through `build.rs`. A build without them cannot talk to the CKS backend at all —
/// [`request`] returns [`ReplayError::NoCksCredentials`] — but the offline table is
/// unaffected, so such a build still authenticates, it just cannot refresh.
///
/// All four in one struct on purpose. Asking the backend and reading its answer are
/// halves of one capability: credentials that cannot decode a response would buy
/// nothing, and a cipher with no credentials would never see one. Keeping them
/// together means a build has the live path or does not, with no third state for a
/// caller to get wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CksCredentials {
    /// The `x-api-key` value.
    pub api_key: &'static str,
    /// The constant `sig` is derived from.
    pub sig_secret: &'static str,
    /// The fixed AES-128-CTR key every response field is encoded under.
    pub field_key: &'static [u8; 16],
    /// The fixed counter block. Reused for every field of every response, which is
    /// what makes this an encoding rather than encryption.
    pub field_iv: &'static [u8; 16],
}

include!(concat!(env!("OUT_DIR"), "/cks_credentials.rs"));

impl CksCredentials {
    /// The credentials this build was given, if any.
    #[must_use]
    pub const fn provisioned() -> Option<Self> {
        CKS_CREDENTIALS
    }
}

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
///
/// # Errors
/// [`ReplayError::NoCksCredentials`] on a build that was not given the carved
/// backend constants.
pub fn request(ts: i64) -> Result<CksRequest, ReplayError> {
    let c = CksCredentials::provisioned().ok_or(ReplayError::NoCksCredentials)?;
    Ok(CksRequest {
        url: format!("{URL}?ts={ts}&sig={}", sig_with(ts, c)),
        user_agent: USER_AGENT,
        api_key: (API_KEY_HEADER, c.api_key),
    })
}

/// The `sig` query parameter for `ts`: `MD5(SECRET || ts)`, lowercase hex.
///
/// # Errors
/// As [`request`].
pub fn sig(ts: i64) -> Result<String, ReplayError> {
    Ok(sig_with(
        ts,
        CksCredentials::provisioned().ok_or(ReplayError::NoCksCredentials)?,
    ))
}

/// `sig` under explicit credentials, so the shape is testable without them.
#[must_use]
pub fn sig_with(ts: i64, c: CksCredentials) -> String {
    let mut h = Md5::new();
    h.update(c.sig_secret.as_bytes());
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
/// [`ReplayError::NoCksCredentials`] on a build without the carved field cipher, then
/// [`ReplayError::Response`] if the body is not JSON, a required key is absent, a
/// value is not base64, or a signature is not 256 bytes.
pub fn decode_response(body: &[u8]) -> Result<CksResponse, ReplayError> {
    decode_response_with(
        body,
        CksCredentials::provisioned().ok_or(ReplayError::NoCksCredentials)?,
    )
}

/// Decode a response body under explicit credentials, so the codec is testable
/// without SoftMedia's. Mirrors [`sig_with`].
///
/// # Errors
/// As [`decode_response`], less the credential check.
pub fn decode_response_with(body: &[u8], c: CksCredentials) -> Result<CksResponse, ReplayError> {
    let raw: RawResponse = serde_json::from_slice(body)
        .map_err(|e| ReplayError::Response(format!("body is not the expected JSON object: {e}")))?;

    let peer_key = unwrap_field(&raw.pri, "pri", c)?;
    let response = CksResponse {
        ica: unwrap_field(&raw.ica, "ica", c)?,
        device_cert: unwrap_field(&raw.cpu, "cpu", c)?,
        peer_cert: unwrap_field(&raw.r#pub, "pub", c)?,
        peer_key_pem: String::from_utf8(peer_key)
            .map_err(|e| ReplayError::Response(format!("pri is not text: {e}")))?,
        sha1: unwrap_field(&raw.sha1, "sha1", c)?,
        sha256: unwrap_field(&raw.sha256, "sha256", c)?,
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
fn unwrap_field(value: &str, name: &str, c: CksCredentials) -> Result<Vec<u8>, ReplayError> {
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .map_err(|e| ReplayError::Response(format!("{name} is not base64: {e}")))?;
    FieldCipher::new(c.field_key.into(), c.field_iv.into()).apply_keystream(&mut bytes);
    Ok(bytes)
}

/// The inverse of [`unwrap_field`]. Only used to build test fixtures — the
/// keystream is fixed, so encoding and decoding are the same operation.
#[cfg(test)]
fn wrap_field(plain: &[u8], c: CksCredentials) -> String {
    let mut bytes = plain.to_vec();
    FieldCipher::new(c.field_key.into(), c.field_iv.into()).apply_keystream(&mut bytes);
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Credentials of our own, so the derivation and the codec are testable without
    /// SoftMedia's. The expected digest is `MD5("castaway-test-secret" + "1700000000")`.
    ///
    /// The field cipher here is ours too, which is what lets the response tests run on
    /// an unprovisioned build: what they assert is that base64-over-CTR round-trips and
    /// that malformed bodies are rejected, and neither depends on *which* key it is.
    const TEST_CREDS: CksCredentials = CksCredentials {
        api_key: "00112233445566778899aabbccddeeff",
        sig_secret: "castaway-test-secret",
        field_key: b"castaway-fieldk1",
        field_iv: b"castaway-fieldiv",
    };

    /// The shape of the derivation — secret first, then the decimal timestamp,
    /// lowercase hex — which is the part that is ours to get wrong. Runs on every
    /// build, because it does not need the carved constants.
    #[test]
    fn sig_is_md5_of_the_secret_then_the_timestamp() {
        let expect = {
            let mut h = Md5::new();
            h.update(b"castaway-test-secret");
            h.update(b"1700000000");
            h.finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(sig_with(1_700_000_000, TEST_CREDS), expect);
        assert_eq!(expect.len(), 32);
        assert!(expect
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    /// The reference vector, confirmed against the live endpoint (HTTP 200 on the
    /// first try). Needs the carved secret, so it only runs on a provisioned build.
    #[test]
    #[cfg_attr(not(cks_credentials), ignore = "needs the carved CKS credentials")]
    fn sig_matches_the_reference_derivation() {
        assert_eq!(
            sig(1_700_000_000).unwrap(),
            "6f12c3bf54f1ddd603e7d0a4478c378b"
        );
        assert_eq!(
            request(1_700_000_000).unwrap().url,
            "https://cast.remotetogo.com/api/v1/cks\
             ?ts=1700000000&sig=6f12c3bf54f1ddd603e7d0a4478c378b"
        );
    }

    #[test]
    #[cfg_attr(not(cks_credentials), ignore = "needs the carved CKS credentials")]
    fn the_request_carries_the_headers_the_backend_expects() {
        let r = request(1).unwrap();
        let c = CksCredentials::provisioned().unwrap();
        assert_eq!(r.user_agent, "AirReceiver/1.0.0 CrKey/1.0");
        // Compared against the carved value rather than a literal: writing the key
        // down here would undo the point of carving it.
        assert_eq!(r.api_key, (API_KEY_HEADER, c.api_key));
        assert_eq!(c.api_key.len(), 32);
    }

    /// Without the carve the live path is unavailable, and says so.
    #[test]
    #[cfg_attr(cks_credentials, ignore = "this build has the credentials")]
    fn an_unprovisioned_build_refuses_to_build_a_request() {
        assert!(matches!(request(1), Err(ReplayError::NoCksCredentials)));
        assert!(matches!(sig(1), Err(ReplayError::NoCksCredentials)));
    }

    /// Without the carve the response codec is unavailable too, and says so — the
    /// live path is one capability, not two.
    #[test]
    #[cfg_attr(cks_credentials, ignore = "this build has the credentials")]
    fn an_unprovisioned_build_refuses_to_decode_a_response() {
        assert!(matches!(
            decode_response(&body(256)),
            Err(ReplayError::NoCksCredentials)
        ));
    }

    fn body(sha1_len: usize) -> Vec<u8> {
        format!(
            r#"{{"ica":"{}","cpu":"{}","pub":"{}","pri":"{}",
                 "sha1":"{}","sha256":"{}","now":1785339945,"nb":1,"na":2}}"#,
            wrap_field(b"ica-der", TEST_CREDS),
            wrap_field(b"cpu-der", TEST_CREDS),
            wrap_field(b"pub-der", TEST_CREDS),
            wrap_field(b"-----BEGIN PRIVATE KEY-----\n", TEST_CREDS),
            wrap_field(&vec![0xAA; sha1_len], TEST_CREDS),
            wrap_field(&vec![0xBB; 256], TEST_CREDS),
        )
        .into_bytes()
    }

    #[test]
    fn decodes_every_field_through_the_fixed_keystream() {
        let r = decode_response_with(&body(256), TEST_CREDS).unwrap();
        assert_eq!(r.ica, b"ica-der");
        assert_eq!(r.device_cert, b"cpu-der");
        assert_eq!(r.peer_cert, b"pub-der");
        assert!(r.peer_key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        assert_eq!(r.sha1, vec![0xAA; 256]);
        assert_eq!(r.sha256, vec![0xBB; 256]);
        assert_eq!(r.now, 1_785_339_945);
    }

    /// One counter block for every field of every response, so the keystream a field
    /// gets depends only on its length. Asserting it here means the property survives
    /// the constants moving out of this file — a carve that returned a per-call IV
    /// would still decode a round-trip, and would still be wrong.
    #[test]
    fn every_field_shares_one_keystream() {
        let a = wrap_field(b"the same sixteen", TEST_CREDS);
        let b = wrap_field(b"the same sixteen", TEST_CREDS);
        assert_eq!(a, b);
        // And a prefix relationship, which only holds if the counter starts over.
        let long = wrap_field(b"the same sixteen bytes, then more", TEST_CREDS);
        let short = wrap_field(b"the same sixteen", TEST_CREDS);
        let (long, short) = (
            base64::engine::general_purpose::STANDARD
                .decode(long)
                .unwrap(),
            base64::engine::general_purpose::STANDARD
                .decode(short)
                .unwrap(),
        );
        assert_eq!(&long[..short.len()], &short[..]);
    }

    /// Unknown keys are ignored rather than fatal: `nb`/`na` are present in live
    /// responses and the reference client does not read them, so the backend is
    /// free to add more.
    #[test]
    fn unknown_keys_do_not_break_decoding() {
        assert!(decode_response_with(&body(256), TEST_CREDS).is_ok());
    }

    #[test]
    fn a_truncated_signature_is_rejected() {
        assert!(matches!(
            decode_response_with(&body(128), TEST_CREDS),
            Err(ReplayError::Response(_))
        ));
    }

    #[test]
    fn a_missing_key_is_rejected() {
        let body = br#"{"ica":"","cpu":"","pub":"","pri":"","sha1":""}"#;
        assert!(matches!(
            decode_response_with(body, TEST_CREDS),
            Err(ReplayError::Response(_))
        ));
    }

    #[test]
    fn a_non_json_body_is_rejected() {
        assert!(matches!(
            decode_response_with(b"<html>502 Bad Gateway</html>", TEST_CREDS),
            Err(ReplayError::Response(_))
        ));
    }
}
