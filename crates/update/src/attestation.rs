//! Verifying GitHub's build provenance — the receiver's half of D59.
//!
//! A release asset is attested by `actions/attest-build-provenance`: a SLSA provenance
//! statement in a DSSE envelope, signed by a certificate Fulcio issued to *this
//! repository's release workflow* and valid for ten minutes, with the signing event
//! recorded in Rekor. Verifying it answers a question a signature of ours could not:
//! these bytes were produced by that workflow on that repository, and forging that
//! requires a visible commit to `release.yml` rather than reading a secret any workflow
//! in the repository could have read.
//!
//! **Two trust anchors, both compiled in, and both for the same reason the minisign key
//! they replace was.** The Sigstore trusted root ([`TRUSTED_ROOT`]) says which Fulcio and
//! which Rekor to believe; the identity ([`RELEASE_IDENTITY`]) says which workflow. A
//! trust anchor that arrived at runtime would be one more thing an attacker can supply —
//! and in particular the identity must *not* come from `[update] repository`, or pointing
//! the receiver at a fork would make that fork's own attestations valid.
//!
//! **Verification is offline.** The bundle carries its Rekor inclusion proof, so nothing
//! here talks to a transparency log; the certificate's ten-minute validity is judged
//! against the log's signed timestamp rather than against today's clock, which is why a
//! months-old release still verifies. The panel is online when it updates — it is
//! fetching a release — so this is robustness rather than necessity (D59 has the longer
//! version, including why the trust root's staleness is not the hazard it looks like).

use std::path::Path;

use sha2::{Digest as _, Sha256};
use sigstore::bundle::verify::{policy::Identity, Verifier};
use sigstore::trust::sigstore::SigstoreTrustRoot;
use thiserror::Error;
use tracing::{info, warn};

/// Sigstore's public-good trusted root: Fulcio's CA, Rekor's key, the CT log keys.
///
/// The public-good instance rather than GitHub's own `fulcio.githubapp.com`, because that
/// is what a *public* repository's attestations chain to — ours are issued by
/// `O=sigstore.dev, CN=sigstore-intermediate`. Refreshed with
/// `gh attestation trusted-root`, which emits both as JSON Lines; this is the first of
/// the two.
pub const TRUSTED_ROOT: &str = include_str!("../sigstore-trusted-root.json");

/// The workflow whose attestations this build accepts, as it appears in the certificate's
/// subject alternative name.
///
/// Overridable at **compile** time, like `CASTAWAY_GIT_REV` and for the same reason: it
/// is a property of the binary, decided by whoever built it. That is what lets a fork
/// point its own panels at its own releases, and it is why it is not a config key — a
/// runtime identity would let anyone who can edit `castaway.toml` redirect the receiver
/// to a repository whose attestations they control.
pub const RELEASE_IDENTITY: &str = match option_env!("CASTAWAY_RELEASE_IDENTITY") {
    Some(identity) => identity,
    None => "https://github.com/schlarpc/castaway/.github/workflows/release.yml@refs/heads/main",
};

/// The OIDC issuer Fulcio recorded. Constant across every GitHub Actions workflow, and
/// checked because an identity without its issuer is only half a name.
pub const OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// A verifier holding the parsed trust root and the identity policy.
///
/// Built once and reused: parsing the trusted root builds a certificate pool and a
/// keyring, which is work worth doing at startup rather than at four in the morning.
pub struct Provenance {
    verifier: Verifier,
    policy: Identity,
}

impl Provenance {
    /// Build a verifier from the trust anchors compiled into this binary.
    ///
    /// # Errors
    /// [`AttestationError::TrustRoot`] if the embedded root is not a trusted root — which
    /// can only mean the checked-in file was damaged, since nothing at runtime supplies
    /// it.
    pub fn embedded() -> Result<Self, AttestationError> {
        Self::with_anchors(TRUSTED_ROOT, RELEASE_IDENTITY, OIDC_ISSUER)
    }

    /// Build a verifier against anchors chosen by the caller — the seam the fixture test
    /// uses to pin a real bundle against the real root without depending on whatever this
    /// build happened to be compiled with.
    ///
    /// # Errors
    /// [`AttestationError::TrustRoot`] if `root` is not a Sigstore trusted root.
    pub fn with_anchors(
        root: &str,
        identity: &str,
        issuer: &str,
    ) -> Result<Self, AttestationError> {
        // `_unchecked` names the fact that this bypasses TUF, which is correct here: a
        // root compiled into the binary is a trust anchor by construction, exactly as the
        // minisign public key it replaces was. TUF exists to establish trust in a root
        // fetched over a network, and this one never is.
        let trust_root = SigstoreTrustRoot::from_trusted_root_json_unchecked(root.as_bytes())
            .map_err(|source| AttestationError::TrustRoot { source })?;
        let verifier = Verifier::new(rekor_config(), trust_root)
            .map_err(|source| AttestationError::TrustRoot { source })?;
        Ok(Self {
            verifier,
            policy: Identity::new(identity, issuer),
        })
    }

    /// Build a verifier with the freshest trust root available, falling back to the one
    /// compiled in.
    ///
    /// **Why both.** The embedded root is cumulative — expired Fulcio CAs and retired
    /// Rekor logs stay in it with their validity windows, so a release signed years ago
    /// keeps verifying for ever. What it cannot cover is the *other* direction: Sigstore
    /// adding a log or an intermediate after this build was made. The copy shipped here
    /// already carries two Rekor logs and two Fulcio CAs for exactly that reason, one of
    /// each retired.
    ///
    /// So the live root is tried first — the panel is online when it updates, it is
    /// fetching a release — and the embedded copy is the floor underneath it. A TUF
    /// repository that cannot be reached degrades to the behaviour we would have had
    /// anyway rather than to nothing.
    ///
    /// This does not escape embedding a trust anchor, and does not pretend to: TUF
    /// bootstraps from its own root, which `sigstore` ships. It swaps a file that rotates
    /// every few years for one built to renew itself.
    ///
    /// # Errors
    /// Only [`AttestationError::TrustRoot`], and only if the *embedded* root is also
    /// unreadable — a failed refresh alone is not an error here.
    pub async fn refreshed(cache: Option<&Path>) -> Result<Self, AttestationError> {
        match SigstoreTrustRoot::new(cache).await {
            Ok(root) => match Self::from_root(root) {
                Ok(verifier) => {
                    info!("auto-update: trust root refreshed from Sigstore's TUF repository");
                    return Ok(verifier);
                }
                // A root that fetched and then would not build a verifier is odd enough to
                // say out loud before falling back past it.
                Err(e) => warn!(error = %e, "auto-update: the refreshed trust root is unusable"),
            },
            Err(e) => info!(
                error = %e,
                "auto-update: could not refresh the trust root; using the one compiled in"
            ),
        }
        Self::embedded()
    }

    /// A verifier over an already-built trust root, with this build's identity policy.
    fn from_root(root: SigstoreTrustRoot) -> Result<Self, AttestationError> {
        Ok(Self {
            verifier: Verifier::new(rekor_config(), root)
                .map_err(|source| AttestationError::TrustRoot { source })?,
            policy: Identity::new(RELEASE_IDENTITY, OIDC_ISSUER),
        })
    }

    /// Check that `bundle_json` is a provenance attestation, by the trusted workflow, over
    /// exactly `artifact`.
    ///
    /// The digest is what binds: `verify_digest` compares the DSSE statement's subject
    /// against the hash of the bytes handed in, so a bundle for some *other* artifact of
    /// the same repository fails here rather than being accepted for the wrong file.
    ///
    /// # Errors
    /// [`AttestationError::Malformed`] if the bundle is not a Sigstore bundle, and
    /// [`AttestationError::Rejected`] for every substantive failure — wrong signer, wrong
    /// digest, broken chain, bad inclusion proof. They are one variant on purpose: the
    /// receiver's answer to all of them is identical, and enumerating them here would
    /// invite a caller to treat some as less serious than others.
    pub async fn verify(&self, artifact: &[u8], bundle_json: &str) -> Result<(), AttestationError> {
        let bundle: sigstore::bundle::Bundle =
            serde_json::from_str(bundle_json).map_err(AttestationError::Malformed)?;
        let mut hasher = Sha256::new();
        hasher.update(artifact);
        // `true` is offline: the bundle's own inclusion proof is used rather than a call
        // to Rekor. See the module docs for why that is the right default even though the
        // panel has a network at this moment.
        self.verifier
            .verify_digest(hasher, bundle, &self.policy, true)
            .await
            .map_err(|source| AttestationError::Rejected {
                source: Box::new(source),
            })
    }

    /// Which workflow this verifier accepts, for the log line that says so at startup.
    #[must_use]
    pub fn identity(&self) -> &str {
        RELEASE_IDENTITY
    }
}

/// Rekor's endpoint configuration. Only consulted when verification is *not* offline,
/// which here it never is — every bundle carries its own inclusion proof.
fn rekor_config() -> sigstore::rekor::apis::configuration::Configuration {
    sigstore::rekor::apis::configuration::Configuration::default()
}

/// Why an attestation was not accepted.
#[derive(Debug, Error)]
pub enum AttestationError {
    /// The trust root compiled into this binary could not be read.
    #[error("the embedded Sigstore trusted root")]
    TrustRoot {
        /// What the parser said.
        #[source]
        source: sigstore::errors::SigstoreError,
    },
    /// The bytes offered were not a Sigstore bundle at all.
    #[error("the attestation bundle is not JSON this build understands")]
    Malformed(#[source] serde_json::Error),
    /// It was a bundle, and it did not check out.
    #[error("the attestation does not verify")]
    Rejected {
        /// What the verifier objected to.
        #[source]
        source: Box<sigstore::bundle::verify::VerificationError>,
    },
}
#[cfg(test)]
mod tests {
    use super::{AttestationError, Provenance, TRUSTED_ROOT};

    /// A real release of this repository, and the real bundle GitHub's attestation API
    /// returned for it. Both checked in verbatim (ground rule 9: findings land as
    /// fixtures), so these tests run against genuine production bytes with no network.
    ///
    /// Regenerating them after a release — the manifest is byte-reproducible from the zip,
    /// which is how this pair was obtained when the first attested release did not upload
    /// its manifest:
    ///
    /// ```text
    /// gh release download <tag> --pattern '*.zip' --dir /tmp/f
    /// nix run .#release-manifest -- /tmp/f/<zip> $(git rev-parse <tag>) \
    ///   $(git rev-list --count <tag>) /tmp/f
    /// cp /tmp/f/manifest.json crates/update/fixtures/attested-manifest.json
    /// d=$(sha256sum crates/update/fixtures/attested-manifest.json | cut -d' ' -f1)
    /// gh api "repos/schlarpc/castaway/attestations/sha256:$d" --jq '.attestations[0].bundle' \
    ///   > crates/update/fixtures/attested-manifest.json.sigstore
    /// ```
    const ATTESTED: &[u8] = include_bytes!("../fixtures/attested-manifest.json");
    const BUNDLE: &str = include_str!("../fixtures/attested-manifest.json.sigstore");
    /// The identity that really signed the fixture, written down rather than read from
    /// [`super::RELEASE_IDENTITY`] — so a build compiled for a fork still tests the bytes
    /// it was made from.
    const FIXTURE_IDENTITY: &str =
        "https://github.com/schlarpc/castaway/.github/workflows/release.yml@refs/heads/main";
    const FIXTURE_ISSUER: &str = "https://token.actions.githubusercontent.com";

    fn verifier() -> Provenance {
        Provenance::with_anchors(TRUSTED_ROOT, FIXTURE_IDENTITY, FIXTURE_ISSUER)
            .expect("the embedded trusted root parses")
    }

    /// What the panel will do once #349 is resolved.
    ///
    /// Ignored rather than deleted, and the fixture kept beside it, because everything
    /// this needs is already here and correct: the trust root parses, the certificate
    /// chains, the identity matches, the signature verifies over the PAE, and the tlog
    /// entry is consistent. The one thing that fails is `sigstore-rs` matching the
    /// artifact against only the *first* subject of the in-toto statement
    /// (sigstore-rs#596), and this fixture's statement names the zip first and the
    /// manifest second — see [`the_fixture_is_sound_and_the_failure_is_upstream`], which
    /// pins that precisely. `release.yml` now attests each asset in its own step, so the
    /// next release's manifest bundle is single-subject: regenerate the fixture pair from
    /// it (recipe above, under a *new* name — the multi-subject pair stays for the pin
    /// test), point these constants at it, and delete this attribute.
    ///
    /// The negative cases that belong beside it — a flipped byte, another workflow's
    /// identity, a bundle that is not a bundle — are deliberately *absent* rather than
    /// written and ignored. Each would assert `is_err()`, which is true today for a reason
    /// that has nothing to do with what they claim to test, and a vacuous test that turns
    /// green on the wrong evidence is worse than a missing one (docs/test-matrix.md §3.5).
    #[ignore = "waiting on the first per-asset-attested release to regenerate the fixture from — #349"]
    #[tokio::test]
    async fn a_real_release_of_this_repository_verifies_offline() {
        verifier()
            .verify(ATTESTED, BUNDLE)
            .await
            .expect("GitHub's own provenance over our own release");
    }

    /// The fixture is good and the blocker is somebody else's, pinned so nobody re-derives
    /// it and so the failure cannot quietly become a *different* failure.
    ///
    /// Two halves. First, the bundle's own numbers agree with its contents — Rekor's
    /// `payloadHash` is the digest of the payload, and its `envelopeHash` is the digest of
    /// the envelope as GitHub sent it. Second, verification nonetheless fails, for exactly
    /// one reason, pinned by the subject assertions below.
    ///
    /// The mechanism, measured (#349): `actions/attest-build-provenance` puts every file
    /// of `subject-path` into *one* statement — ours lists the zip first and the manifest
    /// second — and `sigstore-rs` compares the artifact digest against `subject[0]` only
    /// (sigstore-rs#596; its `intoto.rs` documents the gap). So the manifest, a genuine
    /// subject of a genuine attestation, is rejected for not being the *first* one. The
    /// error surfaces as `SignatureErrorKind::Transparency` — "transparency materials are
    /// inconsistent" — which is why this was first misdiagnosed as a tlog-consistency
    /// failure. It is not: verifying the released *zip* (subject[0]) against this same
    /// bundle passes every check including the tlog ones, and upstream's multi-subject fix
    /// (sigstore-rs#615) makes the manifest verify too, measured on 2026-08-12.
    ///
    /// `release.yml` sidesteps this by attesting each asset in its own step, so this
    /// fixture pair is kept *because* it is multi-subject: it is what a collapsed-back
    /// single attest step would produce, and this test failing on a regenerated fixture
    /// is the signal that the workflow shape changed before upstream shipped.
    #[tokio::test]
    async fn the_fixture_is_sound_and_the_failure_is_upstream() {
        use base64::Engine as _;
        use sha2::{Digest as _, Sha256};

        let bundle: serde_json::Value = serde_json::from_str(BUNDLE).expect("bundle parses");
        let body = bundle["verificationMaterial"]["tlogEntries"][0]["canonicalizedBody"]
            .as_str()
            .expect("a canonicalised body");
        let body: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(body)
                .expect("base64"),
        )
        .expect("the body is JSON");

        // Rekor's own record of what it logged, against what the bundle actually holds.
        let envelope = &bundle["dsseEnvelope"];
        let payload = base64::engine::general_purpose::STANDARD
            .decode(envelope["payload"].as_str().expect("a payload"))
            .expect("base64");
        assert_eq!(
            body["spec"]["payloadHash"]["value"].as_str(),
            Some(format!("{:x}", Sha256::digest(&payload)).as_str()),
            "the fixture's payload does not match what Rekor logged"
        );
        let canonical = serde_json::to_vec(envelope).expect("re-serialise");
        assert_eq!(
            body["spec"]["envelopeHash"]["value"].as_str(),
            Some(format!("{:x}", Sha256::digest(&canonical)).as_str()),
            "the fixture's envelope does not match what Rekor logged"
        );

        // The mechanism: the manifest is a subject of the statement — just not the first.
        // If either half of this stops holding, the failure below is a different bug.
        let statement: serde_json::Value =
            serde_json::from_slice(&payload).expect("the payload is an in-toto statement");
        let subjects: Vec<&str> = statement["subject"]
            .as_array()
            .expect("subjects")
            .iter()
            .map(|s| s["digest"]["sha256"].as_str().expect("a sha256 digest"))
            .collect();
        let artifact = format!("{:x}", Sha256::digest(ATTESTED));
        assert!(
            subjects.contains(&artifact.as_str()),
            "the manifest is no longer a subject of the statement — the fixture is broken"
        );
        assert_ne!(
            subjects.first().copied(),
            Some(artifact.as_str()),
            "the manifest is now subject[0]; sigstore-rs would accept it and \
             a_real_release_of_this_repository_verifies_offline should be un-ignored"
        );

        // And yet. Not `Malformed` (the bundle parses), and not a certificate, identity or
        // signature failure — those all pass. Only the subject match.
        let err = verifier()
            .verify(ATTESTED, BUNDLE)
            .await
            .expect_err("#349: this is expected to fail until upstream is fixed");
        assert!(
            matches!(err, AttestationError::Rejected { .. }),
            "expected a rejection from the verifier, got {err}"
        );
    }

    #[test]
    fn this_build_names_a_workflow_and_an_issuer() {
        // Cheap, and it catches the one way the compile-time override goes wrong: an
        // identity that is empty or not a workflow URI would make every release fail at
        // 4 a.m. with no clue why.
        assert!(
            super::RELEASE_IDENTITY.starts_with("https://github.com/")
                && super::RELEASE_IDENTITY.contains("/.github/workflows/"),
            "CASTAWAY_RELEASE_IDENTITY is not a workflow identity: {}",
            super::RELEASE_IDENTITY
        );
        assert_eq!(
            super::OIDC_ISSUER,
            "https://token.actions.githubusercontent.com"
        );
    }
}
