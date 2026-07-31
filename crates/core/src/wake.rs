//! A cross-thread wake handle for a demand-driven render loop (#59).
//!
//! The kiosk sleeps between frames; everything that queues work for it — a render
//! command, a browser paint, an OSD banner, the exit flag — holds a clone of the same
//! [`Waker`] and calls [`Waker::wake`] after enqueueing. The loop itself arms the waker
//! with its real wake mechanism (a winit `EventLoopProxy`) once the loop exists, which
//! is inevitably *after* the producers were built and handed their clones.
//!
//! Lives in `core` for the same reason [`crate::osd`] does: producers that must not
//! depend on the GPU/render crate still have to be able to wake it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// Wakes the render loop from any thread. Cheap to clone; all clones share one target.
///
/// Unarmed it is a latch, not a no-op: wakes are remembered, and arming fires once if
/// any arrived early, so nothing enqueued during startup sleeps until the first touch.
/// In a build with no render loop the waker is simply never armed and the latch is the
/// whole behaviour.
#[derive(Clone, Default)]
pub struct Waker {
    inner: Arc<WakerInner>,
}

#[derive(Default)]
struct WakerInner {
    /// What a wake does, once armed. `OnceLock` because there is one loop per process
    /// run and re-arming an armed waker is a wiring bug, not a feature.
    target: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Whether a wake arrived before arming.
    pending: AtomicBool,
}

impl Waker {
    /// A fresh, unarmed waker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wake the loop, or remember that it should be woken as soon as it exists.
    pub fn wake(&self) {
        match self.inner.target.get() {
            Some(wake) => wake(),
            None => self.inner.pending.store(true, Ordering::Release),
        }
    }

    /// Install what a wake does, and fire once if any wake arrived before arming.
    ///
    /// A second arm is ignored: the first loop to arm owns the waker for the life of
    /// the process, matching winit's own one-event-loop-per-process rule.
    pub fn arm(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.inner.target.set(Box::new(wake));
        if self.inner.pending.swap(false, Ordering::AcqRel) {
            self.wake();
        }
    }
}

impl std::fmt::Debug for Waker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Waker")
            .field("armed", &self.inner.target.get().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn wakes_reach_the_armed_target() {
        let waker = Waker::new();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        waker.arm(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        waker.wake();
        waker.clone().wake();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_wake_before_arming_fires_once_at_arm_time() {
        let waker = Waker::new();
        waker.wake();
        waker.wake();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        waker.arm(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        // Coalesced: the loop only needs to be told "something happened", not how often.
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_unarmed_waker_is_silent_and_a_second_arm_is_ignored() {
        let waker = Waker::new();
        waker.wake(); // must not panic in a build with no render loop
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        waker.arm(move || {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        waker.arm(|| panic!("second arm must be ignored"));
        waker.wake();
        assert_eq!(count.load(Ordering::SeqCst), 2); // pending flush + explicit wake
    }
}
