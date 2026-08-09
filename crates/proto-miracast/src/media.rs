//! The media plane: RTP datagrams in, [`EncodedFrame`]s out.
//!
//! Two pieces already exist — `substrate-rtp` restores sequence order and [`crate::ts`]
//! demuxes the container — so all this does is join them and enforce the one thing
//! neither can know on its own: which payload type is ours.
//!
//! The reorder depth is small on purpose. A WFD source is on the other end of a
//! single-hop Wi-Fi Direct link, so packets arrive out of order rarely and by one or two
//! at most; buffering deeper would trade the latency that *is* the product for a
//! robustness the link does not need. Ground rule 4: drop late frames.

use bytes::Bytes;
use castaway_core::EncodedFrame;
use substrate_rtp::{Refusal, ReorderBuffer, RtpPacket};

use crate::ts::TsDemux;

/// RFC 3551's static payload type for MPEG-2 transport streams. A WFD source sends this
/// and only this on the media port — the spec fixes it, so a packet with any other
/// payload type is somebody else's traffic that happened to reach our socket.
pub const MP2T_PAYLOAD_TYPE: u8 = 33;

/// How many packets of reordering to absorb before skipping the gap.
const REORDER_DEPTH: usize = 8;

/// Reassembles a Miracast media stream.
pub struct MediaReceiver {
    reorder: ReorderBuffer,
    demux: TsDemux,
    /// Datagrams discarded for not being MP2T-in-RTP.
    foreign: u64,
    /// Datagrams that turned up after their place in the sequence had gone past.
    ///
    /// One number with `duplicate` until #233: collapsed, "[`REORDER_DEPTH`] is too
    /// small for this link" read the same as "the sender duplicates", and those want
    /// different fixes. Read against [`MediaReceiver::lost_datagrams`] — climbing
    /// together, packets are arriving after the buffer gave up waiting on them.
    late: u64,
    /// Datagrams that were copies of a packet still waiting in the reorder buffer:
    /// duplication by the sender or the network, costing bandwidth and nothing else.
    duplicate: u64,
}

impl MediaReceiver {
    /// A receiver with an empty reorder buffer and no program knowledge yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reorder: ReorderBuffer::new(REORDER_DEPTH),
            demux: TsDemux::new(),
            foreign: 0,
            late: 0,
            duplicate: 0,
        }
    }

    /// Feed one UDP datagram. Returns whatever frames that completed.
    pub fn push_datagram(&mut self, datagram: Bytes) -> Vec<EncodedFrame> {
        let Ok(packet) = RtpPacket::parse(datagram) else {
            self.foreign = self.foreign.saturating_add(1);
            return Vec::new();
        };
        if packet.header.payload_type != MP2T_PAYLOAD_TYPE {
            self.foreign = self.foreign.saturating_add(1);
            return Vec::new();
        }
        // A duplicate or a late packet is dropped here rather than reaching the demuxer,
        // where re-feeding bytes it has already seen would break the continuity counter
        // and cost a whole access unit. Counted apart, because they indict different
        // things: `late` the reorder depth, `duplicate` the sender.
        match self.reorder.push(packet) {
            Ok(()) => {}
            Err(Refusal::Late) => {
                self.late = self.late.saturating_add(1);
                return Vec::new();
            }
            Err(Refusal::Duplicate) => {
                self.duplicate = self.duplicate.saturating_add(1);
                return Vec::new();
            }
        }
        let mut out = Vec::new();
        while let Some(packet) = self.reorder.pop() {
            out.extend(self.demux.push(&packet.payload));
        }
        out
    }

    /// Flush the demuxer at end of session — the last video access unit is still waiting
    /// for a next-packet start bit that will never come.
    pub fn flush(&mut self) -> Vec<EncodedFrame> {
        self.demux.flush()
    }

    /// How many datagrams were not MP2T-in-RTP. Nonzero means something else is sending
    /// to our media port, which on a P2P group usually means the port was reused.
    #[must_use]
    pub fn foreign_datagrams(&self) -> u64 {
        self.foreign
    }

    /// How many datagrams never arrived, as counted by the holes they left.
    ///
    /// The link's own number, before any of it is interpreted: reordering that resolves
    /// itself does not appear here, so a nonzero value is loss and nothing else.
    #[must_use]
    pub fn lost_datagrams(&self) -> u64 {
        self.reorder.skipped()
    }

    /// How many datagrams arrived after their place in the sequence had gone past.
    ///
    /// Read beside [`MediaReceiver::lost_datagrams`]: both climbing means the link
    /// reorders more deeply than [`REORDER_DEPTH`] absorbs, so packets are being given
    /// up on and then arriving anyway.
    #[must_use]
    pub const fn late_datagrams(&self) -> u64 {
        self.late
    }

    /// How many datagrams were copies of a packet already waiting in the buffer.
    ///
    /// Duplication by the sender or the network: harmless in itself, and a different
    /// fault from `late` — which is why they are no longer one number (#233).
    #[must_use]
    pub const fn duplicate_datagrams(&self) -> u64 {
        self.duplicate
    }

    /// The demuxer, for the session-level diagnostics that report what the PMT declared.
    #[must_use]
    pub fn demux(&self) -> &TsDemux {
        &self.demux
    }
}

impl Default for MediaReceiver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn rtp(seq: u16, payload_type: u8, payload: &[u8]) -> Bytes {
        let mut pkt = vec![0x80, payload_type];
        pkt.extend_from_slice(&seq.to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes()); // timestamp
        pkt.extend_from_slice(&0u32.to_be_bytes()); // ssrc
        pkt.extend_from_slice(payload);
        Bytes::from(pkt)
    }

    /// 188 bytes of null packet: a legal TS payload that produces no frames.
    fn null_ts() -> Vec<u8> {
        let mut pkt = vec![0u8; crate::ts::TS_PACKET_LEN];
        pkt[0] = 0x47;
        pkt[1] = 0x1F;
        pkt[2] = 0xFF;
        pkt[3] = 0x10;
        pkt
    }

    #[test]
    fn a_foreign_payload_type_is_counted_and_dropped() {
        let mut rx = MediaReceiver::new();
        // PT 96 is a dynamic type; whatever it is, it is not our transport stream.
        assert!(rx.push_datagram(rtp(1, 96, &null_ts())).is_empty());
        assert_eq!(rx.foreign_datagrams(), 1);
    }

    #[test]
    fn a_datagram_too_short_to_be_rtp_is_counted_rather_than_panicking() {
        let mut rx = MediaReceiver::new();
        assert!(rx.push_datagram(Bytes::from_static(b"hi")).is_empty());
        assert_eq!(rx.foreign_datagrams(), 1);
    }

    #[test]
    fn a_duplicate_never_reaches_the_demuxer() {
        // Re-feeding bytes to the demuxer would look like a continuity-counter jump and
        // cost the access unit that was mid-assembly.
        let mut rx = MediaReceiver::new();
        rx.push_datagram(rtp(1, MP2T_PAYLOAD_TYPE, &null_ts()));
        rx.push_datagram(rtp(2, MP2T_PAYLOAD_TYPE, &null_ts()));
        // Seq 2 was emitted, so its copy reads as late; a copy of one still *buffered*
        // is the duplicate case proper — hold packet 4 back by leaving 3 missing.
        rx.push_datagram(rtp(4, MP2T_PAYLOAD_TYPE, &null_ts()));
        rx.push_datagram(rtp(4, MP2T_PAYLOAD_TYPE, &null_ts()));
        assert_eq!(rx.demux().resync_count(), 0);
        assert_eq!(rx.foreign_datagrams(), 0);
        // …and it lands on its own counter (#233): a sender that duplicates and a
        // reorder depth too small for the link are different faults, and reading
        // `late` beside `lost` only works if duplicates are not folded into it.
        assert_eq!(rx.duplicate_datagrams(), 1);
        assert_eq!(rx.late_datagrams(), 0);
        assert_eq!(rx.lost_datagrams(), 0);
    }

    #[test]
    fn a_datagram_that_never_arrives_is_counted_as_lost_once_the_buffer_gives_up() {
        // Below the reorder depth the hole is still open, and calling it a loss then
        // would make every reordered pair look like damage. Past the depth it is real,
        // and the session layer has to act on it (#192).
        let mut rx = MediaReceiver::new();
        let depth = u16::try_from(REORDER_DEPTH).unwrap();
        // 0 and 1 are delivered; 2 never arrives.
        for seq in [0, 1] {
            rx.push_datagram(rtp(seq, MP2T_PAYLOAD_TYPE, &null_ts()));
        }
        for seq in 3..3 + depth {
            rx.push_datagram(rtp(seq, MP2T_PAYLOAD_TYPE, &null_ts()));
        }
        assert_eq!(rx.lost_datagrams(), 0, "seq 2 could still turn up");
        rx.push_datagram(rtp(3 + depth, MP2T_PAYLOAD_TYPE, &null_ts()));
        assert_eq!(rx.lost_datagrams(), 1);
        // And one that turns up after its place has gone past is neither loss nor damage:
        // the hole it would have filled has already been counted. It is `late`, not
        // `duplicate` — beside `lost` climbing, this pair is what says the reorder depth
        // is too small for the link (#233).
        rx.push_datagram(rtp(2, MP2T_PAYLOAD_TYPE, &null_ts()));
        assert_eq!(rx.late_datagrams(), 1);
        assert_eq!(rx.duplicate_datagrams(), 0);
        assert_eq!(rx.lost_datagrams(), 1);
    }

    #[test]
    fn two_lost_datagrams_become_two_video_gaps_and_nothing_else() {
        // The chain `miracast-vm` asserts across a radio, at the tier where the numbers
        // can be reasoned about: one datagram per access unit — which is what the sink
        // sees from a source sending small frames — with two of them withheld.
        //
        // Every step in between is a place the count could be wrong. The reorder buffer
        // holds each hole open for its depth and then gives up on it *once*; the demuxer
        // sees the continuity counter jump *once* per hole; and the session layer turns
        // each of those into one IDR request. Two withheld datagrams have to come out the
        // far end as two, or the sink either asks for keyframes it does not need or fails
        // to ask for one it does (#192).
        use crate::ts::tests as ts;

        let mut rx = MediaReceiver::new();
        let mut tables = ts::ts_packet(crate::ts::PAT_PID, true, 0, &ts::pat(ts::PMT_PID));
        tables.extend_from_slice(&ts::ts_packet(
            ts::PMT_PID,
            true,
            0,
            &ts::pmt(&[(ts::VIDEO_PID, 0x1B)]),
        ));
        rx.push_datagram(rtp(0, MP2T_PAYLOAD_TYPE, &tables));

        // Far enough apart that each hole crosses the reorder depth on its own, which is
        // the same spacing the VM's schedule uses and for the same reason.
        let withheld = [5u16, 16];
        let mut frames = 0;
        for n in 0..40u16 {
            let pes = ts::pes(
                0xE0,
                Some(90_000 + u64::from(n) * 3_600),
                &ts::non_idr_access_unit(),
                false,
            );
            let packet = ts::packetize(ts::VIDEO_PID, (n % 16) as u8, &pes);
            // The sequence number and the continuity counter advance either way: the
            // source produced the packet, and it is the air that lost it.
            if !withheld.contains(&n) {
                frames += rx
                    .push_datagram(rtp(n + 1, MP2T_PAYLOAD_TYPE, &packet))
                    .len();
            }
        }
        assert_eq!(rx.lost_datagrams(), 2);
        assert_eq!(rx.demux().video_gap_count(), 2);
        assert_eq!(rx.late_datagrams(), 0);
        assert_eq!(rx.foreign_datagrams(), 0);
        assert_eq!(rx.demux().resync_count(), 0, "no datagram was ever torn");
        // And the picture survives: an unbounded PES completes at the next one's start,
        // so 38 arrivals can yield at most 37 frames, and each hole costs the access unit
        // it swallowed plus the one it left half-trusted.
        assert!(
            frames >= 33,
            "only {frames} access units survived two losses"
        );
    }

    #[test]
    fn out_of_order_datagrams_reach_the_demuxer_in_sequence() {
        let mut rx = MediaReceiver::new();
        // Seven TS packets to a datagram is what a source sends; the payload here is one,
        // which is enough to prove the ordering.
        rx.push_datagram(rtp(1, MP2T_PAYLOAD_TYPE, &null_ts()));
        rx.push_datagram(rtp(3, MP2T_PAYLOAD_TYPE, &null_ts()));
        rx.push_datagram(rtp(2, MP2T_PAYLOAD_TYPE, &null_ts()));
        // Nothing is emitted (null packets carry nothing), but nothing resynced either:
        // the demuxer saw whole packets in order.
        assert_eq!(rx.demux().resync_count(), 0);
    }
}
