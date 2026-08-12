//! The release manifest: what a release *is*, in the only form a receiver trusts.
//!
//! A GitHub release names a commit, and a commit sha identifies without ordering — it
//! answers "which tree is this?" and cannot answer "is it newer than mine?". The binary
//! already knows which commit it is (`CASTAWAY_GIT_REV`), so the one thing it cannot
//! derive locally is the one thing this file adds: a **monotonic build number**, sound
//! here because this repository commits linearly to `main` and `git rev-list --count`
//! therefore rises by one per commit.
//!
//! Everything else in the manifest exists to bind the bytes: the zip's SHA-256, its
//! size, and its exact asset name. Nothing here is signed — the file's authenticity comes
//! from GitHub's build provenance over it (see [`crate::attestation`]), which is checked
//! before these bytes are ever handed to a parser.
//!
//! Parse, don't validate (ground rule 1): every field is a type that cannot hold a
//! nonsense value, so nothing downstream re-checks a hex string's length or wonders
//! whether an artifact name can contain a path separator.

use std::fmt;
use std::num::NonZeroU64;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// The manifest schema this build understands.
///
/// A single variant, deliberately: an unrecognised schema is a parse failure, which for
/// a kiosk means "keep running what you are running" rather than guessing at fields it
/// has never seen. That makes adding a *required* field a schema bump and makes the
/// forward-compatibility question have exactly one answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaVersion {
    /// The initial schema (#343).
    V1,
}

impl Serialize for SchemaVersion {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::V1 => s.serialize_u32(1),
        }
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match u32::deserialize(d)? {
            1 => Ok(Self::V1),
            other => Err(D::Error::custom(format!(
                "manifest schema {other} is newer than this build understands"
            ))),
        }
    }
}

/// A full 40-character commit sha, lowercase.
///
/// Full rather than short: the release *tag* carries a short prefix (`build-<short>`),
/// and a prefix is not an identifier — two of them collide eventually and the failure
/// would be installing the wrong tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit(String);

impl Commit {
    /// The sha as it appears in the manifest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `build-<short>` tag's short form: the first seven characters, which is what
    /// `git rev-parse --short` gives on this repository and what the release is named
    /// after.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..7]
    }
}

impl fmt::Display for Commit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A monotonic build number: `git rev-list --count HEAD` at the release's commit.
///
/// Non-zero, because zero is the sentinel for "this build does not know its own number"
/// — a dirty tree, or a source tree with no history (see [`InstalledBuild`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BuildNumber(NonZeroU64);

impl BuildNumber {
    /// The build number `count` names, or `None` for zero.
    #[must_use]
    pub const fn new(count: u64) -> Option<Self> {
        match NonZeroU64::new(count) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }

    /// The number itself.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for BuildNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What the running receiver knows about its own place in the ordering.
///
/// `Unknown` is not a failure to look something up — it is the honest state of a build
/// that was made from a dirty tree or outside a checkout, and the same state the idle
/// screen's footer already shows as `unknown`. A receiver that cannot order itself
/// against a release must not install one, which is what [`Manifest::offer_to`] says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledBuild {
    /// This build knows which number it is.
    Known(BuildNumber),
    /// It does not, and no offer can be ordered against it.
    Unknown,
}

impl InstalledBuild {
    /// Read what `crates/app/build.rs` stamped into `CASTAWAY_BUILD`.
    ///
    /// Total, because the alternative is a receiver that refuses to start over a
    /// diagnostic string: anything that is not a positive integer is [`Self::Unknown`],
    /// which is the build.rs sentinel `0` and also whatever a hand-set environment
    /// variable might have been.
    #[must_use]
    pub fn from_stamp(stamp: &str) -> Self {
        stamp
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(BuildNumber::new)
            .map_or(Self::Unknown, Self::Known)
    }
}

impl fmt::Display for InstalledBuild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(n) => write!(f, "{n}"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// A SHA-256 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::Digest as _;
        Self(sha2::Sha256::digest(bytes).into())
    }

    /// The digest a [`sha2::Sha256`] fed incrementally has arrived at — how a 250 MB
    /// artifact is checked without holding it in memory.
    #[must_use]
    pub fn finish(hasher: sha2::Sha256) -> Self {
        use sha2::Digest as _;
        Self(hasher.finalize().into())
    }

    /// The raw digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        let mut bytes = [0u8; 32];
        if text.len() != 64 {
            return Err(D::Error::custom(format!(
                "a sha256 is 64 hex characters, this is {}",
                text.len()
            )));
        }
        for (out, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let hex = std::str::from_utf8(pair).map_err(D::Error::custom)?;
            *out = u8::from_str_radix(hex, 16).map_err(D::Error::custom)?;
        }
        Ok(Self(bytes))
    }
}

/// The name of the release asset the manifest describes.
///
/// A bare file name, checked at the boundary: the updater turns this into a download URL
/// and into a path under the staging directory, and a name carrying `..` or a separator
/// would make a signed manifest into a write-anywhere primitive. The signature makes
/// that a remote-code-execution chain rather than a curiosity, so the check belongs
/// here, where nothing downstream can forget it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactName(String);

impl ArtifactName {
    /// The name as it appears on the release.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for ArtifactName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ArtifactName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        let ok = !name.is_empty()
            && name.len() <= 128
            && name.ends_with(".zip")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && !name.contains("..");
        if ok {
            Ok(Self(name))
        } else {
            Err(D::Error::custom(format!(
                "{name:?} is not a plain `.zip` asset name"
            )))
        }
    }
}

impl Serialize for Commit {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Commit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let sha = String::deserialize(d)?;
        if sha.len() == 40
            && sha
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            Ok(Self(sha))
        } else {
            Err(D::Error::custom(format!(
                "{sha:?} is not a 40-character lowercase commit sha"
            )))
        }
    }
}

/// The largest artifact this receiver will agree to download.
///
/// The Windows tree is around a quarter of a gigabyte, most of it Electron. A ceiling
/// four times that is generous for anything the build could plausibly grow into and
/// still refuses a manifest that would fill the panel's disk — which matters because
/// the size is what the download is bounded by *before* the hash can say anything.
pub const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

/// A release, as `release.yml` describes it and as the receiver reads it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Which schema this file follows.
    pub schema: SchemaVersion,
    /// The commit the release was built from, in full.
    pub commit: Commit,
    /// Its position in the linear history of `main`.
    pub build: BuildNumber,
    /// The release asset holding the deploy tree.
    pub artifact: ArtifactName,
    /// That asset's SHA-256.
    pub sha256: Sha256Digest,
    /// Its length in bytes.
    pub size: u64,
}

/// What an installed build should do about an offered one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// Strictly newer. Stage it.
    Newer,
    /// Same or older. Never install: this is the client-side half of `promote`'s
    /// forward-only movement of Latest, and it is what makes a replayed old release
    /// — the one thing TLS does not stop — a no-op.
    NotNewer,
    /// This build does not know its own number, so nothing can be ordered against it.
    /// A hand-built receiver mid-bisect, and the correct answer is to leave it alone.
    Unorderable,
}

impl Manifest {
    /// Read a manifest from the bytes of a `manifest.json`.
    ///
    /// # Errors
    /// [`ManifestError::Malformed`] for anything that is not a schema-1 manifest with
    /// every field in range, and [`ManifestError::TooLarge`] for one whose artifact is
    /// past [`MAX_ARTIFACT_BYTES`].
    pub fn parse(json: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_slice(json)?;
        if manifest.size == 0 || manifest.size > MAX_ARTIFACT_BYTES {
            return Err(ManifestError::TooLarge {
                size: manifest.size,
            });
        }
        Ok(manifest)
    }

    /// Should a receiver at `installed` take this release?
    #[must_use]
    pub const fn offer_to(&self, installed: InstalledBuild) -> Offer {
        match installed {
            InstalledBuild::Unknown => Offer::Unorderable,
            InstalledBuild::Known(have) => {
                if self.build.get() > have.get() {
                    Offer::Newer
                } else {
                    Offer::NotNewer
                }
            }
        }
    }

    /// Does `digest` match what this manifest says the artifact hashes to?
    #[must_use]
    pub fn covers(&self, digest: Sha256Digest) -> bool {
        digest == self.sha256
    }
}

/// Why a manifest was refused.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// Not a schema-1 manifest, or a field that could not hold the value offered.
    #[error("malformed manifest")]
    Malformed(#[from] serde_json::Error),
    /// An artifact size of zero, or one past [`MAX_ARTIFACT_BYTES`].
    #[error("manifest claims an artifact of {size} bytes")]
    TooLarge {
        /// What it claimed.
        size: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        BuildNumber, InstalledBuild, Manifest, ManifestError, Offer, Sha256Digest,
        MAX_ARTIFACT_BYTES,
    };

    /// Not hand-written: this is the output of `nix run .#release-manifest`, the same
    /// script `release.yml` calls, and `checks.release-manifest` regenerates it and
    /// fails on any difference. So a test that passes here is a statement about what CI
    /// actually produces rather than about a file somebody typed to look like it.
    const FIXTURE: &[u8] = include_bytes!("../fixtures/manifest.json");
    /// The bytes the fixture's `sha256` and `size` were taken from.
    const ARTIFACT: &[u8] = include_bytes!("../fixtures/castaway-windows-electron-ae2f19e.zip");

    fn build(n: u64) -> BuildNumber {
        BuildNumber::new(n).expect("non-zero")
    }

    #[test]
    fn the_fixture_release_yml_signs_is_the_one_this_build_reads() {
        let m = Manifest::parse(FIXTURE).expect("the fixture parses");
        assert_eq!(
            m.commit.as_str(),
            "ae2f19ef1f9d9a2488008f1075b252178ae7ef85"
        );
        assert_eq!(m.commit.short(), "ae2f19e");
        assert_eq!(m.build, build(935));
        assert_eq!(m.artifact.as_str(), "castaway-windows-electron-ae2f19e.zip");
    }

    #[test]
    fn the_digest_the_script_wrote_is_the_one_the_receiver_computes() {
        let m = Manifest::parse(FIXTURE).expect("parse");
        assert_eq!(m.size, ARTIFACT.len() as u64);
        assert!(m.covers(Sha256Digest::of(ARTIFACT)));
        // And the failure direction: one byte fewer is a different artifact.
        assert!(!m.covers(Sha256Digest::of(&ARTIFACT[..ARTIFACT.len() - 1])));
    }

    #[test]
    fn a_manifest_round_trips_through_the_json_ci_writes() {
        let m = Manifest::parse(FIXTURE).expect("parse");
        let json = serde_json::to_vec(&m).expect("serialize");
        assert_eq!(Manifest::parse(&json).expect("reparse"), m);
    }

    #[test]
    fn an_equal_or_lower_build_number_is_never_newer() {
        let m = Manifest::parse(FIXTURE).expect("parse");
        assert_eq!(m.offer_to(InstalledBuild::Known(build(934))), Offer::Newer);
        assert_eq!(
            m.offer_to(InstalledBuild::Known(build(935))),
            Offer::NotNewer
        );
        assert_eq!(
            m.offer_to(InstalledBuild::Known(build(936))),
            Offer::NotNewer
        );
    }

    #[test]
    fn the_build_stamp_reads_back_as_the_number_or_as_not_knowing() {
        assert_eq!(
            InstalledBuild::from_stamp("935"),
            InstalledBuild::Known(build(935))
        );
        assert_eq!(
            InstalledBuild::from_stamp(" 935\n"),
            InstalledBuild::Known(build(935))
        );
        // The build.rs sentinel for a dirty tree, a shallow clone, or no history at all.
        assert_eq!(InstalledBuild::from_stamp("0"), InstalledBuild::Unknown);
        assert_eq!(InstalledBuild::from_stamp(""), InstalledBuild::Unknown);
        assert_eq!(
            InstalledBuild::from_stamp("935-dirty"),
            InstalledBuild::Unknown
        );
    }

    #[test]
    fn a_receiver_that_does_not_know_its_own_build_orders_against_nothing() {
        let m = Manifest::parse(FIXTURE).expect("parse");
        assert_eq!(m.offer_to(InstalledBuild::Unknown), Offer::Unorderable);
    }

    #[test]
    fn a_schema_this_build_has_never_seen_is_refused_rather_than_guessed_at() {
        let json = String::from_utf8(FIXTURE.to_vec())
            .expect("utf8")
            .replace("\"schema\": 1", "\"schema\": 2");
        assert!(matches!(
            Manifest::parse(json.as_bytes()),
            Err(ManifestError::Malformed(_))
        ));
    }

    #[test]
    fn an_artifact_name_that_could_escape_the_staging_directory_is_refused() {
        for name in [
            "../../../../Windows/System32/evil.zip",
            "castaway\\..\\evil.zip",
            "sub/dir.zip",
            "castaway.exe",
        ] {
            let json = String::from_utf8(FIXTURE.to_vec())
                .expect("utf8")
                .replace("castaway-windows-electron-ae2f19e.zip", name);
            assert!(
                matches!(
                    Manifest::parse(json.as_bytes()),
                    Err(ManifestError::Malformed(_))
                ),
                "{name} was accepted as an artifact name"
            );
        }
    }

    #[test]
    fn a_build_number_of_zero_has_no_representation() {
        let json = String::from_utf8(FIXTURE.to_vec())
            .expect("utf8")
            .replace("\"build\": 935", "\"build\": 0");
        assert!(matches!(
            Manifest::parse(json.as_bytes()),
            Err(ManifestError::Malformed(_))
        ));
    }

    #[test]
    fn an_artifact_larger_than_the_panels_disk_is_refused_before_it_is_fetched() {
        let json = String::from_utf8(FIXTURE.to_vec()).expect("utf8").replace(
            "\"size\": 60",
            &format!("\"size\": {}", MAX_ARTIFACT_BYTES + 1),
        );
        assert!(matches!(
            Manifest::parse(json.as_bytes()),
            Err(ManifestError::TooLarge { .. })
        ));
    }

    #[test]
    fn the_digest_is_the_one_sha256sum_prints() {
        // `printf '' | sha256sum`, the value every tool agrees on for no bytes at all.
        assert_eq!(
            Sha256Digest::of(b"").to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
