//! The RTCP a Cast receiver owes its sender.
//!
//! Cast senders retransmit only what the receiver asks for, and stop sending
//! altogether if nobody is acknowledging — so this is not optional telemetry, it is
//! what keeps a mirroring session alive. Every packet we emit is an RTCP *compound*
//! packet: an empty Receiver Report (RFC 3550 §6.4.2 requires a compound packet from a
//! receiver to start with one), optionally a Receiver Reference Time Report, then
//! Cast's own application-defined Feedback message carrying the checkpoint, the packet
//! NACKs, and the frame ACK bit vector.
//!
//! Derived from openscreen's `cast/streaming/impl/compound_rtcp_builder.cc` and
//! `rtcp_common.cc`. Pure: the caller supplies the clock reading, so this builds the
//! same bytes every time for the same inputs and is testable without a socket.

use crate::rtp::{FrameId, NackTarget, PacketNack};

/// Version 2, no padding — the top three bits of an RTCP header's first byte.
const REQUIRED_VERSION_AND_PADDING: u8 = 0b100;
const REPORT_COUNT_BITS: u32 = 5;

const PT_SENDER_REPORT: u8 = 200;
const PT_RECEIVER_REPORT: u8 = 201;
const PT_PAYLOAD_SPECIFIC: u8 = 206;
const PT_EXTENDED_REPORTS: u8 = 207;

const SUBTYPE_PICTURE_LOSS_INDICATOR: u8 = 1;
const SUBTYPE_FEEDBACK: u8 = 15;

/// `'C','A','S','T'` — marks the application-defined feedback message as Cast's.
const CAST_IDENTIFIER: [u8; 4] = *b"CAST";
/// `'C','S','T','2'` — marks the frame-ACK extension that follows the loss fields.
const CST2_IDENTIFIER: [u8; 4] = *b"CST2";

const RECEIVER_REFERENCE_TIME_BLOCK_TYPE: u8 = 4;

/// Each loss field is frame id (1) + first packet id (2) + bit vector (1).
const LOSS_FIELD_SIZE: usize = 4;
/// The ACK bit vector is at least two octets, if only to pad the six-byte CST2 header
/// out to a 4-byte boundary.
const MIN_ACK_BITVECTOR_OCTETS: usize = 2;
/// The octet count is a `u8`, and rule 2 below forces growth in steps of four, so 254
/// is the effective ceiling.
const MAX_ACK_BITVECTOR_OCTETS: usize = 254;
/// The loss-field count is a `u8`.
const MAX_LOSS_FIELDS: usize = 255;

/// The echo of the sender's last Sender Report, carried in our Receiver Report block.
///
/// This is the sender's *only* way to measure the network round trip: it subtracts
/// `delay` from the time between sending that report and hearing this echo. A receiver
/// that never echoes leaves the sender's RTT estimate at zero, which pins its in-flight
/// media budget at the floor — and Chrome's bitrate governor reads the frame drops that
/// budget causes as congestion, so the encoder bitrate walks down to its minimum and
/// stays there. Openscreen: `SenderImpl::OnReceiverReport`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderReportEcho {
    /// [`status_report_id`] of the last Sender Report heard.
    pub report_id: u32,
    /// Time elapsed between hearing that report and sending this feedback, in units of
    /// 1/65536 second (RFC 3550's DLSR).
    pub delay: u32,
}

/// Everything the receiver knows that the sender needs to hear.
#[derive(Debug, Clone)]
pub struct Feedback {
    /// Our SSRC.
    pub receiver_ssrc: u32,
    /// The sender's SSRC.
    pub sender_ssrc: u32,
    /// Every frame up to and including this one is fully received. Truncated to 8
    /// bits on the wire — a design flaw openscreen documents and cannot fix without
    /// breaking millions of deployed senders, and the reason Cast bounds frames in
    /// flight.
    pub checkpoint: FrameId,
    /// The current end-to-end playout delay. Must be non-zero: the field is a `u16`
    /// of milliseconds and zero is not a legal delay.
    pub playout_delay_ms: u16,
    /// Packets to retransmit, sorted by frame then packet id.
    pub nacks: Vec<PacketNack>,
    /// Complete frames *after* the checkpoint, sorted ascending.
    pub acks: Vec<FrameId>,
    /// A wrap-around counter of feedback messages sent.
    pub feedback_count: u8,
    /// Set when the video is undecodable and only a fresh key frame will fix it.
    pub picture_loss: bool,
    /// The NTP timestamp to report, if the caller has a clock reading to offer.
    /// Optional because the pure builder has no clock of its own (ground rule 3).
    pub ntp_timestamp: Option<u64>,
    /// The last Sender Report heard, echoed so the sender can measure the round trip.
    /// `None` until one arrives (the sender sends its first within a frame or two).
    pub last_sender_report: Option<SenderReportEcho>,
}

/// How much of the feedback fitted into the packet.
///
/// A datagram has a hard size limit, so NACKs and ACKs can be dropped. Silently
/// truncating would make a lossy link look like a healthy one, so the builder reports
/// what it left out and the caller can log or re-send it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildReport {
    /// Loss fields written. One field can cover up to nine packets of one frame.
    pub loss_fields_written: usize,
    /// NACKs that did not fit and were dropped.
    pub nacks_dropped: usize,
    /// ACKs that did not fit and were dropped.
    pub acks_dropped: usize,
}

/// Serialize the compound RTCP packet.
///
/// `max_size` bounds the result — pass the path MTU. The mandatory parts (receiver
/// report, feedback header) are always written; the optional NACK and ACK lists are
/// trimmed to fit and reported in [`BuildReport`].
#[must_use]
pub fn build(feedback: &Feedback, max_size: usize) -> (Vec<u8>, BuildReport) {
    let mut out = Vec::with_capacity(max_size.min(1500));

    // 1. Receiver Report. RFC 3550 requires a receiver's compound packet to begin
    //    with one. Once a Sender Report has been heard it carries one report block
    //    echoing it — that echo is what lets the sender measure the round trip (see
    //    [`SenderReportEcho`]). Before then it is empty.
    match &feedback.last_sender_report {
        Some(echo) => {
            append_header(&mut out, PT_RECEIVER_REPORT, 1, 4 + 24);
            out.extend_from_slice(&feedback.receiver_ssrc.to_be_bytes());
            out.extend_from_slice(&feedback.sender_ssrc.to_be_bytes());
            // Loss, extended-highest-sequence and jitter are zero: the one consumer of
            // this block (openscreen's `SenderImpl::OnReceiverReport`) reads only the
            // two report-id fields, and loss is already reported precisely by the NACK
            // fields below. Wiring real numbers here means tracking RTP sequence
            // numbers per packet, which nothing yet needs.
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.extend_from_slice(&echo.report_id.to_be_bytes());
            out.extend_from_slice(&echo.delay.to_be_bytes());
        }
        None => {
            append_header(&mut out, PT_RECEIVER_REPORT, 0, 4);
            out.extend_from_slice(&feedback.receiver_ssrc.to_be_bytes());
        }
    }

    // 2. Receiver Reference Time Report, when the caller gave us a clock reading.
    //    Optional per the Cast spec, but it is what lets the sender estimate the
    //    network's round trip, so senders behave better when it is present.
    if let Some(ntp) = feedback.ntp_timestamp {
        append_header(&mut out, PT_EXTENDED_REPORTS, 0, 4 + 4 + 8);
        out.extend_from_slice(&feedback.receiver_ssrc.to_be_bytes());
        out.push(RECEIVER_REFERENCE_TIME_BLOCK_TYPE);
        out.push(0); // reserved
        out.extend_from_slice(&2u16.to_be_bytes()); // block length, in 32-bit words
        out.extend_from_slice(&ntp.to_be_bytes());
    }

    // 3. Picture Loss Indicator, only while the picture is actually broken.
    if feedback.picture_loss {
        append_header(
            &mut out,
            PT_PAYLOAD_SPECIFIC,
            SUBTYPE_PICTURE_LOSS_INDICATOR,
            8,
        );
        out.extend_from_slice(&feedback.receiver_ssrc.to_be_bytes());
        out.extend_from_slice(&feedback.sender_ssrc.to_be_bytes());
    }

    // 4. Cast Feedback. The header's length field is not known until the loss and ACK
    //    fields are written, so reserve it and patch it afterwards.
    let header_at = out.len();
    out.extend_from_slice(&[0; 4]);
    let body_at = out.len();

    out.extend_from_slice(&feedback.receiver_ssrc.to_be_bytes());
    out.extend_from_slice(&feedback.sender_ssrc.to_be_bytes());
    out.extend_from_slice(&CAST_IDENTIFIER);
    out.push(feedback.checkpoint.lower_8_bits());
    let loss_count_at = out.len();
    out.push(0); // patched below
    out.extend_from_slice(&feedback.playout_delay_ms.to_be_bytes());

    let (loss_fields_written, nacks_dropped) = append_loss_fields(&mut out, feedback, max_size);
    if let Some(slot) = out.get_mut(loss_count_at) {
        // `append_loss_fields` caps at MAX_LOSS_FIELDS, so this cannot truncate.
        *slot = u8::try_from(loss_fields_written).unwrap_or(u8::MAX);
    }

    let acks_dropped = append_ack_fields(&mut out, feedback, max_size);

    let body_len = out.len() - body_at;
    patch_header(
        &mut out,
        header_at,
        PT_PAYLOAD_SPECIFIC,
        SUBTYPE_FEEDBACK,
        body_len,
    );

    (
        out,
        BuildReport {
            loss_fields_written,
            nacks_dropped,
            acks_dropped,
        },
    )
}

/// Write a 4-byte RTCP common header. `payload_size` excludes the header itself and
/// must be a multiple of four — RTCP counts length in 32-bit words.
fn append_header(out: &mut Vec<u8>, packet_type: u8, count_or_subtype: u8, payload_size: usize) {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    patch_header(out, at, packet_type, count_or_subtype, payload_size);
}

fn patch_header(
    out: &mut [u8],
    at: usize,
    packet_type: u8,
    count_or_subtype: u8,
    payload_size: usize,
) {
    let words = u16::try_from(payload_size / 4).unwrap_or(u16::MAX);
    let bytes = [
        (REQUIRED_VERSION_AND_PADDING << REPORT_COUNT_BITS) | count_or_subtype,
        packet_type,
        words.to_be_bytes()[0],
        words.to_be_bytes()[1],
    ];
    if let Some(slot) = out.get_mut(at..at + 4) {
        slot.copy_from_slice(&bytes);
    }
}

/// Write the packet-loss fields, coalescing runs of missing packets within a frame.
///
/// One field names a frame, a first packet id, and a bit vector covering the next
/// eight packet ids — so up to nine losses in one frame cost four bytes. NACKs at or
/// before the checkpoint are skipped: the sender already knows those frames arrived.
fn append_loss_fields(out: &mut Vec<u8>, feedback: &Feedback, max_size: usize) -> (usize, usize) {
    let relevant: Vec<&PacketNack> = feedback
        .nacks
        .iter()
        .filter(|nack| nack.frame_id > feedback.checkpoint)
        .collect();

    let mut written = 0usize;
    let mut index = 0usize;
    while index < relevant.len() {
        // Reserve room for the CST2 header and its minimum bit vector, so a flood of
        // NACKs cannot crowd out the ACKs entirely.
        let reserved = 6 + MIN_ACK_BITVECTOR_OCTETS;
        if written == MAX_LOSS_FIELDS || out.len() + LOSS_FIELD_SIZE + reserved > max_size {
            break;
        }

        let frame_id = relevant[index].frame_id;
        let first = relevant[index].target;
        let first_value = first.wire_value();
        index += 1;

        // Fold the following NACKs for the same frame into the bit vector, as long as
        // they land within the eight ids after `first`.
        let mut bit_vector = 0u8;
        while index < relevant.len() && relevant[index].frame_id == frame_id {
            let NackTarget::Packet(id) = relevant[index].target else {
                // AllPackets for a frame we are already naming packet-by-packet is
                // contradictory; leave it for its own field rather than fold it in.
                break;
            };
            let Some(shift) = id.get().checked_sub(first_value).map(u32::from) else {
                break;
            };
            if shift == 0 || shift > 8 {
                break;
            }
            bit_vector |= 1u8 << (shift - 1);
            index += 1;
        }

        out.push(frame_id.lower_8_bits());
        out.extend_from_slice(&first_value.to_be_bytes());
        out.push(bit_vector);
        written += 1;
    }

    (written, relevant.len() - index)
}

/// Write the CST2 frame-ACK bit vector.
///
/// Bit 0 of the first octet is `checkpoint + 2` — "plus two" because a frame at
/// `checkpoint + 1` being complete would have moved the checkpoint itself.
/// Returns how many ACKs did not fit.
fn append_ack_fields(out: &mut Vec<u8>, feedback: &Feedback, max_size: usize) -> usize {
    if out.len() + 6 + MIN_ACK_BITVECTOR_OCTETS > max_size {
        return feedback.acks.len();
    }

    let first_frame = feedback.checkpoint.value() + 2;
    // Alignment rules: start at two octets and grow four at a time, so the whole RTCP
    // packet stays 4-byte aligned.
    let mut octets = vec![0u8; MIN_ACK_BITVECTOR_OCTETS];
    let mut dropped = 0usize;
    let available = max_size.saturating_sub(out.len() + 6);

    for frame_id in &feedback.acks {
        let bit_index = frame_id.value() - first_frame;
        if bit_index < 0 {
            continue; // Already covered by the checkpoint.
        }
        let Ok(bit_index) = usize::try_from(bit_index) else {
            dropped += 1;
            continue;
        };
        let octet_index = bit_index / 8;
        if octet_index >= octets.len() {
            let needed = octet_index + 1;
            // Round up to the next legal size: 2, then 6, 10, 14, ... (2 + 4n).
            let grown = MIN_ACK_BITVECTOR_OCTETS
                + needed.saturating_sub(MIN_ACK_BITVECTOR_OCTETS).div_ceil(4) * 4;
            if grown > MAX_ACK_BITVECTOR_OCTETS || grown > available {
                dropped += 1;
                continue;
            }
            octets.resize(grown, 0);
        }
        if let Some(octet) = octets.get_mut(octet_index) {
            *octet |= 1u8 << (bit_index % 8);
        }
    }

    out.extend_from_slice(&CST2_IDENTIFIER);
    out.push(feedback.feedback_count);
    // Bounded by MAX_ACK_BITVECTOR_OCTETS above.
    out.push(u8::try_from(octets.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(&octets);
    dropped
}

/// Whether a datagram on the shared media socket is RTCP rather than RTP.
///
/// RFC 5761's demultiplexing rule: RTCP packet types occupy 192..=223, a range RTP's
/// marker-bit-plus-payload-type byte avoids (Cast payload types are 96/97, which read
/// as 96/97 or 224/225 there).
#[must_use]
pub fn is_rtcp(datagram: &[u8]) -> bool {
    matches!(datagram, [first, second, ..]
        if *first >> 6 == 0b10 && (192..=223).contains(second))
}

/// A Sender Report's identifying fields (RFC 3550 §6.4.1), as Cast senders emit them.
///
/// The NTP/RTP pair is the stream's lip-sync anchor; the NTP timestamp doubles as the
/// report's identity, which our Receiver Report echoes back (see [`SenderReportEcho`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderReport {
    /// The sender's SSRC — which stream this report describes.
    pub sender_ssrc: u32,
    /// The sender's clock at the moment of the report, as a 64-bit NTP timestamp.
    pub ntp_timestamp: u64,
    /// The same moment on the stream's RTP clock.
    pub rtp_timestamp: u32,
}

/// Find the Sender Report in a compound RTCP datagram, if it carries one.
///
/// Walks the compound structure the way RFC 3550 prescribes — each packet's length
/// field names the next — and gives up on the first malformed header rather than
/// resynchronize on garbage.
#[must_use]
pub fn find_sender_report(datagram: &[u8]) -> Option<SenderReport> {
    let mut rest = datagram;
    loop {
        let header = rest.get(..4)?;
        if header[0] >> 6 != 0b10 {
            return None;
        }
        let words = usize::from(u16::from_be_bytes([header[2], header[3]]));
        let body = rest.get(4..4 + words * 4)?;
        if header[1] == PT_SENDER_REPORT {
            return Some(SenderReport {
                sender_ssrc: u32::from_be_bytes(body.get(0..4)?.try_into().ok()?),
                ntp_timestamp: u64::from_be_bytes(body.get(4..12)?.try_into().ok()?),
                rtp_timestamp: u32::from_be_bytes(body.get(12..16)?.try_into().ok()?),
            });
        }
        rest = rest.get(4 + words * 4..)?;
    }
}

/// The 32-bit id under which a Sender Report is later named: the middle 32 bits of its
/// NTP timestamp (openscreen's `ToStatusReportId`).
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "the truncation is the definition: the id is the middle 32 of 64 bits"
)]
pub const fn status_report_id(ntp_timestamp: u64) -> u32 {
    (ntp_timestamp >> 16) as u32
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::rtp::PacketId;

    fn nack(frame: i64, packet: u16) -> PacketNack {
        PacketNack {
            frame_id: FrameId::new(frame),
            target: NackTarget::Packet(PacketId::new(packet).unwrap()),
        }
    }

    fn base() -> Feedback {
        Feedback {
            receiver_ssrc: 0x1111_2222,
            sender_ssrc: 0x3333_4444,
            checkpoint: FrameId::new(10),
            playout_delay_ms: 400,
            nacks: Vec::new(),
            acks: Vec::new(),
            feedback_count: 7,
            picture_loss: false,
            ntp_timestamp: None,
            last_sender_report: None,
        }
    }

    /// Every RTCP packet must be a whole number of 32-bit words, or receivers walking
    /// the compound packet will desynchronize partway through.
    fn assert_word_aligned(packet: &[u8]) {
        assert_eq!(packet.len() % 4, 0, "packet is {} bytes", packet.len());
    }

    /// Walk the compound packet the way a sender does, returning (type, subtype, body).
    fn walk(packet: &[u8]) -> Vec<(u8, u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while at + 4 <= packet.len() {
            let count_or_subtype = packet[at] & 0b0001_1111;
            assert_eq!(packet[at] >> 5, REQUIRED_VERSION_AND_PADDING);
            let packet_type = packet[at + 1];
            let words = usize::from(u16::from_be_bytes([packet[at + 2], packet[at + 3]]));
            let body = packet[at + 4..at + 4 + words * 4].to_vec();
            out.push((packet_type, count_or_subtype, body));
            at += 4 + words * 4;
        }
        assert_eq!(
            at,
            packet.len(),
            "length fields must cover the packet exactly"
        );
        out
    }

    #[test]
    fn minimal_feedback_is_well_formed() {
        let (packet, report) = build(&base(), 1472);
        assert_word_aligned(&packet);
        let parts = walk(&packet);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].0, PT_RECEIVER_REPORT);
        assert_eq!(parts[1].0, PT_PAYLOAD_SPECIFIC);
        assert_eq!(parts[1].1, SUBTYPE_FEEDBACK);
        assert_eq!(report.nacks_dropped, 0);

        let body = &parts[1].2;
        assert_eq!(&body[8..12], b"CAST");
        assert_eq!(body[12], 10); // checkpoint, truncated to 8 bits
        assert_eq!(body[13], 0); // no loss fields
        assert_eq!(u16::from_be_bytes([body[14], body[15]]), 400);
        assert_eq!(&body[16..20], b"CST2");
    }

    /// The report block layout is what openscreen's `RtcpReportBlock::ParseOne` walks:
    /// 24 bytes, sender SSRC first, report id and delay last. The sender computes its
    /// round-trip time from those last two fields, so their position is load-bearing.
    #[test]
    fn a_heard_sender_report_is_echoed_in_a_report_block() {
        let mut fb = base();
        fb.last_sender_report = Some(SenderReportEcho {
            report_id: 0xA1B2_C3D4,
            delay: 0x0001_8000, // 1.5 s in 1/65536ths
        });
        let (packet, _) = build(&fb, 1472);
        assert_word_aligned(&packet);
        let parts = walk(&packet);
        assert_eq!(parts[0].0, PT_RECEIVER_REPORT);
        assert_eq!(parts[0].1, 1, "report count must say one block");
        let body = &parts[0].2;
        assert_eq!(body.len(), 4 + 24);
        assert_eq!(&body[0..4], &0x1111_2222u32.to_be_bytes()); // our ssrc
        assert_eq!(&body[4..8], &0x3333_4444u32.to_be_bytes()); // the block names the sender
        assert_eq!(&body[20..24], &0xA1B2_C3D4u32.to_be_bytes());
        assert_eq!(&body[24..28], &0x0001_8000u32.to_be_bytes());
    }

    #[test]
    fn rtcp_is_told_apart_from_rtp_by_the_second_byte() {
        // A Cast RTP packet: version 2, payload type 96 (or 224 with the marker bit).
        assert!(!is_rtcp(&[0x80, 96, 0, 0, 0, 0, 0, 0]));
        assert!(!is_rtcp(&[0x80, 224, 0, 0, 0, 0, 0, 0]));
        // A sender report and a receiver report.
        assert!(is_rtcp(&[0x80, 200, 0, 0]));
        assert!(is_rtcp(&[0x81, 201, 0, 0]));
        // Wrong version bits: not RTP or RTCP at all.
        assert!(!is_rtcp(&[0x40, 200, 0, 0]));
        assert!(!is_rtcp(&[]));
    }

    /// A sender report as openscreen's `SenderReportBuilder` lays it out, preceded by
    /// another RTCP packet so the compound walk is exercised.
    #[test]
    fn the_sender_report_is_found_inside_a_compound_packet() {
        let mut packet = Vec::new();
        // A leading receiver report (senders do not send these, but the walk must not
        // care what comes first).
        packet.extend_from_slice(&[0x80, 201, 0, 1]);
        packet.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        // The sender report: SSRC, 8-byte NTP, RTP timestamp, packet + octet counts.
        packet.extend_from_slice(&[0x80, 200, 0, 6]);
        packet.extend_from_slice(&0x0102_0304u32.to_be_bytes());
        packet.extend_from_slice(&0x0192_A3B4_C5D6_E7F8u64.to_be_bytes());
        packet.extend_from_slice(&0x0055_AA55u32.to_be_bytes());
        packet.extend_from_slice(&42u32.to_be_bytes());
        packet.extend_from_slice(&4200u32.to_be_bytes());

        let report = find_sender_report(&packet).unwrap();
        assert_eq!(report.sender_ssrc, 0x0102_0304);
        assert_eq!(report.ntp_timestamp, 0x0192_A3B4_C5D6_E7F8);
        assert_eq!(report.rtp_timestamp, 0x0055_AA55);

        // Pinned to openscreen's own static_assert for ToStatusReportId.
        assert_eq!(status_report_id(report.ntp_timestamp), 0xA3B4_C5D6);
    }

    #[test]
    fn a_truncated_or_alien_datagram_yields_no_sender_report() {
        // Length field runs past the end.
        assert_eq!(find_sender_report(&[0x80, 200, 0, 6, 0, 0]), None);
        // Not RTCP at all.
        assert_eq!(find_sender_report(b"GET / HTTP/1.1"), None);
        // A compound packet with no sender report in it.
        let (packet, _) = build(&base(), 1472);
        assert_eq!(find_sender_report(&packet), None);
    }

    #[test]
    fn reference_time_report_is_included_only_with_a_clock_reading() {
        let mut fb = base();
        fb.ntp_timestamp = Some(0xAABB_CCDD_1122_3344);
        let (packet, _) = build(&fb, 1472);
        assert_word_aligned(&packet);
        let parts = walk(&packet);
        assert_eq!(parts[1].0, PT_EXTENDED_REPORTS);
        assert_eq!(parts[1].2[4], RECEIVER_REFERENCE_TIME_BLOCK_TYPE);
        assert_eq!(&parts[1].2[8..16], &0xAABB_CCDD_1122_3344u64.to_be_bytes());
    }

    #[test]
    fn consecutive_losses_in_one_frame_collapse_into_one_field() {
        let mut fb = base();
        // Frame 11, packets 3, 4, 5 missing: one field, bits 0 and 1 set.
        fb.nacks = vec![nack(11, 3), nack(11, 4), nack(11, 5)];
        let (packet, report) = build(&fb, 1472);
        assert_word_aligned(&packet);
        assert_eq!(report.loss_fields_written, 1);
        assert_eq!(report.nacks_dropped, 0);

        let parts = walk(&packet);
        let body = &parts[1].2;
        assert_eq!(body[13], 1); // loss field count
        assert_eq!(body[16], 11); // frame id
        assert_eq!(u16::from_be_bytes([body[17], body[18]]), 3); // first packet id
        assert_eq!(body[19], 0b0000_0011); // packets 4 and 5
    }

    #[test]
    fn a_gap_wider_than_the_bit_vector_starts_a_new_field() {
        let mut fb = base();
        // Packet 3, then 20 — 20 is past the eight ids the bit vector can reach.
        fb.nacks = vec![nack(11, 3), nack(11, 20)];
        let (_, report) = build(&fb, 1472);
        assert_eq!(report.loss_fields_written, 2);
        assert_eq!(report.nacks_dropped, 0);
    }

    #[test]
    fn nacks_at_or_before_the_checkpoint_are_not_sent() {
        let mut fb = base();
        // The checkpoint means frames <= 10 are known-complete; asking for them again
        // would make the sender retransmit data we already have.
        fb.nacks = vec![nack(9, 0), nack(10, 1), nack(11, 2)];
        let (_, report) = build(&fb, 1472);
        assert_eq!(report.loss_fields_written, 1);
    }

    #[test]
    fn whole_frame_loss_uses_the_reserved_id() {
        let mut fb = base();
        fb.nacks = vec![PacketNack {
            frame_id: FrameId::new(12),
            target: NackTarget::AllPackets,
        }];
        let (packet, _) = build(&fb, 1472);
        let parts = walk(&packet);
        let body = &parts[1].2;
        assert_eq!(body[16], 12);
        assert_eq!(u16::from_be_bytes([body[17], body[18]]), 0xffff);
        assert_eq!(body[19], 0);
    }

    #[test]
    fn ack_bit_vector_starts_two_past_the_checkpoint() {
        let mut fb = base();
        // checkpoint 10, so bit 0 is frame 12.
        fb.acks = vec![FrameId::new(12), FrameId::new(15)];
        let (packet, report) = build(&fb, 1472);
        assert_word_aligned(&packet);
        assert_eq!(report.acks_dropped, 0);

        let parts = walk(&packet);
        let body = &parts[1].2;
        let cst2 = body.windows(4).position(|w| w == b"CST2").unwrap();
        assert_eq!(body[cst2 + 4], 7); // feedback count
        assert_eq!(body[cst2 + 5], 2); // octet count
                                       // Frame 12 is bit 0, frame 15 is bit 3.
        assert_eq!(body[cst2 + 6], 0b0000_1001);
    }

    #[test]
    fn the_ack_vector_grows_in_legal_steps() {
        let mut fb = base();
        // Frame 40 is bit 28, which needs four octets — but the vector may only be
        // 2, 6, 10, ... octets long, so it must round up to 6.
        fb.acks = vec![FrameId::new(40)];
        let (packet, report) = build(&fb, 1472);
        assert_word_aligned(&packet);
        assert_eq!(report.acks_dropped, 0);
        let parts = walk(&packet);
        let body = &parts[1].2;
        let cst2 = body.windows(4).position(|w| w == b"CST2").unwrap();
        assert_eq!(body[cst2 + 5], 6);
    }

    #[test]
    fn a_small_mtu_drops_nacks_and_says_so() {
        let mut fb = base();
        fb.nacks = (0..200).map(|i| nack(11 + i64::from(i), 0)).collect();
        // Enough for the mandatory parts, not for 200 loss fields.
        let (packet, report) = build(&fb, 128);
        assert!(packet.len() <= 128, "packet was {} bytes", packet.len());
        assert_word_aligned(&packet);
        assert!(
            report.nacks_dropped > 0,
            "should have reported the shortfall"
        );
        assert_eq!(
            report.loss_fields_written + report.nacks_dropped,
            200,
            "every NACK is either written or counted as dropped"
        );
    }
}
