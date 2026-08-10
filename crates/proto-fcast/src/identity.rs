//! The receiver's protocol-v4 TLS identity: a self-signed certificate whose
//! SPKI-SHA-256 fingerprint is the trust anchor senders pin (#248).
//!
//! The fingerprint is advertised in the `fp` TXT record and carried in QR
//! connection URLs, and the sender SDK refuses to run v4 at all without one —
//! measured, not assumed: by bare IP it closes the connection before ever sending
//! a ClientHello. That makes the *key* the identity; the certificate is just its
//! carrier, mirroring the reference receiver's shape (empty DN, no SANs, a
//! 1975-4096 validity window, TLS 1.3 only, no client auth). One deliberate
//! improvement over the reference: it regenerates its key every process start, so
//! its `fp` — and every printed QR code — dies with the process. Ours persists
//! (the app hands the key bytes back for storage), so a QR code on the wall keeps
//! working across restarts.

use std::sync::Arc;

use base64::Engine as _;
use sha2::Digest as _;
use tokio_rustls::TlsAcceptor;

use crate::error::FCastError;

/// The v4 TLS identity: acceptor plus the pinned fingerprint.
pub struct V4Identity {
    acceptor: TlsAcceptor,
    fingerprint: String,
}

impl std::fmt::Debug for V4Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V4Identity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl V4Identity {
    /// Generate a fresh identity. Returns it together with the key (PKCS#8 DER)
    /// so the caller can persist it — the fingerprint is only as durable as the
    /// key is.
    ///
    /// # Errors
    /// [`FCastError::Tls`] if key or certificate generation fails.
    pub fn generate() -> Result<(Self, Vec<u8>), FCastError> {
        let key = rcgen::KeyPair::generate().map_err(tls_err)?;
        let key_der = key.serialize_der();
        Ok((Self::from_key(&key_der)?, key_der))
    }

    /// Rebuild the identity from a persisted key (PKCS#8 DER). Deterministic:
    /// same key, same fingerprint.
    ///
    /// # Errors
    /// [`FCastError::Tls`] if the key does not parse or the certificate cannot be
    /// issued.
    pub fn from_key(key_pkcs8_der: &[u8]) -> Result<Self, FCastError> {
        let key = rcgen::KeyPair::try_from(key_pkcs8_der).map_err(tls_err)?;

        // The certificate mirrors the reference receiver's: an anonymous subject
        // and an effectively-unbounded validity window. Senders never check the
        // name or the dates — the SPKI pin plus TLS 1.3's CertificateVerify is
        // the whole trust story — but an expiring certificate would still break
        // rustls's own serving of it, and there is no rotation story that keeps a
        // printed QR code alive.
        let mut params = rcgen::CertificateParams::default();
        params.not_before = rcgen::date_time_ymd(1975, 1, 1);
        params.not_after = rcgen::date_time_ymd(4096, 1, 1);
        params.distinguished_name = rcgen::DistinguishedName::new();
        let cert = params.self_signed(&key).map_err(tls_err)?;

        // fp = standard (padded) base64 of SHA-256 over the DER SPKI — the whole
        // SubjectPublicKeyInfo (algorithm + key bits), which is what the sender
        // hashes off the presented leaf.
        let spki = rcgen::PublicKeyData::subject_public_key_info(&key);
        let fingerprint =
            base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(&spki));

        let key_der = rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der())
            .map_err(|e| FCastError::Tls(e.to_string()))?;
        let config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], key_der)
                .map_err(tls_err)?;

        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            fingerprint,
        })
    }

    /// The `fp` TXT value: padded-standard base64 of SHA-256 over the SPKI.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The TLS 1.3 acceptor for upgraded connections.
    #[must_use]
    pub fn acceptor(&self) -> &TlsAcceptor {
        &self.acceptor
    }
}

fn tls_err(e: impl std::fmt::Display) -> FCastError {
    FCastError::Tls(e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Same key, same fingerprint — the property that makes a printed QR code
    /// survive a restart, and the reason the key is what gets persisted.
    #[test]
    fn the_fingerprint_is_a_function_of_the_key_alone() {
        let (identity, key_der) = V4Identity::generate().unwrap();
        let rebuilt = V4Identity::from_key(&key_der).unwrap();
        assert_eq!(identity.fingerprint(), rebuilt.fingerprint());

        let (other, _) = V4Identity::generate().unwrap();
        assert_ne!(identity.fingerprint(), other.fingerprint());
    }

    /// The fingerprint has the exact shape the sender SDK decodes: 44 chars of
    /// padded standard base64, decoding to 32 digest bytes.
    #[test]
    fn the_fingerprint_is_padded_standard_base64_of_a_sha256() {
        let (identity, _) = V4Identity::generate().unwrap();
        let fp = identity.fingerprint();
        assert_eq!(fp.len(), 44);
        assert!(fp.ends_with('='));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(fp)
            .unwrap();
        assert_eq!(decoded.len(), 32);
    }

    /// A garbage key is a typed error, not a panic.
    #[test]
    fn a_corrupt_persisted_key_is_refused() {
        assert!(matches!(
            V4Identity::from_key(&[0x30, 0x00]),
            Err(FCastError::Tls(_))
        ));
    }
}
