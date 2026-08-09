//! The deadline poll every test used to hand-roll (#236).
//!
//! One rule from CLAUDE.md ground rule 6, given one home: *a sleep standing in for a
//! condition is a bug*. A poll with a deadline reports what it was waiting for when it
//! expires; a sleep long enough to be reliable on a loaded CI box is a waste on every
//! other run, and an insufficient one reports a wrong number and blames the code under
//! test. Before this crate the same helper was hand-rolled in `app`, `core`,
//! `proto-bluetooth-audio` and `proto-spotify`, each with its own deadline and its own
//! name — this is the copy they now share, and it is a dev-dependency only.
//!
//! Deadlines are read off [`tokio::time::Instant`], so under `start_paused` they are
//! *virtual*: a poll that would take five wall seconds to give up costs nothing when the
//! condition holds, and expires in milliseconds of wall time when it does not — failing,
//! with a message, where an unbounded loop in paused time would spin forever.

use std::future::Future;
use std::time::Duration;

/// How long [`eventually`] waits before giving up. Long enough for anything another task
/// (or a real decode thread) does on its own schedule; short enough that a hung test
/// names its missing condition instead of eating a runner's timeout.
const DEADLINE: Duration = Duration::from_secs(5);

/// How often the condition is re-read. Failure latency, not pass latency, is what a
/// coarser interval would buy back — checking is cheap, so check often.
const POLL: Duration = Duration::from_millis(1);

/// Poll `check` until it yields, or panic naming `what` never happened.
///
/// For conditions another task satisfies on its own schedule — an actor draining a
/// channel, a resolver writing a slot — where "has it happened yet" has no synchronous
/// answer. The closure is synchronous; a condition that must itself `.await` wants
/// [`eventually_async`].
pub async fn eventually<T>(what: &str, check: impl FnMut() -> Option<T>) -> T {
    eventually_within(what, DEADLINE, check).await
}

/// [`eventually`] with the deadline chosen by the caller.
///
/// For waits the default is wrong for in either direction: a real media clip that takes
/// fifteen wall seconds to play out, or a paused-time test whose condition sits behind a
/// backoff ladder measured in virtual minutes.
pub async fn eventually_within<T>(
    what: &str,
    deadline: Duration,
    mut check: impl FnMut() -> Option<T>,
) -> T {
    eventually_async_within(what, deadline, move || std::future::ready(check())).await
}

/// [`eventually`] for a condition that must be `.await`ed — an HTTP round trip into a
/// router, an actor's own async accessor.
pub async fn eventually_async<T, F, Fut>(what: &str, check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    eventually_async_within(what, DEADLINE, check).await
}

/// [`eventually_async`] with the deadline chosen by the caller. The primitive the other
/// three delegate to.
pub async fn eventually_async_within<T, F, Fut>(what: &str, deadline: Duration, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let give_up = tokio::time::Instant::now() + deadline;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < give_up,
            "timed out after {deadline:?} waiting for {what}"
        );
        tokio::time::sleep(POLL).await;
    }
}
