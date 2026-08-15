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
//!
//! **One thing fetches, verifies, stages and activates, and this is it** (#360). A person
//! standing at the panel can ask for a check now, through [`Handle`], and what answers is
//! this same agent on its own loop — a request arriving mid-stage is serialised behind it
//! rather than racing it for the same `versions/` directory. What a manual request skips
//! is the *schedule* and nothing else: [`crate::policy::Request::Manual`] passes the
//! window and the idle gate, and every step of the order of operations above still runs.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use castaway_paths::install::{self, InstallTree, LayoutError, Pointer, VersionId};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::attestation::{AttestationError, Provenance};
use crate::manifest::{BuildNumber, InstalledBuild, Manifest, Offer, Sha256Digest};
use crate::policy::{decide, Action, MinuteOfDay, Observation, Phase, Policy, Request};

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
    /// `[update] enable = false`. Not returned by [`Agent::new`] — the flag stands the
    /// *nightly loop* down, not the agent, so a person can still ask for a check (#360).
    /// It is here because the sentence still has to be said, and this is where the
    /// updater's stand-down prose lives.
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
    /// Re-read every turn of the loop and on every press rather than once at startup, so
    /// "delete it to re-arm" is true without a restart.
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

/// What somebody at the panel can ask the updater for (#360).
///
/// Two, and neither carries a reply: [`Progress`] on the watch channel is the single
/// account of what is happening, and a second one would be a second thing to disagree
/// with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Look now, whatever the hour, and say what is there.
    CheckNow,
    /// Take what the last check found: download it if it is not already on disk, then
    /// move the pointers and hand back to the launcher.
    ActivateNow,
}

/// Where the updater has got to, in terms a screen can render.
///
/// An enum rather than a percentage and some flags, so a screen's `match` stops compiling
/// when a phase is added (ground rule 1) — there is no "unknown state" arm for a new one
/// to fall silently into. Deliberately **not** `#[non_exhaustive]`: the screen that
/// renders this is in another crate, and marking it so would force the wildcard arm this
/// type exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Nothing has been asked of it yet.
    Idle,
    /// Asking the release API what Latest is.
    Checking,
    /// Latest is not newer than what is running. Names the running build, because "up to
    /// date" with no number is not something the person reading it can check.
    UpToDate {
        /// What is running.
        running: InstalledBuild,
    },
    /// Something newer exists and **nothing has been downloaded**. The offer, and the
    /// three facts the signed manifest carries about it.
    Offer {
        /// Its build number.
        build: BuildNumber,
        /// Its short commit.
        commit: String,
        /// How large the download is, from the signed manifest rather than from a
        /// `Content-Length` a server chose.
        bytes: u64,
    },
    /// Fetching the artifact. `total` is the signed manifest's size.
    Downloading {
        /// Bytes written so far.
        received: u64,
        /// Bytes the manifest says there are.
        total: u64,
    },
    /// Checking what arrived against the digest the manifest declared.
    Verifying,
    /// Unpacking it into the install tree.
    Extracting,
    /// Downloaded, verified, extracted and named: a restart away.
    Staged {
        /// Its build number.
        build: BuildNumber,
        /// Its short commit.
        commit: String,
    },
    /// The pointers have moved and the process is leaving.
    Restarting,
    /// It could not be done, in the words [`UpdateError`] already carries.
    Failed {
        /// The error and its causes.
        why: String,
    },
    /// There is no updating to be had on this build, or not right now. Carries the
    /// [`StandDown`] prose, which is written to be read by a person.
    StoodDown {
        /// Why.
        why: String,
    },
}

/// The panel's end of the updater: ask it things, watch what it does.
///
/// Cloneable and cheap. It stays useful when there is no agent at all — a build that
/// stood down publishes its reason through the same channel, so the settings screen has
/// something to say rather than a row that does nothing.
#[derive(Debug, Clone)]
pub struct Handle {
    commands: mpsc::Sender<Command>,
    progress: watch::Receiver<Progress>,
}

impl Handle {
    /// Ask for a check now. `false` means nothing is listening — the agent stood down, or
    /// somebody is pressing faster than it can answer.
    pub fn check_now(&self) -> bool {
        self.commands.try_send(Command::CheckNow).is_ok()
    }

    /// Ask for the offered version to be taken. `false` as for [`Self::check_now`].
    pub fn activate_now(&self) -> bool {
        self.commands.try_send(Command::ActivateNow).is_ok()
    }

    /// A fresh view of what it is doing. The latest state, not every state: a screen
    /// waking after a slow redraw wants the current number, not the queue of old ones.
    #[must_use]
    pub fn progress(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }
}

/// The agent's end of the same, handed to [`Agent::run`].
///
/// Separate from [`Agent::new`] on purpose: `new` is where the stand-downs are decided,
/// and a stand-down still has to be *said*. Keeping this out of the constructor means the
/// caller still holds it on the failing path and can publish the reason.
pub struct Inbox {
    commands: mpsc::Receiver<Command>,
    /// A sender the agent keeps to itself, so `recv` never resolves to `None` and the
    /// loop's `select!` cannot spin against a closed channel once the panel is gone.
    keepalive: mpsc::Sender<Command>,
    progress: watch::Sender<Progress>,
}

impl Inbox {
    /// Say why there is no updater, for the screen that is going to ask.
    pub fn stand_down(self, why: &StandDown) {
        let _ = self.progress.send(Progress::StoodDown {
            why: Chain(why).to_string(),
        });
    }
}

/// How many presses may be outstanding. A person cannot mean four things at once; the
/// slack is so a press during a minutes-long stage is not dropped for being early.
const COMMAND_SLACK: usize = 4;

/// Both ends of the updater's control surface.
///
/// Built before the agent, because the settings catalog needs the [`Handle`] whether or
/// not there turns out to be an agent behind it.
#[must_use]
pub fn control() -> (Handle, Inbox) {
    let (tx, rx) = mpsc::channel(COMMAND_SLACK);
    let (progress_tx, progress_rx) = watch::channel(Progress::Idle);
    (
        Handle {
            commands: tx.clone(),
            progress: progress_rx,
        },
        Inbox {
            commands: rx,
            keepalive: tx,
            progress: progress_tx,
        },
    )
}

/// Where progress goes. A newtype so every report is one call and a dropped receiver — an
/// unattended panel, which is the normal case — is ignored in one place.
#[derive(Clone)]
struct Reporter(watch::Sender<Progress>);

impl Reporter {
    fn say(&self, progress: Progress) {
        // `send` fails only when nothing is listening, and nothing listening is what an
        // unattended panel looks like every night of its life.
        let _ = self.0.send(progress);
    }
}

/// A release the API offered that is not on disk yet.
#[derive(Debug, Clone)]
struct Available {
    version: VersionId,
    manifest: Manifest,
    url: String,
}

/// What one look at the release API concluded.
#[derive(Debug, Clone)]
enum Looked {
    /// Latest is not newer than what is running.
    UpToDate,
    /// Newer, and already complete on disk — staged on a night the panel never went
    /// quiet, or by a check the person made a moment ago.
    AlreadyStaged(Staged),
    /// Newer, and nothing has been downloaded.
    Available(Available),
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
    /// Whether the nightly loop runs. `false` is `[update] enable = false`, which stands
    /// the *schedule* down and leaves the agent alive: a press on "Check for updates" is
    /// not the receiver updating itself, it is a person asking, and the flag was never a
    /// statement about that (#360).
    armed: bool,
    /// The tree waiting for the panel to go quiet, if there is one.
    staged: Option<Staged>,
    /// What a manual check found and has not downloaded. Nothing is fetched until the
    /// person presses the offer, so this is the whole of what that press needs.
    offer: Option<Available>,
    /// Whether to freshen the Sigstore trust root before looking. Off in tests, which
    /// verify a checked-in bundle against the checked-in root and must not depend on
    /// reaching a TUF repository.
    refresh_trust: bool,
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

impl Staged {
    /// How it reads on the glass: complete, and one restart away.
    fn as_progress(&self) -> Progress {
        Progress::Staged {
            build: self.manifest.build,
            commit: self.version.short().to_string(),
        }
    }
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
    /// `armed` is `[update] enable`: it decides whether the nightly loop runs, not
    /// whether the agent exists. Every other guard below is a fact about this *build*
    /// rather than a preference, and each is a refusal.
    ///
    /// # Errors
    /// A [`StandDown`] variant for each of those, evaluated here rather than in the loop
    /// so the receiver's startup log states the situation once instead of every night.
    /// [`StandDown::Hold`] is not among them: a `hold` file is somebody's live decision,
    /// re-read every turn and on every press, so that "delete it to re-arm" is true
    /// without a restart.
    pub fn new(
        armed: bool,
        policy: Policy,
        source: ReleaseSource,
        installed: InstalledBuild,
        activity: Arc<dyn PanelActivity>,
        utc_offset_secs: i32,
    ) -> Result<Self, StandDown> {
        let provenance = Provenance::embedded().map_err(StandDown::TrustAnchors)?;
        if matches!(installed, InstalledBuild::Unknown) {
            return Err(StandDown::UnknownBuild);
        }
        let (tree, running) = InstallTree::of_running_receiver().map_err(StandDown::Unmanaged)?;
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
            armed,
            staged: None,
            offer: None,
            refresh_trust: true,
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
    pub async fn run(mut self, inbox: Inbox) -> Activation {
        let Inbox {
            mut commands,
            // Held for as long as the loop runs, so `recv` below has no `None` to spin on
            // once the panel's own handle is dropped.
            keepalive: _keepalive,
            progress,
        } = inbox;
        let report = Reporter(progress);
        if self.armed {
            info!(
                version = %self.running.short(),
                build = %self.installed,
                window = %format_args!("{}–{}", self.policy.window.start(), self.policy.window.end()),
                "auto-update is armed"
            );
        } else {
            info!(
                version = %self.running.short(),
                build = %self.installed,
                "auto-update: automatic updates are off (`[update] enable = false`); a check \
                 asked for from the settings screen still works"
            );
        }
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
            if !self.armed {
                // No schedule to keep, so the only thing that can happen here is somebody
                // asking. `keepalive` is why this cannot resolve to `None` and spin.
                if let Some(command) = commands.recv().await {
                    if let Some(activation) = self.on_command(command, &report).await {
                        return activation;
                    }
                }
                continue;
            }
            // Re-read every turn rather than once at startup, because "delete it to
            // re-arm" is only half a contract: somebody who drops a `hold` file at 03:00
            // means it for 03:30, and a check made hours earlier would not have seen it.
            if self.holding() {
                let until = self
                    .policy
                    .window
                    .until_open(self.local_minute())
                    .max(self.policy.recheck);
                if let Some(activation) = self.sleep_or_serve(until, &mut commands, &report).await {
                    return activation;
                }
                continue;
            }

            let obs = self.observe(Request::Scheduled);
            match decide(&self.policy, &obs) {
                Action::Wait(d) => {
                    // Leaving the window resets tonight's memory: tomorrow is a fresh
                    // look, whatever happened this time.
                    if !self.policy.window.contains(obs.at) {
                        self.phase = Phase::Fresh;
                    }
                    debug!(minutes = d.as_secs() / 60, "auto-update: waiting");
                    if let Some(activation) = self.sleep_or_serve(d, &mut commands, &report).await {
                        return activation;
                    }
                }
                Action::Check => match self.check(&report).await {
                    Ok(Some(staged)) => {
                        info!(
                            version = %staged.version.short(),
                            build = %staged.manifest.build,
                            "an update is staged and waiting for the panel to go quiet"
                        );
                        report.say(staged.as_progress());
                        self.staged = Some(staged);
                    }
                    Ok(None) => {
                        self.consecutive_failures = 0;
                        self.phase = Phase::UpToDate;
                        report.say(Progress::UpToDate {
                            running: self.installed,
                        });
                    }
                    Err(e) => {
                        // Every one of these — the API down, a bad signature, a corrupt
                        // zip, a full disk — has the same answer, and it is the answer a
                        // kiosk wants: nothing changes and it tries again tomorrow.
                        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                        self.report_failure(&e);
                        report.say(Progress::Failed {
                            why: Chain(&e).to_string(),
                        });
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
                            Ok(activation) => {
                                report.say(Progress::Restarting);
                                return activation;
                            }
                            Err(e) => {
                                warn!(error = %Chain(&e), "auto-update: could not activate");
                                // Put it back: the tree is fine, the pointer write was
                                // not, and the next look is a perfectly good time to retry.
                                self.staged = Some(staged);
                                let recheck = self.policy.recheck;
                                if let Some(activation) =
                                    self.sleep_or_serve(recheck, &mut commands, &report).await
                                {
                                    return activation;
                                }
                            }
                        }
                    } else {
                        let recheck = self.policy.recheck;
                        if let Some(activation) =
                            self.sleep_or_serve(recheck, &mut commands, &report).await
                        {
                            return activation;
                        }
                    }
                }
            }
        }
    }

    /// Wait `d`, unless somebody presses something first — in which case answer that
    /// instead, and let the loop work out where it stands afterwards.
    ///
    /// Every sleep in the loop goes through here, which is what makes "the agent is the
    /// only owner of the install tree" true: a request that arrives while the agent is
    /// mid-stage is not seen until the stage returns, so it queues behind it rather than
    /// racing it for the same staging path.
    async fn sleep_or_serve(
        &mut self,
        d: Duration,
        commands: &mut mpsc::Receiver<Command>,
        report: &Reporter,
    ) -> Option<Activation> {
        let command = tokio::select! {
            () = tokio::time::sleep(d) => None,
            command = commands.recv() => command,
        };
        match command {
            Some(command) => self.on_command(command, report).await,
            None => None,
        }
    }

    /// What the panel looks like to the policy, from one clock read.
    fn observe(&self, request: Request) -> Observation {
        Observation {
            at: self.local_minute(),
            phase: if self.staged.is_some() {
                Phase::Staged
            } else {
                self.phase
            },
            casting: self.activity.casting(),
            idle_for: self.activity.idle_for(),
            request,
        }
    }

    /// Answer somebody standing at the panel.
    async fn on_command(&mut self, command: Command, report: &Reporter) -> Option<Activation> {
        // A `hold` file is a person's decision and outranks another person's press: the
        // one who deployed by hand is not here to be asked, and the file names itself so
        // whoever is here can act on it.
        if self.holding() {
            report.say(Progress::StoodDown {
                why: format!("{}\n{}", StandDown::Hold, self.tree.hold().display()),
            });
            return None;
        }
        let obs = self.observe(Request::Manual);
        match (command, decide(&self.policy, &obs)) {
            (Command::CheckNow, Action::Check) => {
                self.manual_check(report).await;
                None
            }
            // A tree is already staged, so "is there anything newer?" is answered on disk:
            // no download to do, only a restart to offer.
            (Command::CheckNow, Action::Activate) => {
                if let Some(staged) = &self.staged {
                    report.say(staged.as_progress());
                }
                None
            }
            (Command::ActivateNow, _) => self.manual_activate(report).await,
            // `decide` never tells a manual request to wait — the policy tests are
            // exhaustive over its inputs on that point. Saying so beats panicking on a
            // wall, and beats silence, which is indistinguishable from a broken row.
            (Command::CheckNow, Action::Wait(_)) => {
                warn!("auto-update: the schedule refused a manual check, which should not happen");
                report.say(Progress::Failed {
                    why: "the updater would not answer a check just now".to_string(),
                });
                None
            }
        }
    }

    /// One look, asked for by a person: report what is there and download nothing.
    ///
    /// Nothing is fetched here beyond the manifest and its attestation — a quarter of a
    /// gigabyte is not something to start on somebody's behalf without asking, and the
    /// offer this leaves behind is what the asking is.
    async fn manual_check(&mut self, report: &Reporter) {
        report.say(Progress::Checking);
        match self.look().await {
            Ok(Looked::UpToDate) => {
                self.consecutive_failures = 0;
                self.phase = Phase::UpToDate;
                report.say(Progress::UpToDate {
                    running: self.installed,
                });
            }
            Ok(Looked::AlreadyStaged(staged)) => {
                self.consecutive_failures = 0;
                report.say(staged.as_progress());
                self.staged = Some(staged);
            }
            Ok(Looked::Available(available)) => {
                self.consecutive_failures = 0;
                info!(
                    version = %available.version.short(),
                    build = %available.manifest.build,
                    "auto-update: a newer release is on offer to the person at the panel"
                );
                report.say(Progress::Offer {
                    build: available.manifest.build,
                    commit: available.version.short().to_string(),
                    bytes: available.manifest.size,
                });
                self.offer = Some(available);
            }
            Err(e) => {
                warn!(error = %Chain(&e), "auto-update: a manual check found nothing it could trust");
                report.say(Progress::Failed {
                    why: Chain(&e).to_string(),
                });
            }
        }
    }

    /// Take what the last check found, with somebody watching it happen.
    async fn manual_activate(&mut self, report: &Reporter) -> Option<Activation> {
        if self.staged.is_none() {
            let Some(available) = self.offer.take() else {
                // Pressed with nothing on offer — a screen from before a restart, or a
                // stale one. Saying so beats doing nothing, which reads as a dead row.
                report.say(Progress::Failed {
                    why: "there is nothing to install; check again".to_string(),
                });
                return None;
            };
            match self
                .stage(
                    &available.version,
                    &available.manifest,
                    available.url,
                    report,
                )
                .await
            {
                Ok(staged) => self.staged = Some(staged),
                Err(e) => {
                    warn!(error = %Chain(&e), "auto-update: the staging somebody asked for failed");
                    report.say(Progress::Failed {
                        why: Chain(&e).to_string(),
                    });
                    return None;
                }
            }
        }
        let staged = self.staged.take()?;
        match self.activate(&staged.version) {
            Ok(activation) => {
                report.say(Progress::Restarting);
                Some(activation)
            }
            Err(e) => {
                warn!(error = %Chain(&e), "auto-update: could not activate");
                // The tree is fine; the pointer write was not. Keep it — tonight's loop
                // and the next boot both still find it.
                report.say(Progress::Failed {
                    why: Chain(&e).to_string(),
                });
                self.staged = Some(staged);
                None
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
             that release carry manifest.json (a release published before the workflow \
             attested anything does not); and does GitHub hold provenance for that manifest \
             from the workflow this build trusts (`gh attestation verify`). Every one of \
             those fails closed on purpose, so none of them will resolve itself."
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

    /// The nightly path: look, and stage whatever is newer. `Ok(None)` means there was
    /// nothing newer.
    ///
    /// Nobody is watching at 03:30, so the offer and the download are one step here —
    /// which is the only difference between this and the manual path, and it is a
    /// difference about *asking*, not about the order of operations.
    async fn check(&mut self, report: &Reporter) -> Result<Option<Staged>, UpdateError> {
        match self.look().await? {
            Looked::UpToDate => Ok(None),
            Looked::AlreadyStaged(staged) => Ok(Some(staged)),
            Looked::Available(available) => self
                .stage(
                    &available.version,
                    &available.manifest,
                    available.url,
                    report,
                )
                .await
                .map(Some),
        }
    }

    /// One look at the release API, downloading nothing.
    async fn look(&mut self) -> Result<Looked, UpdateError> {
        // Freshen the trust root before looking. Sigstore adds logs and intermediates over
        // time — the embedded copy already carries two of each, one retired — and a panel
        // that has been up for months would otherwise still be judging releases by the
        // root it booted with. A failed refresh keeps the embedded one and says so.
        if self.refresh_trust {
            if let Ok(provenance) = Provenance::refreshed(Some(&self.trust_cache)).await {
                self.provenance = provenance;
            }
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
                return Ok(Looked::UpToDate);
            }
            // `Agent::new` refuses to build in this state, so reaching it would mean the
            // stamp changed under a running process. Refusing is still the right answer.
            Offer::Unorderable => return Ok(Looked::UpToDate),
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
            return Ok(Looked::AlreadyStaged(Staged { version, manifest }));
        }

        let url = release.asset(manifest.artifact.as_str())?;
        Ok(Looked::Available(Available {
            version,
            manifest,
            url,
        }))
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
        report: &Reporter,
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
        let progress = report.clone();
        report.say(Progress::Downloading {
            received: 0,
            total: size,
        });
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
            let digest = download_to(&url, &zip, size, &|received| {
                progress.say(Progress::Downloading {
                    received,
                    total: size,
                });
            })?;
            progress.say(Progress::Verifying);
            if digest != expected {
                return Err(UpdateError::DigestMismatch {
                    expected: expected.to_string(),
                    actual: digest.to_string(),
                });
            }
            let tree = staging_for_task.join("tree");
            progress.say(Progress::Extracting);
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

/// At most this many progress reports over one download.
///
/// Two hundred is a step every 1.2 MB of a quarter-gigabyte artifact — about a second of
/// a hackerspace uplink, and comfortably under the couple of hertz a screen wants. The
/// coalescing is counted in *bytes* rather than measured with a clock, which keeps the
/// read loop deterministic and this constant assertable rather than waited out (#208).
const DOWNLOAD_REPORTS: u64 = 200;

/// Stream `url` into `path`, hashing as it goes and refusing to write more than `size`.
///
/// Hashed on the way past rather than by re-reading the file: a quarter of a gigabyte
/// read twice on a panel's disk is a minute nobody gets back, and the bound is what stops
/// a server that keeps talking from filling the disk before the digest can disagree.
///
/// `on_progress` is called with the byte count so far, at most [`DOWNLOAD_REPORTS`] times.
fn download_to(
    url: &str,
    path: &Path,
    size: u64,
    on_progress: &dyn Fn(u64),
) -> Result<Sha256Digest, UpdateError> {
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
    const CHUNK: usize = 256 * 1024;
    let mut buffer = vec![0u8; CHUNK];
    let mut written = 0u64;
    // One chunk is the floor: reporting more often than the loop reads would be reporting
    // the same number twice.
    let step = (size / DOWNLOAD_REPORTS).max(CHUNK as u64);
    let mut next_report = step;
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
        if written >= next_report {
            on_progress(written);
            next_report = written.saturating_add(step);
        }
    }
    // The last chunk almost never lands on a step boundary, and a bar that stops at 96%
    // and then jumps to "verifying" reads as a stall rather than as an end.
    if written > 0 && written.saturating_add(step) != next_report {
        on_progress(written);
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
    /// published before the release workflow attested anything.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // An ephemeral loopback port for a fake release API, torn down with the test. Not a
    // service surface, so it has no entry in `crates/app/src/surface.rs` to name.
    #![allow(clippy::disallowed_methods)]

    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex};

    use crate::attestation::TRUSTED_ROOT;
    use crate::manifest::BuildNumber;

    use super::*;

    /// The real bundle GitHub issued over a real release of this repository, and the
    /// manifest it covers — the same pair `attestation`'s tests verify offline. Using it
    /// here is what makes a fake release API honest: the server is impersonated, the
    /// *provenance* is not, so the manual check under test runs the whole trust path
    /// rather than a version of it with the interesting step removed.
    const ATTESTED: &[u8] = include_bytes!("../fixtures/attested-manifest.json");
    const BUNDLE: &str = include_str!("../fixtures/attested-manifest.json.sigstore");
    /// The build and commit the fixture manifest names.
    const FIXTURE_BUILD: u64 = 957;
    const FIXTURE_COMMIT: &str = "efc4316";
    const FIXTURE_ARTIFACT_BYTES: u64 = 249_089_697;
    /// Written down rather than read from `RELEASE_IDENTITY`, for the reason
    /// `attestation`'s tests give: a build compiled for a fork still tests these bytes.
    const FIXTURE_IDENTITY: &str =
        "https://github.com/schlarpc/castaway/.github/workflows/release.yml@refs/heads/main";
    const FIXTURE_ISSUER: &str = "https://token.actions.githubusercontent.com";

    /// A panel with nobody at it: the state every test here is in, and the state that
    /// makes the *scheduled* path permissive — so anything a manual request is shown to
    /// do differently is not idleness doing it.
    struct Empty;
    impl PanelActivity for Empty {
        fn casting(&self) -> bool {
            false
        }
        fn idle_for(&self) -> Duration {
            Duration::MAX
        }
    }

    /// A panel in use: casting, touched a second ago. What the schedule refuses.
    struct InUse;
    impl PanelActivity for InUse {
        fn casting(&self) -> bool {
            true
        }
        fn idle_for(&self) -> Duration {
            Duration::ZERO
        }
    }

    /// Somewhere to put an install tree, removed when the test finishes with it.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "castaway-agent-{}-{name}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(path.join("versions")).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A forty-character version name that a failure message can be read at a glance.
    fn version(c: char) -> VersionId {
        VersionId::parse(&std::iter::repeat_n(c, 40).collect::<String>()).unwrap()
    }

    fn build(n: u64) -> InstalledBuild {
        InstalledBuild::Known(BuildNumber::new(n).unwrap())
    }

    /// An agent over a scratch tree, wired to `source`, with the trust anchors the
    /// fixtures were signed under.
    fn agent_for(
        scratch: &Scratch,
        running: &VersionId,
        installed: InstalledBuild,
        source: ReleaseSource,
        activity: Arc<dyn PanelActivity>,
    ) -> Agent {
        Agent {
            tree: InstallTree::at(&scratch.0),
            running: running.clone(),
            installed,
            policy: Policy::default(),
            source,
            provenance: Provenance::with_anchors(TRUSTED_ROOT, FIXTURE_IDENTITY, FIXTURE_ISSUER)
                .unwrap(),
            activity,
            utc_offset_secs: 0,
            armed: true,
            staged: None,
            offer: None,
            // Off: these tests check a checked-in bundle against the checked-in root, and
            // must not depend on reaching a TUF repository.
            refresh_trust: false,
            trust_cache: scratch.0.join("sigstore-cache"),
            phase: Phase::Fresh,
            held: false,
            consecutive_failures: 0,
        }
    }

    /// A gate a test holds shut so it can observe what happens *during* a request.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*open {
                open = self
                    .changed
                    .wait(open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        fn open(&self) {
            *self
                .open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.changed.notify_all();
        }
    }

    /// A release API that answers the three requests a check makes, on localhost.
    ///
    /// Impersonating the *server* is the whole of what it does: every byte it serves for
    /// the manifest and its provenance is a checked-in fixture GitHub really signed.
    struct FakeRelease {
        base_url: String,
        /// Held shut, the manifest response never arrives — which is how a test observes
        /// what the agent does while a look is in flight.
        gate: Arc<Gate>,
        /// How many bytes `/artifact.bin` should serve, for the download-progress test.
        artifact_bytes: usize,
    }

    impl FakeRelease {
        fn start(artifact_bytes: usize, gated: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let base_url = format!("http://127.0.0.1:{port}");
            let gate = Arc::new(Gate::default());
            if !gated {
                gate.open();
            }
            let served = Self {
                base_url: base_url.clone(),
                gate: Arc::clone(&gate),
                artifact_bytes,
            };
            let bodies = served.bodies();
            let gate_for_thread = Arc::clone(&gate);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let bodies = bodies.clone();
                    let gate = Arc::clone(&gate_for_thread);
                    // One thread per connection: a test that holds the gate shut on one
                    // request must not stop the listener answering another.
                    std::thread::spawn(move || serve_one(&stream, &bodies, &gate));
                }
            });
            served
        }

        /// Every path this server knows, and what it answers with.
        fn bodies(&self) -> Arc<Vec<(String, Vec<u8>)>> {
            let digest = format!("sha256:{}", crate::Sha256Digest::of(ATTESTED));
            let listing = serde_json::json!({
                "attestations": [{ "bundle": serde_json::from_str::<serde_json::Value>(BUNDLE).unwrap() }]
            });
            let release = serde_json::json!({
                "tag_name": format!("build-{FIXTURE_COMMIT}"),
                "assets": [
                    { "name": MANIFEST_ASSET,
                      "browser_download_url": format!("{}/manifest.json", self.base_url) },
                    { "name": "castaway-windows-electron-efc4316.zip",
                      "browser_download_url": format!("{}/artifact.bin", self.base_url) },
                ]
            });
            Arc::new(vec![
                (
                    "/repos/schlarpc/castaway/releases/latest".to_string(),
                    serde_json::to_vec(&release).unwrap(),
                ),
                ("/manifest.json".to_string(), ATTESTED.to_vec()),
                (
                    format!("/repos/schlarpc/castaway/attestations/{digest}"),
                    serde_json::to_vec(&listing).unwrap(),
                ),
                (
                    "/artifact.bin".to_string(),
                    vec![0x5au8; self.artifact_bytes],
                ),
            ])
        }

        fn source(&self) -> ReleaseSource {
            ReleaseSource {
                base_url: self.base_url.clone(),
                repository: "schlarpc/castaway".to_string(),
            }
        }

        fn artifact_url(&self) -> String {
            format!("{}/artifact.bin", self.base_url)
        }
    }

    fn serve_one(mut stream: &TcpStream, bodies: &[(String, Vec<u8>)], gate: &Gate) {
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => return,
                Ok(_) => request.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&request);
        let Some(path) = head.split_whitespace().nth(1) else {
            return;
        };
        // The manifest is the gated one: it is the request a look spends its time in.
        if path == "/manifest.json" {
            gate.wait();
        }
        let response = match bodies.iter().find(|(p, _)| p == path) {
            Some((_, body)) => {
                let mut out = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                out.extend_from_slice(body);
                out
            }
            None => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            }
        };
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }

    /// Drive one command through the agent and hand back everything it said.
    ///
    /// Every state, not just the last: the point of a progress channel is the states in
    /// between, and a test that only read `borrow()` at the end would pass over a
    /// download that never reported a byte.
    async fn serve(agent: &mut Agent, command: Command) -> (Option<Activation>, Vec<Progress>) {
        let (tx, mut rx) = watch::channel(Progress::Idle);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let collecting = Arc::clone(&seen);
        let watcher = tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let state = rx.borrow_and_update().clone();
                collecting
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(state);
            }
        });
        let activation = agent.on_command(command, &Reporter(tx)).await;
        // Dropping the sender ends the watcher, which is what makes the collected list
        // complete rather than whatever had arrived by the time the assert ran.
        watcher.await.ok();
        let states = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        (activation, states)
    }

    #[tokio::test]
    async fn a_manual_check_that_finds_nothing_newer_names_the_build_that_is_running() {
        let server = FakeRelease::start(0, false);
        let scratch = Scratch::new("uptodate");
        let running = version('a');
        // Exactly the release on offer: not newer, so nothing to do — and "up to date"
        // has to carry the number, because that is the part the person can check.
        let mut agent = agent_for(
            &scratch,
            &running,
            build(FIXTURE_BUILD),
            server.source(),
            Arc::new(Empty),
        );
        let (activation, states) = serve(&mut agent, Command::CheckNow).await;
        assert!(activation.is_none());
        assert_eq!(
            states,
            vec![
                Progress::Checking,
                Progress::UpToDate {
                    running: build(FIXTURE_BUILD)
                }
            ]
        );
    }

    #[tokio::test]
    async fn a_manual_check_offers_what_is_newer_and_downloads_none_of_it() {
        let server = FakeRelease::start(0, false);
        let scratch = Scratch::new("offer");
        let running = version('a');
        let mut agent = agent_for(
            &scratch,
            &running,
            build(FIXTURE_BUILD - 1),
            server.source(),
            Arc::new(Empty),
        );
        let (_, states) = serve(&mut agent, Command::CheckNow).await;
        assert_eq!(
            states,
            vec![
                Progress::Checking,
                Progress::Offer {
                    build: BuildNumber::new(FIXTURE_BUILD).unwrap(),
                    commit: FIXTURE_COMMIT.to_string(),
                    bytes: FIXTURE_ARTIFACT_BYTES,
                }
            ]
        );
        // The whole promise of the offer: a quarter of a gigabyte of somebody's uplink is
        // not spent until they press the row. Nothing was written under `versions/`, and
        // no staging directory was even created.
        let mut entries = std::fs::read_dir(scratch.0.join("versions")).unwrap();
        assert!(
            entries.next().is_none(),
            "a check that only offered still put something on disk"
        );
    }

    #[tokio::test]
    async fn a_manual_check_over_a_tree_already_staged_offers_a_restart_and_asks_no_network() {
        let scratch = Scratch::new("staged");
        let running = version('a');
        let staged_version = version('b');
        // An unreachable release API: reaching it here would mean the agent had gone to
        // the network for an answer that was already on disk.
        let source = ReleaseSource {
            base_url: "http://127.0.0.1:9".to_string(),
            repository: "schlarpc/castaway".to_string(),
        };
        let mut agent = agent_for(
            &scratch,
            &running,
            build(FIXTURE_BUILD - 1),
            source,
            Arc::new(Empty),
        );
        // Exactly what `Agent::stage` leaves behind: the tree under its final name, with
        // the manifest that describes it written inside.
        let tree = scratch.0.join("versions").join(staged_version.as_str());
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(MANIFEST_ASSET), ATTESTED).unwrap();
        agent.staged = agent.rediscover_staged();
        assert!(
            agent.staged.is_some(),
            "the staged tree was not rediscovered"
        );

        let (activation, states) = serve(&mut agent, Command::CheckNow).await;
        assert!(activation.is_none(), "a check must not restart anything");
        assert_eq!(
            states,
            vec![Progress::Staged {
                build: BuildNumber::new(FIXTURE_BUILD).unwrap(),
                commit: staged_version.short().to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn a_hold_file_refuses_a_manual_check_and_names_itself() {
        let scratch = Scratch::new("hold");
        let running = version('a');
        let source = ReleaseSource {
            base_url: "http://127.0.0.1:9".to_string(),
            repository: "schlarpc/castaway".to_string(),
        };
        let mut agent = agent_for(&scratch, &running, build(1), source, Arc::new(Empty));
        let hold = agent.tree.hold();
        std::fs::write(&hold, b"").unwrap();

        let (activation, states) = serve(&mut agent, Command::CheckNow).await;
        assert!(activation.is_none());
        let [Progress::StoodDown { why }] = states.as_slice() else {
            panic!("a held panel said {states:?}");
        };
        // "Delete it to re-arm" is only actionable if the screen says which file.
        assert!(
            why.contains(&hold.display().to_string()),
            "the hold message does not name the file: {why}"
        );
        // And it outranks the press rather than being overridden by it: a hold means
        // somebody deployed by hand, and they are not here to be asked.
        let (activation, states) = serve(&mut agent, Command::ActivateNow).await;
        assert!(activation.is_none());
        assert!(matches!(states.as_slice(), [Progress::StoodDown { .. }]));
    }

    #[tokio::test]
    async fn a_manual_restart_takes_a_staged_tree_while_the_panel_is_in_use() {
        let scratch = Scratch::new("activate");
        let running = version('a');
        let staged_version = version('b');
        let source = ReleaseSource {
            base_url: "http://127.0.0.1:9".to_string(),
            repository: "schlarpc/castaway".to_string(),
        };
        // Casting, and touched a second ago: everything the nightly schedule refuses to
        // interrupt. The press is somebody saying "yes, interrupt it".
        let mut agent = agent_for(
            &scratch,
            &running,
            build(FIXTURE_BUILD - 1),
            source,
            Arc::new(InUse),
        );
        std::fs::write(
            scratch.0.join("current.txt"),
            format!("{}\n", running.as_str()),
        )
        .unwrap();
        let tree = scratch.0.join("versions").join(staged_version.as_str());
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(MANIFEST_ASSET), ATTESTED).unwrap();
        agent.staged = agent.rediscover_staged();

        let (activation, states) = serve(&mut agent, Command::ActivateNow).await;
        let activation = activation.expect("the press was the whole permission needed");
        assert_eq!(activation.version, staged_version);
        assert_eq!(activation.replacing, running);
        assert_eq!(states, vec![Progress::Restarting]);
        // The pointers moved together, and previous names what was running — the rollback
        // target a manual update needs exactly as much as a nightly one.
        assert_eq!(
            install::read_pointer(&agent.tree, Pointer::Current).unwrap(),
            staged_version
        );
        assert_eq!(
            install::read_pointer(&agent.tree, Pointer::Previous).unwrap(),
            running
        );
    }

    #[tokio::test]
    async fn pressing_the_offer_with_nothing_offered_says_so_rather_than_doing_nothing() {
        let scratch = Scratch::new("nothing");
        let running = version('a');
        let source = ReleaseSource {
            base_url: "http://127.0.0.1:9".to_string(),
            repository: "schlarpc/castaway".to_string(),
        };
        let mut agent = agent_for(&scratch, &running, build(1), source, Arc::new(Empty));
        let (activation, states) = serve(&mut agent, Command::ActivateNow).await;
        assert!(activation.is_none());
        assert!(
            matches!(states.as_slice(), [Progress::Failed { .. }]),
            "a dead row is indistinguishable from a broken one: {states:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_arriving_mid_look_is_serialised_behind_it_rather_than_racing_it() {
        // The one-owner property, observed rather than asserted about the code: while a
        // look is in flight, a second press changes nothing about what the agent is
        // doing, because the agent is not reading its command channel — it is inside the
        // look. Two of these racing would be two downloads into one staging path.
        let server = FakeRelease::start(0, true);
        let scratch = Scratch::new("serialised");
        let running = version('a');
        let mut agent = agent_for(
            &scratch,
            &running,
            build(FIXTURE_BUILD),
            server.source(),
            Arc::new(Empty),
        );
        let (handle, inbox) = control();
        let mut progress = handle.progress();
        assert!(handle.check_now());

        let looking = tokio::spawn(async move {
            let mut commands = inbox.commands;
            let report = Reporter(inbox.progress);
            let recheck = agent.policy.recheck;
            let activation = agent.sleep_or_serve(recheck, &mut commands, &report).await;
            (agent, activation)
        });

        // The look has started and is parked inside the gated manifest request.
        progress
            .wait_for(|p| *p == Progress::Checking)
            .await
            .expect("the check started");
        // A second press, delivered while it is parked. It is accepted by the channel and
        // *not* acted on: the agent is not at a point where it reads one.
        assert!(handle.check_now());
        assert_eq!(*progress.borrow_and_update(), Progress::Checking);
        server.gate.open();
        progress
            .wait_for(|p| matches!(p, Progress::UpToDate { .. }))
            .await
            .expect("the check finished");
        let (_agent, activation) = looking.await.unwrap();
        assert!(activation.is_none());
    }

    #[test]
    fn a_download_reports_often_enough_to_watch_and_not_so_often_it_is_a_flood() {
        // The denominator is the signed manifest's size, and the reports are counted in
        // bytes rather than timed off a clock — so this asserts the shipped coalescing
        // rather than waiting one out (#208).
        let bytes = 4 * 1024 * 1024;
        let server = FakeRelease::start(bytes, false);
        let scratch = Scratch::new("download");
        let path = scratch.0.join("artifact.bin");
        let seen = Mutex::new(Vec::new());
        let digest = download_to(&server.artifact_url(), &path, bytes as u64, &|received| {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(received);
        })
        .unwrap();
        assert_eq!(digest, crate::Sha256Digest::of(&vec![0x5au8; bytes]));
        let seen = seen
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!seen.is_empty(), "a download that reported nothing at all");
        assert!(
            seen.len() as u64 <= DOWNLOAD_REPORTS,
            "{} reports for one download",
            seen.len()
        );
        // Monotonic, and the last one is the whole file: a progress line that went
        // backwards, or stopped short of 100%, reads as a stall.
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "{seen:?}");
        assert_eq!(seen.last().copied(), Some(bytes as u64));
    }
}
