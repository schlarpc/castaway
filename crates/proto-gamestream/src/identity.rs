//! The client identity: a self-signed RSA-2048 certificate and its key.
//!
//! In GameStream the client certificate *is* the credential. Pairing teaches the host
//! to trust exactly this certificate (Sunshine stores it in its state file), and every
//! later HTTPS request authenticates by presenting it as the TLS client certificate —
//! there is no account and no password. Lose the key, pair again.
//!
//! Moonlight generates a 2048-bit RSA key and a SHA-256 self-signed certificate with
//! CN `NVIDIA GameStream Client`; hosts have only ever seen that shape, so this module
//! reproduces it rather than exercising Sunshine's tolerance for anything else. ring
//! (rcgen's backend here) cannot *generate* RSA keys, so the `rsa` crate generates and
//! ring signs the certificate over it.

use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::RsaPrivateKey;

use crate::error::GameStreamError;

/// The CN moonlight-qt writes into its client certificate; kept byte-identical.
const CLIENT_CN: &str = "NVIDIA GameStream Client";

/// How long a generated identity stays valid. Moonlight uses 20 years; a hackerspace
/// panel that outlives this deserves the re-pair it will be asked for.
const VALIDITY_YEARS: i32 = 20;

/// A generated or loaded client identity.
///
/// Constructed once and persisted; the pairing handshake, the TLS client
/// authentication, and the challenge signatures all use this one key.
pub struct ClientIdentity {
    key: RsaPrivateKey,
    key_pkcs8_pem: String,
    cert_pem: String,
    cert_der: Vec<u8>,
}

impl ClientIdentity {
    /// Generate a fresh identity. Slow (RSA keygen in pure Rust, ~seconds in debug
    /// builds) — call once and persist, never per session.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] if key generation or certificate signing fails.
    pub fn generate() -> Result<Self, GameStreamError> {
        let mut rng = rand::rngs::OsRng;
        let key = RsaPrivateKey::new(&mut rng, 2048)
            .map_err(|e| GameStreamError::Identity(e.to_string()))?;
        let key_pkcs8_pem = key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .map_err(|e| GameStreamError::Identity(e.to_string()))?
            .to_string();
        Self::from_key_pem(&key_pkcs8_pem)
    }

    /// Rebuild the identity from a persisted PKCS#8 PEM key, re-deriving the
    /// certificate deterministically is *not* possible (serial, validity), so the
    /// certificate is signed fresh here; use [`Self::from_pem`] when both halves were
    /// persisted — which is the only way the cert Sunshine pinned keeps matching.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] on an unparseable key or signing failure.
    pub fn from_key_pem(key_pkcs8_pem: &str) -> Result<Self, GameStreamError> {
        let key = RsaPrivateKey::from_pkcs8_pem(key_pkcs8_pem)
            .map_err(|e| GameStreamError::Identity(e.to_string()))?;
        let key_der = key
            .to_pkcs8_der()
            .map_err(|e| GameStreamError::Identity(e.to_string()))?;
        let rcgen_key = rcgen::KeyPair::try_from(key_der.as_bytes())
            .map_err(|e| GameStreamError::Identity(e.to_string()))?;

        let mut params = rcgen::CertificateParams::default();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, CLIENT_CN);
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now.replace_year(now.year() + VALIDITY_YEARS).map_err(
            // Only fails on a date that does not exist in the target year (Feb 29).
            |e| GameStreamError::Identity(e.to_string()),
        )?;
        let cert = params
            .self_signed(&rcgen_key)
            .map_err(|e| GameStreamError::Identity(e.to_string()))?;

        let cert_der = cert.der().to_vec();
        Ok(Self {
            key,
            key_pkcs8_pem: key_pkcs8_pem.to_string(),
            cert_pem: crate::pairing::der_to_pem("CERTIFICATE", &cert_der),
            cert_der,
        })
    }

    /// Load a fully persisted identity: certificate PEM + PKCS#8 key PEM.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] if either half fails to parse.
    pub fn from_pem(cert_pem: &str, key_pkcs8_pem: &str) -> Result<Self, GameStreamError> {
        let key = RsaPrivateKey::from_pkcs8_pem(key_pkcs8_pem)
            .map_err(|e| GameStreamError::Identity(e.to_string()))?;
        let cert_der = crate::pairing::pem_to_der(cert_pem)
            .ok_or_else(|| GameStreamError::Identity("certificate PEM is malformed".into()))?;
        Ok(Self {
            key,
            key_pkcs8_pem: key_pkcs8_pem.to_string(),
            cert_pem: cert_pem.to_string(),
            cert_der,
        })
    }

    /// The RSA private key, for pairing-challenge signatures.
    #[must_use]
    pub fn key(&self) -> &RsaPrivateKey {
        &self.key
    }

    /// PKCS#8 PEM of the key, for persistence and rustls client auth.
    #[must_use]
    pub fn key_pem(&self) -> &str {
        &self.key_pkcs8_pem
    }

    /// Certificate PEM — what `/pair` sends (hex-encoded) and what persists.
    #[must_use]
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Certificate DER, for rustls and for the signature-bytes hash in pairing.
    #[must_use]
    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No key material in logs, ever.
        f.debug_struct("ClientIdentity")
            .field("cert_len", &self.cert_der.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use rsa::traits::PublicKeyParts;

    use super::*;

    #[test]
    fn generates_a_2048_bit_rsa_identity_with_the_moonlight_cn() {
        let id = ClientIdentity::generate().unwrap();
        assert_eq!(id.key().size() * 8, 2048);
        // The CN must appear in the cert DER as a UTF8String/PrintableString body.
        let der = id.cert_der();
        let needle = CLIENT_CN.as_bytes();
        assert!(
            der.windows(needle.len()).any(|w| w == needle),
            "generated certificate does not carry the Moonlight client CN — \
             GFE-era hosts have never seen any other subject"
        );
    }

    #[test]
    fn round_trips_through_pem_persistence() {
        let id = ClientIdentity::generate().unwrap();
        let restored = ClientIdentity::from_pem(id.cert_pem(), id.key_pem()).unwrap();
        assert_eq!(restored.cert_der(), id.cert_der());
        assert_eq!(restored.key(), id.key());
    }
}
