//! The receiver's self-update.
//!
//! This crate is the trust boundary of the auto-update path (#26): it turns the bytes a
//! release page hands over into a [`manifest::Manifest`] that GitHub's own build
//! provenance has vouched for, or into an error. Nothing above it re-derives that
//! decision, and nothing below it touches the network.
//!
//! **What authenticates a release.** `actions/attest-build-provenance` signs each asset
//! with a certificate Fulcio issued to this repository's release workflow, valid for ten
//! minutes, logged in Rekor. The receiver checks that bundle against a trust root and a
//! workflow identity it was *compiled* with (see [`attestation`]). There is no signing
//! secret anywhere in the repository, which is the point: forging a release means a
//! visible commit to `release.yml`, not reading a secret that every workflow could read.
//!
//! **What each layer buys.** TLS authenticates the host and stops a passive observer. The
//! attestation authenticates the *bytes* independently of whoever served them, and names
//! who built them. The build number orders them — the one thing neither TLS nor a
//! signature can do, because a replayed old release is correctly signed and correctly
//! served. All three are needed and none is redundant.
//!
//! The order of operations is load-bearing and lives in [`agent`]: the **manifest** is
//! fetched and its provenance checked before it is parsed, and only then is the artifact
//! downloaded, bounded and digested against what that manifest declared. Verifying the
//! small file first is not thrift — attestations are keyed by artifact digest, so
//! verifying the zip directly would mean downloading a quarter of a gigabyte before
//! anything could say whether the bytes were genuine.

#![forbid(unsafe_code)]

pub mod agent;
pub mod attestation;
pub mod manifest;
pub mod policy;

pub use attestation::{AttestationError, Provenance};
pub use manifest::{
    ArtifactName, BuildNumber, Commit, InstalledBuild, Manifest, ManifestError, Offer, Sha256Digest,
};
