//! Bridges the pure [`crypto_cast_auth`] signer to the [`DeviceAuthResponder`] the
//! session calls. Lives here (not in `crypto-cast-auth`) so the dependency flows
//! `proto-cast` → `crypto-cast-auth`, never the reverse.

use std::sync::Arc;

use crypto_cast_auth::{CastDeviceSigner, HashAlgo};

use crate::error::CastError;
use crate::proto::{AuthChallenge, AuthResponse, HashAlgorithm, SignatureAlgorithm};
use crate::session::DeviceAuthResponder;

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
        // The proto2 default hash is SHA1; honor an explicit SHA256 request.
        let hash = match challenge.hash_algorithm {
            Some(h) if h == HashAlgorithm::Sha256 as i32 => HashAlgo::Sha256,
            _ => HashAlgo::Sha1,
        };
        let nonce = challenge.sender_nonce.as_deref();
        let signed = self
            .signer
            .sign(&self.tls_cert_der, nonce, hash)
            .map_err(|e| CastError::Auth(e.to_string()))?;

        let hash_algorithm = match signed.hash {
            HashAlgo::Sha1 => HashAlgorithm::Sha1,
            HashAlgo::Sha256 => HashAlgorithm::Sha256,
        };
        Ok(AuthResponse {
            signature: signed.signature,
            client_auth_certificate: signed.client_auth_certificate,
            intermediate_certificate: signed.intermediate_certificate,
            signature_algorithm: Some(SignatureAlgorithm::RsassaPkcs1v15 as i32),
            sender_nonce: challenge.sender_nonce.clone(),
            hash_algorithm: Some(hash_algorithm as i32),
            crl: None,
        })
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
