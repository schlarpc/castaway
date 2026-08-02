//! The RAOP timing exchange and the resend policy — both pure.
//!
//! ## Who asks whom
//!
//! This surprises people: **the receiver is the NTP client**. We send timing requests to
//! the sender's timing port and it replies; the sender never probes us. So the cadence
//! is ours to choose, and so is what to do with the answers.
//!
//! ## What the offset is worth
//!
//! The sender's epoch is arbitrary — UxPlay's own comment on the matter is that iOS
//! sends "seconds relative to an arbitrary Epoch (the last boot)" and then adds the
//! 1900→1970 constant on top. So the absolute offset is meaningless and only its *rate
//! of change* carries information. That is why nothing here tries to set a clock; it
//! estimates an offset so a sync packet's anchor can be converted into local time, and
//! reports drift so a future resampler has something to work with.
//!
//! The filter is UxPlay's rather than shairport-sync's: keep the last few samples and
//! take the offset from the one with the **lowest round-trip delay**. On a LAN the
//! minimum-delay sample is the one that queued least in both directions, so it carries
//! the least error. shairport-sync's least-squares fit over 128 samples estimates drift
//! in ppm as well, which matters for multi-room; for one screen it is machinery without
//! a customer.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Duration;

/// The instant both planes of one session measure from.
///
/// A mirroring session is two streams that must land on one timeline, and until this
/// existed each picked its own origin at its own first frame — so even with perfectly
/// correct timestamps the relationship between them was discarded before anything
/// downstream could use it, and audio sat wherever it happened to start relative to
/// video.
///
/// Whichever plane produces a frame first sets the origin; the other measures from the
/// same point. Both must already be in the **sender's** nanosecond domain — the video
/// header is there natively, and audio gets there through a sync anchor.
#[derive(Debug, Default)]
pub struct StreamOrigin(OnceLock<u64>);

impl StreamOrigin {
    /// A fresh, unanchored origin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Convert a sender-clock timestamp into a presentation time for this session.
    ///
    /// `saturating_sub` because a frame from *before* the origin is possible when the
    /// two planes start within a packet of each other — it presents at zero rather than
    /// wrapping to seventy years.
    pub fn pts(&self, sender_ns: u64) -> Duration {
        let origin = *self.0.get_or_init(|| sender_ns);
        Duration::from_nanos(sender_ns.saturating_sub(origin))
    }

    /// Whether anything has anchored it yet.
    #[must_use]
    pub fn is_anchored(&self) -> bool {
        self.0.get().is_some()
    }
}

/// What a sync packet establishes: this RTP frame number was that instant on the
/// sender's clock.
///
/// The one thing that lets audio be placed on the same timeline as video. Audio counts
/// frames and video counts nanoseconds, and nothing else in the protocol relates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncAnchor {
    /// The RTP timestamp the anchor refers to.
    pub rtp: u32,
    /// The sender's clock at that instant, in nanoseconds since *its* boot — the same
    /// domain the mirroring header uses, with the 1900 epoch already removed.
    pub sender_ns: u64,
}

impl SyncAnchor {
    /// Where an RTP timestamp falls on the sender's clock.
    ///
    /// RTP timestamps wrap at 2^32 — about 27 hours at 44.1 kHz — so the difference is
    /// read as *signed* serial arithmetic. Comparing instead would put every frame after
    /// a wrap a day and a half in the past.
    #[must_use]
    pub fn sender_ns_of(&self, rtp: u32, sample_rate: u32) -> u64 {
        let raw = rtp.wrapping_sub(self.rtp);
        let delta_frames = if raw > u32::MAX / 2 {
            -i64::from(u32::MAX - raw) - 1
        } else {
            i64::from(raw)
        };
        let rate = i64::from(sample_rate.max(1));
        let delta_ns = delta_frames.saturating_mul(1_000_000_000) / rate;
        let base = i64::try_from(self.sender_ns).unwrap_or(i64::MAX);
        u64::try_from(base.saturating_add(delta_ns).max(0)).unwrap_or(0)
    }
}

/// Payload type for a timing request (we send these).
const PT_TIMING_REQUEST: u8 = 82;

/// The fixed sequence number every implementation puts in a timing request.
const TIMING_SEQNO: u16 = 7;

/// How many timing samples to keep. UxPlay keeps 8; more only helps a drift fit we do
/// not do.
const TIMING_WINDOW: usize = 8;

/// Seconds between 1900 (the NTP epoch) and 1970 (the Unix one).
pub const SECONDS_1900_TO_1970: u64 = 2_208_988_800;

/// A 64-bit NTP timestamp: 32 bits of seconds, 32 of fraction.
///
/// A newtype because the same 8 bytes appear in this protocol with *two* different
/// epochs — the timing channel adds the 1900→1970 constant and the mirroring header does
/// not — and mixing them up yields a 70-year offset that looks like a hang.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct NtpTime(u64);

impl NtpTime {
    /// From the raw 64-bit wire value.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw wire value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// From nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn from_unix_nanos(nanos: u64) -> Self {
        let secs = nanos / 1_000_000_000 + SECONDS_1900_TO_1970;
        let frac = ((nanos % 1_000_000_000) << 32) / 1_000_000_000;
        Self((secs << 32) | (frac & 0xFFFF_FFFF))
    }

    /// As nanoseconds, on whatever epoch the value was built with.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        let secs = self.0 >> 32;
        let frac = self.0 & 0xFFFF_FFFF;
        secs.wrapping_mul(1_000_000_000)
            .wrapping_add((frac * 1_000_000_000) >> 32)
    }
}

/// One completed timing round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingSample {
    /// `remote - local`, in nanoseconds. Signed, and usually enormous (see the module
    /// docs on the sender's epoch).
    pub offset_ns: i64,
    /// The round-trip delay, in nanoseconds. Lower is a better sample.
    pub delay_ns: u64,
}

/// The receiver's side of the timing exchange.
#[derive(Debug, Default)]
pub struct TimingClient {
    /// The last reply's transmit time, echoed into the next request.
    last_remote_transmit: NtpTime,
    /// When that reply arrived locally.
    last_local_receive: NtpTime,
    /// When we last transmitted, so a reply can be matched to it.
    last_local_transmit: NtpTime,
    samples: VecDeque<TimingSample>,
    /// How many requests we have sent, which decides the cadence.
    sent: u32,
}

/// Requests are fast at first so the offset settles quickly, then back off.
///
/// UxPlay probes every 3 s from the start; shairport-sync does three at 300 ms and then
/// every 3 s, which converges in under a second instead of nine. That matters because
/// nothing can be converted to local time until at least one round trip has completed.
const FAST_PROBES: u32 = 3;
/// Interval for the first few probes, in milliseconds.
pub const FAST_INTERVAL_MS: u64 = 300;
/// Steady-state interval, in milliseconds.
pub const STEADY_INTERVAL_MS: u64 = 3_000;

impl TimingClient {
    /// A fresh client with no samples.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How long to wait before sending the next request.
    #[must_use]
    pub const fn next_interval_ms(&self) -> u64 {
        if self.sent < FAST_PROBES {
            FAST_INTERVAL_MS
        } else {
            STEADY_INTERVAL_MS
        }
    }

    /// Build the 32-byte timing request to send now.
    pub fn build_request(&mut self, now: NtpTime) -> [u8; 32] {
        let mut p = [0u8; 32];
        p[0] = 0x80;
        // The marker bit is set on every timing request in every capture.
        p[1] = 0x80 | PT_TIMING_REQUEST;
        p[2..4].copy_from_slice(&TIMING_SEQNO.to_be_bytes());
        // Bytes 4..8 stay zero.
        p[8..16].copy_from_slice(&self.last_remote_transmit.raw().to_be_bytes());
        p[16..24].copy_from_slice(&self.last_local_receive.raw().to_be_bytes());
        p[24..32].copy_from_slice(&now.raw().to_be_bytes());
        self.last_local_transmit = now;
        self.sent = self.sent.saturating_add(1);
        p
    }

    /// Fold in a 32-byte timing reply that arrived at `arrival`.
    ///
    /// Returns the sample it produced, or `None` if the reply refers to a request we did
    /// not send.
    pub fn on_reply(&mut self, reply: &[u8; 32], arrival: NtpTime) -> Option<TimingSample> {
        let field = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&reply[i..i + 8]);
            NtpTime::from_raw(u64::from_be_bytes(b))
        };
        // t0: our transmit, echoed. t1: the sender received it. t2: the sender replied.
        let (t0, t1, t2) = (field(8), field(16), field(24));
        if t0 != self.last_local_transmit {
            // A reply to a request we have already superseded. Dropping it is better
            // than folding in a round trip we cannot time.
            return None;
        }
        let t3 = arrival;

        // Plain NTP. Everything is done in i128 because the two clocks share no epoch
        // and the difference routinely exceeds an i64's nanosecond range on the way in.
        let (t0, t1, t2, t3) = (
            i128::from(t0.as_nanos()),
            i128::from(t1.as_nanos()),
            i128::from(t2.as_nanos()),
            i128::from(t3.as_nanos()),
        );
        let offset = ((t1 - t0) + (t2 - t3)) / 2;
        let delay = (t3 - t0) - (t2 - t1);

        self.last_remote_transmit = field(24);
        self.last_local_receive = arrival;

        let sample = TimingSample {
            offset_ns: i64::try_from(offset).unwrap_or(i64::MAX),
            // A negative delay means the two clocks moved between the readings; treat it
            // as zero rather than discarding a sample we can still use.
            delay_ns: u64::try_from(delay.max(0)).unwrap_or(u64::MAX),
        };
        if self.samples.len() == TIMING_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        Some(sample)
    }

    /// The best offset estimate: the one from the lowest-delay sample.
    #[must_use]
    pub fn offset_ns(&self) -> Option<i64> {
        self.samples
            .iter()
            .min_by_key(|s| s.delay_ns)
            .map(|s| s.offset_ns)
    }

    /// Whether any round trip has completed.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        !self.samples.is_empty()
    }

    /// Convert a sender timestamp into our local clock.
    #[must_use]
    pub fn remote_to_local_ns(&self, remote_ns: u64) -> Option<u64> {
        let offset = self.offset_ns()?;
        let local = i128::from(remote_ns) - i128::from(offset);
        u64::try_from(local.max(0)).ok()
    }
}

/// A run of packets to ask the sender to send again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResendRequest {
    /// First missing sequence number.
    pub first: u16,
    /// How many consecutive packets are missing.
    pub count: u16,
}

/// Decides when a gap in sequence numbers is worth asking about.
///
/// Deliberately simple. RAOP sequence numbers wrap at 2^16 — about 8 minutes of audio at
/// 44.1 kHz — so "is this ahead or behind" is serial arithmetic, not comparison, and a
/// tracker that got that wrong would ask for a resend of every packet after each wrap.
#[derive(Debug, Default)]
pub struct ResendTracker {
    next_expected: Option<u16>,
}

/// A gap larger than this is a stream restart rather than loss, and asking for it would
/// flood the sender with requests it cannot satisfy.
const MAX_GAP: u16 = 128;

impl ResendTracker {
    /// A tracker that has seen nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Note a packet's arrival; returns a resend request if it revealed a gap.
    pub fn on_packet(&mut self, sequence: u16) -> Option<ResendRequest> {
        let Some(expected) = self.next_expected else {
            self.next_expected = Some(sequence.wrapping_add(1));
            return None;
        };

        // Serial arithmetic: how far ahead of what we wanted is this?
        let ahead = sequence.wrapping_sub(expected);
        if ahead == 0 {
            self.next_expected = Some(sequence.wrapping_add(1));
            return None;
        }
        // A packet from the past is a retransmit that arrived, or a duplicate. Either
        // way it fills no new gap and must not move the expectation backwards.
        if ahead > u16::MAX / 2 {
            return None;
        }
        self.next_expected = Some(sequence.wrapping_add(1));
        if ahead > MAX_GAP {
            return None;
        }
        Some(ResendRequest {
            first: expected,
            count: ahead,
        })
    }

    /// Re-seed at where a `FLUSH` said the stream restarts.
    ///
    /// `Some(seq)` is the sequence number the new position starts at, from `RTP-Info`, so
    /// the gap the sender made on purpose is not read as loss *and* real loss of the
    /// first packets after it still is. Forgetting entirely — which is all this used to
    /// do — buys the first half only: the tracker re-seeds from whatever turns up, so if
    /// the first post-flush packets are the ones that go missing, nothing asks for them.
    /// `None` for a sender that sent no `seq`, which is the old behaviour.
    pub fn reset(&mut self, seq: Option<u16>) {
        self.next_expected = seq;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn an_origin_is_set_by_whichever_plane_speaks_first() {
        let o = StreamOrigin::new();
        assert!(!o.is_anchored());
        // Video, say, at 50 s of sender uptime.
        assert_eq!(o.pts(50_000_000_000), Duration::ZERO);
        assert!(o.is_anchored());
        // Audio 16 ms later measures from the *same* point, which is the whole purpose.
        assert_eq!(o.pts(50_016_000_000), Duration::from_millis(16));
    }

    #[test]
    fn a_frame_from_before_the_origin_presents_at_zero() {
        // Reachable when the two planes start within a packet of each other. Without the
        // saturation this wraps to seventy years and the frame never presents.
        let o = StreamOrigin::new();
        o.pts(1_000_000_000);
        assert_eq!(o.pts(999_000_000), Duration::ZERO);
    }

    #[test]
    fn a_sync_anchor_places_rtp_on_the_senders_clock() {
        // The only thing in the protocol that relates a frame counter to nanoseconds.
        let a = SyncAnchor {
            rtp: 1_000_000,
            sender_ns: 50_000_000_000,
        };
        assert_eq!(a.sender_ns_of(1_000_000, 44_100), 50_000_000_000);
        // 44100 frames later is one second later.
        assert_eq!(a.sender_ns_of(1_044_100, 44_100), 51_000_000_000);
        // …and 44100 earlier is one second before.
        assert_eq!(a.sender_ns_of(955_900, 44_100), 49_000_000_000);
    }

    #[test]
    fn an_rtp_wrap_does_not_throw_the_anchor_a_day_and_a_half_out() {
        // RTP wraps every ~27 hours at 44.1 kHz. Read as unsigned, a frame just after
        // the wrap looks like 2^32 frames in the future.
        let a = SyncAnchor {
            rtp: u32::MAX - 44_099,
            sender_ns: 50_000_000_000,
        };
        // 44100 frames on, having wrapped through zero: exactly one second later.
        assert_eq!(a.sender_ns_of(0, 44_100), 51_000_000_000);
    }

    #[test]
    fn ntp_round_trips_through_nanoseconds() {
        let t = NtpTime::from_unix_nanos(1_700_000_000_500_000_000);
        // Within one fractional tick (about 0.23 ns), which is the encoding's own limit.
        let back = t.as_nanos();
        let expected = 1_700_000_000_500_000_000 + SECONDS_1900_TO_1970 * 1_000_000_000;
        assert!(back.abs_diff(expected) < 2, "{back} vs {expected}");
    }

    #[test]
    fn a_request_has_the_shape_every_sender_expects() {
        let mut c = TimingClient::new();
        let p = c.build_request(NtpTime::from_raw(0x1111_2222_3333_4444));
        assert_eq!(p[0], 0x80);
        assert_eq!(p[1], 0xD2, "marker bit set, payload type 82");
        assert_eq!(&p[2..4], &7u16.to_be_bytes());
        // The first request echoes nothing, because there is nothing yet.
        assert_eq!(&p[8..16], &[0u8; 8]);
        assert_eq!(&p[24..32], &0x1111_2222_3333_4444u64.to_be_bytes());
    }

    /// A reply to `request`, with the sender's clock `offset_ns` ahead of ours.
    fn reply_to(request: &[u8; 32], sender_receive: u64, sender_transmit: u64) -> [u8; 32] {
        let mut r = [0u8; 32];
        r[0] = 0x80;
        r[1] = 0x80 | 83;
        r[8..16].copy_from_slice(&request[24..32]); // echo our transmit
        r[16..24].copy_from_slice(&NtpTime::from_unix_nanos(sender_receive).raw().to_be_bytes());
        r[24..32].copy_from_slice(
            &NtpTime::from_unix_nanos(sender_transmit)
                .raw()
                .to_be_bytes(),
        );
        r
    }

    #[test]
    fn a_round_trip_yields_the_offset_and_delay() {
        // Our clock: send at t=0, receive at t=100ms. The sender is exactly 10 s ahead
        // and takes 20 ms to turn the request around.
        let mut c = TimingClient::new();
        let sent_at = 1_000_000_000u64;
        let req = c.build_request(NtpTime::from_unix_nanos(sent_at));
        let sample = c
            .on_reply(
                &reply_to(
                    &req,
                    sent_at + 10_000_000_000 + 40_000_000,
                    sent_at + 10_000_000_000 + 60_000_000,
                ),
                NtpTime::from_unix_nanos(sent_at + 100_000_000),
            )
            .unwrap();
        // offset ≈ 10 s, delay ≈ 100 ms - 20 ms = 80 ms. Milliseconds of slack for the
        // fixed-point conversion.
        assert!(
            (sample.offset_ns - 10_000_000_000).abs() < 2_000_000,
            "offset {}",
            sample.offset_ns
        );
        assert!(
            sample.delay_ns.abs_diff(80_000_000) < 2_000_000,
            "delay {}",
            sample.delay_ns
        );
        assert!(c.is_settled());
    }

    #[test]
    fn the_lowest_delay_sample_wins() {
        // The whole filter: a sample that queued behind other traffic carries more error,
        // so the quietest round trip is the one to believe.
        let mut c = TimingClient::new();
        for (i, rtt_ms) in [200u64, 8, 150].into_iter().enumerate() {
            let sent = 1_000_000_000 + (i as u64) * 1_000_000_000;
            let req = c.build_request(NtpTime::from_unix_nanos(sent));
            // Each reply claims a slightly different offset; the fast one says 10 s.
            let claimed_offset = if rtt_ms == 8 {
                10_000_000_000
            } else {
                12_000_000_000
            };
            let mid = sent + claimed_offset + rtt_ms * 500_000;
            c.on_reply(
                &reply_to(&req, mid, mid),
                NtpTime::from_unix_nanos(sent + rtt_ms * 1_000_000),
            )
            .unwrap();
        }
        let offset = c.offset_ns().unwrap();
        assert!(
            (offset - 10_000_000_000).abs() < 5_000_000,
            "the 8 ms sample should win, got {offset}"
        );
    }

    #[test]
    fn a_reply_to_a_superseded_request_is_ignored() {
        // Folding in a round trip we cannot time would corrupt the estimate with a
        // delay measured against the wrong transmission.
        let mut c = TimingClient::new();
        let stale = c.build_request(NtpTime::from_unix_nanos(1_000_000_000));
        let _fresh = c.build_request(NtpTime::from_unix_nanos(2_000_000_000));
        assert!(c
            .on_reply(
                &reply_to(&stale, 1_100_000_000, 1_100_000_000),
                NtpTime::from_unix_nanos(2_100_000_000)
            )
            .is_none());
    }

    #[test]
    fn probes_are_fast_until_the_offset_settles_then_back_off() {
        let mut c = TimingClient::new();
        assert_eq!(c.next_interval_ms(), FAST_INTERVAL_MS);
        for _ in 0..FAST_PROBES {
            c.build_request(NtpTime::from_raw(0));
        }
        assert_eq!(c.next_interval_ms(), STEADY_INTERVAL_MS);
    }

    #[test]
    fn nothing_converts_before_a_round_trip_completes() {
        let c = TimingClient::new();
        assert!(!c.is_settled());
        assert!(c.offset_ns().is_none());
        assert!(c.remote_to_local_ns(123).is_none());
    }

    // --- resend ---

    #[test]
    fn an_unbroken_run_asks_for_nothing() {
        let mut t = ResendTracker::new();
        for seq in 100..110 {
            assert_eq!(t.on_packet(seq), None);
        }
    }

    #[test]
    fn a_gap_asks_for_exactly_the_missing_run() {
        let mut t = ResendTracker::new();
        t.on_packet(100);
        assert_eq!(
            t.on_packet(104),
            Some(ResendRequest {
                first: 101,
                count: 3
            })
        );
        // And the next in-order packet is quiet again.
        assert_eq!(t.on_packet(105), None);
    }

    #[test]
    fn a_wrap_is_not_mistaken_for_a_gap_of_sixty_five_thousand() {
        // Sequence numbers wrap every ~8 minutes of audio. Comparing rather than doing
        // serial arithmetic would ask for a resend of the entire number space.
        let mut t = ResendTracker::new();
        t.on_packet(u16::MAX);
        assert_eq!(t.on_packet(0), None);
        assert_eq!(t.on_packet(1), None);
    }

    #[test]
    fn a_retransmit_arriving_late_does_not_rewind_the_expectation() {
        let mut t = ResendTracker::new();
        t.on_packet(100);
        t.on_packet(104); // asks for 101..104
                          // 102 turns up afterwards. It fills a gap we already asked about, and must not
                          // make us ask for 103..105 next.
        assert_eq!(t.on_packet(102), None);
        assert_eq!(t.on_packet(105), None);
    }

    #[test]
    fn an_implausible_gap_is_a_restart_rather_than_loss() {
        let mut t = ResendTracker::new();
        t.on_packet(100);
        assert_eq!(
            t.on_packet(5_000),
            None,
            "asking for 4900 packets helps nobody"
        );
    }

    #[test]
    fn a_flush_forgets_where_the_stream_was() {
        let mut t = ResendTracker::new();
        t.on_packet(100);
        t.reset(None);
        assert_eq!(t.on_packet(9_000), None, "a seek is not a gap");
    }

    #[test]
    fn a_flush_that_names_its_sequence_still_notices_loss_right_after_it() {
        // The half that forgetting cannot do. `RTP-Info: seq=` says where the new
        // position starts, so the deliberate gap is not loss — but if the first packets
        // of the *new* stream go missing, that is loss and worth asking about. A tracker
        // that re-seeds from whatever turns up cannot tell the two apart.
        let mut t = ResendTracker::new();
        t.on_packet(100);
        t.reset(Some(9_000));
        assert_eq!(t.on_packet(9_000), None, "the packet the flush pointed at");

        let mut t = ResendTracker::new();
        t.on_packet(100);
        t.reset(Some(9_000));
        assert_eq!(
            t.on_packet(9_003),
            Some(ResendRequest {
                first: 9_000,
                count: 3
            }),
            "three packets of the new position never arrived"
        );
    }
}
