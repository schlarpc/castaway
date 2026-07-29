//! Bridges the pure [`crypto_cast_auth`] signer to the [`DeviceAuthResponder`] the
//! session calls. Lives here (not in `crypto-cast-auth`) so the dependency flows
//! `proto-cast` → `crypto-cast-auth`, never the reverse.

use std::sync::Arc;

use crypto_cast_auth::{CastDeviceSigner, HashAlgo, SigAlgo, SignedAuth};

use crate::error::CastError;
use crate::proto::{AuthChallenge, AuthResponse, HashAlgorithm, SignatureAlgorithm};
use crate::session::DeviceAuthResponder;

/// The hash a challenge asks for. The proto2 default is SHA-1; an explicit
/// SHA-256 request is honoured.
pub(crate) fn requested_hash(challenge: &AuthChallenge) -> HashAlgo {
    match challenge.hash_algorithm {
        Some(h) if h == HashAlgorithm::Sha256 as i32 => HashAlgo::Sha256,
        _ => HashAlgo::Sha1,
    }
}

/// Fill the `AuthResponse` proto from signed material.
///
/// The one place the response is built, so the `sender_nonce` rule is applied
/// once rather than at each responder. That field comes from
/// [`SignedAuth::nonce_echo`] — what the signature actually covers — and never
/// from the challenge: a replayed signature covers the peer certificate alone,
/// and echoing the sender's nonce back would make the sender rebuild a different
/// message and reject a response that is otherwise correct.
pub(crate) fn auth_response(signed: SignedAuth) -> AuthResponse {
    let hash_algorithm = match signed.hash {
        HashAlgo::Sha1 => HashAlgorithm::Sha1,
        HashAlgo::Sha256 => HashAlgorithm::Sha256,
    };
    let signature_algorithm = match signed.algorithm {
        SigAlgo::RsaPkcs1v15 => SignatureAlgorithm::RsassaPkcs1v15,
    };
    AuthResponse {
        signature: signed.signature,
        client_auth_certificate: signed.client_auth_certificate,
        intermediate_certificate: signed.intermediate_certificate,
        signature_algorithm: Some(signature_algorithm as i32),
        sender_nonce: signed.nonce_echo.as_bytes().map(<[u8]>::to_vec),
        hash_algorithm: Some(hash_algorithm as i32),
        // No CRL. Openscreen senders default to `kCrlOptional`, so a receiver that
        // supplies none is still accepted; Chrome fetches the Cast CRL itself.
        crl: None,
    }
}

/// A per-connection auth responder: the shared device signer plus this connection's
/// TLS server certificate (which the challenge is signed over).
pub struct CastAuthResponder {
    signer: Arc<CastDeviceSigner>,
    tls_cert_der: Vec<u8>,
}

impl CastAuthResponder {
    /// Bind the shared `signer` to one connection's `tls_cert_der`.
    #[must_use]
    pub fn new(signer: Arc<CastDeviceSigner>, tls_cert_der: Vec<u8>) -> Self {
        Self {
            signer,
            tls_cert_der,
        }
    }
}

impl DeviceAuthResponder for CastAuthResponder {
    fn respond(&self, challenge: &AuthChallenge) -> Result<AuthResponse, CastError> {
        let signed = self
            .signer
            .sign(
                &self.tls_cert_der,
                challenge.sender_nonce.as_deref(),
                requested_hash(challenge),
            )
            .map_err(|e| CastError::Auth(e.to_string()))?;
        Ok(auth_response(signed))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use prost::Message as _;

    #[test]
    fn responder_produces_a_signed_response() {
        let signer = Arc::new(CastDeviceSigner::generate_dev().unwrap().signer);
        let responder = CastAuthResponder::new(signer, b"tls-cert".to_vec());
        let challenge = AuthChallenge {
            signature_algorithm: None,
            sender_nonce: Some(vec![1, 2, 3, 4]),
            hash_algorithm: Some(HashAlgorithm::Sha256 as i32),
        };
        let resp = responder.respond(&challenge).unwrap();
        assert!(!resp.signature.is_empty());
        assert_eq!(resp.sender_nonce, Some(vec![1, 2, 3, 4]));
        // It encodes into a DeviceAuthMessage cleanly.
        let dam = crate::proto::DeviceAuthMessage {
            challenge: None,
            response: Some(resp),
            error: None,
        };
        let mut buf = Vec::new();
        dam.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());
    }
}
