//! The per-connection protocol-v4 session: packet numbering, opcode leniency,
//! typed error answers, per-session progress cadence, and the heartbeat —
//! everything that happens *after* the in-place TLS upgrade (#248).
//!
//! Pure, like [`crate::session`]: the actor reads the clock and the sockets;
//! this sees frames and `now`. The behavioural contract is the reference
//! receiver's, pinned three ways — the study notes, the captured transcripts in
//! `tests/fixtures/sdk-0.3.0-v4-*.jsonl`, and FUTO's own `fast` conformance
//! driver, whose hostile cases (unknown opcode answered not dropped, wrong
//! direction answered `InvalidPayloadType`, garbage flatbuffers fatal) are the
//! tests here.
//!
//! One deliberate divergence: the reference forgets to count packets whose
//! opcode byte it does not know (its `packet_num` drifts from the sender's own
//! numbering forever after). We count every packet, as the spec says to.

use std::time::Duration;

use fcast_flatbuf::flat;

use crate::error::FCastError;
use crate::session::{DEAD_AFTER, PING_AFTER};
use crate::v4msg::{self, DeviceInfo, Parsed, QueuePosition, V4Inbound};
use crate::wire::{Frame, Opcode, RawFrame};

/// The default per-session progress cadence, until `SetProgressUpdateInterval`
/// changes it. The reference's `DEFAULT_PROGRESS_INTERVAL`.
pub const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// What a v4 sender asked the receiver to do.
#[derive(Debug, Clone, PartialEq)]
pub enum V4Command {
    /// Load new content.
    Load {
        /// What to load.
        source: v4msg::LoadSource,
        /// The raw packet, for the stripped relay to other senders.
        raw: Vec<u8>,
    },
    /// Seek to an absolute position.
    Seek(Duration),
    /// Set the volume (already clamped; a clamp also produced an error reply).
    SetVolume(f64),
    /// Set the playback speed factor (already validated finite and non-zero).
    SetSpeed(f64),
    /// Pause.
    Pause,
    /// Resume.
    Resume,
    /// Stop and unload.
    Stop,
    /// Insert a queue item.
    QueueInsert {
        /// The item.
        item: v4msg::V4MediaItem,
        /// Its on-screen duration.
        playback_duration: Option<Duration>,
        /// Where.
        position: QueuePosition,
        /// The raw packet, for the stripped relay.
        raw: Vec<u8>,
    },
    /// Remove a queue item.
    QueueRemove(QueuePosition),
    /// Jump to a queue item.
    QueueSelect(QueuePosition),
    /// The sender wants to serve `fcomp://` resources.
    CompanionHello,
    /// A sender answered "what is this resource" for a read we issued (#249).
    CompanionInfo {
        /// The read this answers.
        request_id: u32,
        /// The MIME type the sender declared.
        content_type: String,
        /// Its size, when the sender knows it.
        size: Option<u64>,
    },
    /// One `Resource` packet: part of a range we asked for (#249).
    CompanionData(crate::companion::ResourcePart),
    /// Begin a mirroring session.
    StartMirroring(u16),
    /// The sender's SDP offer for the active mirroring session.
    MirroringOffer {
        /// The session it belongs to.
        session_id: u16,
        /// The offer.
        sdp: String,
    },
}

/// The session's reaction to one inbound packet.
#[derive(Debug, Default)]
pub struct V4Reaction {
    /// Frames to write back to this connection.
    pub replies: Vec<Frame>,
    /// The player-facing command, if any.
    pub command: Option<V4Command>,
}

/// One v4 connection's protocol state.
#[derive(Debug)]
pub struct SessionV4 {
    /// Inbound packet counter. The plaintext `Version` was packet 0, counted by
    /// the actor before this session existed; construction seeds accordingly.
    packet_num: u32,
    peer: Option<DeviceInfo>,
    progress_interval: Duration,
    companion_provider: Option<u16>,
    mirroring_session: Option<u16>,
    last_rx: Duration,
    ping_outstanding: bool,
}

impl SessionV4 {
    /// A session whose TLS handshake just completed. `packets_before` is how
    /// many plaintext packets the actor already counted (one: the `Version`).
    #[must_use]
    pub fn new(packets_before: u32, now: Duration) -> Self {
        Self {
            packet_num: packets_before,
            peer: None,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            companion_provider: None,
            mirroring_session: None,
            last_rx: now,
            ping_outstanding: false,
        }
    }

    /// Who the sender said it was, once it has.
    #[must_use]
    pub const fn peer(&self) -> Option<&DeviceInfo> {
        self.peer.as_ref()
    }

    /// This session's progress cadence.
    #[must_use]
    pub const fn progress_interval(&self) -> Duration {
        self.progress_interval
    }

    /// The companion provider id assigned to this connection, if any.
    #[must_use]
    pub const fn companion_provider(&self) -> Option<u16> {
        self.companion_provider
    }

    /// Record the provider id the adapter assigned on `CompanionHello`.
    pub const fn set_companion_provider(&mut self, id: u16) {
        self.companion_provider = Some(id);
    }

    /// The active mirroring session id, if any.
    #[must_use]
    pub const fn mirroring_session(&self) -> Option<u16> {
        self.mirroring_session
    }

    /// Handle one raw inbound packet.
    ///
    /// # Errors
    /// Only session-fatal faults: a garbage flatbuffer, a missing union member.
    /// Everything else — unknown opcodes included — is *answered*, which is the
    /// leniency v4 has that the JSON sessions do not.
    pub fn on_frame(&mut self, now: Duration, raw: &RawFrame) -> Result<V4Reaction, FCastError> {
        let packet_num = self.packet_num;
        // Count every packet, unknown opcodes included — the spec's numbering,
        // not the reference's drifting one.
        self.packet_num = self.packet_num.wrapping_add(1);
        self.last_rx = now;
        self.ping_outstanding = false;

        let Ok(opcode) = Opcode::from_wire(raw.opcode) else {
            return Ok(reply(v4msg::error_frame(
                flat::ErrorKind::InvalidOpcode,
                Some(packet_num),
            )));
        };
        match opcode {
            Opcode::Ping => Ok(reply(Frame::bare(Opcode::Pong))),
            Opcode::Pong => Ok(V4Reaction::default()),
            Opcode::Flatbuf => {
                if raw.body.is_empty() {
                    // A Flatbuf with no body cannot even be verified; the
                    // reference quits the session, and so do we.
                    return Err(FCastError::MissingBody(Opcode::Flatbuf));
                }
                self.on_flatbuf(packet_num, &raw.body)
            }
            // FCompanion resource bytes (#249). Parsed here and routed by request
            // id at the actor; a part for a read nobody is waiting on is dropped
            // there, as the reference drops an unsolicited response.
            Opcode::Resource => Ok(command(V4Command::CompanionData(
                crate::companion::parse_resource(&raw.body)?,
            ))),
            // The whole JSON opcode table, at v4, is a polite typed refusal —
            // including the `SetPlaylistItem` the real SDK verifiably leaks
            // (fixtures, `v4-set-playlist-item`).
            _ => Ok(reply(v4msg::error_frame(
                flat::ErrorKind::InvalidOpcode,
                Some(packet_num),
            ))),
        }
    }

    fn on_flatbuf(&mut self, packet_num: u32, body: &[u8]) -> Result<V4Reaction, FCastError> {
        let message = match v4msg::parse_flatbuf(body)? {
            Parsed::Message(message) => message,
            Parsed::Reply(kind) => {
                return Ok(reply(v4msg::error_frame(kind, Some(packet_num))));
            }
        };
        Ok(match message {
            V4Inbound::SenderIntroduction(info) => {
                self.peer = Some(info);
                V4Reaction::default()
            }
            V4Inbound::Load { source, raw } => command(V4Command::Load { source, raw }),
            V4Inbound::ProgressChanged { position } => command(V4Command::Seek(position)),
            V4Inbound::VolumeChanged(volume) => {
                // NaN is not a volume; out-of-range saturates. Both get the
                // typed error *and* the clamped command — the reference's rule,
                // held by fast's volume_clamped cases.
                let clamped = if volume.is_nan() {
                    0.0
                } else {
                    volume.clamp(0.0, 1.0)
                };
                let mut reaction = command(V4Command::SetVolume(f64::from(clamped)));
                if clamped != volume || volume.is_nan() {
                    reaction.replies.push(v4msg::error_frame(
                        flat::ErrorKind::VolumeOutOfRange,
                        Some(packet_num),
                    ));
                }
                reaction
            }
            V4Inbound::SpeedChanged(speed) => {
                if speed.is_finite() && speed != 0.0 {
                    command(V4Command::SetSpeed(f64::from(speed)))
                } else {
                    // Non-finite or zero: rate forced to 1.0 plus the error,
                    // exactly the reference.
                    let mut reaction = command(V4Command::SetSpeed(1.0));
                    reaction.replies.push(v4msg::error_frame(
                        flat::ErrorKind::RateOutOfRange,
                        Some(packet_num),
                    ));
                    reaction
                }
            }
            V4Inbound::PlaybackStateChanged(state) => match state {
                flat::PlaybackState::Paused => command(V4Command::Pause),
                flat::PlaybackState::Playing => command(V4Command::Resume),
                // Idle/Buffering/Ended are receiver-reported states; a sender
                // requesting one is confused.
                _ => reply(v4msg::error_frame(
                    flat::ErrorKind::InvalidState,
                    Some(packet_num),
                )),
            },
            V4Inbound::StopPlayback => command(V4Command::Stop),
            V4Inbound::QueueInsert {
                item,
                playback_duration,
                position,
                raw,
            } => command(V4Command::QueueInsert {
                item,
                playback_duration,
                position,
                raw,
            }),
            V4Inbound::QueueRemove(position) => command(V4Command::QueueRemove(position)),
            V4Inbound::QueueItemSelected(position) => command(V4Command::QueueSelect(position)),
            V4Inbound::SetProgressUpdateInterval(interval) => {
                self.progress_interval = interval;
                V4Reaction::default()
            }
            V4Inbound::CompanionHelloRequest => command(V4Command::CompanionHello),
            V4Inbound::StartMirroringSession(session_id) => {
                self.mirroring_session = Some(session_id);
                command(V4Command::StartMirroring(session_id))
            }
            V4Inbound::MirroringSessionDescription { session_id, sdp } => {
                if self.mirroring_session == Some(session_id) {
                    command(V4Command::MirroringOffer { session_id, sdp })
                } else {
                    // An offer for a session that isn't active: answered, not
                    // fatal — the reference's `InvalidState`.
                    reply(v4msg::error_frame(
                        flat::ErrorKind::InvalidState,
                        Some(packet_num),
                    ))
                }
            }
            // No track model: a concrete id can never name a track; disabling
            // an unrendered track is vacuously done.
            V4Inbound::ChangeTrack { id: Some(_) } => reply(v4msg::error_frame(
                flat::ErrorKind::MalformedBody,
                Some(packet_num),
            )),
            V4Inbound::ChangeTrack { id: None } => V4Reaction::default(),
            // No subtitle rendering (the capabilities said so).
            V4Inbound::AddSubtitleSource => reply(v4msg::error_frame(
                flat::ErrorKind::InvalidState,
                Some(packet_num),
            )),
            V4Inbound::CompanionResourceInfoResponse {
                request_id,
                content_type,
                size,
            } => command(V4Command::CompanionInfo {
                request_id,
                content_type,
                size,
            }),
        })
    }

    /// Heartbeat tick — the same ladder as the JSON sessions, receiver-owned
    /// (the SDK never pings, measured over a 9 s idle window).
    ///
    /// # Errors
    /// [`FCastError::HeartbeatTimeout`] once a `Ping` has gone unanswered past
    /// [`DEAD_AFTER`].
    pub fn on_tick(&mut self, now: Duration) -> Result<Option<Frame>, FCastError> {
        let idle = now.saturating_sub(self.last_rx);
        if self.ping_outstanding {
            if idle >= DEAD_AFTER {
                return Err(FCastError::HeartbeatTimeout);
            }
            return Ok(None);
        }
        if idle >= PING_AFTER {
            self.ping_outstanding = true;
            return Ok(Some(Frame::bare(Opcode::Ping)));
        }
        Ok(None)
    }
}

fn reply(frame: Frame) -> V4Reaction {
    V4Reaction {
        replies: vec![frame],
        command: None,
    }
}

fn command(command: V4Command) -> V4Reaction {
    V4Reaction {
        replies: Vec::new(),
        command: Some(command),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn raw(opcode: u8, body: &[u8]) -> RawFrame {
        RawFrame {
            opcode,
            body: body.to_vec(),
            consumed: 5 + body.len(),
        }
    }

    fn session() -> SessionV4 {
        SessionV4::new(1, Duration::ZERO)
    }

    /// fast's `invalid_opcode_error_v4` / `none_opcode_error_v4` /
    /// `unsupported_opcode_error_v4`: an opcode v4 does not speak — unknown
    /// bytes, opcode 0, and the whole JSON table — is answered with
    /// `Error{InvalidOpcode, packet_num}` and the session lives on.
    #[test]
    fn wrong_opcodes_are_answered_not_fatal() {
        let mut s = session();
        for (n, opcode) in [(0x7f_u8, "unknown"), (0, "None"), (2, "Pause")]
            .iter()
            .zip(1u32..)
            .map(|((op, _), n)| (n, *op))
        {
            let reaction = s
                .on_frame(Duration::from_millis(u64::from(n)), &raw(opcode, b""))
                .unwrap();
            assert_eq!(reaction.replies.len(), 1, "opcode {opcode}");
            let packet = fcast_flatbuf::root_as_packet(&reaction.replies[0].body).unwrap();
            let error = packet.payload_as_error().unwrap();
            assert_eq!(error.kind(), flat::ErrorKind::InvalidOpcode);
            assert_eq!(error.packet_num(), Some(n), "numbering counts every packet");
        }
    }

    /// fast's `garbage_flatbuf_closes_v4` / `truncated_flatbuf_closes_v4`: a
    /// Flatbuf that fails verification is session-fatal, as is an empty one.
    #[test]
    fn garbage_flatbuffers_are_fatal() {
        let mut s = session();
        assert!(matches!(
            s.on_frame(
                Duration::ZERO,
                &raw(20, &[0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4])
            ),
            Err(FCastError::MalformedFlatbuf(_))
        ));
        let mut s = session();
        assert!(matches!(
            s.on_frame(Duration::ZERO, &raw(20, b"")),
            Err(FCastError::MissingBody(Opcode::Flatbuf))
        ));
    }

    /// fast's `volume_clamped_high_v4`/`_low_v4`: out-of-range volume produces
    /// the clamped command *and* the typed error; NaN forces 0.0.
    #[test]
    fn volume_clamps_with_an_error_beside_it() {
        for (sent, want) in [(1.5_f32, 1.0_f64), (-0.5, 0.0), (f32::NAN, 0.0)] {
            let mut s = session();
            let frame = v4msg::volume_changed_frame(sent);
            let reaction = s.on_frame(Duration::ZERO, &raw(20, &frame.body)).unwrap();
            assert_eq!(reaction.command, Some(V4Command::SetVolume(want)));
            let packet = fcast_flatbuf::root_as_packet(&reaction.replies[0].body).unwrap();
            assert_eq!(
                packet.payload_as_error().unwrap().kind(),
                flat::ErrorKind::VolumeOutOfRange
            );
        }
        // In-range volume: no error rides along.
        let mut s = session();
        let frame = v4msg::volume_changed_frame(0.5);
        let reaction = s.on_frame(Duration::ZERO, &raw(20, &frame.body)).unwrap();
        assert!(reaction.replies.is_empty());
    }

    /// fast's `set_speed_extremes_v4`: non-finite or zero speed → rate 1.0 plus
    /// `RateOutOfRange`.
    #[test]
    fn degenerate_speed_forces_one() {
        for sent in [0.0_f32, f32::NAN, f32::INFINITY] {
            let mut s = session();
            let frame = v4msg::speed_changed_frame(sent);
            let reaction = s.on_frame(Duration::ZERO, &raw(20, &frame.body)).unwrap();
            assert_eq!(reaction.command, Some(V4Command::SetSpeed(1.0)));
            let packet = fcast_flatbuf::root_as_packet(&reaction.replies[0].body).unwrap();
            assert_eq!(
                packet.payload_as_error().unwrap().kind(),
                flat::ErrorKind::RateOutOfRange
            );
        }
    }

    /// A mirroring description must name the active session; anything else is
    /// `InvalidState`, answered rather than fatal.
    #[test]
    fn mirroring_offers_are_gated_by_session_id() {
        let mut s = session();
        let offer = v4msg::mirroring_answer_frame(7, "v=0");
        let reaction = s.on_frame(Duration::ZERO, &raw(20, &offer.body)).unwrap();
        let packet = fcast_flatbuf::root_as_packet(&reaction.replies[0].body).unwrap();
        assert_eq!(
            packet.payload_as_error().unwrap().kind(),
            flat::ErrorKind::InvalidState
        );

        // Start session 7, then the same description is the offer.
        let mut b = fcast_flatbuf::FlatBufferBuilder::new();
        let start = flat::StartMirroringSession::create(
            &mut b,
            &flat::StartMirroringSessionArgs { session_id: 7 },
        )
        .as_union_value();
        let packet = flat::Packet::create(
            &mut b,
            &flat::PacketArgs {
                payload_type: flat::Message::StartMirroringSession,
                payload: Some(start),
            },
        );
        b.finish(packet, None);
        let start_body = b.finished_data().to_vec();
        let reaction = s.on_frame(Duration::ZERO, &raw(20, &start_body)).unwrap();
        assert_eq!(reaction.command, Some(V4Command::StartMirroring(7)));
        let reaction = s.on_frame(Duration::ZERO, &raw(20, &offer.body)).unwrap();
        assert!(matches!(
            reaction.command,
            Some(V4Command::MirroringOffer { session_id: 7, .. })
        ));
    }

    /// The heartbeat ladder holds at v4 with the shipped constants.
    #[test]
    fn the_v4_heartbeat_uses_the_shipped_constants() {
        let mut s = session();
        assert_eq!(
            s.on_tick(PING_AFTER - Duration::from_millis(1)).unwrap(),
            None
        );
        assert_eq!(s.on_tick(PING_AFTER).unwrap().unwrap().opcode, Opcode::Ping);
        assert!(matches!(
            s.on_tick(DEAD_AFTER),
            Err(FCastError::HeartbeatTimeout)
        ));
    }

    /// `SetProgressUpdateInterval` is per-session state, already rounded.
    #[test]
    fn the_progress_interval_is_session_state() {
        let mut s = session();
        assert_eq!(s.progress_interval(), DEFAULT_PROGRESS_INTERVAL);
        let mut b = fcast_flatbuf::FlatBufferBuilder::new();
        let interval = flat::Time::new(250_000);
        let msg = flat::SetProgressUpdateInterval::create(
            &mut b,
            &flat::SetProgressUpdateIntervalArgs {
                interval: Some(&interval),
            },
        )
        .as_union_value();
        let packet = flat::Packet::create(
            &mut b,
            &flat::PacketArgs {
                payload_type: flat::Message::SetProgressUpdateInterval,
                payload: Some(msg),
            },
        );
        b.finish(packet, None);
        let body = b.finished_data().to_vec();
        s.on_frame(Duration::ZERO, &raw(20, &body)).unwrap();
        assert_eq!(s.progress_interval(), Duration::from_millis(300));
    }
}
