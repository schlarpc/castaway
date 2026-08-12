//! When to restart, when to back off, and when to conclude the bits are bad.
//!
//! The whole decision, as a pure function of what just happened (ground rule 3). No
//! clock is read here and no file is touched: the caller measures the run at the process
//! boundary and passes the duration in, which is what lets every constant below be
//! asserted in virtual time rather than waited out (#208/#236).
//!
//! The rule this exists to express, from #342: **a version that was ever healthy never
//! rolls back.** Its crashes are ordinary kiosk restarts — a driver bug, a wedged GPU, a
//! panic in one adapter — and rolling back on those would replace a working receiver
//! with an older one every time the panel had a bad night. Only a version that has never
//! once reported itself up, and that keeps dying quickly, is evidence against the bits.

use std::time::Duration;

/// The exit code the receiver uses to say "re-read `current.txt` and start whatever it
/// names now" — an update activating.
///
/// **Eight bits, deliberately.** Windows exit codes are 32-bit and a memorable
/// `0xCA57` would have been available there — but a Unix `wait` status carries only the
/// low byte, so the same constant would have meant one thing on the box and another in
/// the Linux VM test that drives this loop end to end. A reserved code that is only
/// reserved on one platform is worse than an unmemorable one.
///
/// 87 avoids everything that produces an exit code by itself: the CRT's small values,
/// `sysexits.h`'s 64–78, Rust's 101 for a panic, and the 126–165 band shells and signals
/// claim. It does collide with Windows' `ERROR_INVALID_PARAMETER`, which the receiver has
/// no path for returning as an exit code — and if it ever did, the consequence is benign:
/// the launcher would re-read `current.txt`, find the same version named, and start it
/// again. A false handshake costs one pointer read.
pub const ACTIVATE_EXIT_CODE: i32 = 87;

/// How long a version has to stay up before its exit stops counting against it.
///
/// Two minutes is long enough to get every adapter bound and the display open, and short
/// enough that three strikes is a handful of minutes rather than an evening. It is also
/// the same span the health marker is expected within, so "died young" and "never got
/// healthy" usually agree — and where they disagree, the marker wins, because it is a
/// statement by the receiver rather than an inference from a stopwatch.
pub const SETTLED: Duration = Duration::from_secs(120);

/// How many consecutive young deaths, of a version that has never been healthy, before
/// the launcher concludes the bits are bad rather than the night.
pub const STRIKES_BEFORE_ROLLBACK: u32 = 3;

/// The first backoff step, doubling from there.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);

/// The longest the panel is ever left dark by backoff alone.
///
/// Thirty seconds, not minutes: this is a wall display, and a person standing in front of
/// it reads a long gap as "it is broken" whether or not it is about to come back. The cap
/// exists to stop a tight crash loop from being a busy loop, not to punish.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// How a supervised receiver stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// It exited with [`ACTIVATE_EXIT_CODE`]: an update is ready and the launcher should
    /// re-read `current.txt` before starting anything.
    ///
    /// Activation is always "exit and let the launcher respawn me". A receiver that
    /// spawned its own successor would put it outside the interactive session
    /// (docs/cross-build.md, session 0), where it renders to nothing.
    Activate,
    /// Anything else. A clean exit and a panic are the same thing to a kiosk: the panel
    /// is dark and something has to start it again.
    Ended {
        /// What it exited with, for the log. The decision does not depend on it.
        code: Option<i32>,
    },
    /// There was nothing to spawn — `current.txt` names a tree with no receiver in it.
    ///
    /// A distinct variant rather than a fatal error, because it is exactly the state a
    /// half-deleted or hand-edited install leaves behind, and the answer to it is the
    /// same as for bad bits: go back to the version that worked.
    Missing,
}

/// One run of the receiver, as measured at the process boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// How it stopped.
    pub stopped: Stopped,
    /// How long it was up. Read from one clock, once, by the caller.
    pub lasted: Duration,
    /// Has this version *ever* written its health marker — not "did it this time".
    pub ever_healthy: bool,
    /// Is there a previous version, distinct from this one, to fall back to?
    pub rollback_target: bool,
}

/// What the launcher should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Start the same version again after `after`.
    Restart {
        /// How long to wait first. Zero after a run that settled.
        after: Duration,
    },
    /// Re-read `current.txt` and start whatever it names now, after `after`.
    Reload {
        /// Zero in the ordinary case: an update's downtime is seconds.
        after: Duration,
    },
    /// Copy `previous.txt` over `current.txt`, then start that, after `after`.
    RollBack {
        /// Same backoff as a restart — a rollback that also crash-loops must not spin.
        after: Duration,
    },
}

impl Next {
    /// How long to wait before doing it.
    #[must_use]
    pub const fn after(self) -> Duration {
        match self {
            Self::Restart { after } | Self::Reload { after } | Self::RollBack { after } => after,
        }
    }
}

/// The launcher's memory between runs.
///
/// Two counters that mean different things, which is why they are not one. `backoff`
/// paces *any* rapid loop, including a receiver that keeps asking to be reloaded.
/// `strikes` is evidence about a specific version's bits, and only a crash of a
/// never-healthy version is evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Supervisor {
    strikes: u32,
    backoff: u32,
}

impl Supervisor {
    /// A launcher that has just started, with nothing held against anything.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            strikes: 0,
            backoff: 0,
        }
    }

    /// Consecutive young deaths counted against the current version.
    #[must_use]
    pub const fn strikes(&self) -> u32 {
        self.strikes
    }

    /// Forget everything held against the current version.
    ///
    /// Called when the version being supervised changes — after a rollback, and after a
    /// reload lands on a different tree. Strikes are evidence about *these* bits, and
    /// carrying them across a version boundary would let a bad version take a good one
    /// down with it.
    pub const fn version_changed(&mut self) {
        self.strikes = 0;
        self.backoff = 0;
    }

    /// Decide what to do about `run`.
    pub fn on_exit(&mut self, run: Run) -> Next {
        if run.lasted >= SETTLED {
            // It was up long enough to count as working, whatever it did afterwards.
            self.strikes = 0;
            self.backoff = 0;
        } else {
            self.backoff = self.backoff.saturating_add(1);
            let evidence = !run.ever_healthy && !matches!(run.stopped, Stopped::Activate);
            if evidence {
                self.strikes = self.strikes.saturating_add(1);
            }
        }

        let after = backoff_delay(self.backoff);

        if self.strikes >= STRIKES_BEFORE_ROLLBACK && run.rollback_target && !run.ever_healthy {
            self.version_changed();
            return Next::RollBack { after };
        }
        match run.stopped {
            Stopped::Activate => Next::Reload { after },
            // A version whose receiver is missing is not restartable, but the launcher
            // has nowhere else to go until the strikes add up — so it waits and looks
            // again, which also covers the case of a tree still being written.
            Stopped::Ended { .. } | Stopped::Missing => Next::Restart { after },
        }
    }
}

/// The backoff ladder: nothing, then doubling from [`BACKOFF_BASE`] to [`BACKOFF_CAP`].
#[must_use]
pub fn backoff_delay(consecutive: u32) -> Duration {
    match consecutive {
        0 => Duration::ZERO,
        n => {
            // `n - 1` so the first young death waits `BACKOFF_BASE` rather than double it.
            // Saturating rather than wrapping: a shift past 63 is a crash loop that has
            // been going for a very long time, and the answer is the cap either way.
            let shift = (n - 1).min(u32::BITS);
            BACKOFF_BASE
                .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
                .min(BACKOFF_CAP)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        backoff_delay, Next, Run, Stopped, Supervisor, BACKOFF_BASE, BACKOFF_CAP, SETTLED,
        STRIKES_BEFORE_ROLLBACK,
    };

    /// A run that died young, of a version that never got healthy — the only shape that
    /// counts as evidence against the bits.
    fn bad_run() -> Run {
        Run {
            stopped: Stopped::Ended { code: Some(1) },
            lasted: Duration::from_secs(3),
            ever_healthy: false,
            rollback_target: true,
        }
    }

    #[test]
    fn three_young_deaths_of_a_version_that_never_came_up_roll_back() {
        let mut sup = Supervisor::new();
        for _ in 1..STRIKES_BEFORE_ROLLBACK {
            assert!(matches!(sup.on_exit(bad_run()), Next::Restart { .. }));
        }
        assert!(matches!(sup.on_exit(bad_run()), Next::RollBack { .. }));
        // And the evidence is spent: the version it rolled back to starts clean.
        assert_eq!(sup.strikes(), 0);
    }

    #[test]
    fn a_version_that_was_ever_healthy_is_restarted_forever_and_never_rolled_back() {
        let mut sup = Supervisor::new();
        let crashing = Run {
            ever_healthy: true,
            ..bad_run()
        };
        // Far past the strike count. This is the case the rule exists for: a working
        // receiver having a bad night must not be replaced by an older one.
        for _ in 0..STRIKES_BEFORE_ROLLBACK * 10 {
            assert!(matches!(sup.on_exit(crashing), Next::Restart { .. }));
        }
        assert_eq!(sup.strikes(), 0);
    }

    #[test]
    fn with_nowhere_to_roll_back_to_it_keeps_trying() {
        let mut sup = Supervisor::new();
        let alone = Run {
            rollback_target: false,
            ..bad_run()
        };
        for _ in 0..STRIKES_BEFORE_ROLLBACK * 3 {
            assert!(matches!(sup.on_exit(alone), Next::Restart { .. }));
        }
        // The strikes are still counted, so the moment a `previous.txt` appears the
        // rollback happens rather than starting the count over.
        assert!(sup.strikes() >= STRIKES_BEFORE_ROLLBACK);
        assert!(matches!(sup.on_exit(bad_run()), Next::RollBack { .. }));
    }

    #[test]
    fn a_run_that_settled_wipes_the_slate_even_at_two_strikes() {
        let mut sup = Supervisor::new();
        sup.on_exit(bad_run());
        sup.on_exit(bad_run());
        assert_eq!(sup.strikes(), STRIKES_BEFORE_ROLLBACK - 1);
        // Exactly the shipped constant, in virtual time: no test waits two minutes.
        let settled = Run {
            lasted: SETTLED,
            ..bad_run()
        };
        assert_eq!(
            sup.on_exit(settled),
            Next::Restart {
                after: Duration::ZERO
            }
        );
        assert_eq!(sup.strikes(), 0);
        // One second short of it is still young, and still counts.
        let nearly = Run {
            lasted: SETTLED - Duration::from_secs(1),
            ..bad_run()
        };
        sup.on_exit(nearly);
        assert_eq!(sup.strikes(), 1);
    }

    #[test]
    fn the_handshake_exit_is_not_evidence_against_the_bits() {
        let mut sup = Supervisor::new();
        let activating = Run {
            stopped: Stopped::Activate,
            ..bad_run()
        };
        // An update that activates seconds after start is not a crash — and a receiver
        // that staged an update on its first night would otherwise strike itself out.
        for _ in 0..STRIKES_BEFORE_ROLLBACK * 2 {
            assert!(matches!(sup.on_exit(activating), Next::Reload { .. }));
        }
        assert_eq!(sup.strikes(), 0);
    }

    #[test]
    fn a_handshake_loop_is_still_paced_even_though_it_is_not_a_crash() {
        let mut sup = Supervisor::new();
        let activating = Run {
            stopped: Stopped::Activate,
            ..bad_run()
        };
        assert_eq!(sup.on_exit(activating).after(), BACKOFF_BASE);
        assert_eq!(sup.on_exit(activating).after(), BACKOFF_BASE * 2);
    }

    #[test]
    fn a_current_txt_naming_a_tree_with_no_receiver_rolls_back_like_bad_bits() {
        let mut sup = Supervisor::new();
        let missing = Run {
            stopped: Stopped::Missing,
            lasted: Duration::ZERO,
            ever_healthy: false,
            rollback_target: true,
        };
        for _ in 1..STRIKES_BEFORE_ROLLBACK {
            assert!(matches!(sup.on_exit(missing), Next::Restart { .. }));
        }
        assert!(matches!(sup.on_exit(missing), Next::RollBack { .. }));
    }

    #[test]
    fn the_backoff_ladder_doubles_and_then_stops() {
        assert_eq!(backoff_delay(0), Duration::ZERO);
        assert_eq!(backoff_delay(1), BACKOFF_BASE);
        assert_eq!(backoff_delay(2), BACKOFF_BASE * 2);
        assert_eq!(backoff_delay(3), BACKOFF_BASE * 4);
        assert_eq!(backoff_delay(4), BACKOFF_BASE * 8);
        assert_eq!(backoff_delay(5), BACKOFF_BASE * 16);
        // And the cap holds, including where a naive shift would have overflowed. A
        // launcher that has been looping since the last power cut is still a launcher
        // that tries again every thirty seconds.
        for n in 6..80 {
            assert_eq!(backoff_delay(n), BACKOFF_CAP, "at {n} consecutive");
        }
    }

    #[test]
    fn a_new_version_starts_with_a_clean_record() {
        let mut sup = Supervisor::new();
        sup.on_exit(bad_run());
        sup.on_exit(bad_run());
        sup.version_changed();
        assert_eq!(sup.strikes(), 0);
        assert_eq!(sup.on_exit(bad_run()).after(), BACKOFF_BASE);
    }
}
