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

/// The watching loop. Sleeping is the whole job, so it sleeps rather than polling a
/// condition — there is no condition here to poll, only the passage of time.
fn watch(beats: &Beats, stop: &AtomicBool) {
    // When the stall was first reported, and when it was last reported — the first dates
    // it for the reader, the second is what keeps the repeat to one line per `REPEAT`.
    let mut reported: Option<(Instant, Instant)> = None;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(INTERVAL);
        let now = Instant::now();
        let runtime = beats.runtime.age(now);
        if runtime < RUNTIME_STALL {
            // It came back. Say so, once, because "how long did it last" is the question
            // the next reader will have.
            if let Some((first, _)) = reported.take() {
                info!(
                    stalled_s = now.saturating_duration_since(first).as_secs(),
                    "watchdog: the runtime is scheduling again"
                );
            }
            continue;
        }
        if reported.is_some_and(|(_, last)| now.saturating_duration_since(last) < REPEAT) {
            continue;
        }
        let first = reported.map_or(now, |(first, _)| first);
        reported = Some((first, now));
        warn!(
            runtime_age_s = runtime.as_secs(),
            render_age_s = beats.render.age(now).as_secs(),
            "watchdog: the runtime has not scheduled a timer; the process is wedged or \
             starved (#368)"
        );
    }
}
