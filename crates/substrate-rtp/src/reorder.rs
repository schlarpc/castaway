//! A bounded RTP reorder buffer. It restores sequence order for mildly out-of-order
//! delivery, drops duplicates and late packets, and — when the buffer overflows its
//! depth — skips the missing packet rather than stalling. For live mirroring latency
//! beats freshness (architecture §6), so a gap is a skip, never a wait.

use std::collections::BTreeMap;

use crate::packet::RtpPacket;

/// Buffers RTP packets and yields them in sequence order.
pub struct ReorderBuffer {
    max_depth: usize,
    next: Option<u16>,
    /// Whether we've emitted a packet yet. Before the first emit, an earlier sequence
    /// lowers the baseline (start-of-stream reordering); after it, earlier = late.
    started: bool,
    buf: BTreeMap<u16, RtpPacket>,
    /// Packets never delivered because the buffer stopped waiting for them.
    ///
    /// The one number that says a live stream is being damaged rather than merely
    /// jittery: reordering costs nothing and shows up here as zero, while a real loss
    /// leaves a hole the layer above has to repair (for Miracast, with an M13 — #192).
    skipped: u64,
}

impl ReorderBuffer {
    /// Create a buffer that holds at most `max_depth` packets before skipping a gap.
    #[must_use]
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth: max_depth.max(1),
            next: None,
            started: false,
            buf: BTreeMap::new(),
            skipped: 0,
        }
    }

    /// How many packets the buffer gave up on rather than waiting for.
    #[must_use]
    pub const fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Number of buffered (not-yet-popped) packets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the buffer holds no packets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Insert a packet. Returns `false` if it was dropped as a duplicate or as late
    /// (older than the next packet we expect to emit).
    pub fn push(&mut self, packet: RtpPacket) -> bool {
        let seq = packet.header.sequence;
        match self.next {
            None => {
                self.next = Some(seq);
            }
            Some(next) if serial_lt(seq, next) => {
                if self.started {
                    return false; // late — we've already emitted past this point
                }
                self.next = Some(seq); // pre-emit: lower the baseline for early reorder
            }
            Some(_) => {}
        }
        if self.buf.contains_key(&seq) {
            return false; // duplicate
        }
        self.buf.insert(seq, packet);
        self.enforce_depth();
        true
    }

    /// Pop the next in-order packet, if it has arrived.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        let next = self.next?;
        if let Some(packet) = self.buf.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            self.started = true;
            Some(packet)
        } else {
            None
        }
    }

    /// If a hole has held delivery up for more than `max_depth` packets, give up on it:
    /// advance `next` to the earliest buffered sequence so [`Self::pop`] can proceed.
    /// This is the "drop late frames" policy for live media.
    ///
    /// The depth bounds *how long a gap is waited on*, not how much a caller may leave
    /// undrained — a caller is expected to [`Self::pop`] until it returns `None` after
    /// every push, which every caller does. It used to bound both, and the second job
    /// cost real packets: on giving up on a hole it went on to discard the packet that
    /// had just become deliverable, so one lost datagram was always reported and felt as
    /// two. For MPEG2-TS that is an extra continuity-counter gap, an extra access unit
    /// dropped, and — since #192 — an extra IDR asked of the source (`skipped()` is what
    /// made it visible).
    fn enforce_depth(&mut self) {
        while self.buf.len() > self.max_depth {
            let Some(next) = self.next else { return };
            // Nothing is stuck: the packet we are waiting for is right here, so the
            // backlog is a burst the caller has not drained yet, not a hole.
            if self.buf.contains_key(&next) {
                return;
            }
            // Earliest buffered = smallest forward distance from `next`.
            let Some((&earliest, _)) = self
                .buf
                .iter()
                .min_by_key(|(&seq, _)| seq.wrapping_sub(next))
            else {
                return;
            };
            // Everything between where we were and where we are going is gone. Counted in
            // packets rather than in events, because "one gap" is what a stall looks like
            // and "forty packets" is what it costs.
            self.skipped = self
                .skipped
                .saturating_add(u64::from(earliest.wrapping_sub(next)));
            self.next = Some(earliest);
            // Drop anything now strictly older than the new `next`.
            self.buf.retain(|&seq, _| !serial_lt(seq, earliest));
        }
    }
}

/// RFC 1982 serial-number "less than" for 16-bit sequence numbers (handles wraparound).
fn serial_lt(a: u16, b: u16) -> bool {
    a != b && (b.wrapping_sub(a)) < 0x8000
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use bytes::Bytes;

    fn pkt(seq: u16) -> RtpPacket {
        let mut v = vec![0x80, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        v[2..4].copy_from_slice(&seq.to_be_bytes());
        RtpPacket::parse(Bytes::from(v)).unwrap()
    }

    #[test]
    fn in_order_passthrough() {
        let mut b = ReorderBuffer::new(8);
        for s in 10..15 {
            b.push(pkt(s));
        }
        for s in 10..15 {
            assert_eq!(b.pop().unwrap().header.sequence, s);
        }
        assert!(b.pop().is_none());
    }

    #[test]
    fn reorders_out_of_order() {
        let mut b = ReorderBuffer::new(8);
        b.push(pkt(2));
        b.push(pkt(0));
        b.push(pkt(1));
        assert_eq!(b.pop().unwrap().header.sequence, 0);
        assert_eq!(b.pop().unwrap().header.sequence, 1);
        assert_eq!(b.pop().unwrap().header.sequence, 2);
    }

    #[test]
    fn drops_late_and_duplicate() {
        let mut b = ReorderBuffer::new(8);
        b.push(pkt(5));
        assert!(b.pop().is_some()); // emits 5, next=6
        assert!(!b.push(pkt(5))); // late
        assert!(b.push(pkt(6)));
        assert!(!b.push(pkt(6))); // duplicate
    }

    #[test]
    fn skips_gap_on_overflow() {
        let mut b = ReorderBuffer::new(3);
        // 0 has to be *delivered* first, or there is no gap here to skip: the very first
        // push sets the baseline, so "seq 0 never arrives, 1 2 3 4 do" — which is what
        // this test used to do — is a stream that starts at 1 and loses nothing. It
        // passed anyway, on `first >= 1`, which is true of every packet it could have
        // returned. Nothing has ever exercised this path.
        b.push(pkt(0));
        assert_eq!(b.pop().unwrap().header.sequence, 0);

        // Now 1 is genuinely missing. Within the depth, it is still worth waiting for.
        for s in [2u16, 3, 4] {
            b.push(pkt(s));
        }
        assert!(b.pop().is_none(), "2 cannot be delivered before 1");
        assert_eq!(b.skipped(), 0);

        // Past it, it is not: latency beats freshness, so the gap is skipped.
        b.push(pkt(5));
        let mut got = Vec::new();
        while let Some(p) = b.pop() {
            got.push(p.header.sequence);
        }
        // Exactly the packets that arrived — giving up on the hole used to take 2 with
        // it, so one loss cost two packets and the second was self-inflicted.
        assert_eq!(got, vec![2, 3, 4, 5]);
        // And it says so. Reordering that resolves itself and a loss that never will are
        // indistinguishable from the outside otherwise — one is free and the other
        // corrupts every frame until the next keyframe (#192).
        assert_eq!(b.skipped(), 1, "the missing packet is counted, once");
    }

    #[test]
    fn reordering_that_resolves_itself_is_not_counted_as_loss() {
        let mut b = ReorderBuffer::new(8);
        for s in [2u16, 0, 1, 3] {
            b.push(pkt(s));
        }
        while b.pop().is_some() {}
        assert_eq!(b.skipped(), 0);
        // Nor is a duplicate, or one that turns up after its place has gone past.
        assert!(!b.push(pkt(1)));
        assert_eq!(b.skipped(), 0);
    }

    #[test]
    fn handles_wraparound() {
        let mut b = ReorderBuffer::new(8);
        b.push(pkt(0xFFFE));
        b.push(pkt(0xFFFF));
        b.push(pkt(0x0000));
        assert_eq!(b.pop().unwrap().header.sequence, 0xFFFE);
        assert_eq!(b.pop().unwrap().header.sequence, 0xFFFF);
        assert_eq!(b.pop().unwrap().header.sequence, 0x0000);
    }
}
