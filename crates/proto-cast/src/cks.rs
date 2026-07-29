//! Serving a connection from a [`cast_cks`] credential.
//!
//! The difference from [`crate::auth::CastAuthResponder`] is not the signing — it
//! is that nothing is signed at all. The signature is precomputed, over a peer
//! certificate the credential also carries, so the TLS certificate is not ours to
//! choose: presenting anything else means the sender verifies the signature
//! against the wrong message and rejects a receiver that handshook cleanly.
//!
//! [`CksIdentity`] is therefore the thing that hands out both, from the same
//! credential, in one call.

use std::sync::{Arc, Mutex};

use cast_cks::{CastCredential, CksProvider, Window};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use tracing::debug;

use crate::auth::{auth_response, requested_hash};
use crate::error::CastError;
use crate::proto::{AuthChallenge, AuthResponse};
use crate::session::DeviceAuthResponder;

/// A TLS identity and device-auth responder driven by a CKS credential.
pub struct CksIdentity {
    provider: Arc<CksProvider>,
    /// The rustls config for the credential in force.
    ///
    /// Memoised per window rather than rebuilt per connection: the credential only
    /// changes every two days, and building a `ServerConfig` parses a certificate
    /// and a key.
    cached: Mutex<Option<Cached>>,
}

struct Cached {
    window: Window,
    config: Arc<rustls::ServerConfig>,
}

impl CksIdentity {
    /// Serve connections from `provider`.
    #[must_use]
    pub fn new(provider: Arc<CksProvider>) -> Self {
        Self {
            provider,
            cached: Mutex::new(None),
        }
    }

    /// The credential currently in force.
    #[must_use]
    pub fn credential(&self) -> Arc<CastCredential> {
        self.provider.current()
    }

    /// The acceptor and responder for one connection, from one credential.
    ///
    /// # Errors
    /// [`CastError::Tls`] if the credential's certificate or key is not usable.
    pub(crate) fn for_connection(
        &self,
    ) -> Result<(TlsAcceptor, Box<dyn DeviceAuthResponder>), CastError> {
        let credential = self.provider.current();
        let config = self.config_for(&credential)?;
        Ok((
            TlsAcceptor::from(config),
            Box::new(CksAuthResponder::new(Arc::clone(&credential))),
        ))
    }

    /// The rustls config presenting `credential`'s peer certificate.
    fn config_for(
        &self,
        credential: &CastCredential,
    ) -> Result<Arc<rustls::ServerConfig>, CastError> {
        let mut cached = match self.cached.lock() {
            Ok(guard) => guard,
            // Only a panic under the lock poisons it, and everything under it is
            // infallible cloning. Recover rather than fail the connection.
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(hit) = cached.as_ref() {
            if hit.window == credential.window() {
                return Ok(Arc::clone(&hit.config));
            }
        }

        let (cert_der, key_der) = credential.tls_identity();
        let key = PrivateKeyDer::try_from(key_der.to_vec())
            .map_err(|e| CastError::Tls(format!("CKS peer key: {e}")))?;
        // Name the provider rather than taking the process-default path, which
        // panics when no default is installed (ground rule 7).
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| CastError::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der.to_vec())], key)
        .map_err(|e| CastError::Tls(format!("CKS peer certificate: {e}")))?;

        let config = Arc::new(config);
        debug!(
            origin = %credential.origin(),
            window_end = credential.window().end_unix(),
            "built the Cast TLS config from the CKS credential"
        );
        *cached = Some(Cached {
            window: credential.window(),
            config: Arc::clone(&config),
        });
        Ok(config)
    }
}

/// Answers a challenge with a credential's precomputed signature.
///
/// The counterpart to [`crate::auth::CastAuthResponder`] for material we did not
/// sign. Public so a credential can be exercised without standing up a provider —
/// the device-auth vectors do exactly that, at a fixed clock, from the static
/// table.
pub struct CksAuthResponder {
    credential: Arc<CastCredential>,
}

impl CksAuthResponder {
    /// Answer challenges from `credential`.
    ///
    /// The caller is responsible for presenting `credential`'s peer certificate in
    /// TLS; [`CksIdentity`] is the thing that guarantees it.
    #[must_use]
    pub fn new(credential: Arc<CastCredential>) -> Self {
        Self { credential }
    }
}

impl DeviceAuthResponder for CksAuthResponder {
    fn respond(&self, challenge: &AuthChallenge) -> Result<AuthResponse, CastError> {
        // No signing, and deliberately no use of `challenge.sender_nonce`: the
        // signature was computed over the peer certificate alone. `signed_auth`
        // returns `NonceEcho::Empty`, and `auth_response` turns that into an
        // absent `sender_nonce`, which is what makes the replay verify.
        Ok(auth_response(
            self.credential.signed_auth(requested_hash(challenge)),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::proto::HashAlgorithm;

    async fn identity() -> CksIdentity {
        let provider = CksProvider::resolve(cast_cks::CksConfig {
            network: false,
            cache_path: None,
            ..cast_cks::CksConfig::default()
        })
        .await
        .unwrap();
        CksIdentity::new(Arc::new(provider))
    }

    fn challenge(hash: Option<HashAlgorithm>) -> AuthChallenge {
        AuthChallenge {
            signature_algorithm: None,
            sender_nonce: Some(vec![0xAB; 16]),
            hash_algorithm: hash.map(|h| h as i32),
        }
    }

    /// The property the whole replay depends on. A sender that receives its own
    /// nonce back rebuilds `nonce || cert` and rejects the signature.
    #[tokio::test]
    async fn the_response_does_not_echo_the_senders_nonce() {
        let identity = identity().await;
        let responder = CksAuthResponder::new(identity.credential());
        let response = responder
            .respond(&challenge(Some(HashAlgorithm::Sha256)))
            .unwrap();
        assert_eq!(
            response.sender_nonce, None,
            "a replayed signature covers the certificate alone"
        );
    }

    /// The signature served must be the one for the certificate we present, not
    /// merely a well-formed one.
    #[tokio::test]
    async fn the_signature_matches_the_certificate_the_acceptor_presents() {
        let identity = identity().await;
        let credential = identity.credential();
        let responder = CksAuthResponder::new(Arc::clone(&credential));
        let response = responder
            .respond(&challenge(Some(HashAlgorithm::Sha256)))
            .unwrap();

        let (tls_cert, _) = credential.tls_identity();
        assert_eq!(
            response.signature,
            credential.signature(cast_cks::HashAlgo::Sha256)
        );
        assert_eq!(tls_cert, credential.peer_cert_der());
        assert_eq!(
            response.client_auth_certificate,
            credential.device_cert_der()
        );
        assert_eq!(response.intermediate_certificate.len(), 1);
    }

    /// A challenge with no `hash_algorithm` means SHA-1 in proto2, and the
    /// credential carries a signature for it.
    #[tokio::test]
    async fn an_unspecified_hash_is_answered_with_sha1() {
        let identity = identity().await;
        let responder = CksAuthResponder::new(identity.credential());
        let response = responder.respond(&challenge(None)).unwrap();
        assert_eq!(response.hash_algorithm, Some(HashAlgorithm::Sha1 as i32));
        assert_eq!(
            response.signature,
            identity.credential().signature(cast_cks::HashAlgo::Sha1)
        );
    }

    /// The config is built once per window, not once per connection.
    #[tokio::test]
    async fn the_tls_config_is_memoised_within_a_window() {
        let identity = identity().await;
        let credential = identity.credential();
        let a = identity.config_for(&credential).unwrap();
        let b = identity.config_for(&credential).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// The peer certificate and key have to be a usable rustls pair — a mismatch
    /// here would only show up as a handshake failure at runtime.
    #[tokio::test]
    async fn the_credential_forms_a_usable_tls_config() {
        let identity = identity().await;
        assert!(identity.for_connection().is_ok());
    }
}
