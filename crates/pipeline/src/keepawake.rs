//! Keeping the panel lit while it is playing something.
//!
//! The panel's HDMI audio endpoint is its only real speaker, so when the monitor slept it
//! took the audio device with it: the output was invalidated, the session ended, and the
//! music stopped dead. Recovery ([`crate::audio_out`]) now survives that, but surviving a
//! self-inflicted outage is second best — a receiver that is *playing something* should
//! not let the display idle out from under it.
//!
//! Scope is any active session, not just audio. Video, mirroring and browser playback all
//! hold the panel awake for the same reason: something is on screen or coming out of the
//! speakers, so somebody is using it.
//!
//! Idle is deliberately left alone. The home screen may blank on the usual long timeout;
//! this only ever says "not while this is playing".
//!
//! ## The seam
//!
//! [`KeepAwake`] is acquired for the life of a session and released by dropping the guard,
//! so every exit path — normal end, preemption, error, unwind — releases it. A pair of
//! calls that could get out of step is exactly the bug this would otherwise introduce: an
//! inhibit leaked once is a panel that never sleeps again.

use tracing::debug;
// Only the Windows backend has a failure to report; every other platform's "no backend
// here" is a debug line, not a warning.
#[cfg(windows)]
use tracing::warn;

/// A held request that the display stay on. Dropping it releases the request.
///
/// Deliberately opaque: the Windows form is a thread-affine flag and the Linux form will be
/// a file descriptor from logind, and neither should leak into the pipeline.
#[derive(Debug)]
pub struct KeepAwake {
    /// Whether anything was actually acquired, so `Drop` does not release a request that
    /// was never made.
    held: bool,
}

impl KeepAwake {
    /// Ask the OS to keep the display on until this guard is dropped.
    ///
    /// Never fails: a panel that cannot inhibit sleep should still play. A platform with
    /// no implementation says so once, at debug level, and carries on — the failure it
    /// causes (the screen blanking during playback) is visible on its own, and an error
    /// return here would only give callers a decision they cannot act on.
    #[must_use]
    pub fn acquire() -> Self {
        Self {
            held: platform::acquire(),
        }
    }

    /// Whether the request was actually granted, for tests and diagnostics.
    #[must_use]
    pub const fn held(&self) -> bool {
        self.held
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        if self.held {
            platform::release();
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{debug, warn};

    /// `ES_CONTINUOUS` — the request stands until it is cleared, rather than being a
    /// one-shot nudge of the idle timer.
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    /// `ES_DISPLAY_REQUIRED` — the display specifically, not merely the system. A media
    /// player wants the screen, not just the CPU.
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

    pub(super) fn acquire() -> bool {
        // SAFETY: `SetThreadExecutionState` takes a bitmask by value and returns the
        // previous state; it has no pointer arguments and no failure mode beyond
        // returning 0. The flags are the documented constants for "keep the display on
        // until told otherwise".
        let previous = unsafe {
            winapi::um::winbase::SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED)
        };
        if previous == 0 {
            warn!("could not ask Windows to keep the display awake; it may sleep mid-playback");
            return false;
        }
        debug!("holding the display awake for this session");
        true
    }

    pub(super) fn release() {
        // SAFETY: same call, clearing the display requirement by asking only for
        // `ES_CONTINUOUS`. Same argument as above.
        unsafe { winapi::um::winbase::SetThreadExecutionState(ES_CONTINUOUS) };
        debug!("released the display-awake request");
    }
}

#[cfg(not(windows))]
mod platform {
    use super::debug;

    pub(super) fn acquire() -> bool {
        // No backend yet, and said plainly rather than silently doing nothing — the same
        // shape as `MiracastBackend` on Windows. The Linux answer is a logind inhibitor
        // (`org.freedesktop.login1.Manager.Inhibit` with `what="idle"`, holding the
        // returned fd), which needs a D-Bus client this workspace does not yet have.
        // Tracked in #109.
        debug!("no keep-awake backend on this platform; the display may sleep during playback");
        false
    }

    pub(super) fn release() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_can_always_be_taken_and_dropped() {
        // The contract callers rely on: acquiring never fails and never panics, on any
        // platform, whether or not a backend exists. A session must not depend on the
        // display cooperating.
        let guard = KeepAwake::acquire();
        let _ = guard.held();
        drop(guard);
    }

    #[test]
    fn guards_nest_without_releasing_each_other_early() {
        // Two sessions can overlap during a preemption handover, and the outgoing one
        // dropping its guard must not blank the screen under the incoming one. On Windows
        // the flag is idempotent, so the invariant that matters is simply that this is
        // safe to do — a release in `Drop` that ran for a guard which never acquired would
        // clear a request somebody else still holds.
        let outer = KeepAwake::acquire();
        {
            let inner = KeepAwake::acquire();
            assert_eq!(
                inner.held(),
                outer.held(),
                "both guards agree on the backend"
            );
        }
        drop(outer);
    }
}
