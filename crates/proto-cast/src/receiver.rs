//! The Cast mirroring receiver: many frames in flight, reassembled and handed out.
//!
//! [`crate::rtp::FrameCollector`] assembles one frame. This is the layer above: it owns
//! a sliding window of collectors, decides which frame is next, tracks the checkpoint
//! the sender needs to hear about, and produces the ACK/NACK lists that
//! [`crate::rtcp`] serializes.
//!
//! Pure per ground rule 3. There is no clock here — every timing decision is an input.
//! Whether the next frame is "late enough to give up on" is the caller's call, passed
//! in as a [`Consume`] policy; the NTP reading for the reference-time report is passed
//! to [`CastRtpReceiver::feedback`]. The same packets in the same order always produce
//! the same frames and the same feedback bytes.
//!
//! Modelled on openscreen's `cast/streaming/impl/receiver_impl.cc`.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::rtcp::Feedback;
use crate::rtp::{
    Accepted, CastRtpStream, CollectError, Dependency, EncryptedFrame, FrameCollector, FrameId,
    PacketId, RtpError,
};

/// How many frames may be in flight before the receiver stops accepting new ones.
///
/// Cast truncates frame ids to eight bits on the wire, so the window must stay well
/// under 256 for [`FrameId::expand`] to resolve them unambiguously. openscreen's
/// `kMaxUnackedFrames`.
pub const MAX_UNACKED_FRAMES: i64 = 120;

/// The playout delay assumed until a sender asks for a different one.
pub const DEFAULT_PLAYOUT_DELAY_MS: u16 = 400;

/// What a datagram did to the receiver's state.
///
/// Only [`RtpError`] is a genuine failure. The rest are the ordinary weather of a
/// lossy link — duplicates, stragglers, packets from a frame we gave up on — and the
/// caller's job is to count them, not to react.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Received {
    /// Accepted into a frame that still needs more packets.
    Progress(FrameId),
    /// Accepted, and it was the last packet that frame needed.
    FrameComplete(FrameId),
    /// A duplicate of a packet already held. Retransmission produces these.
    Duplicate(FrameId),
    /// For a frame already delivered or abandoned. Dropped.
    TooOld(FrameId),
    /// So far ahead that holding it would blow the window. Dropped, and the sender
    /// will resend once we have caught up.
    TooFar(FrameId),
    /// Contradicted what earlier packets established about its frame. Dropped.
    Inconsistent(CollectError),
}

/// When the receiver may abandon frames to keep playing.
///
/// An enum and not a `bool` because the two answers are decisions, not flags: ground
/// rule 4 says latency beats freshness, but only the caller knows whether the next
/// frame has run out of time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consume {
    /// Deliver only the frame immediately after the last one delivered. Use while the
    /// next frame still has time to arrive.
    InOrder,
    /// The next frame is late. Deliver the earliest complete frame that can be decoded
    /// standalone, abandoning whatever incomplete frames stand in front of it.
    SkipToDecodable,
}

/// A frame handed to the decoder, and what it cost to get there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// The assembled, still-encrypted frame.
    pub frame: EncryptedFrame,
    /// Frames abandoned to reach this one. Non-zero means a visible glitch.
    pub skipped: usize,
}

/// A sliding window of frames under assembly.
///
/// The window's far edge is [`MAX_UNACKED_FRAMES`] past the last frame *delivered*, not
/// the last one received — so a consumer that stops calling [`Self::next_frame`] stalls
/// the sender rather than growing this queue without bound. That is deliberate
/// backpressure, and it is why the caller must keep consuming.
#[derive(Debug)]
pub struct CastRtpReceiver {
    stream: CastRtpStream,
    receiver_ssrc: u32,
    /// One collector per frame in `(checkpoint, latest_expected]`, including frames no
    /// packet has arrived for yet — those are exactly the ones that need NACKing.
    pending: BTreeMap<FrameId, FrameCollector>,
    last_delivered: FrameId,
    checkpoint: FrameId,
    latest_expected: FrameId,
    playout_delay_ms: u16,
    feedback_count: u8,
    picture_loss: bool,
    last_key_frame: Option<FrameId>,
}

impl CastRtpReceiver {
    /// Start a receiver for one stream — one audio or one video SSRC pair.
    #[must_use]
    pub fn new(sender_ssrc: u32, receiver_ssrc: u32) -> Self {
        Self {
            stream: CastRtpStream::new(sender_ssrc),
            receiver_ssrc,
            pending: BTreeMap::new(),
            last_delivered: FrameId::leader(),
            checkpoint: FrameId::leader(),
            latest_expected: FrameId::leader(),
            playout_delay_ms: DEFAULT_PLAYOUT_DELAY_MS,
            feedback_count: 0,
            picture_loss: false,
            last_key_frame: None,
        }
    }

    /// Every frame up to and including this one is accounted for — received, delivered,
    /// or deliberately abandoned. This is what the sender is told, and what frees it to
    /// stop retransmitting.
    #[must_use]
    pub const fn checkpoint(&self) -> FrameId {
        self.checkpoint
    }

    /// The playout delay currently in force, in milliseconds.
    #[must_use]
    pub const fn playout_delay_ms(&self) -> u16 {
        self.playout_delay_ms
    }

    /// Whether the sender has moved past the frame we owe the decoder.
    ///
    /// This is what tells "the next frame is late" apart from "the stream is idle" —
    /// both make [`Self::next_frame`] return `None`, but only the first is a reason to
    /// start a clock running toward [`Consume::SkipToDecodable`].
    #[must_use]
    pub const fn is_awaiting_frames(&self) -> bool {
        self.latest_expected.value() > self.last_delivered.value()
    }

    /// Feed in one UDP datagram.
    ///
    /// # Errors
    /// [`RtpError`] if the bytes are not a well-formed Cast RTP packet for this
    /// stream's SSRC. That is a reason to drop the datagram, never to end the session.
    pub fn receive(&mut self, datagram: &Bytes) -> Result<Received, RtpError> {
        let packet = self.stream.parse(datagram)?;
        let frame_id = packet.frame_id;

        if frame_id <= self.checkpoint {
            return Ok(Received::TooOld(frame_id));
        }

        if frame_id > self.latest_expected {
            // Refusing to grow past the window is what bounds our memory, and it is
            // also what keeps frame ids unambiguous: an 8-bit id can only be expanded
            // correctly while the window stays far short of 256 frames.
            if frame_id.value() > self.last_delivered.value() + MAX_UNACKED_FRAMES {
                return Ok(Received::TooFar(frame_id));
            }
            // Frames between the old horizon and this one may be entirely lost. Give
            // each an empty collector so `feedback` NACKs them; without this a wholly
            // dropped frame would never be asked for and the stream would wedge.
            let mut next = self.latest_expected.next();
            while next <= frame_id {
                self.pending.insert(next, FrameCollector::new(next));
                next = next.next();
            }
            self.latest_expected = frame_id;
        }

        let Some(collector) = self.pending.get_mut(&frame_id) else {
            // Unreachable: everything in `(checkpoint, latest_expected]` has an entry,
            // and the two branches above put `frame_id` in that range.
            return Ok(Received::TooOld(frame_id));
        };

        match collector.collect(&packet) {
            Ok(Accepted::New) => {}
            Ok(Accepted::Duplicate) => return Ok(Received::Duplicate(frame_id)),
            Err(err) => return Ok(Received::Inconsistent(err)),
        }

        // A playout-delay change rides on packet 0 and takes effect from that frame on.
        if packet.packet_id == PacketId::ZERO {
            if let Some(delay) = packet.new_playout_delay_ms {
                self.playout_delay_ms = delay;
            }
        }

        if !collector.is_complete() {
            return Ok(Received::Progress(frame_id));
        }

        if collector.header().map(|h| h.dependency) == Some(Dependency::KeyFrame) {
            // The decoder can recover from here, so whatever was broken is not any more.
            self.picture_loss = false;
            self.last_key_frame = Some(frame_id);
        }

        if frame_id == self.checkpoint.next() {
            self.advance_checkpoint(frame_id);
        }
        Ok(Received::FrameComplete(frame_id))
    }

    /// Take the next frame for the decoder, if one is ready under `policy`.
    ///
    /// Under [`Consume::SkipToDecodable`] this may abandon incomplete frames to reach a
    /// standalone-decodable one. It never lands on a frame that depends on something it
    /// just threw away, so the picture stays decodable across the skip.
    pub fn next_frame(&mut self, policy: Consume) -> Option<Delivered> {
        let immediate = self.last_delivered.next();
        let mut candidate = immediate;
        let target = loop {
            if candidate > self.latest_expected {
                return None;
            }
            let collector = self.pending.get(&candidate)?;
            if collector.is_complete() {
                let independent = collector
                    .header()
                    .is_some_and(|header| header.dependency != Dependency::Dependent);
                if candidate == immediate {
                    break candidate;
                }
                if policy == Consume::SkipToDecodable && independent {
                    break candidate;
                }
            } else if policy == Consume::InOrder {
                // The frame we owe the decoder has not arrived and we are not allowed
                // to give up on it yet.
                return None;
            }
            candidate = candidate.next();
        };

        let skipped = usize::try_from(target.value() - immediate.value()).unwrap_or(0);
        let mut dropped = immediate;
        while dropped < target {
            self.pending.remove(&dropped);
            dropped = dropped.next();
        }
        let frame = self.pending.remove(&target)?.take_frame()?;
        self.last_delivered = target;
        if target > self.checkpoint {
            // The skipped frames will never be completed, so the sender must be told to
            // stop trying — the checkpoint is the only way to say so.
            self.advance_checkpoint(target);
        }
        Some(Delivered { frame, skipped })
    }

    /// Ask the sender for a fresh key frame, because the picture is undecodable.
    ///
    /// Ignored if the last key frame has not been delivered yet: it is already on its
    /// way and asking again would only cost bandwidth.
    pub fn request_key_frame(&mut self) {
        if self
            .last_key_frame
            .is_some_and(|id| self.last_delivered >= id)
        {
            self.picture_loss = true;
        }
    }

    /// Build the feedback describing what has and has not arrived.
    ///
    /// `ntp_timestamp` is the caller's clock reading for the reference-time report, or
    /// `None` to omit it. Takes `&mut self` only to advance the wrap-around feedback
    /// counter the sender uses to order our reports.
    pub fn feedback(&mut self, ntp_timestamp: Option<u64>) -> Feedback {
        let mut acks = Vec::new();
        let mut nacks = Vec::new();
        for (frame_id, collector) in self.pending.range(self.checkpoint.next()..) {
            if collector.is_complete() {
                acks.push(*frame_id);
            } else {
                nacks.extend(collector.missing_packets());
            }
        }

        let feedback_count = self.feedback_count;
        self.feedback_count = self.feedback_count.wrapping_add(1);

        Feedback {
            receiver_ssrc: self.receiver_ssrc,
            sender_ssrc: self.stream.sender_ssrc(),
            checkpoint: self.checkpoint,
            playout_delay_ms: self.playout_delay_ms,
            nacks,
            acks,
            feedback_count,
            picture_loss: self.picture_loss,
            ntp_timestamp,
            // The receiver has no clock, so the sender-report echo — a *delay*
            // measurement — is the actor's to fill in (see `HeardSenderReport`).
            last_sender_report: None,
        }
    }

    /// Move the checkpoint to `new_checkpoint`, then as far past it as the run of
    /// already-complete frames allows, and forget everything it now covers.
    fn advance_checkpoint(&mut self, new_checkpoint: FrameId) {
        let mut checkpoint = new_checkpoint;
        while checkpoint < self.latest_expected {
            let next = checkpoint.next();
            if !self
                .pending
                .get(&next)
                .is_some_and(FrameCollector::is_complete)
            {
                break;
            }
            checkpoint = next;
        }
        self.checkpoint = checkpoint;
        // Anything at or below the checkpoint is settled; a packet for it would be
        // rejected as TooOld, so the collector is dead weight. Frames still awaiting
        // delivery are kept — only the ones already handed out are gone.
        self.pending
            .retain(|frame_id, _| *frame_id > checkpoint || *frame_id > self.last_delivered);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::rtp::{Dependency, RtpTimestamp};

    const SENDER_SSRC: u32 = 0x0102_0304;
    const RECEIVER_SSRC: u32 = 0x0506_0708;

    /// Build one Cast RTP datagram by hand. Keeping this in the test module rather
    /// than shipping a serializer keeps the crate honest: we are a receiver.
    struct Packet {
        frame_id: i64,
        packet_id: u16,
        max_packet_id: u16,
        key_frame: bool,
        reference: Option<i64>,
        payload: &'static [u8],
    }

    impl Packet {
        fn frame(frame_id: i64, key_frame: bool, payloads: &[&'static [u8]]) -> Vec<Bytes> {
            let max = u16::try_from(payloads.len() - 1).unwrap();
            payloads
                .iter()
                .enumerate()
                .map(|(index, payload)| {
                    Self {
                        frame_id,
                        packet_id: u16::try_from(index).unwrap(),
                        max_packet_id: max,
                        key_frame,
                        reference: None,
                        payload,
                    }
                    .encode()
                })
                .collect()
        }

        fn encode(&self) -> Bytes {
            let mut out = Vec::new();
            out.push(0x80);
            out.push(96); // payload type
            out.extend_from_slice(&0u16.to_be_bytes()); // sequence number
            out.extend_from_slice(&0u32.to_be_bytes()); // rtp timestamp
            out.extend_from_slice(&SENDER_SSRC.to_be_bytes());

            let mut byte12 = 0u8;
            if self.key_frame {
                byte12 |= 0x80;
            }
            if self.reference.is_some() {
                byte12 |= 0x40;
            }
            out.push(byte12);
            out.push(FrameId::new(self.frame_id).lower_8_bits());
            out.extend_from_slice(&self.packet_id.to_be_bytes());
            out.extend_from_slice(&self.max_packet_id.to_be_bytes());
            if let Some(reference) = self.reference {
                out.push(FrameId::new(reference).lower_8_bits());
            }
            out.extend_from_slice(self.payload);
            Bytes::from(out)
        }
    }

    fn receiver() -> CastRtpReceiver {
        CastRtpReceiver::new(SENDER_SSRC, RECEIVER_SSRC)
    }

    #[test]
    fn a_single_packet_frame_completes_and_moves_the_checkpoint() {
        let mut rx = receiver();
        assert_eq!(rx.checkpoint(), FrameId::leader());

        let packets = Packet::frame(0, true, &[b"hello"]);
        assert_eq!(
            rx.receive(&packets[0]).unwrap(),
            Received::FrameComplete(FrameId::first())
        );
        assert_eq!(rx.checkpoint(), FrameId::first());

        let delivered = rx.next_frame(Consume::InOrder).unwrap();
        assert_eq!(delivered.skipped, 0);
        assert_eq!(&delivered.frame.payload[..], b"hello");
        assert_eq!(delivered.frame.header.dependency, Dependency::KeyFrame);
        assert_eq!(delivered.frame.header.rtp_timestamp, RtpTimestamp::zero());
    }

    #[test]
    fn packets_reassemble_in_packet_order_however_they_arrive() {
        let mut rx = receiver();
        let packets = Packet::frame(0, true, &[b"one", b"two", b"three"]);
        // Deliberately out of order — the network does this constantly.
        rx.receive(&packets[2]).unwrap();
        rx.receive(&packets[0]).unwrap();
        assert!(rx.next_frame(Consume::InOrder).is_none());
        rx.receive(&packets[1]).unwrap();

        let delivered = rx.next_frame(Consume::InOrder).unwrap();
        assert_eq!(&delivered.frame.payload[..], b"onetwothree");
    }

    #[test]
    fn a_retransmitted_packet_is_a_duplicate_not_an_error() {
        let mut rx = receiver();
        let packets = Packet::frame(0, true, &[b"a", b"b"]);
        rx.receive(&packets[0]).unwrap();
        assert_eq!(
            rx.receive(&packets[0]).unwrap(),
            Received::Duplicate(FrameId::first())
        );
        rx.receive(&packets[1]).unwrap();
        // Completing the frame moved the checkpoint past it, so a retransmission now
        // reads as too old rather than as a duplicate. Both are drops; the distinction
        // only matters to whoever is counting them.
        assert_eq!(
            rx.receive(&packets[1]).unwrap(),
            Received::TooOld(FrameId::first())
        );
        assert_eq!(
            &rx.next_frame(Consume::InOrder).unwrap().frame.payload[..],
            b"ab"
        );
    }

    #[test]
    fn packets_for_a_settled_frame_are_refused() {
        let mut rx = receiver();
        let packets = Packet::frame(0, true, &[b"a"]);
        rx.receive(&packets[0]).unwrap();
        rx.next_frame(Consume::InOrder).unwrap();
        assert_eq!(
            rx.receive(&packets[0]).unwrap(),
            Received::TooOld(FrameId::first())
        );
    }

    #[test]
    fn a_wholly_lost_frame_is_nacked_even_though_no_packet_of_it_arrived() {
        let mut rx = receiver();
        // Frame 0 arrives, frame 1 vanishes entirely, frame 2 arrives.
        for packet in Packet::frame(0, true, &[b"a"]) {
            rx.receive(&packet).unwrap();
        }
        for packet in Packet::frame(2, false, &[b"c"]) {
            rx.receive(&packet).unwrap();
        }

        let feedback = rx.feedback(None);
        assert_eq!(feedback.checkpoint, FrameId::first());
        assert_eq!(feedback.acks, vec![FrameId::new(2)]);
        // Nothing of frame 1 arrived, so we cannot name its packets — the all-packets
        // sentinel is the only honest request.
        assert_eq!(feedback.nacks.len(), 1);
        assert_eq!(feedback.nacks[0].frame_id, FrameId::new(1));
        assert_eq!(feedback.nacks[0].target, crate::rtp::NackTarget::AllPackets);
    }

    #[test]
    fn the_checkpoint_jumps_over_a_run_of_already_complete_frames() {
        let mut rx = receiver();
        // Frames 1 and 2 arrive before frame 0 does. The checkpoint cannot move until
        // frame 0 lands, and then it must move all the way to 2 in one step.
        for frame in [1, 2] {
            for packet in Packet::frame(frame, false, &[b"x"]) {
                rx.receive(&packet).unwrap();
            }
        }
        assert_eq!(rx.checkpoint(), FrameId::leader());
        for packet in Packet::frame(0, true, &[b"x"]) {
            rx.receive(&packet).unwrap();
        }
        assert_eq!(rx.checkpoint(), FrameId::new(2));
    }

    #[test]
    fn in_order_consumption_waits_for_a_hole_it_could_skip() {
        let mut rx = receiver();
        for packet in Packet::frame(1, true, &[b"key"]) {
            rx.receive(&packet).unwrap();
        }
        // Frame 0 is missing. Frame 1 is a complete key frame, but under InOrder we
        // hold the line and wait for it.
        assert!(rx.next_frame(Consume::InOrder).is_none());
    }

    #[test]
    fn skipping_lands_on_a_decodable_frame_and_reports_the_gap() {
        let mut rx = receiver();
        for packet in Packet::frame(2, true, &[b"key"]) {
            rx.receive(&packet).unwrap();
        }
        let delivered = rx.next_frame(Consume::SkipToDecodable).unwrap();
        assert_eq!(&delivered.frame.payload[..], b"key");
        assert_eq!(delivered.skipped, 2, "frames 0 and 1 were abandoned");
        // The sender must stop retransmitting the frames we gave up on.
        assert_eq!(rx.checkpoint(), FrameId::new(2));
        assert!(rx.feedback(None).nacks.is_empty());
    }

    #[test]
    fn skipping_refuses_to_land_on_a_frame_that_needs_what_was_dropped() {
        let mut rx = receiver();
        // Frame 0 is missing; frame 1 is complete but depends on frame 0. Delivering
        // it would hand the decoder something it cannot decode.
        for packet in Packet::frame(1, false, &[b"delta"]) {
            rx.receive(&packet).unwrap();
        }
        assert!(rx.next_frame(Consume::SkipToDecodable).is_none());
    }

    #[test]
    fn frames_beyond_the_window_are_refused_rather_than_buffered() {
        let mut rx = receiver();
        for packet in Packet::frame(0, true, &[b"a"]) {
            rx.receive(&packet).unwrap();
        }
        rx.next_frame(Consume::InOrder).unwrap();

        // last_delivered is 0, so 120 is the last frame we will hold.
        let far = Packet::frame(MAX_UNACKED_FRAMES + 1, true, &[b"z"]);
        assert_eq!(
            rx.receive(&far[0]).unwrap(),
            Received::TooFar(FrameId::new(MAX_UNACKED_FRAMES + 1))
        );
        let edge = Packet::frame(MAX_UNACKED_FRAMES, true, &[b"z"]);
        assert_eq!(
            rx.receive(&edge[0]).unwrap(),
            Received::FrameComplete(FrameId::new(MAX_UNACKED_FRAMES))
        );
    }

    #[test]
    fn a_playout_delay_change_on_packet_zero_takes_effect() {
        let mut rx = receiver();
        assert_eq!(rx.playout_delay_ms(), DEFAULT_PLAYOUT_DELAY_MS);

        // Byte 12 carries one extension; type 1, size 2, value 800ms.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0x80, 96, 0, 0, 0, 0, 0, 0]);
        bytes.extend_from_slice(&SENDER_SSRC.to_be_bytes());
        bytes.push(0x81); // key frame, one extension
        bytes.push(0); // frame 0
        bytes.extend_from_slice(&0u16.to_be_bytes()); // packet 0
        bytes.extend_from_slice(&0u16.to_be_bytes()); // max packet 0
        bytes.extend_from_slice(&0x0402u16.to_be_bytes()); // ext type 1, size 2
        bytes.extend_from_slice(&800u16.to_be_bytes());
        bytes.extend_from_slice(b"payload");

        rx.receive(&Bytes::from(bytes)).unwrap();
        assert_eq!(rx.playout_delay_ms(), 800);
        assert_eq!(rx.feedback(None).playout_delay_ms, 800);
    }

    #[test]
    fn the_feedback_counter_advances_once_per_report() {
        let mut rx = receiver();
        assert_eq!(rx.feedback(None).feedback_count, 0);
        assert_eq!(rx.feedback(None).feedback_count, 1);
        assert_eq!(rx.feedback(None).feedback_count, 2);
    }

    #[test]
    fn a_key_frame_request_is_only_made_once_the_last_one_is_stale() {
        let mut rx = receiver();
        // Nothing received yet: there is no key frame to be stale, so asking is a
        // no-op rather than a demand the sender cannot satisfy any faster.
        rx.request_key_frame();
        assert!(!rx.feedback(None).picture_loss);

        for packet in Packet::frame(0, true, &[b"key"]) {
            rx.receive(&packet).unwrap();
        }
        for packet in Packet::frame(1, false, &[b"delta"]) {
            rx.receive(&packet).unwrap();
        }
        rx.next_frame(Consume::InOrder).unwrap();

        rx.request_key_frame();
        assert!(rx.feedback(None).picture_loss);

        // A fresh key frame clears the condition without anyone having to say so.
        for packet in Packet::frame(2, true, &[b"key2"]) {
            rx.receive(&packet).unwrap();
        }
        assert!(!rx.feedback(None).picture_loss);
    }

    #[test]
    fn a_corrupt_datagram_does_not_move_the_stream_forward() {
        let mut rx = receiver();
        for packet in Packet::frame(0, true, &[b"a"]) {
            rx.receive(&packet).unwrap();
        }
        let before = rx.checkpoint();
        assert!(rx.receive(&Bytes::from_static(b"not rtp at all")).is_err());
        assert_eq!(rx.checkpoint(), before);
    }
}
