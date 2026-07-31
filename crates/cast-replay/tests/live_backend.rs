//! Confirms the network path against the real CKS backend.
//!
//! `#[ignore]`d: it needs an uplink, it talks to a third party's endpoint, and it
//! fails when that endpoint goes away — which is a fact about the world, not a
//! regression in this tree. The offline path is covered unconditionally by the
//! unit tests in `src/table.rs`.
//!
//! ```text
//! cargo nextest run -p cast-replay --run-ignored all live_backend
//! ```
//!
//! What it establishes, when it passes: the `ts`/`sig` derivation is accepted, the
//! pinned roots match what the endpoint actually serves, the fixed-keystream field
//! decoding is right, and the returned material forms a credential whose window
//! covers now.

use cast_replay::{CredentialOrigin, HashAlgo, ReplayConfig, ReplayProvider};

#[tokio::test]
#[ignore = "requires network access to a third-party endpoint"]
async fn the_backend_serves_a_usable_credential() {
    let provider = ReplayProvider::resolve(ReplayConfig {
        network: true,
        // No cache: a cached credential would satisfy the resolve without ever
        // touching the network, which is the opposite of what this is checking.
        cache_path: None,
        ..ReplayConfig::default()
    })
    .await
    .expect("resolving a credential");

    let auth = provider.current();
    let credential = auth.credential();
    assert_eq!(
        credential.origin(),
        &CredentialOrigin::Network,
        "resolution fell back to the table; the backend path did not work"
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs();
    assert!(
        credential.valid_at(i64::try_from(now).expect("timestamp fits")),
        "the backend returned a window that does not cover now"
    );

    // The two properties the receiver depends on.
    let (tls_cert, tls_key) = credential.tls_identity();
    assert_eq!(tls_cert, credential.peer_cert_der());
    assert!(!tls_key.is_empty(), "the credential carries no TLS key");
    for hash in [HashAlgo::Sha1, HashAlgo::Sha256] {
        assert_eq!(credential.signature(hash).len(), 256);
        assert_eq!(
            credential.signed_auth(hash).nonce_echo,
            cast_replay::NonceEcho::Empty
        );
    }

    // The signature has to verify over the certificate we would present, or the
    // whole exchange fails at the sender.
    verify(
        credential.device_cert_der(),
        credential.peer_cert_der(),
        credential.signature(HashAlgo::Sha256),
    );

    eprintln!(
        "window {} .. {}, origin {}",
        credential.window().start_unix(),
        credential.window().end_unix(),
        credential.origin()
    );
}

/// RSASSA-PKCS1-v1_5 over SHA-256, with the device certificate's public key.
fn verify(device_cert_der: &[u8], message: &[u8], signature: &[u8]) {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::pkcs8::DecodePublicKey as _;
    use rsa::signature::Verifier as _;

    let (_, cert) =
        x509_parser::parse_x509_certificate(device_cert_der).expect("device certificate parses");
    let key = rsa::RsaPublicKey::from_public_key_der(cert.public_key().raw)
        .expect("device certificate carries an RSA key");
    let signature = Signature::try_from(signature).expect("signature is well formed");
    VerifyingKey::<sha2::Sha256>::new(key)
        .verify(message, &signature)
        .expect("the backend's signature must verify over the certificate we present");
}
