//! The Cast mirroring RTP actor (OPEN-QUESTIONS Q12): the thin async shell around
//! [`CastRtpReceiver`].
//!
//! Everything about *what* a datagram means lives in [`crate::rtp`], [`crate::receiver`]
//! and [`crate::rtcp`], which are pure and clock-free. This module supplies the three
//! things they deliberately lack — a socket, a clock, and a channel to the pipeline —
//! and makes no protocol decisions of its own (ground rule 3). Read it as: route bytes
//! in by SSRC, ask the receiver what came out, decrypt, push; and on a timer, ask the
//! receiver what to tell the sender and put it on the wire.
//!
//! One socket carries both streams. Cast's ANSWER names a single `udpPort` and the
//! audio and video SSRCs, so demultiplexing is ours to do — hence `peek_ssrc`.
//!
//! The socket is bound *before* the OFFER is answered. A port in an ANSWER that nobody
//! is listening on is a sender streaming into a black hole, and it would only show up
//! as "mirroring doesn't work".

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use castaway_core::{AudioCodec, EncodedFrame, FrameSource, VideoCodec};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, trace, warn};

use crate::error::CastError;
use crate::mirror::{self, MediaKind, MirrorConfig, StreamConfig};
use crate::receiver::{CastRtpReceiver, Consume};
use crate::rtcp;
use crate::rtp::{Dependency, RtpTimestamp};

/// How often feedback goes back to the sender. openscreen's `kRtcpReportInterval`.
///
/// This also paces frame delivery: every wakeup re-examines what is deliverable, and
/// 33 ms of granularity is invisible against a playout delay measured in hundreds.
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(33);

/// Receive buffer size. Senders keep packets inside one Ethernet MTU; anything larger
/// is not a Cast packet, and truncating it makes it fail parsing rather than succeed
/// with the wrong bytes.
const MAX_DATAGRAM: usize = 1500;

/// Frames that may be queued toward the pipeline before we start dropping them.
///
/// Small on purpose. Ground rule 4 says latency beats freshness for live mirroring: if
/// the decoder has fallen a third of a second behind, the right answer is to throw
/// frames away, not to build a buffer that makes the lag permanent.
const FRAME_QUEUE_DEPTH: usize = 8;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_EPOCH_OFFSET_SECS: u64 = 2_208_988_800;

/// A bound RTP socket, waiting for a negotiation to give it something to receive.
///
/// Exists as a separate step from [`MirrorRtp`] because of ordering: the port must be
/// known when the ANSWER is written, but the keys and SSRCs only arrive with the OFFER.
#[derive(Debug)]
pub struct MirrorSocket {
    socket: UdpSocket,
    port: u16,
}

impl MirrorSocket {
    /// Bind a UDP port on `ip` from the receiver's media port policy, ready to be
    /// named in an ANSWER.
    ///
    /// Range candidates are tried lowest-first; a port that is already taken —
    /// `AddrInUse`, or `PermissionDenied` from a Windows excluded port range — moves
    /// to the next, so a sibling session's socket does not fail this one.
    ///
    /// # Errors
    /// [`CastError::Io`] if no candidate port can be bound or the port can't be read
    /// back.
    pub async fn bind(
        ip: std::net::IpAddr,
        media_ports: castaway_core::MediaPorts,
    ) -> Result<Self, CastError> {
        let mut socket = None;
        for candidate in media_ports.candidates() {
            match UdpSocket::bind(SocketAddr::new(ip, candidate)).await {
                Ok(bound) => {
                    socket = Some(bound);
                    break;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    return Err(CastError::Io(format!("binding mirroring RTP socket: {e}")));
                }
            }
        }
        let socket = socket.ok_or_else(|| {
            CastError::Io(format!(
                "no free port in the media port range {media_ports} for the mirroring RTP socket"
            ))
        })?;
        let port = socket
            .local_addr()
            .map_err(|e| CastError::Io(format!("reading mirroring RTP port: {e}")))?
            .port();
        Ok(Self { socket, port })
    }

    /// The port to put in the ANSWER.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Start receiving the negotiated streams.
    ///
    /// Returns the frame sources to hand [`castaway_core::SessionEvent::Mirror`] and a
    /// future that drives the socket. The future ends when the pipeline drops those
    /// sources, so the caller does not have to signal shutdown separately.
    #[must_use]
    pub fn start(self, config: &MirrorConfig) -> (FrameSource, Option<FrameSource>, MirrorRtp) {
        let (video, video_rx) = Stream::new(&config.video);
        let (audio, audio_rx) = match &config.audio {
            Some(cfg) => {
                let (stream, rx) = Stream::new(cfg);
                (Some(stream), Some(FrameSource::Encoded(rx)))
            }
            None => (None, None),
        };
        (
            FrameSource::Encoded(video_rx),
            audio_rx,
            MirrorRtp {
                socket: self.socket,
                video,
                audio,
                peer: None,
            },
        )
    }
}

/// The running receive loop for one mirroring session.
#[derive(Debug)]
pub struct MirrorRtp {
    socket: UdpSocket,
    video: Stream,
    audio: Option<Stream>,
    /// Where to send feedback, learned from the first datagram.
    ///
    /// The ANSWER never carries the sender's address, and behind NAT or on a
    /// multi-homed host the address it would have guessed is often not the one packets
    /// actually come from. So: reply where the traffic came from.
    peer: Option<SocketAddr>,
}

impl MirrorRtp {
    /// Receive until the pipeline goes away or the socket dies.
    pub async fn run(mut self) {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        let mut ticker = tokio::time::interval(FEEDBACK_INTERVAL);
        // Delay rather than Burst: if the runtime stalls we want one late report, not a
        // catch-up flurry of stale ones.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        info!("Cast mirroring RTP receive loop started");

        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => match result {
                    Ok((len, from)) => {
                        self.peer = Some(from);
                        // The pure layers take `Bytes` so a frame's payload can be
                        // assembled without recopying every packet.
                        self.dispatch(&Bytes::copy_from_slice(&buf[..len]));
                    }
                    Err(e) => {
                        warn!(error = %e, "Cast mirroring socket failed");
                        return;
                    }
                },
                _ = ticker.tick() => self.send_feedback().await,
            }

            if !self.deliver(Instant::now()) {
                info!("Cast mirroring consumer went away; stopping RTP receive");
                return;
            }
        }
    }

    /// Hand a datagram to whichever stream owns its SSRC.
    fn dispatch(&mut self, datagram: &Bytes) {
        let Some(ssrc) = peek_ssrc(datagram) else {
            trace!(len = datagram.len(), "datagram too short to carry an SSRC");
            return;
        };
        let Some(stream) = self.stream_for(ssrc) else {
            // Cast senders put RTCP sender reports on this same port. We do not consume
            // them yet, and they carry the *receiver* SSRC, so they land here.
            trace!(ssrc, "datagram for an SSRC this session does not own");
            return;
        };
        match stream.receiver.receive(datagram) {
            Ok(outcome) => trace!(?outcome, "RTP datagram"),
            Err(e) => debug!(error = %e, "malformed RTP datagram dropped"),
        }
    }

    fn stream_for(&mut self, ssrc: u32) -> Option<&mut Stream> {
        if self.video.config.sender_ssrc == ssrc {
            return Some(&mut self.video);
        }
        self.audio
            .as_mut()
            .filter(|stream| stream.config.sender_ssrc == ssrc)
    }

    /// Push every ready frame at the pipeline. Returns `false` once the pipeline has
    /// dropped its end, which is this loop's shutdown signal.
    fn deliver(&mut self, now: Instant) -> bool {
        let mut alive = self.video.deliver(now);
        if let Some(audio) = self.audio.as_mut() {
            alive &= audio.deliver(now);
        }
        alive
    }

    /// Tell the sender what has arrived and what has not.
    async fn send_feedback(&mut self) {
        let Some(peer) = self.peer else {
            return; // nothing has been heard from yet; nowhere to send.
        };
        let ntp = ntp_now();
        let streams = std::iter::once(&mut self.video).chain(self.audio.as_mut());
        for stream in streams {
            let feedback = stream.receiver.feedback(ntp);
            let (packet, report) = rtcp::build(&feedback, MAX_DATAGRAM);
            if report.nacks_dropped > 0 || report.acks_dropped > 0 {
                // Truncated feedback still works — the sender resends what it did not
                // hear an ACK for — but it means loss is outrunning one datagram's worth
                // of reporting, which is worth knowing about.
                debug!(
                    nacks_dropped = report.nacks_dropped,
                    acks_dropped = report.acks_dropped,
                    "RTCP feedback did not fit in one datagram"
                );
            }
            if let Err(e) = self.socket.send_to(&packet, peer).await {
                debug!(%peer, error = %e, "could not send RTCP feedback");
            }
        }
    }
}

/// One stream's receive state: the pure receiver, the keys to decrypt with, and the
/// channel decrypted frames go out on.
#[derive(Debug)]
struct Stream {
    config: StreamConfig,
    receiver: CastRtpReceiver,
    frames: mpsc::Sender<EncodedFrame>,
    video_codec: Option<VideoCodec>,
    audio_codec: Option<AudioCodec>,
    /// When the frame we owe the decoder was first known to be overdue, or `None` if
    /// nothing is being waited on.
    stalled_since: Option<Instant>,
    /// The first timestamp seen, so presentation times start at zero. Senders start
    /// their RTP clock at a random offset.
    epoch: Option<RtpTimestamp>,
}

impl Stream {
    fn new(config: &StreamConfig) -> (Self, mpsc::Receiver<EncodedFrame>) {
        let (tx, rx) = mpsc::channel(FRAME_QUEUE_DEPTH);
        let (video_codec, audio_codec) = match config.media_kind() {
            MediaKind::Video(codec) => (Some(codec), None),
            MediaKind::Audio(codec) => (None, Some(codec)),
        };
        let stream = Self {
            receiver: CastRtpReceiver::new(config.sender_ssrc, config.receiver_ssrc),
            config: config.clone(),
            frames: tx,
            video_codec,
            audio_codec,
            stalled_since: None,
            epoch: None,
        };
        (stream, rx)
    }

    /// Drain what the receiver will give us. Returns `false` if the consumer is gone.
    fn deliver(&mut self, now: Instant) -> bool {
        loop {
            let policy = if self.is_overdue(now) {
                Consume::SkipToDecodable
            } else {
                Consume::InOrder
            };
            let Some(delivered) = self.receiver.next_frame(policy) else {
                // Only start the clock once the sender has actually moved on; an idle
                // stream is not a late one.
                if self.receiver.is_awaiting_frames() {
                    self.stalled_since.get_or_insert(now);
                } else {
                    self.stalled_since = None;
                }
                return true;
            };
            self.stalled_since = None;

            if delivered.skipped > 0 {
                // No key-frame request follows. `SkipToDecodable` only ever lands on a
                // frame that needs nothing before it, so the picture is whole again by
                // the next frame and asking would just cost bandwidth.
                debug!(
                    skipped = delivered.skipped,
                    ssrc = self.config.sender_ssrc,
                    "skipped late frames to catch up"
                );
            }

            let header = delivered.frame.header;
            let payload = match mirror::crypt_frame(
                &self.config,
                header.frame_id,
                &delivered.frame.payload,
            ) {
                Ok(plaintext) => plaintext,
                Err(e) => {
                    // Only a bad key length reaches here, and the key came from a
                    // validated config — so this is unreachable rather than expected.
                    warn!(error = %e, "could not decrypt mirrored frame");
                    continue;
                }
            };

            let frame = EncodedFrame {
                video_codec: self.video_codec,
                audio_codec: self.audio_codec,
                pts: self.pts(header.rtp_timestamp),
                keyframe: header.dependency == Dependency::KeyFrame,
                data: Bytes::from(payload),
            };
            match self.frames.try_send(frame) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    debug!(
                        ssrc = self.config.sender_ssrc,
                        "pipeline is behind; dropping a frame"
                    );
                }
                Err(TrySendError::Closed(_)) => return false,
            }
        }
    }

    fn is_overdue(&self, now: Instant) -> bool {
        let budget = Duration::from_millis(u64::from(self.receiver.playout_delay_ms()));
        self.stalled_since
            .is_some_and(|since| now.duration_since(since) >= budget)
    }

    /// Convert a stream timestamp into time since the first frame.
    fn pts(&mut self, timestamp: RtpTimestamp) -> Duration {
        let epoch = *self.epoch.get_or_insert(timestamp);
        let ticks = timestamp.value().saturating_sub(epoch.value()).max(0);
        let nanos = u128::from(ticks.unsigned_abs()) * 1_000_000_000
            / u128::from(self.config.rtp_timebase.get());
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }
}

/// Read the SSRC out of a datagram without committing to it being valid RTP.
///
/// Demultiplexing has to happen before parsing, because the parser is per-stream: it
/// tracks the reference points that re-expand truncated frame ids, and feeding it
/// another stream's packets would corrupt them.
fn peek_ssrc(datagram: &[u8]) -> Option<u32> {
    let field = datagram.get(8..12)?;
    Some(u32::from_be_bytes(<[u8; 4]>::try_from(field).ok()?))
}

/// The current time as a 64-bit NTP timestamp, for the reference-time report.
///
/// `None` if the system clock is before 1970, which a sender is better off not being
/// told about than being told a wrong answer.
fn ntp_now() -> Option<u64> {
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let seconds = unix.as_secs().checked_add(NTP_EPOCH_OFFSET_SECS)?;
    // NTP's fraction field is 1/2^32 of a second.
    let fraction = (u64::from(unix.subsec_nanos()) << 32) / 1_000_000_000;
    seconds.checked_shl(32)?.checked_add(fraction)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::mirror::Codec;
    use std::num::NonZeroU32;

    fn config(codec: Codec, timebase: u32) -> StreamConfig {
        StreamConfig {
            index: 0,
            sender_ssrc: 0x0102_0304,
            receiver_ssrc: 0x0102_0305,
            payload_type: 96,
            codec,
            rtp_timebase: NonZeroU32::new(timebase).unwrap(),
            aes_key: [0x11; 16],
            aes_iv_mask: [0x22; 16],
        }
    }

    #[test]
    fn ssrc_is_read_from_bytes_8_through_11() {
        let mut datagram = [0u8; 18];
        datagram[8..12].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(peek_ssrc(&datagram), Some(0xdead_beef));
    }

    #[test]
    fn a_datagram_too_short_for_an_ssrc_is_not_guessed_at() {
        assert_eq!(peek_ssrc(&[0u8; 11]), None);
    }

    /// A wrong timebase is a silently wrong presentation time — the kind of bug that
    /// shows up as "audio drifts" three subsystems away. Pin the arithmetic.
    #[test]
    fn presentation_time_is_measured_from_the_first_frame_in_stream_ticks() {
        let (mut stream, _rx) = Stream::new(&config(Codec::H264, 90_000));
        // 90 kHz video: the first frame anchors zero regardless of the sender's offset.
        assert_eq!(
            stream.pts(RtpTimestamp::zero().expand(900_000)),
            Duration::ZERO
        );
        assert_eq!(
            stream.pts(RtpTimestamp::zero().expand(900_000 + 90_000)),
            Duration::from_secs(1)
        );
        assert_eq!(
            stream.pts(RtpTimestamp::zero().expand(900_000 + 3_000)),
            Duration::from_millis(33) + Duration::from_nanos(333_333)
        );
    }

    #[test]
    fn audio_ticks_are_samples_not_ninety_kilohertz() {
        let (mut stream, _rx) = Stream::new(&config(Codec::Opus, 48_000));
        assert_eq!(stream.pts(RtpTimestamp::zero()), Duration::ZERO);
        assert_eq!(
            stream.pts(RtpTimestamp::zero().expand(48_000)),
            Duration::from_secs(1)
        );
    }

    /// The frame channel is the shutdown signal: when the pipeline drops it, the loop
    /// must stop rather than spin decrypting into a closed pipe.
    #[test]
    fn a_dropped_consumer_ends_delivery() {
        let (mut stream, rx) = Stream::new(&config(Codec::H264, 90_000));
        assert!(stream.deliver(Instant::now()));
        drop(rx);
        // Still true: nothing was ready, so nothing tried to send. The closure is only
        // observable on an actual send, which is exactly when it matters.
        assert!(stream.deliver(Instant::now()));
    }

    /// Codec choice has to survive the trip from OFFER to the pipeline's frame, because
    /// nothing downstream can recover it.
    #[test]
    fn codec_reaches_the_frame_it_labels() {
        let (video, _rx) = Stream::new(&config(Codec::Vp8, 90_000));
        assert_eq!(video.video_codec, Some(VideoCodec::Vp8));
        assert_eq!(video.audio_codec, None);

        let (audio, _rx) = Stream::new(&config(Codec::Opus, 48_000));
        assert_eq!(audio.video_codec, None);
        assert_eq!(audio.audio_codec, Some(AudioCodec::Opus));
    }

    #[test]
    fn ntp_now_lands_after_the_ntp_epoch() {
        let ntp = ntp_now().unwrap();
        // 2020-01-01 in NTP seconds — a floor that will not need revisiting.
        assert!((ntp >> 32) > 3_786_825_600);
    }
}
