//! When to look for an update, and when it is safe to restart into one.
//!
//! The whole schedule as a pure function of the moment (ground rule 3): no clock is read
//! here, no socket is opened, and every constant below is asserted against its shipped
//! value rather than waited out (#208/#236). The agent reads the clock once per turn of
//! its loop and hands the answer in.
//!
//! Two things this is trying to get right, and they pull in opposite directions. An
//! update has to happen — an unattended panel that never takes one is the whole problem.
//! And it must never happen *while somebody is using the panel*, because the failure mode
//! is a wall display going black in the middle of what a person is watching. So: a window
//! at night, and inside that window a further requirement that the panel actually be
//! quiet. Hackerspaces are nocturnal, so 4 a.m. is not automatically free — the window is
//! necessary and nothing like sufficient.
//!
//! Staging is deliberately decoupled from activation. A slow download at 03:58 does not
//! eat the window, and a tree that finished staging after the panel went busy simply
//! waits for tomorrow.

use std::fmt;
use std::time::Duration;

use thiserror::Error;

/// A time of day, to the minute, on the panel's own local calendar.
///
/// Local rather than UTC because the thing being avoided is *people*, and people keep
/// local hours. The cost is one honest caveat: on the two days a year local time jumps,
/// a window can be traversed twice or skipped entirely. Skipped means one missed night,
/// which the next night fixes; traversed twice means one extra check. Neither is worth a
/// timezone database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MinuteOfDay(u16);

/// Minutes in a day.
const DAY: u16 = 24 * 60;

impl MinuteOfDay {
    /// `hour:minute`, or `None` if that is not a time.
    #[must_use]
    pub const fn new(hour: u8, minute: u8) -> Option<Self> {
        if hour < 24 && minute < 60 {
            Some(Self(hour as u16 * 60 + minute as u16))
        } else {
            None
        }
    }

    /// Parse `HH:MM`, the form the config file uses.
    ///
    /// # Errors
    /// [`PolicyError::NotATime`] for anything else. Deliberately strict: `3:30` and
    /// `03:30 AM` are both things somebody might write, and guessing at either is how a
    /// panel ends up updating at an hour nobody chose.
    pub fn parse(text: &str) -> Result<Self, PolicyError> {
        let bad = || PolicyError::NotATime {
            text: text.to_owned(),
        };
        let (h, m) = text.split_once(':').ok_or_else(bad)?;
        if h.len() != 2 || m.len() != 2 {
            return Err(bad());
        }
        let hour = h.parse::<u8>().map_err(|_| bad())?;
        let minute = m.parse::<u8>().map_err(|_| bad())?;
        Self::new(hour, minute).ok_or_else(bad)
    }

    /// Minutes since local midnight.
    #[must_use]
    pub const fn minutes(self) -> u16 {
        self.0
    }

    /// The local minute of day at `unix_secs`, given the machine's offset east of UTC.
    ///
    /// Pure, and takes the offset rather than reading it, for the reason `main` reads it
    /// once at startup: on unix the local offset can only be resolved soundly while the
    /// process is single-threaded, which stops being true the moment the runtime exists.
    #[must_use]
    pub fn at_unix(unix_secs: u64, offset_secs: i32) -> Self {
        const DAY_SECS: u64 = 24 * 60 * 60;
        // A west-of-UTC offset is the same rotation as its complement to the east, so
        // reducing it first keeps the whole calculation in unsigned arithmetic — no
        // signed intermediate that could wrap, and no negative minute to interpret.
        let east = u64::from(offset_secs.rem_euclid(24 * 60 * 60).unsigned_abs());
        let local = (unix_secs + east) % DAY_SECS;
        // `local / 60` is under 1440 by construction.
        Self(u16::try_from(local / 60).unwrap_or(0))
    }
}

impl fmt::Display for MinuteOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

/// The nightly window, `start` inclusive and `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    start: MinuteOfDay,
    end: MinuteOfDay,
}

impl Window {
    /// A window from `start` to `end`.
    ///
    /// Wrapping past midnight is allowed and is not a special case for the caller: a
    /// hackerspace that empties out at 05:00 might well want 05:00–07:00, and one that
    /// empties at midnight might want 23:30–01:00. Both are the same type.
    ///
    /// # Errors
    /// [`PolicyError::EmptyWindow`] if the two are the same minute, which describes no
    /// time at all rather than the whole day.
    pub const fn new(start: MinuteOfDay, end: MinuteOfDay) -> Result<Self, PolicyError> {
        if start.0 == end.0 {
            return Err(PolicyError::EmptyWindow);
        }
        Ok(Self { start, end })
    }

    /// When it opens.
    #[must_use]
    pub const fn start(&self) -> MinuteOfDay {
        self.start
    }

    /// When it closes.
    #[must_use]
    pub const fn end(&self) -> MinuteOfDay {
        self.end
    }

    /// Is `at` inside it?
    #[must_use]
    pub const fn contains(&self, at: MinuteOfDay) -> bool {
        if self.start.0 < self.end.0 {
            at.0 >= self.start.0 && at.0 < self.end.0
        } else {
            // Wraps midnight: everything from the start to the end of the day, and
            // everything from the start of the day to the end.
            at.0 >= self.start.0 || at.0 < self.end.0
        }
    }

    /// How long from `at` until it next opens. Zero if it is open now.
    #[must_use]
    pub fn until_open(&self, at: MinuteOfDay) -> Duration {
        if self.contains(at) {
            return Duration::ZERO;
        }
        minutes(forward(at.0, self.start.0))
    }

    /// How long from `at` until it closes. Zero if it is not open.
    #[must_use]
    pub fn until_close(&self, at: MinuteOfDay) -> Duration {
        if self.contains(at) {
            minutes(forward(at.0, self.end.0))
        } else {
            Duration::ZERO
        }
    }
}

/// Minutes from `from` forward to `to`, going the long way round midnight if it has to.
/// A whole day when they are equal, because "now" is never the answer to "when next".
const fn forward(from: u16, to: u16) -> u16 {
    if to > from {
        to - from
    } else {
        DAY - from + to
    }
}

const fn minutes(n: u16) -> Duration {
    Duration::from_secs(n as u64 * 60)
}

/// The schedule the receiver runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// When updating is allowed at all.
    pub window: Window,
    /// How long the panel has to have gone untouched before a restart is acceptable.
    pub idle_after: Duration,
    /// How often to look again while waiting for that inside the window.
    pub recheck: Duration,
}

/// The default window: 03:30 to 05:00.
///
/// Late enough that a hackerspace evening has ended, early enough to be well clear of
/// anyone arriving. Ninety minutes is six chances at the fifteen-minute idle recheck,
/// which is enough for one late-night session to end without being so wide that a
/// restart could surprise somebody.
const DEFAULT_WINDOW: (u8, u8, u8, u8) = (3, 30, 5, 0);

/// How long the panel must be untouched. Fifteen minutes is longer than a pause in a
/// film and shorter than a room emptying out.
pub const DEFAULT_IDLE_AFTER: Duration = Duration::from_secs(15 * 60);

/// How often to look again while the window is open and the panel is not.
pub const DEFAULT_RECHECK: Duration = Duration::from_secs(15 * 60);

impl Default for Policy {
    fn default() -> Self {
        let (sh, sm, eh, em) = DEFAULT_WINDOW;
        let start = MinuteOfDay::new(sh, sm);
        let end = MinuteOfDay::new(eh, em);
        // The defaults are constants in this file; a `None` here would be this file
        // contradicting itself, and there is no configuration a caller could supply to
        // fix it. Falling back to midnight-to-one keeps the type total without pretending
        // the impossible case is meaningful.
        let window = match (start, end) {
            (Some(start), Some(end)) => Window::new(start, end).ok(),
            _ => None,
        };
        Self {
            window: window.unwrap_or(Window {
                start: MinuteOfDay(0),
                end: MinuteOfDay(60),
            }),
            idle_after: DEFAULT_IDLE_AFTER,
            recheck: DEFAULT_RECHECK,
        }
    }
}

/// Where the updater has got to tonight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// This window has not been looked at yet.
    Fresh,
    /// It has, and there was nothing newer.
    UpToDate,
    /// A tree is staged, complete, and waiting for the panel to go quiet.
    Staged,
}

/// Who is asking.
///
/// The whole schedule below exists to guess at one question — *is there a person using
/// this panel?* — and a manual request is that question already answered, out loud, by
/// somebody standing in front of it (#360). So a manual request skips the window and
/// skips the idle gate, and skips nothing else: the trust path is not a schedule and has
/// no business knowing this type exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// The nightly loop, on its own timer, with nobody watching.
    Scheduled,
    /// Somebody pressed "Check for updates" and is standing there.
    Manual,
}

/// What the panel looks like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// The local time, to the minute.
    pub at: MinuteOfDay,
    /// Where the updater has got to.
    pub phase: Phase,
    /// Is a sender casting right now? Not the same question as idleness: a film playing
    /// to an empty room touches no input device for two hours.
    pub casting: bool,
    /// How long since anybody touched the panel.
    pub idle_for: Duration,
    /// Whose turn this is.
    pub request: Request,
}

/// What to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing, for this long.
    Wait(Duration),
    /// Ask the release API what Latest is.
    Check,
    /// Point `current.txt` at the staged tree and hand back to the launcher.
    Activate,
}

/// Decide.
#[must_use]
pub fn decide(policy: &Policy, obs: &Observation) -> Action {
    // A person is standing at the panel, so there is nothing left to wait for: the window
    // and the idle gate are both proxies for "is anyone here?", and the press answered
    // that. Everything else — a `hold` file, the attestation, the build ordering, the
    // digest — is outside this function and stays exactly where it was.
    if obs.request == Request::Manual {
        return match obs.phase {
            // Already downloaded, verified and named: the only thing left is the restart.
            Phase::Staged => Action::Activate,
            // Including `UpToDate`: "I looked at 03:30 and there was nothing" is not an
            // answer to somebody asking at noon, which is usually the moment just after
            // they pushed a release.
            Phase::Fresh | Phase::UpToDate => Action::Check,
        };
    }
    if !policy.window.contains(obs.at) {
        return Action::Wait(policy.window.until_open(obs.at));
    }
    match obs.phase {
        Phase::Fresh => Action::Check,
        // Nothing newer exists, so there is nothing to do until the window comes round
        // again. One long sleep rather than a poll: `promote` moving Latest at 04:12 is
        // not worth waking up for, and tomorrow's window will find it.
        Phase::UpToDate => Action::Wait(policy.window.until_close(obs.at)),
        Phase::Staged => {
            if obs.casting || obs.idle_for < policy.idle_after {
                Action::Wait(policy.recheck)
            } else {
                Action::Activate
            }
        }
    }
}

/// Why a schedule could not be read.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    /// Not `HH:MM`.
    #[error("{text:?} is not a time of day (want HH:MM, 24-hour)")]
    NotATime {
        /// What was written.
        text: String,
    },
    /// A window that starts and ends at the same minute.
    #[error("the update window starts and ends at the same minute")]
    EmptyWindow,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        decide, Action, MinuteOfDay, Observation, Phase, Policy, PolicyError, Request, Window,
        DEFAULT_IDLE_AFTER,
    };

    fn at(h: u8, m: u8) -> MinuteOfDay {
        MinuteOfDay::new(h, m).expect("a real time")
    }

    fn watching(at: MinuteOfDay, phase: Phase) -> Observation {
        Observation {
            at,
            phase,
            casting: true,
            idle_for: Duration::ZERO,
            request: Request::Scheduled,
        }
    }

    fn quiet(at: MinuteOfDay, phase: Phase) -> Observation {
        Observation {
            at,
            phase,
            casting: false,
            idle_for: DEFAULT_IDLE_AFTER,
            request: Request::Scheduled,
        }
    }

    #[test]
    fn the_shipped_window_is_half_past_three_to_five() {
        // The constants as they ship, asserted rather than waited out. A change to the
        // default hour is a change to when the panel restarts itself, and it should not
        // be possible to make one by accident.
        let policy = Policy::default();
        assert_eq!(policy.window.start().to_string(), "03:30");
        assert_eq!(policy.window.end().to_string(), "05:00");
        assert_eq!(policy.idle_after, Duration::from_secs(15 * 60));
    }

    #[test]
    fn outside_the_window_it_sleeps_until_the_window_opens() {
        let policy = Policy::default();
        // Evening: nine and a half hours to go, and no amount of idleness changes that.
        let action = decide(&policy, &quiet(at(18, 0), Phase::Staged));
        assert_eq!(
            action,
            Action::Wait(Duration::from_secs((9 * 60 + 30) * 60))
        );
        // Just after the window closed: nearly a whole day.
        let action = decide(&policy, &quiet(at(5, 0), Phase::Fresh));
        assert_eq!(
            action,
            Action::Wait(Duration::from_secs((22 * 60 + 30) * 60))
        );
    }

    #[test]
    fn the_window_is_open_at_its_start_and_shut_at_its_end() {
        let policy = Policy::default();
        assert!(policy.window.contains(at(3, 30)));
        assert!(policy.window.contains(at(4, 59)));
        assert!(!policy.window.contains(at(5, 0)));
        assert!(!policy.window.contains(at(3, 29)));
    }

    #[test]
    fn a_window_that_wraps_midnight_is_not_a_special_case_for_the_caller() {
        let window = Window::new(at(23, 30), at(1, 0)).expect("a window");
        assert!(window.contains(at(23, 30)));
        assert!(window.contains(at(0, 0)));
        assert!(window.contains(at(0, 59)));
        assert!(!window.contains(at(1, 0)));
        assert!(!window.contains(at(12, 0)));
        assert_eq!(window.until_open(at(23, 0)), Duration::from_secs(30 * 60));
        assert_eq!(
            window.until_open(at(2, 0)),
            Duration::from_secs(21 * 60 * 60 + 30 * 60)
        );
        assert_eq!(window.until_close(at(23, 45)), Duration::from_secs(75 * 60));
    }

    #[test]
    fn a_window_of_no_width_is_refused_rather_than_read_as_the_whole_day() {
        assert_eq!(
            Window::new(at(3, 30), at(3, 30)),
            Err(PolicyError::EmptyWindow)
        );
    }

    #[test]
    fn inside_the_window_the_first_thing_it_does_is_look() {
        let policy = Policy::default();
        assert_eq!(
            decide(&policy, &watching(at(3, 30), Phase::Fresh)),
            Action::Check
        );
        // Even mid-cast: checking costs one HTTPS request and changes nothing on the
        // panel. It is *activation* that has to wait for quiet, not the looking.
        assert_eq!(
            decide(&policy, &watching(at(4, 30), Phase::Fresh)),
            Action::Check
        );
    }

    #[test]
    fn nothing_newer_means_one_long_sleep_rather_than_a_poll() {
        let policy = Policy::default();
        assert_eq!(
            decide(&policy, &quiet(at(3, 30), Phase::UpToDate)),
            Action::Wait(Duration::from_secs(90 * 60))
        );
    }

    #[test]
    fn a_staged_update_waits_for_the_panel_to_be_genuinely_unused() {
        let policy = Policy::default();
        // Somebody is casting: the panel is in use even if nobody has touched it for
        // hours, which is exactly what a film looks like.
        let mid_film = Observation {
            casting: true,
            idle_for: Duration::from_secs(2 * 60 * 60),
            ..quiet(at(4, 0), Phase::Staged)
        };
        assert_eq!(decide(&policy, &mid_film), Action::Wait(policy.recheck));
        // Nothing casting, but somebody touched the glass a minute ago.
        let just_touched = Observation {
            casting: false,
            idle_for: Duration::from_secs(60),
            ..quiet(at(4, 0), Phase::Staged)
        };
        assert_eq!(decide(&policy, &just_touched), Action::Wait(policy.recheck));
        // Both conditions, and only then.
        assert_eq!(
            decide(&policy, &quiet(at(4, 0), Phase::Staged)),
            Action::Activate
        );
    }

    #[test]
    fn a_staged_tree_that_missed_its_window_waits_for_tomorrow_rather_than_restarting_late() {
        let policy = Policy::default();
        // 05:01, panel quiet, update ready — and the answer is still "not now". The
        // window is the promise; keeping it when it is inconvenient is what makes it one.
        let action = decide(&policy, &quiet(at(5, 1), Phase::Staged));
        assert!(matches!(action, Action::Wait(d) if d > Duration::from_secs(20 * 60 * 60)));
    }

    /// The same observation, asked for by a person rather than by the clock.
    fn by_hand(obs: Observation) -> Observation {
        Observation {
            request: Request::Manual,
            ..obs
        }
    }

    #[test]
    fn a_manual_request_never_waits_for_the_window() {
        let policy = Policy::default();
        // Two in the afternoon, which is as far from 03:30–05:00 as the day gets. The
        // scheduled answer is nine and a half hours of sleep; the manual one is "look
        // now", because the person asking has just pushed a release.
        let afternoon = quiet(at(14, 0), Phase::Fresh);
        assert!(matches!(decide(&policy, &afternoon), Action::Wait(_)));
        assert_eq!(decide(&policy, &by_hand(afternoon)), Action::Check);
    }

    #[test]
    fn a_manual_request_looks_again_even_though_tonight_already_concluded() {
        let policy = Policy::default();
        // The nightly loop looked at 03:30, found nothing, and is asleep until the window
        // shuts. A press at 03:45 is a *new* question and gets a new look.
        let settled = quiet(at(3, 45), Phase::UpToDate);
        assert!(matches!(decide(&policy, &settled), Action::Wait(_)));
        assert_eq!(decide(&policy, &by_hand(settled)), Action::Check);
    }

    #[test]
    fn a_manual_restart_skips_the_idle_gate_because_the_person_is_the_idle_gate() {
        let policy = Policy::default();
        // Inside the window, so the idle gate is the *only* thing refusing: casting, and
        // the glass touched a second ago. The press overrides it — it is somebody saying
        // "yes, interrupt this", which is the one thing the idle gate is guessing at.
        let busy = Observation {
            casting: true,
            idle_for: Duration::ZERO,
            ..quiet(at(4, 0), Phase::Staged)
        };
        assert_eq!(decide(&policy, &busy), Action::Wait(policy.recheck));
        assert_eq!(decide(&policy, &by_hand(busy)), Action::Activate);
    }

    #[test]
    fn a_manual_request_is_the_only_one_that_is_never_told_to_wait() {
        let policy = Policy::default();
        // Every combination that exists, so a phase added later cannot quietly acquire a
        // manual answer of "wait" — which on the glass is a screen that says nothing.
        for phase in [Phase::Fresh, Phase::UpToDate, Phase::Staged] {
            for casting in [false, true] {
                for hour in [0, 4, 14, 23] {
                    let obs = Observation {
                        at: at(hour, 0),
                        phase,
                        casting,
                        idle_for: Duration::ZERO,
                        request: Request::Manual,
                    };
                    assert!(
                        !matches!(decide(&policy, &obs), Action::Wait(_)),
                        "a manual request was told to wait: {obs:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_local_minute_is_the_unix_second_shifted_by_the_machines_offset() {
        // 1786500000 = 2026-08-11T02:00:00Z.
        let unix = 1_786_500_000;
        assert_eq!(MinuteOfDay::at_unix(unix, 0), at(2, 0));
        assert_eq!(MinuteOfDay::at_unix(unix, 2 * 3600), at(4, 0));
        // West of UTC, which puts local time on the *previous* day — the case a
        // subtraction that stays in `u64` gets catastrophically wrong, and the case an
        // American hackerspace is in every night of the year.
        assert_eq!(MinuteOfDay::at_unix(unix, -10 * 3600), at(16, 0));
        // And the half-hour zones, because they exist and 30 is not 0.
        assert_eq!(MinuteOfDay::at_unix(unix, 5 * 3600 + 1800), at(7, 30));
    }

    #[test]
    fn a_time_is_read_strictly_or_not_at_all() {
        assert_eq!(MinuteOfDay::parse("03:30").expect("parses"), at(3, 30));
        assert_eq!(MinuteOfDay::parse("00:00").expect("parses"), at(0, 0));
        assert_eq!(MinuteOfDay::parse("23:59").expect("parses"), at(23, 59));
        for hostile in ["3:30", "03:30 AM", "24:00", "03:60", "0330", "", "03:30:00"] {
            assert!(
                matches!(
                    MinuteOfDay::parse(hostile),
                    Err(PolicyError::NotATime { .. })
                ),
                "{hostile:?} was accepted as a time"
            );
        }
    }
}
