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
