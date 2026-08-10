//! The per-connection FCast session: version negotiation, opcode legality, heartbeat,
//! event subscriptions, and per-version outbound translation.
//!
//! Pure and synchronous (ground rule 3): the actor reads the clock once at its
//! boundary and passes `now` (monotonic, since connection start) and `wall_ms` (unix
//! milliseconds, for the wire's `generationTime` fields) inward. Nothing here touches
//! a socket or a clock, which is what lets the captured reference-sender transcripts
//! replay against it byte-for-byte (`tests/real_sender_transcripts.rs`).
//!
//! ## Version scope (#241)
//!
//! This receiver implements protocol v1-v3 — the length-prefixed JSON protocol — and
//! answers every hello with `Version {{ version: 3 }}`. Protocol v4 moved to TLS 1.3
//! with FlatBuffers bodies and WebRTC mirroring; per the spec's own negotiation rule
//! the party with the higher version downgrades, and the official sender SDK
//! verifiably does (it runs a v3 session against a v3 receiver). A v4-only peer that
//! upgrades to TLS anyway stops framing as JSON and is disconnected by the first
//! fault — declined, not guessed at, in the D32 tradition. v4 is issue #248.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::error::FCastError;
use crate::messages::{
    json_frame, media_item_event_frame, EventSubscription, EventSubscriptionMessage,
    InitialReceiverMessage, InitialSenderMessage, MediaItemEventKind, PlayMessage, PlayState,
    PlayUpdateMessage, PlaybackErrorMessage, PlaybackUpdateV1, PlaybackUpdateV2, PlaybackUpdateV3,
    SeekMessage, SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage, VersionMessage,
    VolumeUpdateV1, VolumeUpdateV2,
};
use crate::wire::{Frame, Opcode};

/// The protocol version this receiver speaks and advertises.
pub const PROTOCOL_VERSION: u64 = 3;

/// Idle time after which we probe the peer with a `Ping` (v2+ sessions only — v1 has
/// no `Ping` opcode). The reference implementation's policy, stated in the v4 spec's
/// heartbeat section and used by its receiver for every JSON session.
pub const PING_AFTER: Duration = Duration::from_secs(3);

/// Idle time after which an unanswered `Ping` means the connection is dead.
pub const DEAD_AFTER: Duration = Duration::from_secs(6);

/// The negotiated protocol version of one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionVersion {
    /// Protocol v1: play/pause/resume/stop/seek/volume, no heartbeat.
    V1,
    /// Protocol v2: + speed, errors, ping/pong.
    V2,
    /// Protocol v3: + initial handshake, playlists, event subscriptions.
    V3,
}

impl SessionVersion {
    /// The wire version number.
    #[must_use]
    pub const fn number(self) -> u64 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
            Self::V3 => 3,
        }
    }
}

/// Who this receiver is, for the v3 `Initial` handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverIdentity {
    /// The advertised friendly name.
    pub display_name: String,
    /// The application name.
    pub app_name: String,
    /// The application version.
    pub app_version: String,
}

/// Boundary state the session needs to answer a handshake: the clock, who we are,
/// and what is currently loaded. Read once per frame at the actor boundary.
#[derive(Debug, Clone, Copy)]
pub struct SessionContext<'a> {
    /// Unix milliseconds, for `generationTime` fields.
    pub wall_ms: u64,
    /// Who we are.
    pub receiver: &'a ReceiverIdentity,
    /// What is loaded right now, echoed to a v3 sender that joins mid-session.
    pub play_data: Option<&'a PlayMessage>,
    /// Current volume (0.0-1.0), told to v1/v2 senders that have no `Initial` to
    /// carry it — the reference receiver does the same, because volume otherwise
    /// only broadcasts on change.
    pub volume: f64,
}

/// What a sender asked the receiver to do — the session's output toward the player.
#[derive(Debug, Clone, PartialEq)]
pub enum SenderCommand {
    /// Load and play new content. Boxed: a `PlayMessage` dwarfs every other variant.
    Load(Box<PlayMessage>),
    /// Pause playback.
    Pause,
    /// Resume playback.
    Resume,
    /// Stop and unload.
    Stop,
    /// Seek to an absolute position.
    Seek(Duration),
    /// Set volume (already clamped to 0.0-1.0).
    SetVolume(f64),
    /// Set the playback speed factor.
    SetSpeed(f64),
    /// Jump to a playlist item.
    SetPlaylistItem(u64),
}

/// The session's reaction to one inbound frame: frames to write back, and at most
/// one command for the player.
#[derive(Debug, Default, PartialEq)]
pub struct Reaction {
    /// Frames to write to this connection, in order.
    pub replies: Vec<Frame>,
    /// The player-facing command, if the frame carried one.
    pub command: Option<SenderCommand>,
}

impl Reaction {
    fn none() -> Self {
        Self::default()
    }

    fn command(command: SenderCommand) -> Self {
        Self {
            replies: Vec::new(),
            command: Some(command),
        }
    }

    fn reply(frame: Frame) -> Self {
        Self {
            replies: vec![frame],
            command: None,
        }
    }
}

/// A state change on the receiver that sessions may need to hear about, in
/// version-neutral form; [`Session::frame_update`] translates it per session.
///
/// Owned, because one update fans out to every connected session and outlives the
/// player-lock scope that produced it.
#[derive(Debug, Clone, PartialEq)]
pub enum ReceiverUpdate {
    /// The playback snapshot changed (or the periodic tick fired while playing).
    Playback(PlaybackSnapshot),
    /// The volume changed.
    Volume(f64),
    /// Playback failed on our side.
    Error {
        /// The human-readable reason (v1-v3's `PlaybackError`).
        message: String,
        /// The typed kind (v4's `Error` broadcast).
        kind: fcast_flatbuf::flat::ErrorKind,
    },
    /// What is loaded changed (v3 broadcasts this as `PlayUpdate`).
    PlayChanged(Option<PlayMessage>),
    /// A media item event fired (v3, subscribed sessions only).
    MediaItem {
        /// Which event.
        kind: MediaItemEventKind,
        /// The item it is about.
        item: PlayMessage,
    },
    /// A v4 `Load` to relay to *other* v4 senders, stripped (#248). v1-v3
    /// sessions hear the same load through [`ReceiverUpdate::PlayChanged`].
    V4Load {
        /// The raw inbound packet body.
        raw: Vec<u8>,
    },
    /// A v4 `QueueInsert` relay (stripped), other senders only.
    QueueInsertRelay {
        /// The raw inbound packet body.
        raw: Vec<u8>,
    },
    /// A v4 `QueueRemove` relay, other senders only.
    QueueRemoveRelay(crate::v4msg::QueuePosition),
    /// A v4 `QueueItemSelected`: relayed to other senders when a sender chose,
    /// broadcast to everyone when the receiver advanced (autoplay/end-of-item).
    QueueSelectRelay {
        /// The selected position.
        position: crate::v4msg::QueuePosition,
        /// Receiver-initiated selections go to every sender, the originator too.
        initiated_by_receiver: bool,
    },
    /// The speed actually in force (v4 `SpeedChanged` broadcast; v1-v3 senders
    /// read speed out of their `PlaybackUpdate`s instead).
    SpeedActual(f64),
    /// A position discontinuity — a seek landed. v4 senders hear it as an
    /// immediate `ProgressChanged`; v1-v3 senders already got the position in the
    /// `Playback` update that rides beside this.
    Progress {
        /// The new position.
        position: std::time::Duration,
        /// The duration, when the pipeline knows it.
        duration: Option<std::time::Duration>,
    },
}

/// A version-neutral playback snapshot; fields the pipeline does not know stay
/// `None` and each version's serializer decides what its senders require.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PlaybackSnapshot {
    /// Playback state.
    pub state: PlayState,
    /// Position in seconds.
    pub time: Option<f64>,
    /// Duration in seconds.
    pub duration: Option<f64>,
    /// Speed factor (this pipeline always plays at 1.0).
    pub speed: f64,
    /// Playlist index, when a playlist is loaded.
    pub item_index: Option<u64>,
}

/// Which events this session subscribed to (v3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventSubscriptions {
    media_start: bool,
    media_end: bool,
    media_change: bool,
    key_down: BTreeSet<String>,
    key_up: BTreeSet<String>,
}

impl EventSubscriptions {
    fn set(&mut self, event: EventSubscription, on: bool) {
        match event {
            EventSubscription::MediaItemStart => self.media_start = on,
            EventSubscription::MediaItemEnd => self.media_end = on,
            EventSubscription::MediaItemChange => self.media_change = on,
            EventSubscription::KeyDown(keys) => {
                for key in keys {
                    if on {
                        self.key_down.insert(key);
                    } else {
                        self.key_down.remove(&key);
                    }
                }
            }
            EventSubscription::KeyUp(keys) => {
                for key in keys {
                    if on {
                        self.key_up.insert(key);
                    } else {
                        self.key_up.remove(&key);
                    }
                }
            }
        }
    }

    /// Whether a media-item event of `kind` should be delivered here.
    #[must_use]
    pub const fn wants(&self, kind: MediaItemEventKind) -> bool {
        match kind {
            MediaItemEventKind::Start => self.media_start,
            MediaItemEventKind::End => self.media_end,
            MediaItemEventKind::Change => self.media_change,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing received yet. A `Version` moves to `Active` explicitly; any other
    /// legal-for-v1 opcode means a v1 sender, which never announces itself.
    AwaitingVersion,
    /// Negotiated.
    Active(SessionVersion),
}

/// One connection's protocol state.
#[derive(Debug)]
pub struct Session {
    phase: Phase,
    /// `now` of the last inbound frame.
    last_rx: Duration,
    /// A `Ping` is in flight and unanswered.
    ping_outstanding: bool,
    subscriptions: EventSubscriptions,
    peer: Option<InitialSenderMessage>,
}

impl Session {
    /// A fresh session, plus the greeting to write immediately: both parties must
    /// announce their version on connect (v3 spec, connection establishment).
    #[must_use]
    pub fn new() -> (Self, Frame) {
        let session = Self {
            phase: Phase::AwaitingVersion,
            last_rx: Duration::ZERO,
            ping_outstanding: false,
            subscriptions: EventSubscriptions::default(),
            peer: None,
        };
        let greeting = json_frame(
            Opcode::Version,
            &VersionMessage {
                version: PROTOCOL_VERSION,
            },
        );
        (session, greeting)
    }

    /// The negotiated version, once there is one.
    #[must_use]
    pub const fn version(&self) -> Option<SessionVersion> {
        match self.phase {
            Phase::AwaitingVersion => None,
            Phase::Active(version) => Some(version),
        }
    }

    /// What the sender said about itself in `Initial`, if it has.
    #[must_use]
    pub const fn peer(&self) -> Option<&InitialSenderMessage> {
        self.peer.as_ref()
    }

    /// This session's event subscriptions.
    #[must_use]
    pub const fn subscriptions(&self) -> &EventSubscriptions {
        &self.subscriptions
    }

    /// Handle one inbound frame.
    ///
    /// # Errors
    /// Any [`FCastError`] means the connection should be dropped — the reference
    /// receiver does the same for every fault this can return.
    pub fn on_frame(
        &mut self,
        now: Duration,
        ctx: &SessionContext<'_>,
        frame: &Frame,
    ) -> Result<Reaction, FCastError> {
        self.last_rx = now;
        self.ping_outstanding = false;

        match self.phase {
            Phase::AwaitingVersion => match frame.opcode {
                Opcode::Version => self.negotiate(ctx, frame),
                // A v1 sender never announces itself — the first thing it says is a
                // v1 verb. Anything outside v1's table before a `Version` is a peer
                // we don't understand.
                Opcode::None
                | Opcode::Play
                | Opcode::Pause
                | Opcode::Resume
                | Opcode::Stop
                | Opcode::Seek
                | Opcode::SetVolume => {
                    self.phase = Phase::Active(SessionVersion::V1);
                    self.dispatch(SessionVersion::V1, frame)
                }
                other => Err(FCastError::IllegalOpcode {
                    opcode: other,
                    version: 1,
                }),
            },
            Phase::Active(version) => self.dispatch(version, frame),
        }
    }

    /// Heartbeat tick. Returns a `Ping` to write when the connection has idled past
    /// [`PING_AFTER`], nothing while it is healthy.
    ///
    /// v1 sessions get no heartbeat in either direction: the opcode does not exist
    /// there, and a strict v1 sender would treat one as garbage.
    ///
    /// # Errors
    /// [`FCastError::HeartbeatTimeout`] once a `Ping` has gone unanswered past
    /// [`DEAD_AFTER`] — drop the connection.
    pub fn on_tick(&mut self, now: Duration) -> Result<Option<Frame>, FCastError> {
        let Phase::Active(version) = self.phase else {
            return Ok(None);
        };
        if version == SessionVersion::V1 {
            return Ok(None);
        }
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

    /// Translate a receiver-side update into this session's dialect, or `None` when
    /// this session should not hear about it (wrong version, not subscribed, or the
    /// handshake hasn't happened yet).
    #[must_use]
    pub fn frame_update(&self, wall_ms: u64, update: &ReceiverUpdate) -> Option<Frame> {
        let version = self.version()?;
        match update {
            ReceiverUpdate::Playback(snapshot) => Some(match version {
                SessionVersion::V1 => json_frame(
                    Opcode::PlaybackUpdate,
                    &PlaybackUpdateV1 {
                        time: snapshot.time.unwrap_or(0.0),
                        state: snapshot.state,
                    },
                ),
                // v2 requires every field; what the pipeline doesn't know yet is
                // reported as zero, which is also what the reference receiver sends
                // before its player has a duration.
                SessionVersion::V2 => json_frame(
                    Opcode::PlaybackUpdate,
                    &PlaybackUpdateV2 {
                        generation_time: wall_ms,
                        time: snapshot.time.unwrap_or(0.0),
                        duration: snapshot.duration.unwrap_or(0.0),
                        state: snapshot.state,
                        speed: snapshot.speed,
                    },
                ),
                SessionVersion::V3 => json_frame(
                    Opcode::PlaybackUpdate,
                    &PlaybackUpdateV3 {
                        generation_time: wall_ms,
                        state: snapshot.state,
                        time: snapshot.time,
                        duration: snapshot.duration,
                        speed: Some(snapshot.speed),
                        item_index: snapshot.item_index,
                    },
                ),
            }),
            ReceiverUpdate::Volume(volume) => Some(match version {
                SessionVersion::V1 => {
                    json_frame(Opcode::VolumeUpdate, &VolumeUpdateV1 { volume: *volume })
                }
                SessionVersion::V2 | SessionVersion::V3 => json_frame(
                    Opcode::VolumeUpdate,
                    &VolumeUpdateV2 {
                        generation_time: wall_ms,
                        volume: *volume,
                    },
                ),
            }),
            // v1 has no error opcode; a v1 sender simply sees the state go idle.
            ReceiverUpdate::Error { message, .. } => match version {
                SessionVersion::V1 => None,
                SessionVersion::V2 | SessionVersion::V3 => Some(json_frame(
                    Opcode::PlaybackError,
                    &PlaybackErrorMessage {
                        message: message.clone(),
                    },
                )),
            },
            ReceiverUpdate::PlayChanged(play) => match version {
                SessionVersion::V1 | SessionVersion::V2 => None,
                SessionVersion::V3 => Some(json_frame(
                    Opcode::PlayUpdate,
                    &PlayUpdateMessage {
                        generation_time: wall_ms,
                        play_data: play.clone(),
                    },
                )),
            },
            ReceiverUpdate::MediaItem { kind, item } => {
                if version == SessionVersion::V3 && self.subscriptions.wants(*kind) {
                    Some(media_item_event_frame(wall_ms, *kind, item))
                } else {
                    None
                }
            }
            // v4-only surfaces; the JSON dialects have no message for them.
            // (Queue movement still reaches v3 senders as `itemIndex` in their
            // `PlaybackUpdate`s, and the load itself as `PlayChanged`.)
            ReceiverUpdate::V4Load { .. }
            | ReceiverUpdate::QueueInsertRelay { .. }
            | ReceiverUpdate::QueueRemoveRelay(_)
            | ReceiverUpdate::QueueSelectRelay { .. }
            | ReceiverUpdate::SpeedActual(_)
            | ReceiverUpdate::Progress { .. } => None,
        }
    }

    fn negotiate(
        &mut self,
        ctx: &SessionContext<'_>,
        frame: &Frame,
    ) -> Result<Reaction, FCastError> {
        let msg: VersionMessage = crate::messages::parse_body(frame)?;
        let version = match msg.version {
            0 => return Err(FCastError::IllegalVersion),
            1 => SessionVersion::V1,
            2 => SessionVersion::V2,
            // A peer newer than us downgrades to what we advertised (its rule as the
            // higher-version party); the session runs at our version, not its.
            3.. => SessionVersion::V3,
        };
        self.phase = Phase::Active(version);
        Ok(match version {
            // v1/v2 have no `Initial`, but a sender still needs the current volume
            // to start in sync — it only broadcasts on change.
            SessionVersion::V1 => Reaction::reply(json_frame(
                Opcode::VolumeUpdate,
                &VolumeUpdateV1 { volume: ctx.volume },
            )),
            SessionVersion::V2 => Reaction::reply(json_frame(
                Opcode::VolumeUpdate,
                &VolumeUpdateV2 {
                    generation_time: ctx.wall_ms,
                    volume: ctx.volume,
                },
            )),
            SessionVersion::V3 => Reaction::reply(json_frame(
                Opcode::Initial,
                &InitialReceiverMessage {
                    display_name: Some(ctx.receiver.display_name.clone()),
                    app_name: Some(ctx.receiver.app_name.clone()),
                    app_version: Some(ctx.receiver.app_version.clone()),
                    play_data: ctx.play_data.cloned(),
                },
            )),
        })
    }

    fn dispatch(&mut self, version: SessionVersion, frame: &Frame) -> Result<Reaction, FCastError> {
        let illegal = || {
            Err(FCastError::IllegalOpcode {
                opcode: frame.opcode,
                version: version.number(),
            })
        };
        match frame.opcode {
            // --- every version ---
            Opcode::None => Ok(Reaction::none()),
            Opcode::Play => Ok(Reaction::command(SenderCommand::Load(Box::new(
                crate::messages::parse_body::<PlayMessage>(frame)?,
            )))),
            Opcode::Pause => Ok(Reaction::command(SenderCommand::Pause)),
            Opcode::Resume => Ok(Reaction::command(SenderCommand::Resume)),
            Opcode::Stop => Ok(Reaction::command(SenderCommand::Stop)),
            Opcode::Seek => {
                let msg: SeekMessage = crate::messages::parse_body(frame)?;
                // A non-finite or negative target is not a position. Refused with a
                // typed fault rather than clamped to something the sender never asked
                // for; the reference receiver drops these too.
                Duration::try_from_secs_f64(msg.time).map_or_else(
                    |_| {
                        Err(FCastError::MalformedBody {
                            opcode: Opcode::Seek,
                            detail: format!("unrepresentable seek target {}", msg.time),
                        })
                    },
                    |target| Ok(Reaction::command(SenderCommand::Seek(target))),
                )
            }
            Opcode::SetVolume => {
                let msg: SetVolumeMessage = crate::messages::parse_body(frame)?;
                // Clamped, not refused: protocol v4 defines exactly this (clamp +
                // `VolumeOutOfRange` note), and a slider overshooting into 1.02 is a
                // position, unlike a NaN seek.
                Ok(Reaction::command(SenderCommand::SetVolume(
                    msg.volume.clamp(0.0, 1.0),
                )))
            }
            // Receiver-direction opcodes a confused sender might echo back. Ignored,
            // as the reference receiver ignores them, in every version.
            Opcode::PlaybackUpdate
            | Opcode::VolumeUpdate
            | Opcode::PlaybackError
            | Opcode::PlayUpdate
            | Opcode::Event => Ok(Reaction::none()),

            // --- v2+ ---
            Opcode::Ping if version >= SessionVersion::V2 => {
                Ok(Reaction::reply(Frame::bare(Opcode::Pong)))
            }
            Opcode::Pong if version >= SessionVersion::V2 => Ok(Reaction::none()),
            Opcode::SetSpeed if version >= SessionVersion::V2 => {
                let msg: SetSpeedMessage = crate::messages::parse_body(frame)?;
                Ok(Reaction::command(SenderCommand::SetSpeed(msg.speed)))
            }
            Opcode::Version => Err(FCastError::UnexpectedVersion),

            // --- v3 ---
            Opcode::Initial if version >= SessionVersion::V3 => {
                self.peer = Some(crate::messages::parse_body(frame)?);
                Ok(Reaction::none())
            }
            Opcode::SetPlaylistItem if version >= SessionVersion::V3 => {
                let msg: SetPlaylistItemMessage = crate::messages::parse_body(frame)?;
                Ok(Reaction::command(SenderCommand::SetPlaylistItem(
                    msg.item_index,
                )))
            }
            Opcode::SubscribeEvent if version >= SessionVersion::V3 => {
                let msg: EventSubscriptionMessage = crate::messages::parse_body(frame)?;
                self.subscriptions.set(msg.event, true);
                Ok(Reaction::none())
            }
            Opcode::UnsubscribeEvent if version >= SessionVersion::V3 => {
                let msg: EventSubscriptionMessage = crate::messages::parse_body(frame)?;
                self.subscriptions.set(msg.event, false);
                Ok(Reaction::none())
            }

            // Legal opcodes used outside their version: a v1 sender has no
            // `SetSpeed`, a v2 sender no `SetPlaylistItem`, and no JSON session has
            // v4's `Flatbuf`/`Resource`. Accepting them would run a protocol the
            // sender never agreed to.
            Opcode::Ping
            | Opcode::Pong
            | Opcode::SetSpeed
            | Opcode::Initial
            | Opcode::SetPlaylistItem
            | Opcode::SubscribeEvent
            | Opcode::UnsubscribeEvent
            | Opcode::Flatbuf
            | Opcode::Resource => illegal(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn identity() -> ReceiverIdentity {
        ReceiverIdentity {
            display_name: "dma.space/screen".into(),
            app_name: "castaway".into(),
            app_version: "0.1.0".into(),
        }
    }

    fn ctx<'a>(
        receiver: &'a ReceiverIdentity,
        play: Option<&'a PlayMessage>,
    ) -> SessionContext<'a> {
        SessionContext {
            wall_ms: 1_754_700_000_000,
            receiver,
            play_data: play,
            volume: 0.8,
        }
    }

    fn version_frame(version: u64) -> Frame {
        json_frame(Opcode::Version, &VersionMessage { version })
    }

    /// v3 spec, connection establishment: both parties must announce their version.
    /// Ours is the greeting, and it is byte-identical to what the capture harness
    /// sent the reference sender (which accepted it and ran a v3 session).
    #[test]
    fn the_greeting_is_version_3() {
        let (_, greeting) = Session::new();
        assert_eq!(
            crate::wire::encode(&greeting).unwrap(),
            [
                0x0e, 0x00, 0x00, 0x00, 0x0b, b'{', b'"', b'v', b'e', b'r', b's', b'i', b'o', b'n',
                b'"', b':', b'3', b'}'
            ]
        );
    }

    /// A v3 hello gets `Initial` back, carrying the loaded content so a sender that
    /// joins mid-session starts in sync.
    #[test]
    fn a_v3_hello_is_answered_with_initial_and_play_data() {
        let receiver = identity();
        let play: PlayMessage =
            serde_json::from_str(r#"{"container":"video/mp4","url":"http://h/v.mp4"}"#).unwrap();
        let (mut session, _) = Session::new();
        let reaction = session
            .on_frame(
                Duration::ZERO,
                &ctx(&receiver, Some(&play)),
                &version_frame(3),
            )
            .unwrap();
        assert_eq!(session.version(), Some(SessionVersion::V3));
        assert_eq!(reaction.replies.len(), 1);
        assert_eq!(reaction.replies[0].opcode, Opcode::Initial);
        let body: serde_json::Value = serde_json::from_slice(&reaction.replies[0].body).unwrap();
        assert_eq!(body["displayName"], "dma.space/screen");
        assert_eq!(body["playData"]["url"], "http://h/v.mp4");
    }

    /// The sender SDK says `Version {{ version: 4 }}` first and downgrades on our
    /// reply (verified against the real sender in `tests/fixtures/`); the session
    /// runs at v3, never at a version we don't implement.
    #[test]
    fn a_v4_hello_runs_a_v3_session() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(4))
            .unwrap();
        assert_eq!(session.version(), Some(SessionVersion::V3));
    }

    /// v1/v2 senders have no `Initial`; they get the current volume instead, because
    /// it otherwise only broadcasts on change and a fresh sender would show a stale
    /// slider. The reference receiver does the same.
    #[test]
    fn v1_and_v2_hellos_are_answered_with_the_current_volume() {
        let receiver = identity();
        for (version, expected) in [
            (1, r#"{"volume":0.8}"#.to_owned()),
            (2, {
                r#"{"generationTime":1754700000000,"volume":0.8}"#.to_owned()
            }),
        ] {
            let (mut session, _) = Session::new();
            let reaction = session
                .on_frame(
                    Duration::ZERO,
                    &ctx(&receiver, None),
                    &version_frame(version),
                )
                .unwrap();
            assert_eq!(reaction.replies[0].opcode, Opcode::VolumeUpdate);
            assert_eq!(
                String::from_utf8(reaction.replies[0].body.clone()).unwrap(),
                expected,
                "v{version} volume shape"
            );
        }
    }

    /// A v1 sender never announces itself — its first frame is a verb. That frame
    /// both fixes the version and takes effect.
    #[test]
    fn a_verb_before_any_version_means_a_v1_sender() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        let reaction = session
            .on_frame(
                Duration::ZERO,
                &ctx(&receiver, None),
                &Frame::with_body(
                    Opcode::Play,
                    br#"{"container":"video/mp4","url":"http://h/v.mp4"}"#.to_vec(),
                ),
            )
            .unwrap();
        assert_eq!(session.version(), Some(SessionVersion::V1));
        assert!(matches!(reaction.command, Some(SenderCommand::Load(_))));
    }

    /// `Version {{ version: 0 }}` is not a version. The reference receiver rejects
    /// it; so do we.
    #[test]
    fn version_zero_is_a_fault() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        assert!(matches!(
            session.on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(0)),
            Err(FCastError::IllegalVersion)
        ));
    }

    /// Opcode legality follows the *negotiated* version: `SetPlaylistItem` from a v2
    /// session is running a protocol the sender never agreed to.
    #[test]
    fn opcodes_outside_the_negotiated_version_are_refused() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(2))
            .unwrap();
        assert!(matches!(
            session.on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &Frame::with_body(Opcode::SetPlaylistItem, br#"{"itemIndex":1}"#.to_vec()),
            ),
            Err(FCastError::IllegalOpcode { version: 2, .. })
        ));
        // And a v1 session has no Ping.
        let (mut v1, _) = Session::new();
        v1.on_frame(
            Duration::ZERO,
            &ctx(&receiver, None),
            &Frame::bare(Opcode::Pause),
        )
        .unwrap();
        assert!(matches!(
            v1.on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &Frame::bare(Opcode::Ping)
            ),
            Err(FCastError::IllegalOpcode { version: 1, .. })
        ));
    }

    /// The heartbeat ladder, asserted against the shipped constants in virtual time
    /// (ground rule 6): quiet until [`PING_AFTER`], one `Ping`, then a fault at
    /// [`DEAD_AFTER`] — and any inbound frame resets the whole ladder.
    #[test]
    fn the_heartbeat_ladder_uses_the_shipped_constants() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();

        let just_before_ping = PING_AFTER - Duration::from_millis(1);
        assert_eq!(session.on_tick(just_before_ping).unwrap(), None);
        let ping = session.on_tick(PING_AFTER).unwrap().unwrap();
        assert_eq!(ping.opcode, Opcode::Ping);
        // No second ping while one is outstanding.
        assert_eq!(
            session
                .on_tick(PING_AFTER + Duration::from_secs(1))
                .unwrap(),
            None
        );
        let just_before_dead = DEAD_AFTER - Duration::from_millis(1);
        assert_eq!(session.on_tick(just_before_dead).unwrap(), None);
        assert!(matches!(
            session.on_tick(DEAD_AFTER),
            Err(FCastError::HeartbeatTimeout)
        ));

        // A pong (any frame) resets the ladder.
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();
        session.on_tick(PING_AFTER).unwrap().unwrap();
        session
            .on_frame(
                PING_AFTER + Duration::from_millis(10),
                &ctx(&receiver, None),
                &Frame::bare(Opcode::Pong),
            )
            .unwrap();
        assert_eq!(
            session
                .on_tick(PING_AFTER + Duration::from_millis(11))
                .unwrap(),
            None,
            "the ladder should restart from the pong"
        );
    }

    /// v1 sessions get no heartbeat: the opcode does not exist in their protocol.
    #[test]
    fn v1_sessions_are_never_pinged() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(
                Duration::ZERO,
                &ctx(&receiver, None),
                &Frame::bare(Opcode::Pause),
            )
            .unwrap();
        assert_eq!(session.on_tick(Duration::from_secs(3600)).unwrap(), None);
    }

    #[test]
    fn a_ping_is_answered_with_a_pong() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(2))
            .unwrap();
        let reaction = session
            .on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &Frame::bare(Opcode::Ping),
            )
            .unwrap();
        assert_eq!(reaction.replies, vec![Frame::bare(Opcode::Pong)]);
    }

    /// One update, three dialects: what each version's senders are told about the
    /// same playback moment is exactly what their spec requires — v2 gets every
    /// field, v1 only time and state, v3 gets `null`-omission.
    #[test]
    fn playback_updates_are_translated_per_session_version() {
        let receiver = identity();
        let snapshot = PlaybackSnapshot {
            state: PlayState::Playing,
            time: Some(12.5),
            duration: Some(596.5),
            speed: 1.0,
            item_index: None,
        };
        let update = ReceiverUpdate::Playback(snapshot);

        let mut sessions = Vec::new();
        for version in [1, 2, 3] {
            let (mut session, _) = Session::new();
            session
                .on_frame(
                    Duration::ZERO,
                    &ctx(&receiver, None),
                    &version_frame(version),
                )
                .unwrap();
            sessions.push(session);
        }
        let bodies: Vec<String> = sessions
            .iter()
            .map(|s| String::from_utf8(s.frame_update(77, &update).unwrap().body).unwrap())
            .collect();
        assert_eq!(bodies[0], r#"{"time":12.5,"state":1}"#);
        assert_eq!(
            bodies[1],
            r#"{"generationTime":77,"time":12.5,"duration":596.5,"state":1,"speed":1.0}"#
        );
        assert_eq!(
            bodies[2],
            r#"{"generationTime":77,"state":1,"time":12.5,"duration":596.5,"speed":1.0}"#
        );
    }

    /// What must *not* reach a session: errors to v1 (no opcode for them),
    /// `PlayUpdate` to v1/v2, events to the unsubscribed, anything to a session
    /// still awaiting its version.
    #[test]
    fn updates_a_version_cannot_carry_are_withheld() {
        let receiver = identity();
        let play: PlayMessage =
            serde_json::from_str(r#"{"container":"video/mp4","url":"http://h/v.mp4"}"#).unwrap();

        let (fresh, _) = Session::new();
        assert_eq!(fresh.frame_update(1, &ReceiverUpdate::Volume(0.5)), None);

        let (mut v1, _) = Session::new();
        v1.on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(1))
            .unwrap();
        assert_eq!(
            v1.frame_update(
                1,
                &ReceiverUpdate::Error {
                    message: "boom".into(),
                    kind: fcast_flatbuf::flat::ErrorKind::Internal
                }
            ),
            None
        );
        assert_eq!(
            v1.frame_update(1, &ReceiverUpdate::PlayChanged(Some(play.clone()))),
            None
        );

        let (mut v3, _) = Session::new();
        v3.on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();
        // Not subscribed: no event.
        assert_eq!(
            v3.frame_update(
                1,
                &ReceiverUpdate::MediaItem {
                    kind: MediaItemEventKind::End,
                    item: play.clone()
                }
            ),
            None
        );
        // Subscribe (the reference sender does this for MediaItemEnd on every
        // connection), then the event flows.
        v3.on_frame(
            Duration::from_secs(1),
            &ctx(&receiver, None),
            &Frame::with_body(Opcode::SubscribeEvent, br#"{"event":{"type":1}}"#.to_vec()),
        )
        .unwrap();
        assert!(v3
            .frame_update(
                1,
                &ReceiverUpdate::MediaItem {
                    kind: MediaItemEventKind::End,
                    item: play.clone()
                }
            )
            .is_some());
        // And unsubscribe turns it back off.
        v3.on_frame(
            Duration::from_secs(2),
            &ctx(&receiver, None),
            &Frame::with_body(
                Opcode::UnsubscribeEvent,
                br#"{"event":{"type":1}}"#.to_vec(),
            ),
        )
        .unwrap();
        assert_eq!(
            v3.frame_update(
                1,
                &ReceiverUpdate::MediaItem {
                    kind: MediaItemEventKind::End,
                    item: play.clone()
                }
            ),
            None
        );
    }

    /// A NaN or negative seek target is not a position; it is refused as a typed
    /// fault rather than clamped to a place the sender never asked for.
    #[test]
    fn an_unrepresentable_seek_is_a_typed_fault() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();
        assert!(matches!(
            session.on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &Frame::with_body(Opcode::Seek, br#"{"time":-5.0}"#.to_vec()),
            ),
            Err(FCastError::MalformedBody { .. })
        ));
    }

    /// An overshooting volume is clamped (a slider position saturates); v4 writes
    /// this rule down and it is the obviously-right reading for v1-v3.
    #[test]
    fn volume_saturates_rather_than_wrapping() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();
        let reaction = session
            .on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &Frame::with_body(Opcode::SetVolume, br#"{"volume":1.5}"#.to_vec()),
            )
            .unwrap();
        assert_eq!(reaction.command, Some(SenderCommand::SetVolume(1.0)));
    }

    /// Renegotiating the version mid-session is not part of any published version.
    #[test]
    fn a_second_version_message_is_a_fault() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();
        assert!(matches!(
            session.on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &version_frame(3)
            ),
            Err(FCastError::UnexpectedVersion)
        ));
    }

    /// `Initial` records who the peer is — surfaced in logs and the OSD.
    #[test]
    fn initial_records_the_peer() {
        let receiver = identity();
        let (mut session, _) = Session::new();
        session
            .on_frame(Duration::ZERO, &ctx(&receiver, None), &version_frame(3))
            .unwrap();
        session
            .on_frame(
                Duration::from_secs(1),
                &ctx(&receiver, None),
                &Frame::with_body(
                    Opcode::Initial,
                    br#"{"appName":"FCast Sender SDK v0.3.0","appVersion":"0.3.0"}"#.to_vec(),
                ),
            )
            .unwrap();
        assert_eq!(
            session.peer().unwrap().app_name.as_deref(),
            Some("FCast Sender SDK v0.3.0")
        );
    }
}
