//! Pulling a GPU buffer handle out of the browser process into ours.
//!
//! The browser (D36) renders offscreen into GPU buffers and tells us about them over a
//! text protocol — but a handle is only meaningful inside the process that owns it, so
//! the number in the message is useless until the kernel is asked to make it ours. Both
//! platforms can do that, and interestingly they do it the *same way round*: the consumer
//! reaches into the producer, given only the producer's own numbering. Nothing has to be
//! sent in-band, and the browser needs no native code to cooperate.
//!
//! - **Linux**: `pidfd_open(2)` + `pidfd_getfd(2)`. Requires `PTRACE_MODE_ATTACH`, so
//!   under the usual `kernel.yama.ptrace_scope = 1` it works for a direct child and
//!   fails for anything re-parented — and under scope 2 or 3 it fails outright. Since
//!   #271 this is the **fallback**, not the arrangement: production passes the
//!   descriptors themselves with `SCM_RIGHTS` (`crate::electron_fd_plane`, sent by the
//!   `castaway-browser-fd` addon — the "cannot be driven from JavaScript" objection is
//!   answered by that one small native piece), and this path carries the session only
//!   when the addon is absent.
//! - **Windows**: `DuplicateHandle`, with the child's process handle as the source. The
//!   handle `CreateProcess` returned already carries `PROCESS_DUP_HANDLE`, so there is
//!   no ptrace-equivalent policy to depend on — which is why Windows has no fd plane.
//!
//! The asymmetry that remains is ownership: an fd and an NT handle are both "close this
//! when done", but through different calls, so [`LocalHandle`] is a `cfg` pair rather
//! than one type. Everything above this module names only `LocalHandle`.
#![allow(unsafe_code)]

use crate::error::PipelineError;

/// A handle as the *producer* numbers it: an fd on Linux, an NT `HANDLE` on Windows.
///
/// Deliberately not a `RawFd` or a `HANDLE` — those types imply "valid in this process",
/// which is exactly what this is not. It is a number that means something somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteHandle(pub i64);

#[cfg(unix)]
pub use unix::{LocalHandle, ProcessRef};

#[cfg(windows)]
pub use windows::{LocalHandle, ProcessRef};

#[cfg(unix)]
mod unix {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};

    use super::{PipelineError, RemoteHandle};

    /// A descriptor now owned by this process; closed on drop.
    pub type LocalHandle = OwnedFd;

    /// A pidfd for the browser process, used to fetch its descriptors.
    #[derive(Debug)]
    pub struct ProcessRef(OwnedFd);

    impl ProcessRef {
        /// Open a reference to a running process by pid.
        ///
        /// A pidfd rather than the bare pid on purpose: it pins the *identity* of the
        /// process, so a pid recycled after the browser dies cannot have its descriptors
        /// read by mistake.
        ///
        /// # Errors
        /// [`PipelineError::GpuImport`] if the process is gone or not attachable.
        pub fn open(pid: u32) -> Result<Self, PipelineError> {
            let pid = i64::from(pid);
            // SAFETY: `pidfd_open` takes a pid and flags and returns a new fd or -1. No
            // memory crosses the boundary.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                return Err(PipelineError::GpuImport(format!(
                    "pidfd_open({pid}): {}",
                    std::io::Error::last_os_error()
                )));
            }
            // `syscall` returns `long`; a descriptor always fits in `RawFd`, so a value
            // that does not is the kernel disagreeing with its own ABI rather than
            // something to truncate quietly.
            let fd = RawFd::try_from(fd)
                .map_err(|_| PipelineError::GpuImport(format!("pidfd_open returned {fd}")))?;
            // SAFETY: the syscall returned a fresh descriptor nothing else owns.
            Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        /// Duplicate one of the process's descriptors into ours.
        ///
        /// # Errors
        /// [`PipelineError::GpuImport`] if the descriptor does not exist there, or the
        /// ptrace policy forbids the fetch — the failure a hardened box or a re-parented
        /// browser produces, so the message names it.
        pub fn pull(&self, remote: RemoteHandle) -> Result<LocalHandle, PipelineError> {
            // SAFETY: `pidfd_getfd` duplicates the named descriptor out of the process
            // behind `self.0`, which is a live pidfd we own. Nothing is dereferenced.
            let fd =
                unsafe { libc::syscall(libc::SYS_pidfd_getfd, self.0.as_raw_fd(), remote.0, 0) };
            if fd < 0 {
                let err = std::io::Error::last_os_error();
                return Err(PipelineError::GpuImport(format!(
                    "pidfd_getfd({}): {err} — needs PTRACE_MODE_ATTACH; under \
                     kernel.yama.ptrace_scope=1 the browser must be a direct child",
                    remote.0
                )));
            }
            let fd = RawFd::try_from(fd)
                .map_err(|_| PipelineError::GpuImport(format!("pidfd_getfd returned {fd}")))?;
            // SAFETY: the syscall returned a fresh descriptor nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::io::{AsRawHandle as _, OwnedHandle};

    use winapi::shared::minwindef::FALSE;
    use winapi::um::handleapi::DuplicateHandle;
    use winapi::um::processthreadsapi::GetCurrentProcess;
    use winapi::um::winnt::{DUPLICATE_SAME_ACCESS, HANDLE};

    use super::{PipelineError, RemoteHandle};

    /// A handle now owned by this process; `CloseHandle`d on drop.
    pub type LocalHandle = OwnedHandle;

    /// The browser's process handle, used to duplicate its handles into ours.
    #[derive(Debug)]
    pub struct ProcessRef(OwnedHandle);

    impl ProcessRef {
        /// Take a reference from the handle `CreateProcess` returned for the child.
        ///
        /// That handle already carries `PROCESS_DUP_HANDLE`, so unlike the Linux side
        /// there is no policy that can withdraw this at runtime. Ownership transfers:
        /// keep the `Child` alive for as long as you intend to talk to it, and clone the
        /// handle before handing it here if the caller still needs its own.
        #[must_use]
        pub const fn from_child_handle(handle: OwnedHandle) -> Self {
            Self(handle)
        }

        /// Duplicate one of the process's handles into ours.
        ///
        /// # Errors
        /// [`PipelineError::GpuImport`] if the source handle is invalid or the
        /// duplication is refused.
        pub fn pull(&self, remote: RemoteHandle) -> Result<LocalHandle, PipelineError> {
            let mut out: HANDLE = std::ptr::null_mut();
            // SAFETY: `self.0` is a live process handle with PROCESS_DUP_HANDLE;
            // `GetCurrentProcess` is a pseudo-handle needing no release; `out` is a live
            // local the call writes on success. `remote.0` is only interpreted inside the
            // source process, so a bogus value fails the call rather than corrupting us.
            let ok = unsafe {
                DuplicateHandle(
                    self.0.as_raw_handle().cast(),
                    remote.0 as HANDLE,
                    GetCurrentProcess(),
                    &raw mut out,
                    0,
                    FALSE,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 || out.is_null() {
                return Err(PipelineError::GpuImport(format!(
                    "DuplicateHandle({:#x}): {}",
                    remote.0,
                    std::io::Error::last_os_error()
                )));
            }
            // SAFETY: `DuplicateHandle` succeeded, so `out` is a fresh handle this
            // process owns and nothing else will close.
            Ok(unsafe {
                <OwnedHandle as std::os::windows::io::FromRawHandle>::from_raw_handle(out.cast())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_handle_is_just_a_number_from_elsewhere() {
        // The whole point of the newtype: it must not be confusable with a local one.
        // If this ever starts converting to RawFd/HANDLE implicitly, the type has lost
        // the property it exists for.
        let remote = RemoteHandle(108);
        assert_eq!(remote.0, 108);
        assert_eq!(remote, RemoteHandle(108));
        assert_ne!(remote, RemoteHandle(109));
    }

    #[cfg(unix)]
    #[test]
    fn pulling_from_a_process_that_does_not_exist_is_an_error_not_a_panic() {
        // pid 0 is never a real target for pidfd_open. A browser that died between the
        // paint message and the pull must degrade to a dropped frame.
        assert!(ProcessRef::open(0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pulling_a_descriptor_the_browser_does_not_have_is_an_error() {
        // Our own process stands in for the browser: fetching a descriptor number that
        // cannot be open proves the failure path returns rather than aborting.
        let me = ProcessRef::open(std::process::id()).expect("open self");
        assert!(me.pull(RemoteHandle(i64::from(i32::MAX))).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_pulled_descriptor_is_independent_of_the_original() {
        use std::os::fd::AsRawFd as _;
        // Pulling from ourselves is the loopback case, and it proves the returned fd is
        // a genuinely new one rather than the same number handed back.
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        let me = ProcessRef::open(std::process::id()).expect("open self");
        let pulled = me
            .pull(RemoteHandle(i64::from(file.as_raw_fd())))
            .expect("pull own fd");
        assert_ne!(pulled.as_raw_fd(), file.as_raw_fd());
        drop(pulled);
        // Closing the duplicate must not have closed the original.
        assert!(file.metadata().is_ok());
    }
}
