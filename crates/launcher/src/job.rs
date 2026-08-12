//! The Windows job object that makes the panel's process tree die with the launcher.
//!
//! Without one, `taskkill /F /IM launcher.exe` — or the launcher crashing — leaves the
//! receiver and its Electron children running, holding the display and the ports, and the
//! next launcher start finds every socket taken. `deploy-windows` deals with the same
//! shape today by killing the whole tree explicitly (`taskkill /F /T`), which is only
//! available to something that is doing the killing rather than being killed.
//!
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates every member when the last handle to
//! the job closes, and the launcher holds the only handle — so "the launcher is gone"
//! and "the tree is gone" become the same event, enforced by the kernel rather than by
//! anybody remembering to clean up.
//!
//! The launcher assigns **itself** to the job where it can, because job membership is
//! inherited: every descendant is then a member the instant it exists, with no window
//! between `CreateProcess` returning and an `AssignProcessToJobObject` call. Where that
//! is refused — a launcher already inside a job that forbids nesting — it falls back to
//! assigning each child, which has that window but is much better than nothing.
//!
//! Everything here is a no-op off Windows. The launcher builds and runs on Linux so the
//! supervision loop can be driven by a test (ground rule 5), and a Linux test that wanted
//! this guarantee would want a different mechanism anyway.

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle as _;
    use std::process::Child;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    /// A job object holding the launcher's process tree.
    pub struct Job {
        handle: HANDLE,
        joined_self: bool,
    }

    impl Job {
        /// Create the job, set kill-on-close, and try to join it.
        ///
        /// # Errors
        /// Whatever Windows says if the job cannot be created or configured. The launcher
        /// treats that as "carry on without the guarantee, loudly" rather than as fatal:
        /// a panel with a supervision gap is better than a dark one.
        pub fn create() -> Result<Self, windows::core::Error> {
            // SAFETY: `CreateJobObjectW(None, None)` takes an optional security
            // descriptor and an optional name, and null for both is the documented way to
            // ask for an unnamed job with default security. The returned handle is owned
            // by this struct and closed in `Drop`.
            let handle = unsafe { CreateJobObjectW(None, None)? };

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: the pointer is to a live, fully initialised
            // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` on this stack frame, and the length
            // is that type's own size — which is what the
            // `JobObjectExtendedLimitInformation` class requires. The call does not
            // retain the pointer.
            let set = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    u32::try_from(std::mem::size_of_val(&limits)).unwrap_or(u32::MAX),
                )
            };
            if let Err(e) = set {
                // SAFETY: `handle` came from `CreateJobObjectW` and has not been closed.
                unsafe { CloseHandle(handle).ok() };
                return Err(e);
            }

            // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no closing,
            // and `AssignProcessToJobObject` is documented to accept it. Failure here is
            // an ordinary error (the launcher may already be in a job that forbids
            // nesting), not a safety problem.
            let joined_self =
                unsafe { AssignProcessToJobObject(handle, GetCurrentProcess()).is_ok() };

            Ok(Self {
                handle,
                joined_self,
            })
        }

        /// Did the launcher itself join, so that every descendant is a member by
        /// inheritance?
        pub const fn joined_self(&self) -> bool {
            self.joined_self
        }

        /// Put `child` in the job. A no-op worth doing anyway when [`Self::joined_self`]
        /// is true — Windows answers "already a member" and the launcher does not have to
        /// branch on which mechanism got it there.
        ///
        /// # Errors
        /// Whatever Windows says. The caller logs it and carries on.
        pub fn adopt(&self, child: &Child) -> Result<(), windows::core::Error> {
            if self.joined_self {
                return Ok(());
            }
            let handle = HANDLE(child.as_raw_handle());
            // SAFETY: the handle belongs to `child`, which outlives this call, and
            // `AssignProcessToJobObject` neither closes nor retains it.
            unsafe { AssignProcessToJobObject(self.handle, handle) }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // Closing the last handle is what fires kill-on-close, so this is not
            // housekeeping — it is the mechanism.
            //
            // SAFETY: `handle` came from `CreateJobObjectW`, is closed exactly once, and
            // nothing else holds a copy.
            unsafe { CloseHandle(self.handle).ok() };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::process::Child;

    /// The stand-in off Windows. It reports itself as not having joined anything, so the
    /// launcher's startup line says plainly that the guarantee is absent rather than
    /// implying one that is not there.
    pub struct Job(());

    impl Job {
        /// Always succeeds, and always does nothing.
        ///
        /// # Errors
        /// Never.
        pub const fn create() -> Result<Self, std::io::Error> {
            Ok(Self(()))
        }

        /// Always false: there is no job to have joined.
        pub const fn joined_self(&self) -> bool {
            false
        }

        /// Does nothing.
        ///
        /// # Errors
        /// Never.
        pub const fn adopt(&self, _child: &Child) -> Result<(), std::io::Error> {
            Ok(())
        }
    }
}

pub use imp::Job;
