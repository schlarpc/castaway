//! Input from off the main thread, queued for the router (#18).
//!
//! The kiosk owns the winit event loop and therefore the main thread (architecture §6);
//! a remote peer's contacts arrive on the tokio runtime. This is the seam between them,
//! and it is deliberately the same shape as every other producer that queues work for
//! that loop: push, then [`castaway_core::Waker::wake`], and the loop drains when it next
//! runs. No new mechanism, and no new winit user-event type.
//!
//! Two things it does beyond being a channel, both of which exist because the far end is
//! a phone on a network rather than a thread we control:
//!
//! - **Moves coalesce.** A pointer reporting at 120 Hz into a panel presenting at 60
//!   would otherwise hand the browser host a backlog it dispatches one CDP message at a
//!   time — every one of them stale on arrival. Only the newest move for a contact
//!   survives a drain; the phases that mean something ([`TouchPhase::Down`],
//!   [`TouchPhase::Up`], [`TouchPhase::Cancel`]) never coalesce with anything.
//! - **Ends are never dropped.** The queue is bounded, because a peer that floods it must
//!   not be able to grow it without limit. But an event that *ends* a contact, or a
//!   [`RemoteEvent::Gone`], is always accepted: losing one leaves the panel believing a
//!   finger is down for the rest of the session, which is the failure the whole
//!   origin-tracking design exists to prevent. A `Down` dropped under flood is harmless
//!   by comparison — every layer treats an unknown contact as a miss.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use castaway_core::Waker;
use tracing::warn;

use crate::{ContactId, Input, InputOrigin, TouchPhase};

/// How many events may be waiting before the queue starts refusing the droppable ones.
///
/// Generous: a drain happens every frame, so reaching this at all means the loop is
/// wedged or a peer is misbehaving. Small enough that neither can cost real memory.
const MAX_PENDING: usize = 4096;

/// One thing a remote peer did, in the order it did it.
///
/// Disconnection is *in* the queue rather than beside it because the ordering matters: a
/// peer that presses and then drops must have the press applied and then cancelled, and
/// two separate lists could not say which came first.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteEvent {
    /// Something to route, exactly as if it had come off the glass.
    Input(Input),
    /// This origin has gone away. Everything it holds is cancelled — not released, since
    /// a dropped connection did not *finish* a gesture and completing one would commit
    /// whatever it was over.
    Gone(InputOrigin),
    /// Go back to the home screen.
    ///
    /// In the queue rather than a callback beside it, for the same reason [`Self::Gone`]
    /// is: order against the input matters. Home cancels whatever is down, so a press
    /// applied *after* it would be stranded — and a callback racing the drain could
    /// deliver them either way round.
    Home,
}

/// The queue itself. Cloneable by `Arc`; every producer and the loop share one.
#[derive(Debug)]
pub struct RemoteInputQueue {
    pending: Mutex<VecDeque<RemoteEvent>>,
    wake: Waker,
}

impl RemoteInputQueue {
    /// A queue that wakes `wake` whenever something lands in it.
    #[must_use]
    pub fn new(wake: Waker) -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            wake,
        }
    }

    /// Queue one event and wake the loop.
    ///
    /// Never blocks on the loop and never fails: a full queue drops droppable events (see
    /// the module docs) rather than making every caller handle backpressure it cannot do
    /// anything useful about.
    pub fn push(&self, event: RemoteEvent) {
        {
            // A poisoned lock means a drain panicked while holding it. Input is not worth
            // taking the process down over, and the next drain will find a consistent
            // queue either way, so the poison is stepped over rather than propagated.
            let mut pending = match self.pending.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            if pending.len() >= MAX_PENDING && is_droppable(&event) {
                warn!(
                    pending = pending.len(),
                    "remote input: queue full, dropping a move"
                );
                return;
            }
            pending.push_back(event);
        }
        self.wake.wake();
    }

    /// Queue one input.
    pub fn push_input(&self, input: Input) {
        self.push(RemoteEvent::Input(input));
    }

    /// Say that an origin has gone away.
    pub fn push_gone(&self, origin: InputOrigin) {
        self.push(RemoteEvent::Gone(origin));
    }

    /// Ask the panel to go home.
    pub fn push_home(&self) {
        self.push(RemoteEvent::Home);
    }

    /// Take everything waiting, with stale moves already removed.
    ///
    /// Returns them in arrival order. Empty is the overwhelmingly common case — the loop
    /// calls this every time it runs — so it costs one uncontended lock and nothing else.
    #[must_use]
    pub fn drain(&self) -> Vec<RemoteEvent> {
        let batch: Vec<RemoteEvent> = {
            let mut pending = match self.pending.lock() {
                Ok(pending) => pending,
                Err(poisoned) => poisoned.into_inner(),
            };
            if pending.is_empty() {
                return Vec::new();
            }
            pending.drain(..).collect()
        };
        coalesce_moves(batch)
    }

    /// Whether anything is waiting, without taking it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self.pending.lock() {
            Ok(pending) => pending.is_empty(),
            Err(poisoned) => poisoned.into_inner().is_empty(),
        }
    }
}

/// Whether an event may be discarded when the queue is full.
///
/// Only a move. Everything else either ends a contact or announces that a peer has, and
/// dropping one of those is what strands a finger.
fn is_droppable(event: &RemoteEvent) -> bool {
    matches!(
        event,
        RemoteEvent::Input(Input::Touch(t)) if t.phase == TouchPhase::Move
    )
}

/// Drop every move a later move in the same batch supersedes.
///
/// Walks backwards so "later" is known before "earlier" is decided. A contact's run of
/// moves collapses to its last one; anything that is not a move — a press, a release, a
/// cancel — breaks the run, so a move before a release is kept and is not folded into a
/// move that happens after the *next* press with the same id.
fn coalesce_moves(batch: Vec<RemoteEvent>) -> Vec<RemoteEvent> {
    let mut superseded = vec![false; batch.len()];
    let mut seen: HashSet<ContactId> = HashSet::new();
    for (index, event) in batch.iter().enumerate().rev() {
        let RemoteEvent::Input(Input::Touch(touch)) = event else {
            continue;
        };
        if touch.phase == TouchPhase::Move {
            // `insert` is false when a later move for this contact already claimed it.
            if !seen.insert(touch.id) {
                superseded[index] = true;
            }
        } else {
            seen.remove(&touch.id);
        }
    }
    batch
        .into_iter()
        .zip(superseded)
        .filter_map(|(event, dead)| (!dead).then_some(event))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{PointerEvent, RemoteId, TouchEvent};

    fn touch(id: ContactId, phase: TouchPhase, x: f32) -> RemoteEvent {
        RemoteEvent::Input(Input::Touch(TouchEvent::new(id, phase, x, 0.5)))
    }

    fn queue() -> RemoteInputQueue {
        RemoteInputQueue::new(Waker::new())
    }

    #[test]
    fn a_run_of_moves_collapses_to_the_newest() {
        let q = queue();
        let id = ContactId::remote(RemoteId::new(1), 0);
        q.push(touch(id, TouchPhase::Down, 0.1));
        for step in 1..=5 {
            #[allow(clippy::cast_precision_loss)]
            q.push(touch(id, TouchPhase::Move, 0.1 + step as f32 * 0.1));
        }
        let drained = q.drain();
        assert_eq!(drained.len(), 2, "the press, and one move");
        assert_eq!(drained[0], touch(id, TouchPhase::Down, 0.1));
        let RemoteEvent::Input(Input::Touch(last)) = drained[1] else {
            panic!("expected a touch");
        };
        assert_eq!(last.phase, TouchPhase::Move);
        assert!((last.x - 0.6).abs() < 1e-6, "the newest position survives");
    }

    #[test]
    fn coalescing_is_per_contact() {
        // Two peers dragging at once must not collapse into each other.
        let q = queue();
        let (a, b) = (
            ContactId::remote(RemoteId::new(1), 0),
            ContactId::remote(RemoteId::new(2), 0),
        );
        q.push(touch(a, TouchPhase::Move, 0.1));
        q.push(touch(b, TouchPhase::Move, 0.2));
        q.push(touch(a, TouchPhase::Move, 0.3));
        q.push(touch(b, TouchPhase::Move, 0.4));
        let drained = q.drain();
        assert_eq!(drained.len(), 2, "one surviving move each");
        assert_eq!(drained[0], touch(a, TouchPhase::Move, 0.3));
        assert_eq!(drained[1], touch(b, TouchPhase::Move, 0.4));
    }

    #[test]
    fn a_release_breaks_the_run() {
        // The move before a release is the last position the finger was at when it
        // lifted. Folding it into a move from the *next* press would teleport the
        // following gesture's start.
        let q = queue();
        let id = ContactId::panel(1);
        q.push(touch(id, TouchPhase::Move, 0.1));
        q.push(touch(id, TouchPhase::Up, 0.1));
        q.push(touch(id, TouchPhase::Down, 0.9));
        q.push(touch(id, TouchPhase::Move, 0.95));
        assert_eq!(q.drain().len(), 4, "nothing here supersedes anything");
    }

    #[test]
    fn ends_and_departures_are_never_coalesced_away() {
        let q = queue();
        let id = ContactId::remote(RemoteId::new(1), 0);
        q.push(touch(id, TouchPhase::Move, 0.1));
        q.push(touch(id, TouchPhase::Cancel, 0.1));
        q.push_gone(InputOrigin::Remote(RemoteId::new(1)));
        let drained = q.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(
            drained[2],
            RemoteEvent::Gone(InputOrigin::Remote(RemoteId::new(1)))
        );
    }

    #[test]
    fn a_departure_keeps_its_place_in_the_order() {
        // A peer that presses and then drops must have the press applied *before* the
        // cancellation, or the cancellation finds nothing and the press strands a finger.
        let q = queue();
        let peer = RemoteId::new(1);
        q.push(touch(ContactId::remote(peer, 0), TouchPhase::Down, 0.5));
        q.push_gone(InputOrigin::Remote(peer));
        let drained = q.drain();
        assert!(matches!(drained[0], RemoteEvent::Input(_)));
        assert!(matches!(drained[1], RemoteEvent::Gone(_)));
    }

    #[test]
    fn a_flood_cannot_grow_the_queue_without_limit() {
        let q = queue();
        let id = ContactId::remote(RemoteId::new(1), 0);
        for _ in 0..(MAX_PENDING * 2) {
            q.push(touch(id, TouchPhase::Move, 0.5));
        }
        assert!(!q.is_empty());
        // Coalescing then takes the flood down to one, but the point is the bound before
        // it: the queue never held more than MAX_PENDING.
        assert_eq!(q.drain().len(), 1);
    }

    #[test]
    fn a_flood_still_cannot_lose_a_release() {
        // The one thing dropping must never do. A peer that floods moves and then lifts
        // has to have the lift land, or the contact is stranded forever.
        let q = queue();
        let id = ContactId::remote(RemoteId::new(1), 0);
        for _ in 0..(MAX_PENDING * 2) {
            q.push(touch(id, TouchPhase::Move, 0.5));
        }
        q.push(touch(id, TouchPhase::Up, 0.5));
        q.push_gone(InputOrigin::Remote(RemoteId::new(1)));
        let drained = q.drain();
        assert_eq!(drained[drained.len() - 2], touch(id, TouchPhase::Up, 0.5));
        assert_eq!(
            drained[drained.len() - 1],
            RemoteEvent::Gone(InputOrigin::Remote(RemoteId::new(1)))
        );
    }

    #[test]
    fn home_keeps_its_place_against_the_input_around_it() {
        // Home cancels whatever is down, so a press applied after it would be stranded.
        // A callback beside the queue could deliver these either way round.
        let q = queue();
        let id = ContactId::remote(RemoteId::new(1), 0);
        q.push(touch(id, TouchPhase::Down, 0.5));
        q.push_home();
        q.push(touch(id, TouchPhase::Up, 0.5));
        let drained = q.drain();
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[1], RemoteEvent::Home);
    }

    #[test]
    fn wheels_are_kept_whole() {
        // Scroll distance is the sum of its deltas, so dropping one loses travel rather
        // than staleness. Only positions supersede.
        let q = queue();
        for _ in 0..4 {
            q.push_input(Input::Pointer(PointerEvent::Wheel {
                x: 0.5,
                y: 0.5,
                dx: 0.0,
                dy: -40.0,
            }));
        }
        assert_eq!(q.drain().len(), 4);
    }

    #[test]
    fn draining_an_empty_queue_is_nothing() {
        let q = queue();
        assert!(q.is_empty());
        assert!(q.drain().is_empty());
        assert!(q.drain().is_empty());
    }

    #[test]
    fn a_push_wakes_the_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let waker = Waker::new();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        waker.arm(move || {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        let q = RemoteInputQueue::new(waker);
        q.push(touch(ContactId::panel(0), TouchPhase::Down, 0.5));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }
}
