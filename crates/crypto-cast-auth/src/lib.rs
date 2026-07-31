//! # crypto-cast-auth
//!
//! The Cast device-auth signer. CASTv2 opens every session with an `AuthChallenge`;
//! the receiver must reply with its device certificate chain and a signature over the
//! TLS server certificate (optionally prefixed with the sender's nonce). This crate
//! owns that signing — pure, given the key + cert material as bytes (no protobuf, no
//! socket), so `proto-cast` assembles the `AuthResponse` proto from a [`SignedAuth`].
//!
//! At n=1 the credential is a fixed local input (hackerspace notes / #40);
//! [`CastDeviceSigner::generate_dev`] makes an ephemeral one for local testing.
#![forbid(unsafe_code)]

use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rsa::signature::{SignatureEncoding, Signer};
use rsa::{RsaPrivateKey, RsaPublicKey};
use sha1::Sha1;
use sha2::Sha256;
use thiserror::Error;

mod dev;

pub use dev::DevCredential;

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

    /// A development certificate could not be issued (dev mode).
    #[error("dev certificate generation failed: {0}")]
    DevCert(String),
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

/// What the receiver must put in `AuthResponse.sender_nonce`.
///
/// Not a free choice, and not simply "whatever the sender sent". Openscreen
/// builds the blob it verifies from the nonce the receiver *echoes*, not the one
/// it issued:
///
/// ```text
/// size_t nonce_response_size = nonce_response.size();
/// ErrorOr<std::vector<uint8_t>> nonce_plus_peer_cert_der =
///     peer_cert.SerializeToDER(nonce_response_size);
/// ```
///
/// So the echo has to describe what the signature actually covers. Getting it
/// wrong costs nothing at the TLS layer and fails every session at the auth
/// layer, which is why it travels with the signature instead of being decided at
/// the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NonceEcho {
    /// Echo nothing: the signature covers the peer certificate alone. Required by
    /// replayed signatures, which cannot have covered a nonce chosen after they
    /// were computed.
    Empty,
    /// Echo these bytes: the signature covers `nonce || peer_cert_der`.
    Sender(Vec<u8>),
}

impl NonceEcho {
    /// The bytes to echo, or `None` to leave the field unset.
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Empty => None,
            Self::Sender(nonce) => Some(nonce),
        }
    }
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
    /// What to echo in `sender_nonce`, determined by what [`Self::signature`]
    /// covers. See [`NonceEcho`].
    pub nonce_echo: NonceEcho,
}

/// Signs Cast device-auth challenges with a fixed device key + certificate chain.
#[derive(Clone)]
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

    /// Generate an ephemeral development credential: a self-signed dev root and a
    /// device certificate issued under it, with the extensions a Cast sender's path
    /// builder insists on. For local dev/tests only — a real sender only trusts a chain
    /// rooted in Google's device CA (#40), and the returned
    /// [`DevCredential::root_ca_der`] is exactly the thing it will not have.
    ///
    /// The point of issuing real X.509 here rather than a placeholder byte string is
    /// that the *rest* of the auth response then becomes testable: chain order, digest
    /// choice, key usage and the signed-blob layout are all verifiable against a real
    /// sender implementation, leaving the missing credential as the only open item.
    ///
    /// # Errors
    /// [`CastAuthError::KeyGen`] if RSA key generation fails, [`CastAuthError::DevCert`]
    /// if certificate issuance does.
    pub fn generate_dev() -> Result<DevCredential, CastAuthError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| CastAuthError::DevCert(e.to_string()))?;
        Self::generate_dev_at(
            i64::try_from(now.as_secs()).map_err(|e| CastAuthError::DevCert(e.to_string()))?,
        )
    }

    /// [`CastDeviceSigner::generate_dev`] with the clock supplied, so the certificate
    /// windows a test asserts on do not depend on when the test runs.
    ///
    /// # Errors
    /// As [`CastDeviceSigner::generate_dev`].
    pub fn generate_dev_at(now_unix: i64) -> Result<DevCredential, CastAuthError> {
        let mut rng = rand::thread_rng();
        let root_key =
            RsaPrivateKey::new(&mut rng, 2048).map_err(|e| CastAuthError::KeyGen(e.to_string()))?;
        let device_key =
            RsaPrivateKey::new(&mut rng, 2048).map_err(|e| CastAuthError::KeyGen(e.to_string()))?;
        dev::issue(&root_key, &device_key, now_unix)
    }

    /// [`CastDeviceSigner::generate_dev_at`] with the keys supplied too. With fixed keys
    /// and a fixed clock the whole credential — and therefore the whole auth response —
    /// is byte-identical every run, which is what lets the device-auth vectors be checked
    /// in and compared rather than merely regenerated.
    ///
    /// # Errors
    /// [`CastAuthError::DevCert`] if certificate issuance fails.
    pub fn dev_from_keys(
        root_key: &RsaPrivateKey,
        device_key: &RsaPrivateKey,
        now_unix: i64,
    ) -> Result<DevCredential, CastAuthError> {
        dev::issue(root_key, device_key, now_unix)
    }

    /// PKCS#8 DER for the device key, so a caller that has to hand the key to another
    /// library (rcgen, in the dev-credential path) does not need the key type.
    fn pkcs8_der(key: &RsaPrivateKey) -> Result<Vec<u8>, CastAuthError> {
        key.to_pkcs8_der()
            .map(|d| d.as_bytes().to_vec())
            .map_err(|e| CastAuthError::DevCert(e.to_string()))
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
            // We just signed whatever `sender_nonce` was prepended, so the echo is
            // exactly that — echoing more or less than we signed is what breaks.
            nonce_echo: sender_nonce.map_or(NonceEcho::Empty, |n| NonceEcho::Sender(n.to_vec())),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;

    /// Generating two RSA-2048 keys is slow enough that the tests share one credential.
    fn dev() -> &'static DevCredential {
        static DEV: std::sync::OnceLock<DevCredential> = std::sync::OnceLock::new();
        DEV.get_or_init(|| CastDeviceSigner::generate_dev_at(1_800_000_000).unwrap())
    }

    #[test]
    fn signature_verifies_sha256() {
        let signer = &dev().signer;
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
        let signer = &dev().signer;
        let tls_cert = b"cert";
        let signed = signer.sign(tls_cert, None, HashAlgo::Sha1).unwrap();
        let vk = VerifyingKey::<Sha1>::new(signer.public_key());
        let sig = Signature::try_from(signed.signature.as_slice()).unwrap();
        assert!(vk.verify(tls_cert, &sig).is_ok());
    }

    #[test]
    fn wrong_message_fails_verification() {
        let signer = &dev().signer;
        let signed = signer.sign(b"cert-a", None, HashAlgo::Sha256).unwrap();
        let vk = VerifyingKey::<Sha256>::new(signer.public_key());
        let sig = Signature::try_from(signed.signature.as_slice()).unwrap();
        assert!(vk.verify(b"cert-b", &sig).is_err());
    }

    /// The echo has to describe what was signed. A signer that prepends a nonce
    /// must echo it; one that signs the certificate alone must echo nothing, or
    /// the sender rebuilds a different message and verification fails.
    #[test]
    fn nonce_echo_describes_what_was_signed() {
        let signer = &dev().signer;
        let nonce = [0x5Au8; 16];
        assert_eq!(
            signer
                .sign(b"cert", Some(&nonce), HashAlgo::Sha256)
                .unwrap()
                .nonce_echo,
            NonceEcho::Sender(nonce.to_vec())
        );
        assert_eq!(
            signer
                .sign(b"cert", None, HashAlgo::Sha256)
                .unwrap()
                .nonce_echo,
            NonceEcho::Empty
        );
    }

    #[test]
    fn carries_cert_chain() {
        let signer = CastDeviceSigner::new(
            dev().signer.key.clone(),
            vec![1, 2, 3],
            vec![vec![4, 5], vec![6]],
        );
        let signed = signer.sign(b"x", None, HashAlgo::Sha256).unwrap();
        assert_eq!(signed.client_auth_certificate, vec![1, 2, 3]);
        assert_eq!(signed.intermediate_certificate.len(), 2);
    }

    /// The dev credential used to be the byte string `castaway-dev-device-cert`, which no
    /// sender can parse — so every requirement past parsing went untested. These assert on
    /// the DER a sender actually reads, and each one is a rejection reason in openscreen's
    /// `boringssl_trust_store.cc`.
    #[test]
    fn dev_device_cert_satisfies_the_path_builder() {
        let (_, cert) = x509_parser::parse_x509_certificate(&dev().signer.client_cert_der).unwrap();

        let usage = cert
            .key_usage()
            .unwrap()
            .expect("a leaf with no key usage extension is rejected outright")
            .value;
        assert!(usage.digital_signature(), "leaf needs digitalSignature");

        // sha256WithRSAEncryption. Only the two RSA OIDs are accepted; rcgen's default
        // ECDSA leaf would be refused for its signature algorithm alone.
        assert_eq!(
            cert.signature_algorithm.algorithm.to_id_string(),
            "1.2.840.113549.1.1.11"
        );
    }

    #[test]
    fn dev_root_can_issue() {
        let (_, root) = x509_parser::parse_x509_certificate(&dev().root_ca_der).unwrap();
        let constraints = root
            .basic_constraints()
            .unwrap()
            .expect("an issuer with no basicConstraints is rejected")
            .value;
        assert!(constraints.ca, "the root must assert the CA bit");
        assert!(
            root.key_usage().unwrap().unwrap().value.key_cert_sign(),
            "an issuer whose key usage omits keyCertSign is rejected"
        );
    }

    /// Same keys plus same clock must give the same bytes, or the checked-in device-auth
    /// vectors could not be compared against a fresh run.
    #[test]
    fn dev_credential_is_deterministic() {
        let root = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let device = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let a = CastDeviceSigner::dev_from_keys(&root, &device, 1_800_000_000).unwrap();
        let b = CastDeviceSigner::dev_from_keys(&root, &device, 1_800_000_000).unwrap();
        assert_eq!(a.root_ca_der, b.root_ca_der);
        assert_eq!(
            a.signer.client_cert_der, b.signer.client_cert_der,
            "certificate issuance must not introduce randomness"
        );
    }
}
