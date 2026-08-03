//! The render loop's source of "now".
//!
//! Everything the loop does on a deadline — the transport strip's anchor, the pill's
//! fade, a deferred layer clear, the demand calculation — reads time from here rather
//! than from `Instant::now()` directly. In production that *is* `Instant::now()`, at the
//! cost of one branch on a path that already touches the GPU.
//!
//! It exists for the tests. Before it, "the clock re-anchored on a new reading" was
//! asserted as "less than 40 ms of wall clock has passed since I sent the card", which is
//! the same statement only on an idle machine — and under a full `nix flake check`, with
//! 388 tests running in parallel against a software rasterizer, it is not. That check
//! failed at 50.5 ms and passed when re-run alone (#156). A gate that is red for reasons
//! unrelated to the change under test teaches you to re-run it until it is green, which
//! is the same thing as not having one.
//!
//! So a test installs [`ManualClock`], advances it by hand, and asserts on the number it
//! chose. No sleeps, no tolerances, and no dependence on how fast the host is.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where the render loop reads the time.
#[derive(Clone, Debug, Default)]
pub struct RenderClock {
    /// `None` — the ordinary case — means the monotonic clock.
    manual: Option<Arc<Manual>>,
}

impl RenderClock {
    /// The monotonic clock.
    #[must_use]
    pub fn monotonic() -> Self {
        Self { manual: None }
    }

    /// A clock that only moves when a test says so, and the handle to move it with.
    #[must_use]
    pub fn manual() -> (Self, ManualClock) {
        let manual = Arc::new(Manual {
            base: Instant::now(),
            elapsed_nanos: AtomicU64::new(0),
        });
        (
            Self {
                manual: Some(Arc::clone(&manual)),
            },
            ManualClock(manual),
        )
    }

    /// What time it is.
    #[must_use]
    pub fn now(&self) -> Instant {
        match &self.manual {
            Some(manual) => manual.now(),
            None => Instant::now(),
        }
    }
}

/// The handle by which a test moves a [`RenderClock::manual`] clock.
///
/// Cloneable and shared with the loop, so an advance is visible to it immediately; the
/// loop still has to be pumped for anything to be *drawn* at the new time.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<Manual>);

impl ManualClock {
    /// Move time forward.
    pub fn advance(&self, by: Duration) {
        // Saturating rather than wrapping: a test that asks for centuries gets a clock
        // pinned at the end of time rather than one that silently rewinds.
        let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
        let mut current = self.0.elapsed_nanos.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(nanos);
            match self.0.elapsed_nanos.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// What time this clock says it is.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.0.now()
    }
}

/// A fixed origin plus however far a test has moved it.
///
/// Nanoseconds in an atomic rather than an `Instant` behind a lock: `Instant` has no
/// public constructor, so the origin is captured once and every reading is an offset from
/// it — and the loop reads the clock from more than one place per frame.
#[derive(Debug)]
struct Manual {
    base: Instant,
    elapsed_nanos: AtomicU64,
}

impl Manual {
    fn now(&self) -> Instant {
        self.base + Duration::from_nanos(self.elapsed_nanos.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manual_clock_stands_still_until_it_is_advanced() {
        let (clock, handle) = RenderClock::manual();
        let start = clock.now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            clock.now(),
            start,
            "wall clock passing must not move a manual clock"
        );
        handle.advance(Duration::from_secs(90));
        assert_eq!(clock.now(), start + Duration::from_secs(90));
    }

    #[test]
    fn advances_accumulate() {
        let (clock, handle) = RenderClock::manual();
        let start = clock.now();
        for _ in 0..10 {
            handle.advance(Duration::from_millis(100));
        }
        assert_eq!(clock.now(), start + Duration::from_secs(1));
    }

    #[test]
    fn the_monotonic_clock_moves_on_its_own() {
        let clock = RenderClock::monotonic();
        let start = clock.now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(clock.now() > start);
    }
}
