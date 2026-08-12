//! The receiver's self-update.
//!
//! This crate is the trust boundary of the auto-update path (#26): it turns the bytes a
//! release page hands over into a [`manifest::Manifest`] that has been checked against a
//! key compiled into this binary, or into an error. Nothing above it re-derives that
//! decision, and nothing below it touches the network.
//!
//! **The threat model, stated honestly.** This defends the path between the panel and
//! GitHub — a CDN, a redirect, a machine on the LAN answering DNS. It does *not* defend
//! full compromise of the GitHub account: anyone who can write a workflow can read the
//! signing secret out of it. For one panel on one LAN pulling from its own repository
//! that is the right amount of paranoia, and claiming more would be theatre.
//!
//! **What each layer buys.** TLS authenticates the host and stops a passive observer.
//! The signature authenticates the *bytes* independently of whoever served them. The
//! build number orders them — the one thing neither TLS nor a signature can do, because
//! a replayed old release is correctly signed and correctly served. All three are needed
//! and none is redundant.
//!
//! The order operations happen in is load-bearing and is fixed by [`verify_release`]:
//! **signature first, then parse**. A JSON parser is a lot of code to point at bytes a
//! stranger chose, and there is no reason to: the signature is checkable against the raw
//! file.

#![forbid(unsafe_code)]

pub mod agent;
pub mod manifest;
pub mod minisign;
pub mod policy;

use thiserror::Error;

pub use manifest::{
    ArtifactName, BuildNumber, Commit, InstalledBuild, Manifest, ManifestError, Offer, Sha256Digest,
};
pub use minisign::{PublicKey, Signature, SignatureError};

/// The release signing key this build trusts, as a minisign public key file.
///
/// Checked into the tree because it is public and because a key that arrives at runtime
/// is not a trust anchor — it is one more thing an attacker can supply. Its secret half
/// is a repository Actions secret; `nix run .#release-keygen` makes the pair and says
/// what to do with each side.
///
/// Before that has been run the file carries a comment and no payload, and
/// [`release_key`] answers [`SignatureError::NoKey`]. That is a *state*, not a failure:
/// a build with no key cannot verify a release, so it does not update, and it says so.
///
/// `CASTAWAY_RELEASE_PUBKEY` overrides it at **compile** time, which is the same shape as
/// `CASTAWAY_GIT_REV` and for the same reason: it is a property of the binary, decided by
/// whoever built it. That is what lets the VM test build a receiver that trusts the
/// checked-in *test* key and then drive the whole update loop against a release signed
/// with its secret half — and it is also how somebody running their own fork points a
/// panel at their own releases without editing the tree.
pub const RELEASE_KEY: &str = match option_env!("CASTAWAY_RELEASE_PUBKEY") {
    Some(key) => key,
    None => include_str!("../release-key.pub"),
};

/// The key from [`RELEASE_KEY`].
///
/// # Errors
/// [`SignatureError::NoKey`] if this tree has no release key yet; the parse variants if
/// the file is there and damaged.
pub fn release_key() -> Result<PublicKey, SignatureError> {
    PublicKey::parse(RELEASE_KEY)
}

/// Why a release was refused.
#[derive(Debug, Error)]
pub enum ReleaseError {
    /// The signature did not check out against the embedded key. Nothing was parsed.
    #[error("the release signature")]
    Signature(#[from] SignatureError),
    /// The signature was good and the file behind it was not a manifest this build
    /// understands — which is a release-process bug rather than an attack, since only
    /// the key holder can get this far.
    #[error("the release manifest")]
    Manifest(#[from] ManifestError),
}

/// A release that has been checked: the manifest, and the trusted comment its signer
/// attached.
#[derive(Debug, Clone)]
pub struct VerifiedRelease {
    /// What the release claims to be.
    pub manifest: Manifest,
    /// The signer's own description of it, worth a log line — it is the one string in
    /// the whole exchange that an attacker cannot have written.
    pub trusted_comment: String,
}

/// Check `signature` over `manifest_json` with `key`, then read the manifest.
///
/// # Errors
/// [`ReleaseError::Signature`] if the bytes are not the ones the key holder signed, and
/// [`ReleaseError::Manifest`] if they are but do not describe a release this build can
/// act on.
pub fn verify_release(
    key: &PublicKey,
    manifest_json: &[u8],
    signature: &str,
) -> Result<VerifiedRelease, ReleaseError> {
    let signature = Signature::parse(signature)?;
    let trusted_comment = key.verify(manifest_json, &signature)?.to_owned();
    Ok(VerifiedRelease {
        manifest: Manifest::parse(manifest_json)?,
        trusted_comment,
    })
}

#[cfg(test)]
mod tests {
    use super::{release_key, verify_release, PublicKey, ReleaseError, SignatureError};

    const TEST_PUB: &str = include_str!("../fixtures/test-release.pub");
    const MANIFEST: &[u8] = include_bytes!("../fixtures/manifest.json");
    const MANIFEST_SIG: &str = include_str!("../fixtures/manifest.json.minisig");

    #[test]
    fn a_signed_release_verifies_and_parses_in_that_order() {
        let key = PublicKey::parse(TEST_PUB).expect("key");
        let release = verify_release(&key, MANIFEST, MANIFEST_SIG).expect("verified");
        assert_eq!(release.manifest.build.get(), 935);
        assert!(release.trusted_comment.contains("935"));
    }

    #[test]
    fn a_manifest_whose_signature_is_wrong_is_never_parsed() {
        let key = PublicKey::parse(TEST_PUB).expect("key");
        // Valid JSON, valid schema, everything in range — and unsigned. If the parse ran
        // first this would come back as a manifest error and the ordering claim in this
        // crate's documentation would be false.
        let forged = String::from_utf8(MANIFEST.to_vec())
            .expect("utf8")
            .replace("\"build\": 935", "\"build\": 99999");
        assert!(matches!(
            verify_release(&key, forged.as_bytes(), MANIFEST_SIG),
            Err(ReleaseError::Signature(SignatureError::BadSignature))
        ));
    }

    #[test]
    fn this_tree_says_out_loud_whether_it_carries_a_release_key() {
        // Not an assertion about *which* answer: before `release-keygen` has been run
        // the checked-in file is a comment, and after it is a key. Both are states this
        // crate has a defined behaviour for, and the point of the test is that the third
        // possibility — a file that is neither — is a build failure at `include_str!`
        // and a parse failure here rather than something discovered at 4 a.m.
        match release_key() {
            Ok(key) => assert_ne!(format!("{:?}", key.key_id()), String::new()),
            Err(SignatureError::NoKey) => {}
            Err(other) => panic!("release-key.pub is present and damaged: {other}"),
        }
    }
}
