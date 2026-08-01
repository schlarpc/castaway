//! The single-instance guard.
//!
//! A doubled service start used to be a visible mess with nobody there to close it, and a
//! *quiet* one (#100). The second process came up fully — its own kiosk window, its own
//! browser child — and only then failed on the first port it could not have, inside a
//! spawned task that logged one warning and did not exit. What was left on the glass was a
//! second full-screen kiosk showing the clock, discovering nothing, advertising nothing,
//! answering nothing, and indistinguishable from a healthy receiver until someone tried to
//! cast to it.
//!
//! So the guard runs before the window, the runtime and the browser child, and a second
//! launch says so and exits rather than rendering.
//!
//! # Why a lock and not a PID file
//!
//! The whole property this needs is that the lock disappears when the holder does —
//! *however* it goes, including a power cut mid-frame. Both backends get that from the OS
//! closing the handle at process death, so there is nothing to clean up and nothing to go
//! stale. A PID file has the opposite failure: a crash leaves a file naming a process that
//! is gone, and a panel that then refuses to start is a worse outcome than the doubled
//! start this exists to prevent.
//!
//! # The two backends
//!
//! Both take the same lock file in the state directory, and both leave it readable so the
//! loser can name the winner rather than saying only that something is wrong.
//!
//! - **Unix** — `flock(LOCK_EX | LOCK_NB)`. Advisory, which is all that is wanted: it does
//!   not stop anyone opening the file to read the pid out of it.
//! - **Windows** — the file is opened with `FILE_SHARE_READ` and nothing else, so a second
//!   process asking for write access is refused with a sharing violation. Read access is
//!   still shared, which is what keeps the pid readable.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

/// The lock file, relative to the state directory.
const LOCK_FILE: &str = "castaway.lock";

/// Why the guard could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    /// Another castaway holds the lock. The one outcome callers must handle differently:
    /// it is the expected answer to a doubled start, not a malfunction.
    #[error("castaway is already running{}", match .pid {
        Some(pid) => format!(" (pid {pid})"),
        None => String::new(),
    })]
    AlreadyRunning {
        /// The holder's process id, if it could be read back. Best-effort: the file is
        /// locked before it is written, so there is a moment where it is empty.
        pid: Option<u32>,
    },

    /// The lock file could not be created or opened at all — a missing state directory
    /// that cannot be made, or one that is not writable.
    #[error("opening the instance lock at {path}")]
    Unusable {
        /// The path that could not be used.
        path: String,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}

/// A held single-instance lock.
///
/// The lock lives in the open file handle, so this value must outlive everything it is
/// guarding — hold it for the whole of `main`. Dropping it releases the lock, which is why
/// it is `#[must_use]`: binding it to `_` would take the lock and give it straight back,
/// and the second launch this exists to stop would then succeed.
#[must_use = "the lock is released when this is dropped; hold it for the process lifetime"]
#[derive(Debug)]
pub struct InstanceLock {
    /// Kept open, not read. Closing it is what releases the lock.
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Where the lock is held, for logging.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Take the single-instance lock, or say who already has it.
///
/// # Errors
/// [`InstanceError::AlreadyRunning`] if another castaway holds it — the expected answer to
/// a doubled start. [`InstanceError::Unusable`] if the lock file cannot be opened at all.
pub fn acquire(state_dir: &Path) -> Result<InstanceLock, InstanceError> {
    let path = state_dir.join(LOCK_FILE);
    let unusable = |source: std::io::Error| InstanceError::Unusable {
        path: path.display().to_string(),
        source,
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(unusable)?;
    }

    let mut file = open_exclusive(&path).map_err(|e| match e {
        // Distinguishing "someone has it" from "the disk said no" is the whole point of
        // the error type: one is a normal second launch and one needs an operator.
        Contention::Held => InstanceError::AlreadyRunning {
            pid: read_pid(&path),
        },
        Contention::Io(e) => unusable(e),
    })?;

    // The pid is written *after* the lock is taken, so it can only ever describe the
    // process that actually holds it. Truncate first, or a shorter pid would leave digits
    // of the previous holder's behind and the message would name a process that is gone.
    let _ = file.set_len(0);
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();

    Ok(InstanceLock { _file: file, path })
}

/// Why an exclusive open failed: someone holds it, or the filesystem refused.
enum Contention {
    Held,
    Io(std::io::Error),
}

/// Read the holder's pid, best-effort.
///
/// Never an error: this only ever decorates a message. An empty or unparsable file means
/// the holder has the lock but has not written its pid yet, and "already running" without
/// a number is still the right thing to say.
fn read_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(unix)]
fn open_exclusive(path: &Path) -> Result<File, Contention> {
    use rustix::fs::{flock, FlockOperation};

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(Contention::Io)?;

    // Non-blocking: the answer wanted is "is it taken", not "wait until it is not". A
    // blocking lock here would hang the second launch forever instead of telling anyone.
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(Contention::Held),
        Err(e) => Err(Contention::Io(std::io::Error::from(e))),
    }
}

#[cfg(windows)]
fn open_exclusive(path: &Path) -> Result<File, Contention> {
    use std::os::windows::fs::OpenOptionsExt as _;

    /// `FILE_SHARE_READ`. Spelled out rather than pulled from `windows-sys`, because one
    /// constant is not worth a dependency in the crate that is meant to be wiring.
    ///
    /// Read sharing and nothing else: a second process asking for write access — which is
    /// exactly what a second castaway does — is refused, while anything opening read-only
    /// to look at the pid still succeeds.
    const FILE_SHARE_READ: u32 = 0x0000_0001;

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .map_err(|e| {
            // ERROR_SHARING_VIOLATION (32) is the holder saying no. Everything else is a
            // real filesystem problem and must not be reported as a doubled start.
            if e.raw_os_error() == Some(32) {
                Contention::Held
            } else {
                Contention::Io(e)
            }
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "castaway-instance-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The defect: a second launch came all the way up and served nothing.
    ///
    /// Both backends conflict with themselves within one process — `flock` is per open
    /// file description, and a Windows share mode applies to every opener — so this is a
    /// real test of the mechanism and not a stand-in for one.
    #[test]
    fn a_second_acquire_is_refused_while_the_first_is_held() {
        let dir = temp_dir("refused");
        let first = acquire(&dir).unwrap();

        match acquire(&dir) {
            Err(InstanceError::AlreadyRunning { pid }) => {
                assert_eq!(
                    pid,
                    Some(std::process::id()),
                    "the message must name the holder, or it says only that something is wrong"
                );
            }
            Err(e) => panic!("wrong error: {e}"),
            Ok(_) => panic!("two instances took the same lock"),
        }

        drop(first);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The property that rules out a PID file: releasing must need no cleanup.
    ///
    /// Here the release is a `drop`; in production it is the process dying, which the OS
    /// treats identically. A guard that only released on a *clean* exit would turn every
    /// crash into a receiver that refuses to start.
    #[test]
    fn releasing_the_lock_lets_the_next_launch_have_it() {
        let dir = temp_dir("released");

        let first = acquire(&dir).unwrap();
        let lock_path = first.path().to_path_buf();
        drop(first);

        let second = acquire(&dir).expect("the lock was not released");
        // The file is still there — it is the lock's *identity*, not a flag — so its
        // continued existence must not be what a launch tests.
        assert!(lock_path.exists(), "the lock file is reused, not deleted");
        drop(second);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A launch must not be blocked by a lock file left behind by a previous boot.
    #[test]
    fn a_leftover_lock_file_from_a_dead_process_does_not_block_a_start() {
        let dir = temp_dir("stale");
        // Exactly what a killed process leaves: the file, with a pid in it, unlocked.
        std::fs::write(dir.join(LOCK_FILE), "999999").unwrap();

        let lock = acquire(&dir).expect("a stale lock file must not stop a start");
        assert_eq!(
            read_pid(&dir.join(LOCK_FILE)),
            Some(std::process::id()),
            "the new holder rewrites the pid"
        );
        drop(lock);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The state directory is created on a first-ever boot rather than being required.
    #[test]
    fn the_state_directory_is_created_if_it_is_missing() {
        let dir = temp_dir("mkdir").join("nested").join("deeper");
        assert!(!dir.exists());
        let lock = acquire(&dir).expect("the guard creates its own directory");
        assert!(dir.exists());
        drop(lock);
        std::fs::remove_dir_all(temp_dir("mkdir")).ok();
    }
}
