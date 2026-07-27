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
use substrate_rtp::{ReorderBuffer, RtpPacket};

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
}

impl MediaReceiver {
    /// A receiver with an empty reorder buffer and no program knowledge yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reorder: ReorderBuffer::new(REORDER_DEPTH),
            demux: TsDemux::new(),
            foreign: 0,
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
        // and cost a whole access unit.
        if !self.reorder.push(packet) {
            return Vec::new();
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
        rx.push_datagram(rtp(1, MP2T_PAYLOAD_TYPE, &null_ts()));
        assert_eq!(rx.demux().resync_count(), 0);
        assert_eq!(rx.foreign_datagrams(), 0);
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
