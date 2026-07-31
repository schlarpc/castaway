//! Serving a connection from a [`cast_replay`] credential.
//!
//! The difference from [`crate::auth::CastAuthResponder`] is not the signing — it
//! is that nothing is signed at all. The signature is precomputed, over a peer
//! certificate the credential also carries, so the TLS certificate is not ours to
//! choose: presenting anything else means the sender verifies the signature
//! against the wrong message and rejects a receiver that handshook cleanly.
//!
//! [`ReplayIdentity`] is therefore the thing that hands out both, from the same
//! credential, in one call.

use std::sync::{Arc, Mutex};

use cast_replay::{CastCredential, ReplayProvider, ServableCrl, Window};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;
use tracing::debug;

use crate::auth::{auth_response, requested_hash};
use crate::error::CastError;
use crate::proto::{AuthChallenge, AuthResponse};
use crate::session::DeviceAuthResponder;

/// A TLS identity and device-auth responder driven by a CKS credential.
pub struct ReplayIdentity {
    provider: Arc<ReplayProvider>,
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

impl ReplayIdentity {
    /// Serve connections from `provider`.
    #[must_use]
    pub fn new(provider: Arc<ReplayProvider>) -> Self {
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
        let mut responder = ReplayAuthResponder::new(Arc::clone(&credential));
        if let Some(crl) = self.servable_crl(&credential) {
            responder = responder.with_crl(&crl);
        }
        Ok((TlsAcceptor::from(config), Box::new(responder)))
    }

    /// The CRL to attach for `credential`, if one is held and does not revoke it.
    ///
    /// The check is per connection rather than per fetch because *which* chain we present
    /// is a runtime decision — `identity_order`, table coverage and whether either
    /// backend answers all move it — so "is this document safe to send" is only
    /// answerable against the credential actually in force.
    ///
    /// A CRL that names us is withheld, not served. Attaching it would hand the sender
    /// its own reason to refuse this receiver, turning a receiver that works in Chrome
    /// into one that works nowhere; withholding it leaves exactly the behaviour we had
    /// before there was a CRL at all. It is also the only notice we get that the
    /// identity has been revoked, so it is logged at `error` rather than counted.
    fn servable_crl(&self, credential: &CastCredential) -> Option<ServableCrl> {
        let crl = self.provider.current_crl()?;
        let mut chain: Vec<&[u8]> = vec![credential.device_cert_der()];
        chain.extend(credential.intermediates_der().iter().map(Vec::as_slice));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_secs()).ok())?;

        match crl.servable_for(&chain, now) {
            Ok(Ok(servable)) => Some(servable),
            Ok(Err(cast_replay::ServeRefusal::OutsideWindow)) => {
                debug!("holding back a Cast CRL that is outside its validity window");
                None
            }
            Ok(Err(refusal @ cast_replay::ServeRefusal::RevokesUs(_))) => {
                tracing::error!(
                    origin = %credential.origin(),
                    reason = %refusal,
                    "the published Cast CRL revokes the identity this receiver presents; \
                     withholding it, which keeps Chrome working and leaves Chromium-based \
                     senders refusing us. This identity needs replacing (D41)."
                );
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not evaluate the Cast CRL against our chain");
                None
            }
        }
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
pub struct ReplayAuthResponder {
    credential: Arc<CastCredential>,
    /// The CRL to attach, already checked against `credential`'s chain. `None` means
    /// either that none is held or that the one held revokes us — the two are the same
    /// decision here, and [`ReplayIdentity::servable_crl`] is where they are told apart
    /// and logged.
    crl: Option<Vec<u8>>,
}

impl ReplayAuthResponder {
    /// Answer challenges from `credential`.
    ///
    /// The caller is responsible for presenting `credential`'s peer certificate in
    /// TLS; [`ReplayIdentity`] is the thing that guarantees it.
    #[must_use]
    pub fn new(credential: Arc<CastCredential>) -> Self {
        Self {
            credential,
            crl: None,
        }
    }

    /// Attach a CRL that has already been cleared against this credential's chain.
    ///
    /// Takes a [`ServableCrl`] rather than bytes so the check cannot be skipped: the
    /// only way to obtain one is [`cast_replay::CastCrl::servable_for`], which is handed
    /// the chain and refuses when it names us.
    #[must_use]
    pub fn with_crl(mut self, crl: &ServableCrl) -> Self {
        self.crl = Some(crl.bytes().to_vec());
        self
    }
}

impl DeviceAuthResponder for ReplayAuthResponder {
    fn respond(&self, challenge: &AuthChallenge) -> Result<AuthResponse, CastError> {
        // No signing, and deliberately no use of `challenge.sender_nonce`: the
        // signature was computed over the peer certificate alone. `signed_auth`
        // returns `NonceEcho::Empty`, and `auth_response` turns that into an
        // absent `sender_nonce`, which is what makes the replay verify.
        Ok(auth_response(
            self.credential.signed_auth(requested_hash(challenge)),
            self.crl.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::proto::HashAlgorithm;

    async fn identity() -> ReplayIdentity {
        let provider = ReplayProvider::resolve(cast_replay::ReplayConfig {
            network: false,
            cache_path: None,
            ..cast_replay::ReplayConfig::default()
        })
        .await
        .unwrap();
        ReplayIdentity::new(Arc::new(provider))
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
        let responder = ReplayAuthResponder::new(identity.credential());
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
        let responder = ReplayAuthResponder::new(Arc::clone(&credential));
        let response = responder
            .respond(&challenge(Some(HashAlgorithm::Sha256)))
            .unwrap();

        let (tls_cert, _) = credential.tls_identity();
        assert_eq!(
            response.signature,
            credential.signature(cast_replay::HashAlgo::Sha256)
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
        let responder = ReplayAuthResponder::new(identity.credential());
        let response = responder.respond(&challenge(None)).unwrap();
        assert_eq!(response.hash_algorithm, Some(HashAlgorithm::Sha1 as i32));
        assert_eq!(
            response.signature,
            identity.credential().signature(cast_replay::HashAlgo::Sha1)
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
