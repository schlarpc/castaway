//! The launcher process: spawn, wait, decide, repeat.
//!
//! Everything interesting is in [`castaway_launcher::supervise`]. This file reads the
//! clock once per run, spawns a process, and does what the decision says — which is the
//! whole of ground rule 3's split, applied to a supervisor instead of a protocol.
//!
//! It is synchronous and single-threaded on purpose. There is no I/O here to overlap: the
//! launcher spends its life blocked in `wait()`, and a runtime would be a dependency, a
//! thread pool and a shutdown story bought for nothing.

// FFI (ground rule 8): the Windows job object, and nothing else. Every unsafe block
// carries its SAFETY note. The rest of the launcher is `forbid(unsafe_code)` in lib.rs.
#![allow(unsafe_code)]

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use castaway_launcher::supervise::{Next, Run, Stopped, Supervisor, ACTIVATE_EXIT_CODE};
use castaway_paths::install::{self, InstallTree, LayoutError, Pointer, VersionId, VersionTree};

mod job;

/// Roll `castaway.log` once it passes this. A crash loop writes the same page of text
/// forever, and the panel runs unattended for weeks; one generation is enough to keep the
/// evidence of what started the loop while bounding what it costs.
const LOG_ROLL_BYTES: u64 = 16 * 1024 * 1024;

fn main() -> ExitCode {
    match run() {
        // `run` only returns on something it cannot proceed past. Everything else — a
        // crash, a missing tree, a receiver that will not start — is handled by looping,
        // because the alternative is a dark panel that stays dark.
        Err(e) => {
            let mut line = format!("launcher: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                line.push_str(&format!(": {cause}"));
                source = cause.source();
            }
            eprintln!("{line}");
            ExitCode::FAILURE
        }
    }
}

/// Why the launcher gave up before it started.
#[derive(Debug, thiserror::Error)]
enum Fatal {
    /// The install tree is not where this executable is, or is unreadable.
    #[error("the install tree")]
    Layout(#[from] LayoutError),
    /// `castaway.log` could not be opened. Refusing to start rather than running blind:
    /// an unattended panel whose only diagnostic is a log file is not worth starting
    /// without one, and the failure is a permissions problem a human has to fix anyway.
    #[error("opening {path}")]
    Log {
        /// The log we could not open.
        path: PathBuf,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}

/// The kiosk loop. Never returns `Ok`.
fn run() -> Result<std::convert::Infallible, Fatal> {
    let tree = match std::env::args().nth(1) {
        // `--root` exists for tests and for a second install on one machine; the box
        // passes nothing, because the tree is wherever the launcher was installed.
        Some(flag) if flag == "--root" => {
            let root = std::env::args().nth(2).unwrap_or_default();
            InstallTree::at(root)
        }
        _ => InstallTree::of_running_launcher()?,
    };

    let mut log = Log::open(&tree)?;
    log.say(&format!(
        "launcher starting; install tree at {}",
        tree.root().display()
    ));

    // One job for the whole life of the launcher. Kill-on-close is what makes the panel's
    // process tree die with the launcher rather than leaving an orphaned Electron holding
    // the display — the same failure `deploy-windows` handles with `taskkill /T`, which
    // there is no equivalent of when the parent is killed rather than doing the killing.
    let job = job::Job::create();
    match &job {
        Ok(j) if j.joined_self() => log.say("job object: this process and its descendants"),
        Ok(_) => log.say("job object: created, children assigned individually"),
        Err(e) => log.say(&format!(
            "job object: unavailable ({e}); an Electron child could outlive a killed launcher"
        )),
    }

    let mut supervisor = Supervisor::new();
    let mut running: Option<VersionId> = None;

    loop {
        let current = match install::read_pointer(&tree, Pointer::Current) {
            Ok(id) => id,
            Err(e) => {
                // Not fatal, and this is the difference between a kiosk and a service: a
                // pointer that is missing right now may be a deploy in flight, and the
                // launcher's job is to keep looking. It has nothing else to do.
                log.say(&format!("cannot read current.txt: {e}; waiting"));
                std::thread::sleep(supervise_pause());
                continue;
            }
        };
        if running.as_ref() != Some(&current) {
            if running.is_some() {
                log.say(&format!("now running version {}", current.short()));
                supervisor.version_changed();
            }
            running = Some(current.clone());
        }

        let version = tree.version(&current);
        let started = Instant::now();
        let stopped = spawn_and_wait(&version, &mut log, job.as_ref().ok());
        let lasted = started.elapsed();
        // Read once, here, at the boundary — the decision core is handed a fact, not a
        // path it might consult twice and get two answers from.
        let ever_healthy = version.is_healthy();
        let previous = install::read_pointer(&tree, Pointer::Previous).ok();
        let rollback_target = previous.as_ref().is_some_and(|p| *p != current);

        log.say(&format!(
            "version {} {} after {:.1}s (healthy={ever_healthy})",
            current.short(),
            describe(stopped),
            lasted.as_secs_f32()
        ));

        let next = supervisor.on_exit(Run {
            stopped,
            lasted,
            ever_healthy,
            rollback_target,
        });

        if let Next::RollBack { .. } = next {
            // `previous` is `Some` here — `rollback_target` was what let the decision
            // reach this arm — but the launcher does not get to assume that, because a
            // deploy could have removed the file in between.
            match &previous {
                Some(target) => {
                    log.say(&format!(
                        "version {} has never been healthy and keeps dying; rolling back to {}",
                        current.short(),
                        target.short()
                    ));
                    if let Err(e) = install::write_pointer(&tree, Pointer::Current, target) {
                        log.say(&format!("rollback could not write current.txt: {e}"));
                    }
                }
                None => log.say("rollback wanted, but previous.txt is gone; carrying on"),
            }
        }

        let after = next.after();
        if !after.is_zero() {
            log.say(&format!("restarting in {:.0}s", after.as_secs_f32()));
        }
        std::thread::sleep(after);
    }
}

/// How long to wait when there is nothing to run at all.
///
/// Not part of the backoff ladder: this is "the tree is mid-deploy", not "the bits are
/// bad", and the two want different answers. Five seconds is short enough that a deploy
/// finishing is picked up promptly and long enough not to spin.
const fn supervise_pause() -> std::time::Duration {
    std::time::Duration::from_secs(5)
}

fn describe(stopped: Stopped) -> String {
    match stopped {
        Stopped::Activate => "asked to be reloaded into a new version".to_string(),
        Stopped::Ended { code: Some(c) } => format!("exited with code {c}"),
        Stopped::Ended { code: None } => "was killed by a signal".to_string(),
        Stopped::Missing => "has no receiver to run".to_string(),
    }
}

/// Start the receiver and wait for it, sending everything it prints to the shared log.
fn spawn_and_wait(version: &VersionTree, log: &mut Log, job: Option<&job::Job>) -> Stopped {
    let receiver = version.receiver();
    if !receiver.exists() {
        log.say(&format!("no receiver at {}", receiver.display()));
        return Stopped::Missing;
    }

    // Two handles onto one file rather than a pipe and a pump: the launcher has no
    // business reading the receiver's output, and a pipe nobody drains is a receiver that
    // blocks on its own logging.
    let (out, err) = match (log.handle(), log.handle()) {
        (Some(a), Some(b)) => (a, b),
        _ => (Stdio::null(), Stdio::null()),
    };

    let mut command = Command::new(&receiver);
    command
        .current_dir(version.path())
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            log.say(&format!("could not start {}: {e}", receiver.display()));
            return Stopped::Missing;
        }
    };
    if let Some(job) = job {
        if let Err(e) = job.adopt(&child) {
            log.say(&format!(
                "could not put the receiver in the job object: {e}"
            ));
        }
    }

    match child.wait() {
        Ok(status) if status.code() == Some(ACTIVATE_EXIT_CODE) => Stopped::Activate,
        Ok(status) => Stopped::Ended {
            code: status.code(),
        },
        Err(e) => {
            log.say(&format!("waiting for the receiver failed: {e}"));
            Stopped::Ended { code: None }
        }
    }
}

/// The shared `castaway.log`: the launcher's own lines and the receiver's output, in one
/// file that outlives any single version so a log tail survives an update.
struct Log {
    path: PathBuf,
    rolled: PathBuf,
    file: std::fs::File,
}

impl Log {
    fn open(tree: &InstallTree) -> Result<Self, Fatal> {
        let path = tree.log();
        // Truncated at launcher start rather than appended to, matching what `run.cmd`
        // did: a log that begins at the last boot is one somebody can read top to bottom.
        // Within a launcher's life it accumulates, so a crash loop's whole history is
        // there.
        let open = |truncate: bool| {
            OpenOptions::new()
                .create(true)
                .write(true)
                .append(!truncate)
                .truncate(truncate)
                .open(&path)
                .map_err(|source| Fatal::Log {
                    path: path.clone(),
                    source,
                })
        };
        drop(open(true)?);
        // Reopened in append mode, and that is not tidiness: the launcher's own handle
        // and each child's share one file, and a handle with its own write offset would
        // overwrite whatever the receiver printed since the last launcher line. `O_APPEND`
        // (and Windows' `FILE_APPEND_DATA`) is what makes every writer land at the end.
        let file = open(false)?;
        Ok(Self {
            path,
            rolled: tree.rolled_log(),
            file,
        })
    }

    /// A line from the launcher itself, timestamped so it can be lined up against the
    /// receiver's own log.
    fn say(&mut self, message: &str) {
        let line = format!("[launcher {}] {message}\n", now());
        let _ = self.file.write_all(line.as_bytes());
        let _ = self.file.flush();
        // Also to stderr, which is where a test harness and an interactive run see it.
        // On the box this goes to the scheduled task's nowhere, which is exactly why the
        // file is the primary.
        eprint!("{line}");
    }

    /// A handle for a child's stdout or stderr, rolling the file first if it has grown
    /// past its bound.
    fn handle(&mut self) -> Option<Stdio> {
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > LOG_ROLL_BYTES) {
            let _ = std::fs::rename(&self.path, &self.rolled);
            if let Ok(fresh) = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
            {
                self.file = fresh;
            }
        }
        OpenOptions::new()
            .append(true)
            .open(&self.path)
            .ok()
            .map(Stdio::from)
    }
}

/// An RFC 3339 timestamp, or a bare marker if the clock cannot be formatted — a log line
/// without a time is worth more than no log line.
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "?".to_string())
}
