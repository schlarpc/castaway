//! A thread that watches the parts of the process that cannot report on themselves (#368).
//!
//! On 2026-08-15 the receiver stopped serving: the HTTP host accepted TCP and answered
//! nothing, no phone could join, and the log's last line was an ordinary one from a minute
//! earlier. From the outside the box could say that two of twenty-six threads were
//! `Running` and that the process was burning two cores; from the inside it said nothing
//! at all, because everything that logs was inside the stall.
//!
//! This is the way out of that: an ordinary OS thread, deliberately *not* on the runtime,
//! holding a [`Heartbeat`] that the runtime touches and another that the render loop
//! touches. It cannot be starved by either of them, so it can report a stall while the
//! stall is happening rather than after it clears — which, in the observed wedge, never
//! happened at all.
//!
//! What it deliberately does not do is judge. A render loop that has not drawn for a
//! minute is the *normal* state of an idle panel (#59), so the line reports both ages and
//! says which threshold tripped; reading it is a person's job.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use castaway_core::Heartbeat;
use tracing::{info, warn};

/// How often the watchdog looks.
const INTERVAL: Duration = Duration::from_secs(1);

/// How long the runtime may go without a beat before it is worth a line.
///
/// The beating task sleeps a second, so this is five missed beats: long enough that a
/// loaded box under a big cast does not produce a line, short enough that a stall is dated
/// to within a few seconds of when it started.
const RUNTIME_STALL: Duration = Duration::from_secs(5);

/// How often to repeat the line while a stall continues.
///
/// A stall that lasts minutes should leave a trail rather than one line — the trail is
/// what says whether it ever ended — but not one a second.
const REPEAT: Duration = Duration::from_secs(15);

/// The heartbeats a running receiver keeps touching.
///
/// Cloned into the parts that beat them; the watchdog holds the other end.
#[derive(Debug, Clone)]
pub struct Beats {
    /// Beaten by a task on the tokio runtime. Its age is how long the runtime has gone
    /// without scheduling an ordinary timer.
    pub runtime: Heartbeat,
    /// Beaten by the kiosk's event loop on every pass. A large age is not by itself a
    /// fault: an idle panel sleeps until something wakes it.
    pub render: Heartbeat,
}

impl Beats {
    /// Fresh heartbeats, as of `now`.
    #[must_use]
    pub fn new(now: Instant) -> Self {
        Self {
            runtime: Heartbeat::new(now),
            render: Heartbeat::new(now),
        }
    }
}

/// Start the beating task on the runtime and the watching thread beside it.
///
/// The task is the *subject*: it does nothing but wake once a second and say so, so its
/// age measures exactly one thing — whether the runtime is still scheduling work. Anything
/// else would confound "the runtime stopped" with "this particular job got slow".
pub fn spawn(runtime: &tokio::runtime::Handle, beats: &Beats, stop: Arc<AtomicBool>) {
    let beat = beats.runtime.clone();
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(INTERVAL).await;
            beat.beat(Instant::now());
        }
    });

    let beats = beats.clone();
    std::thread::Builder::new()
        .name("castaway-watchdog".to_owned())
        .spawn(move || watch(&beats, &stop))
        .map_or_else(
            |e| warn!(error = %e, "watchdog: could not start; a stall will go unreported"),
            |_| info!(interval_s = INTERVAL.as_secs(), "watchdog: watching"),
        );
}

/// What one look at the heartbeats is worth saying.
///
/// The decision is separated from the saying so it can be asserted against a clock a test
/// chose rather than one it waited out: `RUNTIME_STALL` and `REPEAT` are what ship, and a
/// test that slept them would be asserting the sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Say {
    /// Nothing has changed that a reader needs.
    Nothing,
    /// The runtime is not scheduling. `render` rides along and is never judged — an idle
    /// panel that has not drawn for an hour is doing exactly what #59 asked of it.
    Stalled { runtime: Duration, render: Duration },
    /// It is scheduling again, `since_first_line` after the stall was first reported.
    Recovered { since_first_line: Duration },
}

/// The watchdog's memory between looks: when the current stall was first reported, and
/// when it was last reported. The first dates the stall for the reader, the second is what
/// keeps the trail to one line per [`REPEAT`].
#[derive(Debug, Default)]
struct Reporter {
    reported: Option<(Instant, Instant)>,
}

impl Reporter {
    /// One look, with the clock already read. Pure: same inputs, same answer.
    fn look(&mut self, now: Instant, runtime: Duration, render: Duration) -> Say {
        if runtime < RUNTIME_STALL {
            // It came back. Say so once, because "how long did it last" is the question
            // the next reader will have.
            return match self.reported.take() {
                Some((first, _)) => Say::Recovered {
                    since_first_line: now.saturating_duration_since(first),
                },
                None => Say::Nothing,
            };
        }
        if self
            .reported
            .is_some_and(|(_, last)| now.saturating_duration_since(last) < REPEAT)
        {
            return Say::Nothing;
        }
        let first = self.reported.map_or(now, |(first, _)| first);
        self.reported = Some((first, now));
        Say::Stalled { runtime, render }
    }
}

/// The watching loop. Sleeping is the whole job, so it sleeps rather than polling a
/// condition — there is no condition here to poll, only the passage of time.
///
/// The clock is read once a pass and handed to [`Reporter::look`]; nothing below this
/// function asks what time it is (ground rule 3).
fn watch(beats: &Beats, stop: &AtomicBool) {
    let mut reporter = Reporter::default();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(INTERVAL);
        let now = Instant::now();
        match reporter.look(now, beats.runtime.age(now), beats.render.age(now)) {
            Say::Nothing => {}
            Say::Stalled { runtime, render } => warn!(
                runtime_age_s = runtime.as_secs(),
                render_age_s = render.as_secs(),
                "watchdog: the runtime has not scheduled a timer; the process is wedged or \
                 starved (#368)"
            ),
            Say::Recovered { since_first_line } => info!(
                stalled_s = since_first_line.as_secs(),
                "watchdog: the runtime is scheduling again"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for "whenever the process started". Nothing reads it as a wall clock.
    fn epoch() -> Instant {
        Instant::now()
    }

    /// One look a second, as the shipped loop takes them, with the runtime's beat `age`
    /// old at each one.
    fn look_at(reporter: &mut Reporter, t0: Instant, second: u64, age: Duration) -> Say {
        reporter.look(t0 + Duration::from_secs(second), age, Duration::ZERO)
    }

    #[test]
    fn a_runtime_that_keeps_beating_never_produces_a_line() {
        let t0 = epoch();
        let mut reporter = Reporter::default();
        // The beating task sleeps a second, so an age that oscillates up to one second is
        // the ordinary state of a healthy process, forever.
        for second in 0..600 {
            let age = Duration::from_millis(if second % 2 == 0 { 100 } else { 1_000 });
            assert_eq!(look_at(&mut reporter, t0, second, age), Say::Nothing);
        }
    }

    #[test]
    fn the_first_line_waits_for_the_shipped_threshold() {
        let t0 = epoch();
        let mut reporter = Reporter::default();
        // A hair under is not a stall: four missed beats on a loaded box is a bad second,
        // not a wedge, and a watchdog that cried at four would be ignored by five.
        let nearly = RUNTIME_STALL - Duration::from_millis(1);
        assert_eq!(look_at(&mut reporter, t0, 1, nearly), Say::Nothing);
        assert_eq!(
            look_at(&mut reporter, t0, 2, RUNTIME_STALL),
            Say::Stalled {
                runtime: RUNTIME_STALL,
                render: Duration::ZERO,
            }
        );
    }

    #[test]
    fn a_continuing_stall_leaves_a_trail_at_the_shipped_interval() {
        let t0 = epoch();
        let mut reporter = Reporter::default();
        let mut lines = Vec::new();
        // A hundred seconds of wedge, looked at once a second as the loop does, with the
        // age growing the way a stopped runtime's does.
        for second in 1..=100 {
            let age = RUNTIME_STALL + Duration::from_secs(second);
            if let Say::Stalled { runtime, .. } = look_at(&mut reporter, t0, second, age) {
                lines.push((second, runtime));
            }
        }
        // The first line, then one every REPEAT — not one a second, and not only one.
        let repeat = REPEAT.as_secs();
        let expected: Vec<u64> = std::iter::once(1)
            .chain((1..).map(|n| 1 + n * repeat).take_while(|s| *s <= 100))
            .collect();
        assert_eq!(
            lines.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            expected,
            "one line, then a trail every {repeat}s"
        );
        // And each one carries the age as it stood, so the trail says the stall is
        // deepening rather than repeating a stale number.
        for (second, runtime) in &lines {
            assert_eq!(*runtime, RUNTIME_STALL + Duration::from_secs(*second));
        }
    }

    #[test]
    fn a_stall_that_ends_says_so_once_and_dates_itself_from_the_first_line() {
        let t0 = epoch();
        let mut reporter = Reporter::default();
        assert!(matches!(
            look_at(&mut reporter, t0, 10, RUNTIME_STALL),
            Say::Stalled { .. }
        ));
        // Thirty seconds later the runtime schedules again: one line, measured from the
        // line that opened the trail — which is what a reader pairing them up needs.
        assert_eq!(
            look_at(&mut reporter, t0, 40, Duration::from_millis(200)),
            Say::Recovered {
                since_first_line: Duration::from_secs(30),
            }
        );
        // Once. A healthy runtime does not go on announcing its health.
        assert_eq!(
            look_at(&mut reporter, t0, 41, Duration::from_millis(200)),
            Say::Nothing
        );
    }

    #[test]
    fn a_second_stall_is_dated_from_its_own_first_line_rather_than_the_previous_one() {
        let t0 = epoch();
        let mut reporter = Reporter::default();
        let _ = look_at(&mut reporter, t0, 10, RUNTIME_STALL);
        let _ = look_at(&mut reporter, t0, 20, Duration::ZERO);
        assert!(matches!(
            look_at(&mut reporter, t0, 30, RUNTIME_STALL),
            Say::Stalled { .. }
        ));
        assert_eq!(
            look_at(&mut reporter, t0, 45, Duration::ZERO),
            Say::Recovered {
                since_first_line: Duration::from_secs(15),
            }
        );
    }

    #[test]
    fn a_render_loop_that_has_not_drawn_for_an_hour_is_reported_but_never_judged() {
        let t0 = epoch();
        let mut reporter = Reporter::default();
        let asleep = Duration::from_secs(3_600);
        // An idle panel: nothing has asked for a frame since the last session ended. That
        // is #59 working, not a fault, and it must not produce a line of its own.
        assert_eq!(
            reporter.look(t0 + Duration::from_secs(1), Duration::ZERO, asleep),
            Say::Nothing
        );
        // When the runtime does stall, the same age rides along in the line — because
        // "wedged with the panel asleep" and "wedged mid-cast" are different bugs, and
        // the reader is the one who tells them apart.
        assert_eq!(
            reporter.look(t0 + Duration::from_secs(2), RUNTIME_STALL, asleep),
            Say::Stalled {
                runtime: RUNTIME_STALL,
                render: asleep,
            }
        );
    }
}
