//! When the kiosk next needs to draw (#59).
//!
//! The render loop used to free-run — redraw requested unconditionally at the end of
//! every redraw — and the idle panel burned a core presenting ~970 identical frames a
//! second at a 60 Hz display. The demand model replaces that: every frame, the loop
//! *recomputes from standing facts* what it owes the glass next, and sleeps the rest.
//! A per-frame predicate rather than flags set on transitions, so nothing has to
//! remember to schedule a wake-up — forgetting was not representable in the old model
//! either, because the old model never slept.

use std::time::Instant;

/// What the render loop owes the glass, computed fresh each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demand {
    /// Nothing will change until an external event arrives; sleep until woken.
    Idle,
    /// Something changes at a known instant (a scheduled clear, a banner's TTL, the
    /// transport clock's next visible second); sleep until then.
    At(Instant),
    /// Something is in continuous motion; draw every display refresh.
    Frame,
}

impl Demand {
    /// A deadline that may not exist. `None` demands nothing.
    #[must_use]
    pub fn deadline(at: Option<Instant>) -> Self {
        at.map_or(Self::Idle, Self::At)
    }

    /// Combine two demands: the more urgent wins, and of two deadlines the earlier.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Frame, _) | (_, Self::Frame) => Self::Frame,
            (Self::At(a), Self::At(b)) => Self::At(a.min(b)),
            (Self::At(t), Self::Idle) | (Self::Idle, Self::At(t)) => Self::At(t),
            (Self::Idle, Self::Idle) => Self::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn frame_beats_everything() {
        let t = Instant::now();
        assert_eq!(Demand::Frame.merge(Demand::Idle), Demand::Frame);
        assert_eq!(Demand::Idle.merge(Demand::Frame), Demand::Frame);
        assert_eq!(Demand::At(t).merge(Demand::Frame), Demand::Frame);
    }

    #[test]
    fn the_earlier_deadline_wins() {
        let sooner = Instant::now();
        let later = sooner + Duration::from_secs(5);
        assert_eq!(
            Demand::At(later).merge(Demand::At(sooner)),
            Demand::At(sooner)
        );
        assert_eq!(Demand::Idle.merge(Demand::At(later)), Demand::At(later));
    }

    #[test]
    fn idle_is_the_unit() {
        assert_eq!(Demand::Idle.merge(Demand::Idle), Demand::Idle);
        assert_eq!(Demand::deadline(None), Demand::Idle);
    }
}
