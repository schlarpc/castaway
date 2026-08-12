//! The receiver's one shutdown signal.
//!
//! A **latch**, not an event, and that distinction is the whole module. `tokio::sync::Notify`
//! — what this replaced — wakes the waiters registered at the moment it fires and keeps
//! nothing for anyone who arrives later. Every consumer here is a task that registers its
//! wait somewhere inside a startup or a loop, so "fires before the waiter registers" is not
//! a corner case; it is a race that the shape of the program decides, and it lost.
//!
//! The way it lost is worth keeping: the auto-updater finds a staged tree during
//! `Agent::run`'s first turn, which is roughly 75ms after boot, while `serve` is still
//! binding adapters and does not reach its own wait for another 120ms. The signal went
//! nowhere. The receiver logged that it was shutting down so the launcher could start the
//! new version, then carried on serving for ever — the launcher saw no exit, the panel
//! never took the update, and nothing anywhere said so (#345, and the `update-vm` check).
//!
//! So: a value that stays fired once fired, and a `wait` that checks it before it waits.
//! A latch cannot lose an edge, because it does not have edges.

use tokio::sync::watch;

/// "Time to stop", shared by every task that has to hear it.
///
/// Cheap to clone — a `watch::Sender` is an `Arc` inside — so it is passed by value like
/// the `Arc<Notify>` it replaced.
#[derive(Clone)]
pub struct Shutdown(watch::Sender<bool>);

impl Shutdown {
    /// A latch that has not fired.
    #[must_use]
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }

    /// Fire it. Idempotent, and it does not matter whether anyone is listening yet:
    /// every [`Shutdown::wait`] from here on returns immediately.
    ///
    /// `send_replace` rather than `send` because `send` reports "no receivers left" as an
    /// error, and that is not one — a shutdown with nothing still running has done its job.
    pub fn fire(&self) {
        self.0.send_replace(true);
    }

    /// Resolve when it has fired, *including* when it fired before this was called.
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        // `wait_for` tests the current value before it waits. That is the entire fix; a
        // bare `changed()` would be `Notify` again with more steps.
        //
        // The error case is "the sender is gone", which cannot happen while `self` holds
        // it, and would mean nothing is left to shut down in any case.
        let _ = rx.wait_for(|fired| *fired).await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::Shutdown;

    /// The defect this type exists for: fire first, wait afterwards, and still be told.
    /// With `Notify::notify_waiters` this hangs for ever, which is what shipped.
    #[tokio::test]
    async fn a_wait_that_arrives_after_the_firing_still_returns() {
        let shutdown = Shutdown::new();
        shutdown.fire();
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.wait())
            .await
            .expect("a latch fired before the wait must still release it");
    }

    /// The ordinary direction, which `Notify` also got right.
    #[tokio::test]
    async fn a_waiter_already_parked_is_released() {
        let shutdown = Shutdown::new();
        let waiter = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move { shutdown.wait().await })
        };
        // Let the waiter park before firing, so this is the pre-registered case rather
        // than the one above wearing a different hat.
        tokio::task::yield_now().await;
        shutdown.fire();
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("a parked waiter must be released")
            .expect("the waiting task must not panic");
    }

    /// Every consumer is a separate task, so one firing has to release all of them —
    /// `notify_one`'s stored permit would have released exactly one.
    #[tokio::test]
    async fn one_firing_releases_every_waiter() {
        let shutdown = Shutdown::new();
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let shutdown = shutdown.clone();
                tokio::spawn(async move { shutdown.wait().await })
            })
            .collect();
        tokio::task::yield_now().await;
        shutdown.fire();
        for waiter in waiters {
            tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
                .await
                .expect("every waiter must be released by one firing")
                .expect("no waiting task may panic");
        }
    }

    /// Firing twice is what a ctrl-c during an update handover would do.
    #[tokio::test]
    async fn firing_twice_is_not_an_error() {
        let shutdown = Shutdown::new();
        shutdown.fire();
        shutdown.fire();
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown.wait())
            .await
            .expect("a latch fired twice is still fired");
    }
}
