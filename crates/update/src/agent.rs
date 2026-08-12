//! The updater actor: look, stage, hand over.
//!
//! The thin I/O shell around [`crate::policy`] (ground rule 3): it reads the clock once
//! per turn, asks the panel what it is doing, and does what the decision says. Every
//! failure in here degrades to "keep running what you are running, try again tomorrow",
//! because that is the correct kiosk failure mode and because there is nobody at the
//! panel to tell.
//!
//! **The order of operations is the security property.** Signature, then build number,
//! then digest, then extract, then — separately, later, and only when the panel is quiet
//! — activate. Nothing is written under a name the launcher would spawn until all four
//! have passed, which is `deploy-windows`' own no-stamp-on-half-finished principle
//! applied to a tree instead of a file.
//!
//! The blocking work — HTTP, hashing a quarter of a gigabyte, unzipping it — runs on
//! `spawn_blocking` rather than on the runtime (ground rule 4). A panel that stutters
//! while it downloads its own update would be a worse bug than not updating.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use castaway_paths::install::{self, InstallTree, LayoutError, Pointer, VersionId};
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::attestation::{AttestationError, Provenance};
use crate::manifest::{InstalledBuild, Manifest, Offer, Sha256Digest};
use crate::policy::{decide, Action, MinuteOfDay, Observation, Phase, Policy};

/// The name the release workflow gives the manifest.
const MANIFEST_ASSET: &str = "manifest.json";

/// How large an attestation bundle is allowed to be. Ours are a few kilobytes; the bound
/// is here because this is fetched before anything about it has been checked.
const MAX_BUNDLE_BYTES: u64 = 256 * 1024;

/// What the panel says about itself when the receiver is asked.
///
/// A trait rather than a function pointer because both answers are live readings, and a
/// seam rather than a `cfg` because "how idle is this panel" has a genuinely different
/// answer per platform — `GetLastInputInfo` on Windows, and on Linux whatever the app can
/// honestly say. Keeping it out here means the policy and this actor are the same code on
/// both (ground rule 5).
pub trait PanelActivity: Send + Sync + 'static {
    /// Is a sender casting right now?
    fn casting(&self) -> bool;
    /// How long since anybody touched the panel.
    fn idle_for(&self) -> Duration;
}

/// Where releases are fetched from.
#[derive(Debug, Clone)]
pub struct ReleaseSource {
    /// The API root. Overridable so a test harness can impersonate it — which is what
    /// makes the whole loop drivable in a VM with no network and no GitHub.
    pub base_url: String,
    /// `owner/name`.
    pub repository: String,
}

impl ReleaseSource {
    fn latest_url(&self) -> String {
        format!(
            "{}/repos/{}/releases/latest",
            self.base_url.trim_end_matches('/'),
            self.repository
        )
    }
}

/// Why the updater is not running at all.
///
/// Each of these is a *state*, not a fault: the receiver says so once, at startup, and
/// carries on doing its job. They are separate variants because the answer to each is a
/// different action by a different person.
#[derive(Debug, Error)]
pub enum StandDown {
    /// `[update] enabled = false`.
    #[error("switched off by `[update] enable = false` in castaway.toml")]
    Disabled,
    /// The trust anchors compiled into this build could not be read, so nothing can be
    /// verified. Only reachable if the checked-in Sigstore trusted root was damaged.
    #[error("the trust anchors compiled into this build")]
    TrustAnchors(#[source] AttestationError),
    /// A dirty tree, a shallow clone, or no history: this build cannot order itself
    /// against a release, so it must not take one. Exactly the hand-built receiver
    /// somebody is mid-bisect on.
    #[error(
        "this build does not know its own build number — a dirty tree, a shallow clone, or \
         no history — so it cannot tell whether a release is newer than itself, and will \
         not take one. A build from a clean checkout knows."
    )]
    UnknownBuild,
    /// A `hold` file at the install root. A hand deploy wrote it; deleting it re-arms.
    #[error(
        "a `hold` file at the install root: somebody deployed this by hand, and the updater \
         stands down until they delete it"
    )]
    Hold,
    /// Not running under a launcher, so there is nothing to hand over to.
    #[error(
        "not installed under a launcher: this receiver is not inside a `versions/<sha>/` \
         tree, so there is nothing to hand over to. `nix run .#windows-migrate` is what puts \
         a box onto that layout; a development build is expected to land here."
    )]
    Unmanaged(#[source] LayoutError),
}

/// The updater, once it has decided it is allowed to run.
pub struct Agent {
    tree: InstallTree,
    running: VersionId,
    installed: InstalledBuild,
    policy: Policy,
    source: ReleaseSource,
    provenance: Provenance,
    activity: Arc<dyn PanelActivity>,
    /// The machine's UTC offset, read once while the process was single-threaded. On unix
    /// that is the only moment it can be read soundly, which is why the app reads it in
    /// `main` and passes it down — the same value `seasonal_rollover` runs on.
    utc_offset_secs: i32,
    /// The tree waiting for the panel to go quiet, if there is one.
    staged: Option<Staged>,
    /// Where tonight has got to. Derived state the loop keeps between turns — the policy
    /// is a function and does not remember anything.
    phase: Phase,
    /// Where the TUF client keeps its cached metadata between refreshes.
    trust_cache: PathBuf,
    /// Whether the `hold` file was there last time round, so its appearance and its
    /// removal are each one log line rather than one per look.
    held: bool,
    /// How many looks in a row have come to nothing because something failed.
    ///
    /// The whole design fails closed — every error means "keep running what you are
    /// running, try again tomorrow" — and the cost of that is a panel which can stop
    /// updating for a month without ever saying anything louder than one `warn` a night,
    /// each indistinguishable from the last. This counter is what makes a wedge legible:
    /// the log line grows a count and, past a week, tells a human what to do about it.
    consecutive_failures: u32,
}

/// After this many failed nights, the log stops describing tonight and starts describing
/// the *situation*. A week is long enough that a flaky uplink has recovered and short
/// enough that somebody still remembers deploying whatever broke it.
const WEDGED_AFTER: u32 = 7;

/// A release that has been downloaded, verified and extracted under its final name.
#[derive(Debug, Clone)]
struct Staged {
    version: VersionId,
    manifest: Manifest,
}

/// What the agent concludes when it is time to restart.
#[derive(Debug, Clone)]
pub struct Activation {
    /// The version `current.txt` now names.
    pub version: VersionId,
    /// What it was before, so a log line can say what changed.
    pub replacing: VersionId,
}

impl Agent {
    /// Build an agent, or say why there will not be one.
    ///
    /// # Errors
    /// A [`StandDown`] variant for each of the guards, evaluated here rather than in the
    /// loop so the receiver's startup log states the situation once instead of every
    /// night.
    pub fn new(
        enabled: bool,
        policy: Policy,
        source: ReleaseSource,
        installed: InstalledBuild,
        activity: Arc<dyn PanelActivity>,
        utc_offset_secs: i32,
    ) -> Result<Self, StandDown> {
        if !enabled {
            return Err(StandDown::Disabled);
        }
        let provenance = Provenance::embedded().map_err(StandDown::TrustAnchors)?;
        if matches!(installed, InstalledBuild::Unknown) {
            return Err(StandDown::UnknownBuild);
        }
        let (tree, running) = InstallTree::of_running_receiver().map_err(StandDown::Unmanaged)?;
        if tree.hold().exists() {
            return Err(StandDown::Hold);
        }
        Ok(Self {
            tree,
            running,
            installed,
            policy,
            source,
            provenance,
            trust_cache: castaway_paths::host().cache().join("sigstore"),
            activity,
            utc_offset_secs,
            staged: None,
            phase: Phase::Fresh,
            held: false,
            consecutive_failures: 0,
        })
    }

    /// Run until it is time to restart into a new version.
    ///
    /// The caller is expected to shut the receiver down cleanly and exit with
    /// [`castaway_launcher::supervise::ACTIVATE_EXIT_CODE`]'s value — which this crate
    /// does not name, because the launcher owns that constant and the app is what wires
    /// the two together.
    pub async fn run(mut self) -> Activation {
        info!(
            version = %self.running.short(),
            build = %self.installed,
            window = %format_args!("{}–{}", self.policy.window.start(), self.policy.window.end()),
            "auto-update is armed"
        );
        // Once per boot, and before anything is staged: a tree left over from a night the
        // panel never went quiet is still good, and re-downloading it would be a quarter
        // of a gigabyte spent on nothing.
        self.staged = self.rediscover_staged();
        if let Some(staged) = &self.staged {
            info!(
                version = %staged.version.short(),
                "a staged update from an earlier night is still waiting"
            );
        }

        loop {
            // Re-read every turn rather than once at startup, because "delete it to
            // re-arm" is only half a contract: somebody who drops a `hold` file at 03:00
            // means it for 03:30, and a check made hours earlier would not have seen it.
            if self.holding() {
                tokio::time::sleep(
                    self.policy
                        .window
                        .until_open(self.local_minute())
                        .max(self.policy.recheck),
                )
                .await;
                continue;
            }

            let obs = Observation {
                at: self.local_minute(),
                phase: if self.staged.is_some() {
                    Phase::Staged
                } else {
                    self.phase
                },
                casting: self.activity.casting(),
                idle_for: self.activity.idle_for(),
            };
            match decide(&self.policy, &obs) {
                Action::Wait(d) => {
                    // Leaving the window resets tonight's memory: tomorrow is a fresh
                    // look, whatever happened this time.
                    if !self.policy.window.contains(obs.at) {
                        self.phase = Phase::Fresh;
                    }
                    debug!(minutes = d.as_secs() / 60, "auto-update: waiting");
                    tokio::time::sleep(d).await;
                }
                Action::Check => match self.check().await {
                    Ok(Some(staged)) => {
                        info!(
                            version = %staged.version.short(),
                            build = %staged.manifest.build,
                            "an update is staged and waiting for the panel to go quiet"
                        );
                        self.staged = Some(staged);
                    }
                    Ok(None) => {
                        self.consecutive_failures = 0;
                        self.phase = Phase::UpToDate;
                    }
                    Err(e) => {
                        // Every one of these — the API down, a bad signature, a corrupt
                        // zip, a full disk — has the same answer, and it is the answer a
                        // kiosk wants: nothing changes and it tries again tomorrow.
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                        self.report_failure(&e);
                        self.phase = Phase::UpToDate;
                    }
                },
                Action::Activate => {
                    // `decide` only reaches this arm when `self.staged` is `Some` — but
                    // the `else` is not decoration: without it a `None` here would spin
                    // the loop with nothing to sleep on, and so would a pointer write
                    // that keeps failing on a full disk. Every path out of this arm waits.
                    if let Some(staged) = self.staged.take() {
                        match self.activate(&staged.version) {
                            Ok(activation) => return activation,
                            Err(e) => {
                                warn!(error = %Chain(&e), "auto-update: could not activate");
                                // Put it back: the tree is fine, the pointer write was
                                // not, and the next look is a perfectly good time to retry.
                                self.staged = Some(staged);
                                tokio::time::sleep(self.policy.recheck).await;
                            }
                        }
                    } else {
                        tokio::time::sleep(self.policy.recheck).await;
                    }
                }
            }
        }
    }

    /// Say what went wrong tonight — and, once it has been going on long enough to be a
    /// state rather than a bad night, say *that* instead, in terms somebody standing at
    /// the panel can act on.
    ///
    /// Failing closed was chosen deliberately: a panel that has not updated in months
    /// wants a human, not a receiver that gets cleverer. The cost of that choice is paid
    /// here, in saying so plainly rather than leaving it to whoever thinks to read a log.
    fn report_failure(&self, error: &UpdateError) {
        let nights = self.consecutive_failures;
        if nights < WEDGED_AFTER {
            warn!(
                error = %Chain(error),
                nights,
                "auto-update: nothing taken tonight; the panel keeps running what it has"
            );
            return;
        }
        warn!(
            error = %Chain(error),
            nights,
            running = %self.running.short(),
            build = %self.installed,
            source = %self.source.latest_url(),
            "auto-update has taken nothing for {nights} nights running and needs a person. \
             This receiver keeps working — it is only the *updating* that is stuck. Check, \
             in this order: can the panel reach the release API at all (the URL above); does \
             that release carry manifest.json and manifest.json.minisig (a release published \
             before the signing key existed does not — see #347); and does the signature \
             verify against the key this build was compiled with. Every one of those fails \
             closed on purpose, so none of them will resolve itself."
        );
    }

    /// Is a human driving? One log line when it appears and one when it goes, because
    /// this is looked at every quarter of an hour and neither state is news twice.
    fn holding(&mut self) -> bool {
        let held = self.tree.hold().exists();
        if held != self.held {
            if held {
                info!("auto-update: a hold file appeared; standing down until it is removed");
            } else {
                info!("auto-update: the hold file is gone; armed again");
            }
            self.held = held;
        }
        held
    }

    /// The local time, to the minute, from one clock read.
    fn local_minute(&self) -> MinuteOfDay {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        MinuteOfDay::at_unix(unix, self.utc_offset_secs)
    }

    /// Is there already a complete, newer tree sitting in `versions/`?
    fn rediscover_staged(&self) -> Option<Staged> {
        let current = install::read_pointer(&self.tree, Pointer::Current).ok();
        let entries = std::fs::read_dir(self.tree.versions()).ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|n| VersionId::parse(n).ok()) else {
                continue;
            };
            if current.as_ref() == Some(&id) {
                continue;
            }
            // The manifest is written into the tree at staging time precisely so this is
            // answerable without the network.
            let Ok(bytes) = std::fs::read(entry.path().join(MANIFEST_ASSET)) else {
                continue;
            };
            let Ok(manifest) = Manifest::parse(&bytes) else {
                continue;
            };
            if manifest.offer_to(self.installed) == Offer::Newer {
                return Some(Staged {
                    version: id,
                    manifest,
                });
            }
        }
        None
    }

    /// One look at the release API. `Ok(None)` means there was nothing newer.
    async fn check(&mut self) -> Result<Option<Staged>, UpdateError> {
        // Freshen the trust root before looking. Sigstore adds logs and intermediates over
        // time — the embedded copy already carries two of each, one retired — and a panel
        // that has been up for months would otherwise still be judging releases by the
        // root it booted with. A failed refresh keeps the embedded one and says so.
        if let Ok(provenance) = Provenance::refreshed(Some(&self.trust_cache)).await {
            self.provenance = provenance;
        }

        let source = self.source.clone();
        let release = tokio::task::spawn_blocking(move || fetch_latest(&source))
            .await
            .map_err(|_| UpdateError::Cancelled)??;

        // The manifest first, and it is the *small* file for a reason that is structural
        // rather than thrifty. GitHub keys attestations by artifact digest, so verifying
        // the zip directly would mean downloading a quarter of a gigabyte before anything
        // could say whether the bytes were genuine — with no trustworthy size to bound
        // that download by. Verifying 240 bytes first yields both the digest and the size.
        let manifest_url = release.asset(MANIFEST_ASSET)?;
        let manifest_json =
            tokio::task::spawn_blocking(move || fetch_bytes(&manifest_url, 64 * 1024))
                .await
                .map_err(|_| UpdateError::Cancelled)??;

        // Provenance, then parse. The order is the point: a JSON parser is a lot of code
        // to aim at bytes a stranger chose, and the attestation is checkable against the
        // raw file.
        self.verify_provenance(&manifest_json).await?;
        let manifest = Manifest::parse(&manifest_json)?;
        match manifest.offer_to(self.installed) {
            Offer::Newer => {}
            Offer::NotNewer => {
                debug!(
                    offered = %manifest.build,
                    installed = %self.installed,
                    "auto-update: Latest is not newer than what is running"
                );
                return Ok(None);
            }
            // `Agent::new` refuses to build in this state, so reaching it would mean the
            // stamp changed under a running process. Refusing is still the right answer.
            Offer::Unorderable => return Ok(None),
        }

        let version = VersionId::parse(manifest.commit.as_str())
            .map_err(|source| UpdateError::Layout { source })?;
        // Already there and complete — which happens when a staging succeeded and the
        // panel then never went quiet, and the phase was lost to a restart.
        if self
            .tree
            .version(&version)
            .path()
            .join(MANIFEST_ASSET)
            .exists()
        {
            return Ok(Some(Staged { version, manifest }));
        }

        let url = release.asset(manifest.artifact.as_str())?;
        let staged = self.stage(&version, &manifest, url).await?;
        Ok(Some(staged))
    }

    /// Fetch the build provenance for exactly these bytes and check it.
    ///
    /// The attestation is looked up by the artifact's own digest, which is what makes this
    /// safe to do over an API response nothing has yet vouched for: a server that returns
    /// somebody else's bundle fails the digest comparison inside the verifier, and one that
    /// returns nothing fails here. Neither can make us accept the wrong bytes.
    ///
    /// A release may carry more than one attestation, so each is tried and the first that
    /// verifies wins. That is not laxity — every one of them has to be a bundle signed by
    /// the trusted workflow *over this digest*; being offered several wrong ones is
    /// indistinguishable from being offered none.
    async fn verify_provenance(&self, artifact: &[u8]) -> Result<(), UpdateError> {
        use sha2::Digest as _;
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(artifact));
        let url = format!(
            "{}/repos/{}/attestations/{digest}",
            self.source.base_url.trim_end_matches('/'),
            self.source.repository
        );
        let body = tokio::task::spawn_blocking(move || fetch_bytes(&url, MAX_BUNDLE_BYTES))
            .await
            .map_err(|_| UpdateError::Cancelled)??;
        let listing: AttestationListing =
            serde_json::from_slice(&body).map_err(UpdateError::AttestationListing)?;
        if listing.attestations.is_empty() {
            return Err(UpdateError::NoAttestation { digest });
        }

        let mut last = None;
        for entry in listing.attestations {
            let bundle = entry.bundle.to_string();
            match self.provenance.verify(artifact, &bundle).await {
                Ok(()) => {
                    info!(
                        signer = %self.provenance.identity(),
                        "auto-update: the release carries provenance from the trusted workflow"
                    );
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
        }
        Err(UpdateError::Attestation {
            source: Box::new(last.expect("the listing was not empty")),
        })
    }

    /// Download, verify and extract, then name it — in that order, and never sooner.
    async fn stage(
        &self,
        version: &VersionId,
        manifest: &Manifest,
        url: String,
    ) -> Result<Staged, UpdateError> {
        let staging = self.tree.staging(version);
        let final_path = self.tree.version(version).path().to_path_buf();
        let expected = manifest.sha256;
        let size = manifest.size;
        let signed_browser = self.tree.version(&self.running).path().join("browser");
        let running_is_vmp_signed = has_vmp_signatures(&signed_browser);

        info!(
            version = %version.short(),
            mb = size / (1024 * 1024),
            "auto-update: staging"
        );
        let staging_for_task = staging.clone();
        let final_for_task = final_path.clone();
        tokio::task::spawn_blocking(move || {
            // A leftover from a download that died halfway: it was never named, so
            // removing it is free.
            let _ = std::fs::remove_dir_all(&staging_for_task);
            std::fs::create_dir_all(&staging_for_task).map_err(|source| UpdateError::Io {
                what: "creating the staging directory",
                path: staging_for_task.clone(),
                source,
            })?;
            let zip = staging_for_task.join("artifact.zip");
            let digest = download_to(&url, &zip, size)?;
            if digest != expected {
                return Err(UpdateError::DigestMismatch {
                    expected: expected.to_string(),
                    actual: digest.to_string(),
                });
            }
            let tree = staging_for_task.join("tree");
            unzip_stripping_top_level(&zip, &tree)?;
            let _ = std::fs::remove_file(&zip);

            // The #344 landmine, checked from the receiver's side: a tree whose Electron
            // binaries carry no VMP signature plays fine against Widevine's test service
            // and is refused licences by the real one. If the version *running* is signed
            // and the one offered is not, something in the release path stopped signing —
            // and taking that update would break DRM on an unattended panel with no
            // symptom other than "Netflix stopped working".
            if running_is_vmp_signed && !has_vmp_signatures(&tree.join("browser")) {
                return Err(UpdateError::UnsignedBrowser);
            }

            // Named last, and atomically. Until this rename the launcher cannot spawn it,
            // because a `.staging-` directory is not a name `VersionId::parse` accepts.
            std::fs::rename(&tree, &final_for_task).map_err(|source| UpdateError::Io {
                what: "naming the staged tree",
                path: final_for_task.clone(),
                source,
            })?;
            let _ = std::fs::remove_dir_all(&staging_for_task);
            Ok::<_, UpdateError>(())
        })
        .await
        .map_err(|_| UpdateError::Cancelled)??;

        // The manifest travels into the tree so a later boot can rediscover what this is
        // without asking the network — and so a human looking at `versions/` can tell.
        let manifest_path = final_path.join(MANIFEST_ASSET);
        if let Ok(bytes) = serde_json::to_vec_pretty(manifest) {
            let _ = std::fs::write(&manifest_path, bytes);
        }

        Ok(Staged {
            version: version.clone(),
            manifest: manifest.clone(),
        })
    }

    /// Move the pointers. The launcher does the rest.
    fn activate(&self, version: &VersionId) -> Result<Activation, UpdateError> {
        let replacing = install::read_pointer(&self.tree, Pointer::Current)
            .map_err(|source| UpdateError::Layout { source })?;
        // Previous first: if the machine loses power between these two writes, the worst
        // state is a `previous.txt` naming what is still current, which costs one
        // unavailable rollback rather than a pointer pair that names nothing.
        install::write_pointer(&self.tree, Pointer::Previous, &replacing)
            .map_err(|source| UpdateError::Layout { source })?;
        install::write_pointer(&self.tree, Pointer::Current, version)
            .map_err(|source| UpdateError::Layout { source })?;
        info!(
            from = %replacing.short(),
            to = %version.short(),
            "auto-update: restarting into the new version"
        );
        Ok(Activation {
            version: version.clone(),
            replacing,
        })
    }
}

/// Mark the running version healthy, then tidy up.
///
/// Called by the app once every enabled adapter is bound and advertising, after a delay
/// long enough that "it came up" means something. The marker is what the launcher's
/// rollback rule reads: a version that has written it once is never rolled back again,
/// because its later crashes are a bad night rather than bad bits.
///
/// Failures are logged and ignored. A marker that could not be written costs one
/// unnecessary rollback in a future crash loop; refusing to serve casts over it would
/// cost the panel.
pub fn mark_healthy_and_tidy(installed: InstalledBuild) {
    let Ok((tree, running)) = InstallTree::of_running_receiver() else {
        return;
    };
    let marker = tree.version(&running).health_marker();
    match std::fs::write(&marker, b"") {
        Ok(()) => info!(version = %running.short(), "this version is up; marked healthy"),
        Err(e) => {
            warn!(error = %e, path = %marker.display(), "could not mark this version healthy")
        }
    }
    collect_old_versions(&tree, &running, installed);
}

/// Delete every version tree that is neither running, nor the rollback target, nor a
/// staged update waiting for tonight.
///
/// `deploy-windows`' hard-won lesson applies verbatim: **verify the delete happened**.
/// `rmdir` reports success while leaving the tree behind when a handle is open, and
/// Defender and the search indexer both hold handles on a freshly written directory for
/// a while. So a straggler is tolerated and left for the next boot rather than treated as
/// a failure — the disk cost of one extra tree is a few hundred megabytes, and the cost
/// of deleting one that is still in use is a panel that does not start.
fn collect_old_versions(tree: &InstallTree, running: &VersionId, installed: InstalledBuild) {
    let previous = install::read_pointer(tree, Pointer::Previous).ok();
    let Ok(entries) = std::fs::read_dir(tree.versions()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| VersionId::parse(n).ok()) else {
            // Not a version name — a `.staging-` directory from a download that died.
            // Those are free to remove, and nothing else has a claim on them.
            if name.to_string_lossy().starts_with(".staging-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
            continue;
        };
        if &id == running || previous.as_ref() == Some(&id) {
            continue;
        }
        // A tree newer than what is running is an update waiting for a quiet night, not
        // rubbish. Deleting it would make a panel that is busy every night re-download a
        // quarter of a gigabyte for ever.
        if is_newer_than(&entry.path(), installed) {
            continue;
        }
        if std::fs::remove_dir_all(entry.path()).is_err() || entry.path().exists() {
            debug!(
                version = %id.short(),
                "an old version could not be removed yet; leaving it for the next boot"
            );
        } else {
            info!(version = %id.short(), "removed an old version");
        }
    }
}

fn is_newer_than(dir: &Path, installed: InstalledBuild) -> bool {
    std::fs::read(dir.join(MANIFEST_ASSET))
        .ok()
        .and_then(|bytes| Manifest::parse(&bytes).ok())
        .is_some_and(|m| m.offer_to(installed) == Offer::Newer)
}

/// Does this browser tree carry castLabs VMP signatures?
fn has_vmp_signatures(browser: &Path) -> bool {
    std::fs::read_dir(browser).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sig"))
        })
    })
}

/// GitHub's attestation listing: `{"attestations": [{"bundle": {...}}]}`.
///
/// The bundle is kept as raw JSON rather than parsed here, because the thing that parses
/// a Sigstore bundle is the verifier, and handing it the bytes it was given keeps this
/// from becoming a second opinion about the format.
#[derive(Debug, Deserialize)]
struct AttestationListing {
    #[serde(default)]
    attestations: Vec<AttestationEntry>,
}

#[derive(Debug, Deserialize)]
struct AttestationEntry {
    bundle: serde_json::Value,
}

/// The two fields of a GitHub release this receiver reads.
///
/// Not `deny_unknown_fields`: the API returns fifty of them and adds more, and none of
/// them is trusted anyway — the signature is what decides, and this is only how the bytes
/// are found.
#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn asset(&self, name: &str) -> Result<String, UpdateError> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| UpdateError::NoAsset {
                name: name.to_owned(),
                release: self.tag_name.clone(),
            })
    }
}

/// How long any single request may take. Generous for the artifact — a quarter of a
/// gigabyte over a hackerspace's uplink at three in the morning is not fast — and the
/// window is ninety minutes wide, so a download that cannot finish inside this was never
/// going to finish inside the window either.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Separately, because `ureq`'s connect phase has its own thirty-second default that the
/// overall timeout does not govern — the same trap `proto-dlna` documents.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// An honest User-Agent. The panel makes one HTTPS request a night; saying what it is
/// costs nothing and is most of the difference between a kiosk and something furtive.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(REQUEST_TIMEOUT)
        .timeout_connect(CONNECT_TIMEOUT)
        .redirects(8)
        .user_agent(concat!(
            "castaway/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/schlarpc/castaway; auto-update)"
        ))
        .build()
}

fn fetch_latest(source: &ReleaseSource) -> Result<Release, UpdateError> {
    let url = source.latest_url();
    let body = agent()
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|source| UpdateError::Http {
            url: url.clone(),
            source: Box::new(source),
        })?;
    let mut text = String::new();
    body.into_reader()
        .take(1024 * 1024)
        .read_to_string(&mut text)
        .map_err(|source| UpdateError::Io {
            what: "reading the release listing",
            path: PathBuf::from(&url),
            source,
        })?;
    serde_json::from_str(&text).map_err(UpdateError::ReleaseJson)
}

fn fetch_bytes(url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let response = agent()
        .get(url)
        .call()
        .map_err(|source| UpdateError::Http {
            url: url.to_owned(),
            source: Box::new(source),
        })?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|source| UpdateError::Io {
            what: "reading",
            path: PathBuf::from(url),
            source,
        })?;
    Ok(bytes)
}

/// Stream `url` into `path`, hashing as it goes and refusing to write more than `size`.
///
/// Hashed on the way past rather than by re-reading the file: a quarter of a gigabyte
/// read twice on a panel's disk is a minute nobody gets back, and the bound is what stops
/// a server that keeps talking from filling the disk before the digest can disagree.
fn download_to(url: &str, path: &Path, size: u64) -> Result<Sha256Digest, UpdateError> {
    use sha2::Digest as _;

    let response = agent()
        .get(url)
        .call()
        .map_err(|source| UpdateError::Http {
            url: url.to_owned(),
            source: Box::new(source),
        })?;
    let mut out = std::fs::File::create(path).map_err(|source| UpdateError::Io {
        what: "creating",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = sha2::Sha256::new();
    let mut reader = response.into_reader().take(size);
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader.read(&mut buffer).map_err(|source| UpdateError::Io {
            what: "downloading",
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        std::io::Write::write_all(&mut out, &buffer[..n]).map_err(|source| UpdateError::Io {
            what: "writing",
            path: path.to_path_buf(),
            source,
        })?;
        written += n as u64;
    }
    if written != size {
        return Err(UpdateError::ShortDownload {
            expected: size,
            actual: written,
        });
    }
    Ok(Sha256Digest::finish(hasher))
}

/// Extract `zip` into `into`, dropping the single wrapping directory the archive carries.
///
/// The release archive holds one top-level directory named for the artifact
/// (`nix/windows.nix`'s `mkArchive`), and a version tree holds the receiver at its root.
/// Stripping it here is what makes `versions/<sha>/castaway.exe` true.
///
/// Every entry's path is checked against the destination before it is opened. The zip
/// came off a signed manifest's digest, so this is defence in depth rather than the first
/// line — but a path-traversal check that only runs when you expect trouble is not a
/// check.
fn unzip_stripping_top_level(zip: &Path, into: &Path) -> Result<(), UpdateError> {
    let file = std::fs::File::open(zip).map_err(|source| UpdateError::Io {
        what: "opening",
        path: zip.to_path_buf(),
        source,
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(UpdateError::Zip)?;
    std::fs::create_dir_all(into).map_err(|source| UpdateError::Io {
        what: "creating",
        path: into.to_path_buf(),
        source,
    })?;

    let mut wrapper: Option<std::ffi::OsString> = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(UpdateError::Zip)?;
        let Some(name) = entry.enclosed_name() else {
            return Err(UpdateError::HostileEntry {
                name: entry.name().to_owned(),
            });
        };
        // Drop the wrapper directory — and check there is exactly one of it. An archive
        // whose entries are not all under a single top level is not the archive this
        // receiver knows how to install, and merging two of them would produce a tree that
        // is neither.
        let mut parts = name.components();
        let Some(top) = parts.next() else { continue };
        match &wrapper {
            None => wrapper = Some(top.as_os_str().to_owned()),
            Some(first) if first == top.as_os_str() => {}
            Some(_) => {
                return Err(UpdateError::HostileEntry {
                    name: entry.name().to_owned(),
                })
            }
        }
        let relative: PathBuf = parts.collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = into.join(&relative);
        if !target.starts_with(into) {
            return Err(UpdateError::HostileEntry {
                name: entry.name().to_owned(),
            });
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&target).map_err(|source| UpdateError::Io {
                what: "creating",
                path: target.clone(),
                source,
            })?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| UpdateError::Io {
                what: "creating",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut out = std::fs::File::create(&target).map_err(|source| UpdateError::Io {
            what: "creating",
            path: target.clone(),
            source,
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|source| UpdateError::Io {
            what: "extracting",
            path: target.clone(),
            source,
        })?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode));
        }
    }
    Ok(())
}

/// Everything that can stop one night's update. All of them mean the same thing to the
/// panel — keep running what you are running — and they are separate variants so the log
/// line says which.
#[derive(Debug, Error)]
pub enum UpdateError {
    /// The request failed, or the server said no.
    #[error("fetching {url}")]
    Http {
        /// What was being fetched.
        url: String,
        /// What the client said.
        #[source]
        source: Box<ureq::Error>,
    },
    /// The release listing was not the JSON this receiver reads.
    #[error("the release listing")]
    ReleaseJson(#[source] serde_json::Error),
    /// A release without the asset the updater needs. The usual cause is a release
    /// published before the signing secret existed (#347).
    #[error("release {release} carries no {name}")]
    NoAsset {
        /// Which asset.
        name: String,
        /// Which release.
        release: String,
    },
    /// The attestation listing was not the JSON this build reads.
    #[error("the attestation listing")]
    AttestationListing(#[source] serde_json::Error),
    /// GitHub has no attestation for these bytes. The usual cause is a release published
    /// before the workflow attested anything.
    #[error("no build provenance exists for {digest}")]
    NoAttestation {
        /// The digest that was looked up.
        digest: String,
    },
    /// There was provenance and it did not check out.
    #[error("the build provenance")]
    Attestation {
        /// What the verifier objected to.
        #[source]
        source: Box<AttestationError>,
    },
    /// The manifest was not one this build understands.
    #[error("the release manifest")]
    Manifest(#[from] crate::ManifestError),
    /// The artifact's digest is not the one the signed manifest names.
    #[error("the artifact hashes {actual}, the manifest says {expected}")]
    DigestMismatch {
        /// What the manifest claimed.
        expected: String,
        /// What arrived.
        actual: String,
    },
    /// The download stopped early. A separate variant from a digest mismatch because the
    /// causes are different — a dropped connection, not a substituted file — and so is
    /// what a reader should conclude from seeing it twice.
    #[error("the download ended after {actual} of {expected} bytes")]
    ShortDownload {
        /// What the manifest claimed.
        expected: u64,
        /// What arrived.
        actual: u64,
    },
    /// An archive entry whose path escapes the destination.
    #[error("the archive holds an entry named {name:?}")]
    HostileEntry {
        /// The entry's own name.
        name: String,
    },
    /// The offered tree's Electron binaries carry no VMP signature and the running one's
    /// do. Taking it would kill DRM playback silently (#344).
    #[error("the offered release is not VMP-signed and the running one is")]
    UnsignedBrowser,
    /// The zip could not be read.
    #[error("the artifact archive")]
    Zip(#[source] zip::result::ZipError),
    /// Something on disk refused.
    #[error("{what} {path}")]
    Io {
        /// Which operation.
        what: &'static str,
        /// Which path.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
    /// The install tree could not be read or written.
    #[error("the install tree")]
    Layout {
        /// What the layout said.
        #[source]
        source: LayoutError,
    },
    /// The receiver is shutting down.
    #[error("cancelled")]
    Cancelled,
}

/// An error and its causes on one line, because `tracing` renders only the outermost.
struct Chain<'a>(&'a dyn std::error::Error);

impl std::fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}
