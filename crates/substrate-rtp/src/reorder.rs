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
        }
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

    /// If over depth, skip the gap: advance `next` to the earliest buffered sequence so
    /// [`Self::pop`] can proceed. This is the "drop late frames" policy for live media.
    fn enforce_depth(&mut self) {
        while self.buf.len() > self.max_depth {
            let Some(next) = self.next else { return };
            // Earliest buffered = smallest forward distance from `next`.
            if let Some((&earliest, _)) = self
                .buf
                .iter()
                .min_by_key(|(&seq, _)| seq.wrapping_sub(next))
            {
                self.next = Some(earliest);
                // Drop anything now strictly older than the new `next`.
                let cutoff = earliest;
                self.buf.retain(|&seq, _| !serial_lt(seq, cutoff));
            }
            // Emit by popping happens via pop(); here we just moved `next` forward, but
            // if buffer is still over depth (all contiguous), drop the oldest.
            if self.buf.len() > self.max_depth {
                if let Some(next2) = self.next {
                    self.buf.remove(&next2);
                    self.next = Some(next2.wrapping_add(1));
                }
            }
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
        // seq 0 never arrives; 1,2,3,4 do — buffer overflows depth 3 and must skip 0.
        for s in [1u16, 2, 3, 4] {
            b.push(pkt(s));
        }
        // First pop should not stall on the missing 0.
        let first = b.pop().unwrap().header.sequence;
        assert!(
            first >= 1,
            "must have skipped the missing seq 0, got {first}"
        );
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
