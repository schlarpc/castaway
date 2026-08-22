//! "Check for updates": a check on demand, an offer, and a download the person can watch
//! (#360).
//!
//! The updater looks once a night, in a window nobody is awake for. That is the right
//! default for an unattended panel and the wrong behaviour for the one moment somebody
//! *is* standing at it — having just pushed a release, or having walked over because
//! something looks wrong. This is that moment's screen.
//!
//! It is a window onto the agent, never a second copy of it. Nothing here fetches,
//! verifies, stages or activates: it sends [`Command`]s and renders [`Progress`], so the
//! order of operations that is this feature's security property stays in exactly one
//! place (`castaway_update::agent`). What it adds is words — the agent reports states, and
//! a person needs sentences.

use castaway_update::agent::{Handle, Progress};
use castaway_update::manifest::InstalledBuild;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{Action, Drilldown, Report, Row, Setting, Stage};

/// The row that accepts the offer. Opaque to `shell_nav`, which hands it straight back.
const TAKE_IT: &str = "take-it";
/// The row that asks again after a failure.
const AGAIN: &str = "again";

/// Ask the updater for a check now, and watch what it says.
pub struct UpdateCheckSetting {
    handle: Handle,
    /// What this receiver is, so "up to date" can carry a number the person can check.
    installed: InstalledBuild,
    /// `[update] enable`. It stands the *nightly loop* down, not this row: a press is
    /// not the receiver updating itself, it is somebody asking (#360). The screen says
    /// so rather than pretending the schedule is running.
    armed: bool,
}

impl UpdateCheckSetting {
    /// A row driving `handle`, for a receiver at `installed`.
    #[must_use]
    pub const fn new(handle: Handle, installed: InstalledBuild, armed: bool) -> Self {
        Self {
            handle,
            installed,
            armed,
        }
    }
}

impl Setting for UpdateCheckSetting {
    fn id(&self) -> &'static str {
        "update-check"
    }

    fn title(&self) -> String {
        "Check for updates".into()
    }

    fn summary(&self) -> String {
        // On every build, including the ones where it cannot work: a row that disappears
        // is indistinguishable from a row that is broken.
        if matches!(*self.handle.progress().borrow(), Progress::StoodDown { .. }) {
            return "Not available on this build".into();
        }
        // A menu row's detail line, so it is held to the length of one: the sentences
        // belong on the drill-down, where there is room for them. "Running build 957 —
        // automatic updates are off" ran off the side of the panel.
        let running = format!("Build {}", self.installed);
        if self.armed {
            running
        } else {
            format!("{running} — auto-update off")
        }
    }

    fn drilldown(&self) -> Drilldown<'_> {
        Drilldown::Action(self)
    }
}

impl Action for UpdateCheckSetting {
    fn start(&self) -> mpsc::UnboundedReceiver<Report> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut progress = self.handle.progress();
        let asked = self.handle.check_now();
        let now = progress.borrow_and_update().clone();
        // What is true this instant whatever the press meant: a stand-down, or work the
        // night loop already has in flight — a press landing mid-download should show the
        // download, which is also what makes the two paths visibly one thing. Every other
        // state is an answer to an older question, and the press is the new one.
        let live = matches!(
            now,
            Progress::Downloading { .. }
                | Progress::Verifying
                | Progress::Extracting
                | Progress::Restarting
                | Progress::StoodDown { .. }
        );
        let first = if asked && !live {
            Progress::Checking
        } else {
            now
        };
        let armed = self.armed;
        let _ = tx.send(report_for(&first, armed));
        tokio::spawn(async move {
            while progress.changed().await.is_ok() {
                let state = progress.borrow_and_update().clone();
                if tx.send(report_for(&state, armed)).is_err() {
                    // Nobody is watching any more. The work carries on without us: this
                    // task is a window onto the agent, never its owner, and a person who
                    // wandered off should come back to a panel that took the update.
                    break;
                }
            }
        });
        rx
    }

    fn press(&self, row_id: &str) {
        match row_id {
            TAKE_IT => {
                if !self.handle.activate_now() {
                    warn!("settings: the updater did not take the download request");
                }
            }
            AGAIN => {
                if !self.handle.check_now() {
                    warn!("settings: the updater did not take the check request");
                }
            }
            other => debug!(%other, "settings: a row the update check did not mint"),
        }
    }
}

/// One agent state, as a screen.
///
/// A free function over `&Progress` rather than a method on the setting, so every
/// sentence a person can read off this row is assertable without an agent, a network or a
/// clock. The `match` is exhaustive on purpose: a phase added to [`Progress`] should stop
/// this compiling rather than acquire a silent default (ground rule 1).
fn report_for(progress: &Progress, armed: bool) -> Report {
    /// What accepting the offer costs, said plainly. The panel really does go dark, and
    /// somebody's film really does stop — an offer that did not say so would be a
    /// surprise dressed as a button.
    const WHAT_IT_COSTS: &str = "The panel goes blank for about a minute and comes back on \
                                 the new version. Anything casting right now stops.";

    let working = |say: &str| Report {
        say: say.to_string(),
        stage: Stage::Working,
        rows: Vec::new(),
    };
    // Only where somebody is deciding: appended to an answer, not to a progress line.
    let note = |say: String| {
        if armed {
            say
        } else {
            format!("{say} Automatic updates are off, so nothing is taken on its own.")
        }
    };

    match progress {
        // Not normally seen — `start` asks for a check before it renders anything — but a
        // press that the command channel refused lands here, and "checking" is still what
        // the person asked for.
        Progress::Idle | Progress::Checking => working("Checking for a newer build…"),
        Progress::UpToDate { running } => Report {
            say: note(format!(
                "This panel is running build {running}, which is the newest there is."
            )),
            stage: Stage::Settled,
            rows: Vec::new(),
        },
        Progress::Offer {
            build,
            commit,
            bytes,
        } => Report {
            say: note(format!(
                "Build {build} ({commit}) is available — {}. Nothing has been downloaded yet.",
                megabytes(*bytes)
            )),
            stage: Stage::Settled,
            rows: vec![Row {
                id: TAKE_IT.to_string(),
                label: "Download and restart now".to_string(),
                detail: Some(WHAT_IT_COSTS.to_string()),
            }],
        },
        // The unit once, on the total: "108 of 237 MB" is one quantity being filled, and
        // repeating "MB" on both halves reads as two.
        Progress::Downloading { received, total } => working(&format!(
            "{}% — {} of {}",
            percent(*received, *total),
            received / MEGABYTE,
            megabytes(*total)
        )),
        // The denominator above is the signed manifest's size, and this is what earns it:
        // the bytes are checked against the digest that manifest declared, not against a
        // length the server chose.
        Progress::Verifying => working("Checking what arrived against the signed manifest…"),
        Progress::Extracting => working("Unpacking it…"),
        Progress::Staged { build, commit } => Report {
            say: note(format!("Build {build} ({commit}) is downloaded and ready.")),
            stage: Stage::Settled,
            rows: vec![Row {
                id: TAKE_IT.to_string(),
                label: "Restart now".to_string(),
                detail: Some(WHAT_IT_COSTS.to_string()),
            }],
        },
        Progress::Restarting => working("Restarting into the new version…"),
        Progress::Failed { why } => Report {
            say: why.clone(),
            stage: Stage::Failed,
            rows: vec![Row {
                id: AGAIN.to_string(),
                label: "Try again".to_string(),
                detail: None,
            }],
        },
        // No retry row: every one of these is a state somebody else has to change — a
        // deploy, a `hold` file, a build from a clean checkout — and a button that cannot
        // work is worse than no button. The words say what to do.
        Progress::StoodDown { why } => Report {
            say: why.clone(),
            stage: Stage::Failed,
            rows: Vec::new(),
        },
    }
}

/// What a megabyte is here. Binary, because it is what every other size on this panel is
/// measured in and a quarter of a gigabyte is not a number anybody checks to three digits.
const MEGABYTE: u64 = 1024 * 1024;

/// Bytes as a person reads them.
fn megabytes(bytes: u64) -> String {
    format!("{} MB", bytes / MEGABYTE)
}

/// How far along, as a whole number. Zero rather than a division by zero for a manifest
/// claiming nothing, which `Manifest::parse` already refuses anyway.
fn percent(received: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    (received.saturating_mul(100) / total).min(100)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use castaway_update::manifest::BuildNumber;

    use super::*;

    fn build(n: u64) -> BuildNumber {
        BuildNumber::new(n).unwrap()
    }

    #[test]
    fn up_to_date_names_the_build_that_is_running() {
        // "Up to date" with no number is not something the person reading it can check
        // against what they just pushed.
        let report = report_for(
            &Progress::UpToDate {
                running: InstalledBuild::Known(build(957)),
            },
            true,
        );
        assert_eq!(report.stage, Stage::Settled);
        assert!(report.say.contains("957"), "{}", report.say);
        assert!(
            report.rows.is_empty(),
            "nothing to press when there is nothing to do"
        );
    }

    #[test]
    fn an_offer_names_the_build_the_commit_and_the_size_and_offers_one_row() {
        let report = report_for(
            &Progress::Offer {
                build: build(957),
                commit: "efc4316".into(),
                bytes: 249_089_697,
            },
            true,
        );
        assert_eq!(report.stage, Stage::Settled);
        for wanted in ["957", "efc4316", "237 MB"] {
            assert!(
                report.say.contains(wanted),
                "{wanted} missing: {}",
                report.say
            );
        }
        // Nothing is downloaded before the press, and the screen has to say so — the
        // whole difference between an offer and something that happens to somebody.
        assert!(
            report.say.contains("Nothing has been downloaded"),
            "{}",
            report.say
        );
        let [row] = report.rows.as_slice() else {
            panic!("an offer with {} rows", report.rows.len());
        };
        assert_eq!(row.id, TAKE_IT);
        let detail = row.detail.as_deref().unwrap_or_default();
        // The two things it costs, both stated before the press rather than discovered
        // after it.
        assert!(detail.contains("blank"), "{detail}");
        assert!(detail.to_lowercase().contains("casting"), "{detail}");
    }

    #[test]
    fn a_download_reads_as_a_number_that_climbs_rather_than_as_working() {
        // Four minutes of "working…" reads as a hang. The denominator is the signed
        // manifest's size, which is the number worth watching.
        let report = report_for(
            &Progress::Downloading {
                received: 113_246_208,
                total: 249_089_697,
            },
            true,
        );
        assert_eq!(report.stage, Stage::Working);
        assert_eq!(report.say, "45% — 108 of 237 MB");
        assert!(report.rows.is_empty(), "nothing to press mid-download");
    }

    #[test]
    fn a_staged_tree_offers_a_restart_and_never_a_download() {
        // The case where the night loop already fetched it and the panel never went
        // quiet: there is nothing left to spend, only a restart to agree to.
        let report = report_for(
            &Progress::Staged {
                build: build(957),
                commit: "efc4316".into(),
            },
            true,
        );
        assert_eq!(report.stage, Stage::Settled);
        assert!(
            report.say.contains("downloaded and ready"),
            "{}",
            report.say
        );
        let [row] = report.rows.as_slice() else {
            panic!("{:?}", report.rows);
        };
        assert_eq!(row.label, "Restart now");
        assert!(!row.label.to_lowercase().contains("download"), "{row:?}");
    }

    #[test]
    fn a_failure_keeps_the_updaters_own_words_and_offers_another_go() {
        let report = report_for(
            &Progress::Failed {
                why: "fetching https://api.github.com/…: dns error".into(),
            },
            true,
        );
        assert_eq!(report.stage, Stage::Failed);
        assert!(report.say.contains("dns error"), "{}", report.say);
        assert_eq!(report.rows.first().map(|r| r.id.as_str()), Some(AGAIN));
    }

    #[test]
    fn a_stand_down_reaches_the_glass_verbatim_and_offers_nothing_that_cannot_work() {
        // Each `StandDown` variant is already written as prose for a person; the screen's
        // job is to not paraphrase it. And none of them is fixed by pressing anything, so
        // none of them gets a button.
        for reason in [
            castaway_update::agent::StandDown::Hold,
            castaway_update::agent::StandDown::UnknownBuild,
            castaway_update::agent::StandDown::Disabled,
        ] {
            let why = reason.to_string();
            let report = report_for(&Progress::StoodDown { why: why.clone() }, true);
            assert_eq!(report.stage, Stage::Failed);
            assert_eq!(report.say, why);
            assert!(report.rows.is_empty(), "{report:?}");
        }
    }

    #[test]
    fn a_receiver_whose_schedule_is_off_says_so_where_somebody_is_deciding() {
        // `[update] enable = false` no longer means the row does nothing — it means the
        // *nightly loop* does nothing, and the screen has to keep those apart.
        let answer = report_for(
            &Progress::UpToDate {
                running: InstalledBuild::Known(build(957)),
            },
            false,
        );
        assert!(
            answer.say.contains("Automatic updates are off"),
            "{}",
            answer.say
        );
        // But not on a progress line, where it is noise rather than news.
        let mid = report_for(
            &Progress::Downloading {
                received: 1,
                total: 100,
            },
            false,
        );
        assert!(!mid.say.contains("Automatic"), "{}", mid.say);
    }

    #[test]
    fn the_menu_rows_summary_stays_a_label_rather_than_a_sentence() {
        // It is a settings-menu detail line, and the picker does not wrap or clip one: a
        // sentence there runs off the side of the panel, which is what "Running build 957
        // — automatic updates are off" did. The ceiling is not a measurement of the glass,
        // it is what keeps this the same shape as every other setting's summary ("System
        // default", "off").
        const LABEL: usize = 32;
        let (handle, _inbox) = castaway_update::agent::control();
        for armed in [true, false] {
            let setting =
                UpdateCheckSetting::new(handle.clone(), InstalledBuild::Known(build(957)), armed);
            let summary = setting.summary();
            assert!(
                summary.chars().count() <= LABEL,
                "{summary:?} is {} characters",
                summary.chars().count()
            );
            // Still says which build, because that is the number somebody standing at the
            // panel came to check.
            assert!(summary.contains("957"), "{summary}");
        }
    }
}
