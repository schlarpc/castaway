//! Flow control, in both directions the controller meters: its data buffers
//! ([`AclCredits`]) and its command window ([`CommandCredits`]).
//!
//! A controller advertises how many ACL fragments it can hold in
//! `HCI_Read_Buffer_Size`, and the host **must not** have more than that outstanding.
//! There is no backpressure on the transport to discover this with: a dongle handed a
//! fragment it has no buffer for discards it and says nothing. The peer then waits
//! forever for a reply that was written, acknowledged by the USB stack, and thrown away
//! — which is exactly the shape of #71, where an L2CAP configuration
//! response never reached BlueZ and the link idled out with no error anywhere.
//!
//! Buffers come back via `HCI_Number_Of_Completed_Packets`, per connection handle. This
//! is pure bookkeeping — `fn(state, event) -> state` with no I/O (ground rule 3) — so the
//! whole exhaustion-and-recovery cycle is testable without a radio.
//!
//! The command window works the same way and is metered by a different field entirely
//! (`Num_HCI_Command_Packets`, on every Command Complete and Command Status); see
//! [`CommandCredits`] for why one being counted did not make the other safe.

use std::collections::{HashMap, VecDeque};

use crate::command::Command;
use crate::opcode::OpCode;
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

/// The controller's *command* window, as credits.
///
/// A different pool from [`AclCredits`], and confusing the two is easy: that one counts
/// data buffers returned by `Number_Of_Completed_Packets`, this one counts the command
/// slots the controller advertises in the `Num_HCI_Command_Packets` field of every
/// Command Complete and Command Status. Most controllers advertise exactly **one**, so a
/// host that sends whenever it has something to say routinely puts two commands in flight
/// and the second is discarded — which presents as one phone stuck in "Connecting…"
/// during a two-phone connect storm, with nothing in any log.
///
/// One credit is one command. Commands that arrive with no credit are queued in order
/// rather than dropped: every one of them is a reply the controller is waiting for or a
/// step of a sequence that only makes sense in order.
///
/// The Bluetooth core spec says the host may assume **one** credit before the controller
/// has said otherwise, which is what makes the very first `Reset` sendable.
#[derive(Debug, Clone)]
pub struct CommandCredits {
    /// Credits granted and not yet spent.
    available: u8,
    /// Commands with nowhere to go until a credit comes back.
    waiting: VecDeque<Command>,
    /// What is in flight, oldest first, for the timeout to name.
    in_flight: VecDeque<OpCode>,
}

impl Default for CommandCredits {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandCredits {
    /// A window with the one credit the spec grants before the controller speaks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            available: 1,
            waiting: VecDeque::new(),
            in_flight: VecDeque::new(),
        }
    }

    /// Commands the controller has not answered yet.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Commands waiting for a credit.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// The oldest command still unanswered — the one a timeout is about.
    #[must_use]
    pub fn oldest_in_flight(&self) -> Option<OpCode> {
        self.in_flight.front().copied()
    }

    /// Offer a command. `Some` means send it now; `None` means it is queued.
    #[must_use]
    pub fn submit(&mut self, command: Command) -> Option<Command> {
        if self.available == 0 {
            self.waiting.push_back(command);
            return None;
        }
        self.available -= 1;
        self.in_flight.push_back(command.opcode());
        Some(command)
    }

    /// The controller answered a command and said how many it will now accept.
    ///
    /// Returns the queued commands that fit in the new window, in the order they were
    /// submitted. The count is taken as the controller's own statement of the whole
    /// window rather than as an increment — that is what the field means, and treating it
    /// as an increment is how a host ends up over the limit after a burst of events.
    #[must_use]
    pub fn answered(&mut self, opcode: OpCode, allowed_packets: u8) -> Vec<Command> {
        // Remove *this* opcode rather than the oldest: a controller may answer out of
        // order, and dropping the wrong entry would make the timeout blame an innocent
        // command. An answer to something we never sent leaves the queue alone.
        if let Some(at) = self.in_flight.iter().position(|op| *op == opcode) {
            self.in_flight.remove(at);
        }
        self.available = allowed_packets;
        self.release()
    }

    /// Give up on the oldest in-flight command, returning its slot.
    ///
    /// The controller's answer is not coming — the documented idle stall on this
    /// project's dongle loses one outright — and a host that waits for it waits forever.
    /// Returns the abandoned opcode (for the log) and whatever the freed slot lets
    /// through. Nothing is re-sent: a command whose completion was merely *late* would
    /// then be executed twice, and `AcceptConnectionRequest` is not idempotent.
    #[must_use]
    pub fn abandon_oldest(&mut self) -> (Option<OpCode>, Vec<Command>) {
        let Some(opcode) = self.in_flight.pop_front() else {
            return (None, Vec::new());
        };
        // At least one, so a controller that never grants a credit still makes progress.
        self.available = self.available.max(1);
        (Some(opcode), self.release())
    }

    /// Everything the current window has room for, oldest first.
    fn release(&mut self) -> Vec<Command> {
        let mut out = Vec::new();
        while self.available > 0 {
            let Some(command) = self.waiting.pop_front() else {
                break;
            };
            self.available -= 1;
            self.in_flight.push_back(command.opcode());
            out.push(command);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::opcode::OpCode;

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

    #[test]
    fn the_second_command_waits_for_the_first_to_be_answered() {
        // The failure: most controllers advertise one command packet, so a host that
        // sends on every event puts two in flight and the second is discarded. Nothing
        // reports it — the reply the phone is waiting for was simply never executed.
        let mut window = CommandCredits::new();
        assert_eq!(window.submit(Command::Reset), Some(Command::Reset));
        assert_eq!(
            window.submit(Command::ReadBdAddr),
            None,
            "no credit left; it must queue rather than go out"
        );
        assert_eq!(window.waiting(), 1);

        let released = window.answered(OpCode::RESET, 1);
        assert_eq!(released, vec![Command::ReadBdAddr]);
        assert_eq!(window.waiting(), 0);
        assert_eq!(window.in_flight(), 1);
    }

    #[test]
    fn a_wider_window_releases_everything_that_fits_in_order() {
        // A controller that grants more than one is entitled to be believed, and the
        // order still matters: bring-up is a sequence.
        let mut window = CommandCredits::new();
        assert!(window.submit(Command::Reset).is_some());
        assert!(window.submit(Command::ReadBdAddr).is_none());
        assert!(window.submit(Command::ReadBufferSize).is_none());
        assert!(window.submit(Command::ReadLocalVersion).is_none());

        let released = window.answered(OpCode::RESET, 2);
        assert_eq!(
            released,
            vec![Command::ReadBdAddr, Command::ReadBufferSize],
            "two credits, the two oldest, in order"
        );
        assert_eq!(window.waiting(), 1);
    }

    #[test]
    fn the_count_is_the_window_and_not_an_increment() {
        // `Num_HCI_Command_Packets` is what the controller will accept *now*. Adding it
        // to what we already had is how a host ends up over the limit after a burst.
        let mut window = CommandCredits::new();
        assert!(window.submit(Command::Reset).is_some());
        assert!(window.answered(OpCode::RESET, 1).is_empty());
        assert!(window.answered(OpCode::READ_BD_ADDR, 1).is_empty());
        assert!(window.submit(Command::ReadBdAddr).is_some());
        assert!(
            window.submit(Command::ReadBufferSize).is_none(),
            "one credit means one, however many events granted it"
        );
    }

    #[test]
    fn an_answer_names_which_command_it_answers() {
        // Out-of-order answers are legal, and blaming the oldest for one of them would
        // make the timeout abandon a command that is doing nothing wrong.
        let mut window = CommandCredits::new();
        assert!(window.submit(Command::Reset).is_some());
        assert!(window.answered(OpCode::RESET, 2).is_empty());
        assert!(window.submit(Command::ReadBdAddr).is_some());
        assert!(window.submit(Command::ReadBufferSize).is_some());

        let _ = window.answered(OpCode::READ_BUFFER_SIZE, 2);
        assert_eq!(window.oldest_in_flight(), Some(OpCode::READ_BD_ADDR));
        assert_eq!(window.in_flight(), 1);
    }

    #[test]
    fn abandoning_a_lost_command_gets_the_queue_moving_again() {
        // The stall this ends: the dongle stops answering when idle, the completion
        // never arrives, and bring-up waits for it forever — no `Ready`, no
        // `WriteScanEnable`, and a receiver nobody can find, with nothing in the log.
        let mut window = CommandCredits::new();
        assert!(window.submit(Command::Reset).is_some());
        assert!(window.submit(Command::ReadBdAddr).is_none());

        let (lost, released) = window.abandon_oldest();
        assert_eq!(lost, Some(OpCode::RESET));
        assert_eq!(released, vec![Command::ReadBdAddr], "the queue moves on");
        // And nothing is re-sent: a merely late completion would otherwise execute the
        // command twice.
        assert_eq!(window.in_flight(), 1);
    }

    #[test]
    fn abandoning_with_nothing_in_flight_is_not_a_windfall() {
        let mut window = CommandCredits::new();
        let (lost, released) = window.abandon_oldest();
        assert_eq!(lost, None);
        assert!(released.is_empty());
        // The one credit the spec grants is still exactly one.
        assert!(window.submit(Command::Reset).is_some());
        assert!(window.submit(Command::ReadBdAddr).is_none());
    }

    #[test]
    fn an_answer_to_something_we_never_sent_still_opens_the_window() {
        // Controllers do emit unsolicited Command Completes (opcode 0x0000 is the
        // documented "credits only" form). It must not corrupt the in-flight list.
        let mut window = CommandCredits::new();
        assert!(window.submit(Command::Reset).is_some());
        let released = window.answered(OpCode::new(0x0000), 2);
        assert!(released.is_empty());
        assert_eq!(window.in_flight(), 1, "the Reset is still unanswered");
        assert!(window.submit(Command::ReadBdAddr).is_some());
    }
}
