//! The launcher against real processes: crash, back off, strike out, roll back.
//!
//! [`castaway_launcher::supervise`] proves the decision in virtual time. This proves the
//! *plumbing* around it — that the pointer files are read and written where both this and
//! the updater expect, that a child's exit code reaches the decision, that the reserved
//! handshake code lands on the version `current.txt` names now, and that a rollback
//! actually changes what runs. None of that is visible to a unit test, and all of it is
//! what #346's acceptance test does by hand on the box.
//!
//! It runs on Linux, against the real launcher binary, with shell scripts standing in for
//! the receiver (ground rule 5: the platform seam is the job object, and everything else
//! is the same code the panel runs). Wall time is real here — a supervisor's subject is
//! `wait()` — so the deadline poll is what keeps a loaded CI box from turning a slow run
//! into a wrong answer rather than a late one.

use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

use castaway_launcher::supervise::ACTIVATE_EXIT_CODE;
use castaway_paths::install::{self, InstallTree, Pointer, VersionId};
use castaway_test_support::eventually_blocking_within;

/// Two versions, distinguishable at a glance in a failure message.
const GOOD: &str = "9000000000000000000000000000000000000000";
const BAD: &str = "bad0000000000000000000000000000000000000";
const NEXT: &str = "cafe000000000000000000000000000000000000";

/// The launcher's ladder is 0s, 1s, 2s before the third strike, and the rollback is
/// written before the wait that follows it — so the interesting event lands about three
/// seconds in. Twice the slack, because the point of a deadline is to survive a loaded
/// box without hiding a real hang.
const PATIENCE: Duration = Duration::from_secs(30);

fn version(tree: &InstallTree, sha: &str) -> install::VersionTree {
    tree.version(&VersionId::parse(sha).expect("a well-formed sha"))
}

/// Install a fake receiver: a shell script, which is a perfectly good executable.
fn install_receiver(tree: &InstallTree, sha: &str, body: &str) {
    let v = version(tree, sha);
    std::fs::create_dir_all(v.path()).expect("version directory");
    let script = format!("#!/bin/sh\n{body}\n");
    std::fs::write(v.receiver(), script).expect("write receiver");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(v.receiver(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
}

fn point(tree: &InstallTree, which: Pointer, sha: &str) {
    install::write_pointer(tree, which, &VersionId::parse(sha).expect("sha")).expect("pointer");
}

fn current(tree: &InstallTree) -> Option<String> {
    install::read_pointer(tree, Pointer::Current)
        .ok()
        .map(|id| id.to_string())
}

/// Start the launcher against `root`. Killed on drop — the *whole* process group, which is
/// the point: killing the launcher alone orphans whatever receiver it had running, and a
/// `sleep 600` standing in for a kiosk would then outlive the test by ten minutes. On the
/// panel that job is the Windows job object's; here it is this.
struct Launcher(Child);

impl Launcher {
    fn start(root: &Path) -> Self {
        let exe = env!("CARGO_BIN_EXE_launcher");
        let mut command = Command::new(exe);
        command.arg("--root").arg(root);
        #[cfg(unix)]
        {
            // Its own group, so the group id is the launcher's own pid and everything it
            // spawns joins by inheritance.
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        Self(command.spawn().expect("start the launcher"))
    }
}

impl Drop for Launcher {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // `kill -- -<pgid>`, rather than a libc dependency for one signal in one test.
            let _ = Command::new("kill")
                .arg("-9")
                .arg("--")
                .arg(format!("-{}", self.0.id()))
                .status();
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_version_that_never_comes_up_is_rolled_back_to_the_one_that_did() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = InstallTree::at(dir.path());
    install::ensure_tree(&tree).expect("tree");

    // The bad bits: they die at once and never write a health marker. This is the shape
    // of a release that is broken on *this* box — a missing DLL, a driver it cannot open
    // — which is the only failure a rollback can fix and the one it exists for.
    install_receiver(&tree, BAD, "echo 'bad version starting'; exit 3");
    // The known-good one, which says so and then stays up.
    install_receiver(
        &tree,
        GOOD,
        "echo 'good version starting'; touch \"$(dirname \"$0\")/.healthy\"; sleep 600",
    );
    // The good version has been healthy before, which is what makes it the rollback
    // target rather than merely the older one.
    std::fs::write(version(&tree, GOOD).health_marker(), b"").expect("marker");

    point(&tree, Pointer::Current, BAD);
    point(&tree, Pointer::Previous, GOOD);

    let _launcher = Launcher::start(dir.path());

    eventually_blocking_within(
        "the launcher to roll back to the healthy version",
        PATIENCE,
        || current(&tree).filter(|c| c == GOOD),
    );
    // And it is actually running it, not merely pointing at it.
    eventually_blocking_within("the rolled-back version to start", PATIENCE, || {
        std::fs::read_to_string(tree.log())
            .ok()
            .filter(|log| log.contains("good version starting"))
    });
}

#[test]
fn a_version_that_was_healthy_keeps_being_restarted_rather_than_rolled_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = InstallTree::at(dir.path());
    install::ensure_tree(&tree).expect("tree");

    // Crashes immediately, every time — and has been healthy before. A working receiver
    // having a bad night: restarting it forever is right, and replacing it with an older
    // one on the third crash would be a regression nobody asked for.
    install_receiver(&tree, GOOD, "echo 'crashing again'; exit 1");
    std::fs::write(version(&tree, GOOD).health_marker(), b"").expect("marker");
    install_receiver(&tree, BAD, "echo 'the old one'; sleep 600");

    point(&tree, Pointer::Current, GOOD);
    point(&tree, Pointer::Previous, BAD);

    let _launcher = Launcher::start(dir.path());

    // Four crashes is one past the strike count, so a launcher that was going to roll
    // back has had every chance to.
    eventually_blocking_within(
        "four restarts of the healthy-but-crashing version",
        PATIENCE,
        || {
            let log = std::fs::read_to_string(tree.log()).ok()?;
            (log.matches("crashing again").count() >= 4).then_some(())
        },
    );
    assert_eq!(current(&tree).as_deref(), Some(GOOD));
}

#[test]
fn the_handshake_exit_starts_whatever_current_txt_names_now() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = InstallTree::at(dir.path());
    install::ensure_tree(&tree).expect("tree");

    // What the updater does at activation: point `current.txt` at the staged tree and
    // exit with the reserved code. Never spawn the successor itself — on the box that
    // would put it outside the interactive session, where it renders to nothing.
    install_receiver(
        &tree,
        GOOD,
        &format!(
            "echo 'first version'; printf '{NEXT}\\n' > \"$(dirname \"$0\")/../../current.txt\"; \
             exit {ACTIVATE_EXIT_CODE}"
        ),
    );
    install_receiver(&tree, NEXT, "echo 'the updated version'; sleep 600");
    point(&tree, Pointer::Current, GOOD);

    let _launcher = Launcher::start(dir.path());

    eventually_blocking_within(
        "the launcher to start the version it was pointed at",
        PATIENCE,
        || {
            std::fs::read_to_string(tree.log())
                .ok()
                .filter(|log| log.contains("the updated version"))
        },
    );
    // The handshake is not a crash: the log says so in as many words, which is what a
    // human reading a panel's log at 9 a.m. needs to distinguish an update from a fault.
    let log = std::fs::read_to_string(tree.log()).expect("log");
    assert!(log.contains("asked to be reloaded"), "{log}");
}

#[test]
fn a_current_txt_pointing_at_nothing_does_not_stop_the_launcher() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = InstallTree::at(dir.path());
    install::ensure_tree(&tree).expect("tree");

    // No `versions/<BAD>` at all — the state a half-finished deploy or a hand-edited
    // pointer leaves. The launcher has to keep looking rather than exiting, because
    // exiting is a dark panel until somebody logs in.
    point(&tree, Pointer::Current, BAD);
    point(&tree, Pointer::Previous, GOOD);
    install_receiver(&tree, GOOD, "echo 'the fallback'; sleep 600");
    std::fs::write(version(&tree, GOOD).health_marker(), b"").expect("marker");

    let _launcher = Launcher::start(dir.path());

    eventually_blocking_within(
        "the launcher to fall back to a tree that exists",
        PATIENCE,
        || {
            std::fs::read_to_string(tree.log())
                .ok()
                .filter(|log| log.contains("the fallback"))
        },
    );
}

#[test]
fn a_restart_keeps_the_previous_launchers_transcript() {
    // #361. The launcher's own lines — the restart ladder, the health verdict, the
    // rollback decision — live only in `castaway.log`, because the receiver's dated logs
    // cannot cover the process that starts it and outlives each of its crashes. That file
    // used to be truncated at every launcher start, so the explanation for a rollback was
    // destroyed by the restart that followed it: at 4 a.m. the decision was made, and by
    // morning the panel could not say why it was running what it was running.
    let dir = tempfile::tempdir().expect("tempdir");
    let tree = InstallTree::at(dir.path());
    install::ensure_tree(&tree).expect("tree");

    install_receiver(
        &tree,
        GOOD,
        "echo 'first boot marker'; touch \"$(dirname \"$0\")/.healthy\"; sleep 600",
    );
    point(&tree, Pointer::Current, GOOD);
    point(&tree, Pointer::Previous, GOOD);

    {
        let _launcher = Launcher::start(dir.path());
        eventually_blocking_within("the first launcher to write its log", PATIENCE, || {
            std::fs::read_to_string(tree.log())
                .ok()
                .filter(|log| log.contains("first boot marker"))
        });
    } // dropped: this launcher and its receiver are gone, as a reboot would leave them.

    // The second boot. Same tree, same pointers — only the process is new.
    install_receiver(
        &tree,
        GOOD,
        "echo 'second boot marker'; touch \"$(dirname \"$0\")/.healthy\"; sleep 600",
    );
    let _launcher = Launcher::start(dir.path());
    eventually_blocking_within("the second launcher to write its log", PATIENCE, || {
        std::fs::read_to_string(tree.log())
            .ok()
            .filter(|log| log.contains("second boot marker"))
    });

    // The live log is the current boot, readable from the top — the property the truncate
    // was there to get, and which rolling keeps.
    let live = std::fs::read_to_string(tree.log()).expect("the live log");
    assert!(
        !live.contains("first boot marker"),
        "the live log should begin at this boot, not accumulate every boot: {live}"
    );

    // And the boot that explains this one is still on disk beside it, which is the whole
    // point: a rollback decided in one launcher's life is read in the next one's.
    let kept = std::fs::read_to_string(tree.rolled_log())
        .expect("the previous boot's log should have been rolled, not destroyed");
    assert!(
        kept.contains("first boot marker"),
        "the rolled log should hold the previous boot verbatim: {kept}"
    );
}
