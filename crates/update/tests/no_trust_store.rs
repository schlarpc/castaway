//! The trust-root refresh degrades on a host with no readable CA store (#351).
//!
//! `Provenance::refreshed` is written to fall back: whatever goes wrong reaching
//! Sigstore's TUF repository, the embedded root is still there and the update check goes
//! on. A *panic* escapes that, and `sigstore` has one on the path — it builds its HTTP
//! client with `reqwest::Client::new()`, which panics rather than erroring when the TLS
//! backend will not initialise, and `rustls-platform-verifier` will not initialise where
//! no CA certificates can be read. That is not an exotic host: it is the Nix build
//! sandbox, and it is where the nightly update task would die silently — the
//! unattended-silence failure D59 exists to avoid.
//!
//! An integration test with one test in it because it needs its own process: the trust
//! store is named by an environment variable, and setting one under a test harness that
//! runs tests as threads would reach into every other test in the binary.

use std::path::Path;

use castaway_update::attestation::Provenance;

/// Paths that are not certificate stores and not files at all. `rustls-native-certs`,
/// which `rustls-platform-verifier` reads the Linux store through, honours `SSL_CERT_FILE`
/// and falls back to `SSL_CERT_DIR` — so both have to go, and poisoning only the first
/// reproduces nothing on a dev shell that sets both. Together they give a box with a
/// perfectly good trust store the view of one that has none.
const NO_SUCH_STORE: [(&str, &str); 2] = [
    ("SSL_CERT_FILE", "/no-cert-file.crt"),
    ("SSL_CERT_DIR", "/no-cert-dir"),
];

#[tokio::test]
async fn a_host_with_no_readable_trust_store_falls_back_to_the_embedded_root() {
    for (var, path) in NO_SUCH_STORE {
        assert!(
            !Path::new(path).exists(),
            "{path} exists, so this test is no longer testing anything"
        );
        std::env::set_var(var, path);
    }

    // Pre-#351 this call did not return at all: `sigstore`'s client construction panicked
    // through it, and in the receiver that unwind took the nightly update task with it.
    let provenance = Provenance::refreshed(None)
        .await
        .expect("a refresh that cannot even build a client still has the embedded root");
    assert_eq!(
        provenance.identity(),
        castaway_update::attestation::RELEASE_IDENTITY
    );
}
