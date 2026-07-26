//! ACL flow control: the controller's data buffers, counted.
//!
//! A controller advertises how many ACL fragments it can hold in
//! `HCI_Read_Buffer_Size`, and the host **must not** have more than that outstanding.
//! There is no backpressure on the transport to discover this with: a dongle handed a
//! fragment it has no buffer for discards it and says nothing. The peer then waits
//! forever for a reply that was written, acknowledged by the USB stack, and thrown away
//! — which is exactly the shape of OPEN-QUESTIONS Q26, where an L2CAP configuration
//! response never reached BlueZ and the link idled out with no error anywhere.
//!
//! Buffers come back via `HCI_Number_Of_Completed_Packets`, per connection handle. This
//! is pure bookkeeping — `fn(state, event) -> state` with no I/O (ground rule 3) — so the
//! whole exhaustion-and-recovery cycle is testable without a radio.

use std::collections::HashMap;

use crate::packet::ConnectionHandle;

/// The controller's ACL buffer pool, as credits.
///
/// One credit is one *fragment*, not one L2CAP PDU: an SDP record or an AVDTP capability
/// response routinely exceeds a dongle's 340-byte buffer and costs several.
///
/// Outstanding fragments are tracked per handle even though the pool is shared, because
/// the two events that return credits are per-handle: completion, and a link dropping
/// while fragments are still queued in the controller. A disconnected link's buffers are
/// flushed without a completion event, so nothing else would ever give them back.
#[derive(Debug, Clone)]
pub struct AclCredits {
    capacity: u16,
    outstanding: HashMap<u16, u16>,
    total: u16,
}

impl AclCredits {
    /// A pool of `capacity` fragments.
    ///
    /// Clamped to at least one: a controller reporting zero buffers would otherwise be a
    /// permanent stall, and every real controller has at least one.
    #[must_use]
    pub fn new(capacity: u16) -> Self {
        Self {
            capacity: capacity.max(1),
            outstanding: HashMap::new(),
            total: 0,
        }
    }

    /// Resize the pool, once `HCI_Read_Buffer_Size` says how big it really is.
    ///
    /// Shrinking below what is already outstanding is allowed and simply means no claim
    /// succeeds until enough completions arrive; the alternative — refusing to shrink —
    /// would leave us over the controller's real limit, which is the bug being fixed.
    pub fn set_capacity(&mut self, capacity: u16) {
        self.capacity = capacity.max(1);
    }

    /// The pool size.
    #[must_use]
    pub const fn capacity(&self) -> u16 {
        self.capacity
    }

    /// Fragments the controller has not reported complete.
    #[must_use]
    pub const fn outstanding(&self) -> u16 {
        self.total
    }

    /// How many fragments may be sent right now.
    #[must_use]
    pub const fn available(&self) -> u16 {
        self.capacity.saturating_sub(self.total)
    }

    /// Take one credit for a fragment on `handle`, or report that none is free.
    #[must_use]
    pub fn claim(&mut self, handle: ConnectionHandle) -> bool {
        if self.available() == 0 {
            return false;
        }
        self.total += 1;
        *self.outstanding.entry(handle.raw()).or_insert(0) += 1;
        true
    }

    /// Return credits for fragments the controller reports it has finished with.
    ///
    /// Returns how many were actually released, which may be fewer than `count`: a
    /// controller that over-reports — or a duplicated event — must not inflate the pool
    /// past its real size, since that reintroduces exactly the overflow this exists to
    /// prevent.
    pub fn complete(&mut self, handle: ConnectionHandle, count: u16) -> u16 {
        let Some(entry) = self.outstanding.get_mut(&handle.raw()) else {
            return 0;
        };
        let released = count.min(*entry);
        *entry -= released;
        if *entry == 0 {
            self.outstanding.remove(&handle.raw());
        }
        self.total -= released;
        released
    }

    /// Reclaim everything still outstanding on a link that just went away.
    ///
    /// The controller flushes a disconnected handle's buffers without reporting them
    /// complete, so without this the pool leaks a credit per unsent fragment and a
    /// long-running receiver eventually wedges after enough phones have come and gone.
    pub fn link_down(&mut self, handle: ConnectionHandle) -> u16 {
        let reclaimed = self.outstanding.remove(&handle.raw()).unwrap_or(0);
        self.total -= reclaimed;
        reclaimed
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn handle(raw: u16) -> ConnectionHandle {
        ConnectionHandle::new(raw).unwrap()
    }

    #[test]
    fn a_full_pool_refuses_the_next_fragment() {
        // The whole point: without this the fragment goes to a controller with nowhere to
        // put it, is dropped silently, and the peer waits for a reply that never comes.
        let mut credits = AclCredits::new(2);
        let h = handle(0x000B);
        assert!(credits.claim(h));
        assert!(credits.claim(h));
        assert!(!credits.claim(h), "a third fragment must wait");
        assert_eq!(credits.available(), 0);
    }

    #[test]
    fn completions_hand_the_credits_back() {
        let mut credits = AclCredits::new(2);
        let h = handle(0x000B);
        assert!(credits.claim(h));
        assert!(credits.claim(h));
        assert_eq!(credits.complete(h, 2), 2);
        assert_eq!(credits.available(), 2);
        assert!(credits.claim(h));
    }

    #[test]
    fn the_pool_is_shared_across_links_because_the_controller_is() {
        // Two phones, one set of buffers. Accounting per link and summing at the end
        // would let two links together exceed what the controller advertised.
        let mut credits = AclCredits::new(3);
        let (a, b) = (handle(0x000B), handle(0x000C));
        assert!(credits.claim(a));
        assert!(credits.claim(b));
        assert!(credits.claim(b));
        assert!(
            !credits.claim(a),
            "the pool is exhausted regardless of link"
        );
        assert_eq!(credits.complete(b, 2), 2);
        assert!(credits.claim(a));
    }

    #[test]
    fn an_over_reported_completion_cannot_inflate_the_pool() {
        // A controller that reports more completions than we sent — or an event we
        // somehow see twice — would otherwise raise the ceiling above the real buffer
        // count, which is the exact failure this type exists to prevent.
        let mut credits = AclCredits::new(4);
        let h = handle(0x000B);
        assert!(credits.claim(h));
        assert_eq!(credits.complete(h, 99), 1, "only what was outstanding");
        assert_eq!(credits.available(), 4);
        assert_eq!(credits.complete(h, 5), 0, "and nothing on an idle handle");
        assert_eq!(credits.available(), 4);
    }

    #[test]
    fn a_dropped_link_returns_the_fragments_the_controller_flushed() {
        // No completion event ever arrives for these. Without reclaiming them the pool
        // shrinks by one credit per phone that walks off mid-write.
        let mut credits = AclCredits::new(2);
        let (a, b) = (handle(0x000B), handle(0x000C));
        assert!(credits.claim(a));
        assert!(credits.claim(a));
        assert_eq!(credits.link_down(a), 2);
        assert_eq!(credits.available(), 2);
        assert!(credits.claim(b));
        assert_eq!(
            credits.link_down(a),
            0,
            "a link down twice is not a windfall"
        );
    }

    #[test]
    fn learning_the_real_buffer_count_resizes_the_pool() {
        // Bring-up starts conservative and `HCI_Read_Buffer_Size` says what it really is.
        let mut credits = AclCredits::new(1);
        let h = handle(0x000B);
        assert!(credits.claim(h));
        assert!(!credits.claim(h));
        credits.set_capacity(8);
        assert_eq!(credits.available(), 7);
        assert!(credits.claim(h));
    }
}
