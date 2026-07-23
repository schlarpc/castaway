//! # crypto-cast-auth
//!
//! The Cast device-auth signer. CASTv2 opens every session with an `AuthChallenge`;
//! the receiver must reply with its device certificate chain and a signature over the
//! TLS server certificate (optionally prefixed with the sender's nonce). This crate
//! owns that signing — pure, given the key + cert material as bytes (no protobuf, no
//! socket), so `proto-cast` assembles the `AuthResponse` proto from a [`SignedAuth`].
//!
//! At n=1 the credential is a fixed local input (hackerspace notes / OPEN-QUESTIONS Q2);
//! [`CastDeviceSigner::generate_dev`] makes an ephemeral one for local testing.
#![forbid(unsafe_code)]

use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use sha2::Sha256;
use thiserror::Error;

/// Errors from the signer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CastAuthError {
    /// The private key PEM/DER could not be parsed.
    #[error("invalid device key: {0}")]
    InvalidKey(String),

    /// Signing failed.
    #[error("signing failed: {0}")]
    Sign(String),

    /// Key generation failed (dev mode).
    #[error("key generation failed: {0}")]
    KeyGen(String),
}

/// Hash algorithm requested by the challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    /// SHA-1 (legacy senders).
    Sha1,
    /// SHA-256 (modern default).
    Sha256,
}

/// Signature algorithm. Cast uses RSASSA-PKCS1-v1_5 in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigAlgo {
    /// RSASSA-PKCS1-v1_5.
    RsaPkcs1v15,
}

/// The material `proto-cast` needs to build an `AuthResponse`.
#[derive(Debug, Clone)]
pub struct SignedAuth {
    /// The signature bytes.
    pub signature: Vec<u8>,
    /// The device (leaf) certificate, DER.
    pub client_auth_certificate: Vec<u8>,
    /// Intermediate certificates, DER.
    pub intermediate_certificate: Vec<Vec<u8>>,
    /// The hash algorithm used.
    pub hash: HashAlgo,
    /// The signature algorithm used.
    pub algorithm: SigAlgo,
}

/// Signs Cast device-auth challenges with a fixed device key + certificate chain.
pub struct CastDeviceSigner {
    key: RsaPrivateKey,
    client_cert_der: Vec<u8>,
    intermediates_der: Vec<Vec<u8>>,
}

impl CastDeviceSigner {
    /// Build from an in-memory key and certificate chain (DER).
    #[must_use]
    pub fn new(
        key: RsaPrivateKey,
        client_cert_der: Vec<u8>,
        intermediates_der: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            key,
            client_cert_der,
            intermediates_der,
        }
    }

    /// Load the device key from a PKCS#8 PEM string plus its DER cert chain.
    ///
    /// # Errors
    /// [`CastAuthError::InvalidKey`] if the PEM can't be parsed.
    pub fn from_pkcs8_pem(
        key_pem: &str,
        client_cert_der: Vec<u8>,
        intermediates_der: Vec<Vec<u8>>,
    ) -> Result<Self, CastAuthError> {
        let key = RsaPrivateKey::from_pkcs8_pem(key_pem)
            .map_err(|e| CastAuthError::InvalidKey(e.to_string()))?;
        Ok(Self::new(key, client_cert_der, intermediates_der))
    }

    /// Generate an ephemeral 2048-bit key with a placeholder "certificate" (the public
    /// key's DER stands in). For local dev/tests only — a real sender wants a chain
    /// rooted in Google's device CA (OPEN-QUESTIONS Q2).
    ///
    /// # Errors
    /// [`CastAuthError::KeyGen`] if RSA key generation fails.
    pub fn generate_dev() -> Result<Self, CastAuthError> {
        let mut rng = rand::thread_rng();
        let key =
            RsaPrivateKey::new(&mut rng, 2048).map_err(|e| CastAuthError::KeyGen(e.to_string()))?;
        // Placeholder cert bytes: not a real X.509, fine for signature round-trips.
        let cert = b"castaway-dev-device-cert".to_vec();
        Ok(Self::new(key, cert, Vec::new()))
    }

    /// The device public key (for verifying our own signatures in tests).
    #[must_use]
    pub fn public_key(&self) -> RsaPublicKey {
        RsaPublicKey::from(&self.key)
    }

    /// Sign a challenge. `tls_cert_der` is the receiver's TLS server certificate for
    /// this connection; `sender_nonce`, if present, is prepended before hashing.
    ///
    /// # Errors
    /// [`CastAuthError::Sign`] if signing fails.
    pub fn sign(
        &self,
        tls_cert_der: &[u8],
        sender_nonce: Option<&[u8]>,
        hash: HashAlgo,
    ) -> Result<SignedAuth, CastAuthError> {
        let mut message =
            Vec::with_capacity(sender_nonce.map_or(0, <[u8]>::len) + tls_cert_der.len());
        if let Some(nonce) = sender_nonce {
            message.extend_from_slice(nonce);
        }
        message.extend_from_slice(tls_cert_der);

        let signature = match hash {
            HashAlgo::Sha256 => {
                let signing_key = SigningKey::<Sha256>::new(self.key.clone());
                signing_key
                    .try_sign(&message)
                    .map_err(|e| CastAuthError::Sign(e.to_string()))?
                    .to_vec()
            }
            HashAlgo::Sha1 => {
                let signing_key = SigningKey::<Sha1>::new(self.key.clone());
                signing_key
                    .try_sign(&message)
                    .map_err(|e| CastAuthError::Sign(e.to_string()))?
                    .to_vec()
            }
        };

        Ok(SignedAuth {
            signature,
            client_auth_certificate: self.client_cert_der.clone(),
            intermediate_certificate: self.intermediates_der.clone(),
            hash,
            algorithm: SigAlgo::RsaPkcs1v15,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;

    #[test]
    fn signature_verifies_sha256() {
        let signer = CastDeviceSigner::generate_dev().unwrap();
        let tls_cert = b"the-tls-server-cert-der";
        let nonce = [0xABu8; 16];
        let signed = signer
            .sign(tls_cert, Some(&nonce), HashAlgo::Sha256)
            .unwrap();

        let mut message = nonce.to_vec();
        message.extend_from_slice(tls_cert);
        let vk = VerifyingKey::<Sha256>::new(signer.public_key());
        let sig = Signature::try_from(signed.signature.as_slice()).unwrap();
        assert!(vk.verify(&message, &sig).is_ok());
    }

    #[test]
    fn signature_verifies_without_nonce_sha1() {
        let signer = CastDeviceSigner::generate_dev().unwrap();
        let tls_cert = b"cert";
        let signed = signer.sign(tls_cert, None, HashAlgo::Sha1).unwrap();
        let vk = VerifyingKey::<Sha1>::new(signer.public_key());
        let sig = Signature::try_from(signed.signature.as_slice()).unwrap();
        assert!(vk.verify(tls_cert, &sig).is_ok());
    }

    #[test]
    fn wrong_message_fails_verification() {
        let signer = CastDeviceSigner::generate_dev().unwrap();
        let signed = signer.sign(b"cert-a", None, HashAlgo::Sha256).unwrap();
        let vk = VerifyingKey::<Sha256>::new(signer.public_key());
        let sig = Signature::try_from(signed.signature.as_slice()).unwrap();
        assert!(vk.verify(b"cert-b", &sig).is_err());
    }

    #[test]
    fn carries_cert_chain() {
        let signer = CastDeviceSigner::new(
            CastDeviceSigner::generate_dev().unwrap().key,
            vec![1, 2, 3],
            vec![vec![4, 5], vec![6]],
        );
        let signed = signer.sign(b"x", None, HashAlgo::Sha256).unwrap();
        assert_eq!(signed.client_auth_certificate, vec![1, 2, 3]);
        assert_eq!(signed.intermediate_certificate.len(), 2);
    }
}
