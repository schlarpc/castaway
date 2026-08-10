//! Confirms the network path against both borrowed identities' live endpoints —
//! the CKS backend (AirReceiver's) and the AirServer credential database.
//!
//! ## The decision, and the job that carries it (#183, #294)
//!
//! **Stays `#[ignore]`d.** Same call and the same reasoning as
//! `pipeline/tests/filter_subscriptions.rs`: the dependency is somebody else's uptime, so
//! a red gate on a *code* change would say nothing about the change that turned it red.
//!
//! `#[ignore]`d: these need an uplink, they talk to third parties' endpoints, and they
//! fail when an endpoint goes away or an API changes — which is a fact about the world,
//! not a regression in this tree. The offline paths are covered unconditionally by the
//! unit tests in `src/cks.rs` and `src/airserver.rs`.
//!
//! But that world *is* what #40 names as undetectable in advance: a revocation or an
//! endpoint change lands as a silent fallback, not an error. So these run on a schedule
//! rather than never — the nightly job #183 asked for is now
//! `.github/workflows/live-endpoints.yml` (#294), deliberately **out** of
//! `nix flake check` so an endpoint outage never reds a code change. The workflow failing
//! is the signal.
//!
//! ```text
//! cargo nextest run -p cast-replay --run-ignored all -E 'binary(live_backend)'
//! ```
//!
//! The `-E 'binary(live_backend)'` filterset, not a bare `live_backend` positional:
//! nextest matches a positional substring against the test *name*, and neither test's
//! name contains `live_backend` (it is the binary id), so the positional form silently
//! selects zero tests. The filterset is what actually runs both.
//!
//! What they establish, when they pass: for CKS, the `ts`/`sig` derivation is accepted,
//! the pinned roots match what the endpoint serves, the fixed-keystream field decoding is
//! right; for AirServer, the fetched SQLite database decrypts and yields a credential;
//! and in both cases the returned material forms a credential whose window covers now and
//! whose signature verifies over the certificate we would present.

use cast_replay::{CredentialOrigin, HashAlgo, Identity, ReplayConfig, ReplayProvider};

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

#[tokio::test]
#[ignore = "requires network access to a third-party endpoint"]
async fn the_airserver_endpoint_serves_a_usable_credential() {
    // AirServer's live path caches the fetched ~14 MB database on disk, so unlike the
    // CKS test it needs a writable path rather than `None`. A fresh temp dir with no
    // database in it forces the fetch — the whole point of this test — and is gone when
    // the test ends.
    let dir = tempfile::tempdir().expect("a temp dir for the fetched database");

    let provider = ReplayProvider::resolve(ReplayConfig {
        network: true,
        // Only AirServer, so resolution cannot satisfy itself from CKS first and skip
        // the endpoint this test exists to exercise.
        identity_order: vec![Identity::AirServer],
        cache_path: None,
        airserver_db_path: Some(dir.path().join("airserver.sqlite")),
        // KEK left at the build's provisioned value: without the carve this path is
        // unavailable and the assertion below reports it, rather than pretending.
        ..ReplayConfig::default()
    })
    .await
    .expect("resolving an AirServer credential");

    let auth = provider.current();
    let credential = auth.credential();
    assert_eq!(
        credential.origin(),
        &CredentialOrigin::AirServerLive,
        "resolution fell back to the table; the AirServer endpoint path did not work"
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs();
    assert!(
        credential.valid_at(i64::try_from(now).expect("timestamp fits")),
        "the endpoint returned a database whose window does not cover now"
    );

    // The same two properties the receiver depends on as for CKS.
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

    verify(
        credential.device_cert_der(),
        credential.peer_cert_der(),
        credential.signature(HashAlgo::Sha256),
    );

    eprintln!(
        "airserver window {} .. {}, origin {}",
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
