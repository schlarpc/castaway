//! The pure CASTv2 session state machine (ground rule 3): fold an incoming
//! [`CastMessage`] into `(outgoing messages, optional SessionEvent)`. No sockets, no
//! TLS, no timers — the actor drives it. This is what makes the wire-fixture tests
//! possible without a real Chrome sender.

use castaway_core::{ControlTxn, MediaUri, SessionEvent};
use prost::Message as _;
use tracing::{debug, warn};

use crate::error::CastError;
use crate::messages::{
    self, ns, App, AppAvailabilityRequest, Envelope, LaunchRefusal, LaunchRequest, LoadRequest,
    RunningApp, SetVolumeRequest,
};
use crate::proto::{
    auth_error, AuthError, AuthResponse, CastMessage, DeviceAuthMessage, PayloadType,
};

/// Signs the device-auth challenge for one connection. Implemented in
/// `crypto-cast-auth`; the session just forwards the challenge and ships the response.
pub trait DeviceAuthResponder: Send + Sync {
    /// Produce an [`AuthResponse`] for `challenge`. The impl closes over the receiver's
    /// device certificate + the TLS peer-cert hash for this connection.
    ///
    /// # Errors
    /// Implementation-defined; a failure makes the session return an `AuthError`.
    fn respond(&self, challenge: &crate::proto::AuthChallenge) -> Result<AuthResponse, CastError>;
}

/// The result of folding one message.
#[derive(Debug, Default)]
pub struct Reaction {
    /// Messages to write back on the channel.
    pub outgoing: Vec<CastMessage>,
    /// Session events to forward to the session manager, in order.
    ///
    /// A list rather than an `Option` because one message really can mean two things to
    /// the manager — a `SET_VOLUME` carrying both a level and a mute flag is the case
    /// that forced it — and silently dropping the second is how a mute that never lifts
    /// happens.
    pub events: Vec<SessionEvent>,
    /// A negotiated mirroring config the actor should start receiving on. The session
    /// can't emit [`SessionEvent::Mirror`] itself (that needs an I/O-fed frame channel),
    /// so it surfaces the config and the actor wires the RTP receiver + `FrameSource`.
    pub start_mirror: Option<crate::mirror::MirrorConfig>,
    /// A `LAUNCH` this session has accepted in principle and cannot finish on its own.
    ///
    /// Hosting an application needs a registry lookup and a browser, both of which are
    /// I/O — so the fold stops here and the actor finishes it, coming back through
    /// [`CastSession::page_hosted`] or [`CastSession::page_refused`]. The sender is
    /// deliberately left waiting in the meantime: a `RECEIVER_STATUS` sent before the
    /// page exists is the "connected phone, black panel" failure #16 opens with.
    pub launch_page: Option<PendingLaunch>,
    /// Messages a hosted application owns, for the actor to hand to the page.
    pub to_page: Vec<PageMessage>,
}

/// A `LAUNCH` waiting on a lookup and a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLaunch {
    /// The app id to resolve.
    pub app_id: String,
    /// The request to answer once it is resolved.
    pub request_id: i64,
    /// The sender waiting on it, which becomes the session's controller.
    pub sender: String,
    /// The session id, minted here rather than when the page comes up.
    ///
    /// It has to exist *before* the platform is told about the application, because the
    /// page is handed it in `ready` and echoes it to the vendor's own cloud — so a
    /// session id invented later would be a second one, and `RECEIVER_STATUS` and the
    /// page would disagree about which session this is.
    pub session_id: String,
    /// The virtual-connection id media messages address once the app is running.
    pub transport_id: String,
}

/// A sender's message on a namespace a hosted application declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageMessage {
    /// The namespace it arrived on.
    pub namespace: String,
    /// Which sender sent it.
    pub sender: String,
    /// The payload, untouched.
    pub data: String,
}

impl Reaction {
    /// A reaction that only writes messages back.
    fn reply(outgoing: Vec<CastMessage>) -> Self {
        Self {
            outgoing,
            ..Self::default()
        }
    }

    /// A reaction that writes messages back and forwards a session event.
    fn reply_with(outgoing: Vec<CastMessage>, event: SessionEvent) -> Self {
        Self {
            outgoing,
            events: vec![event],
            ..Self::default()
        }
    }
}

/// The receiver-side Cast session for one sender connection.
pub struct CastSession {
    receiver_id: String,
    app: Option<RunningApp>,
    volume: f32,
    muted: bool,
    media_session_id: i64,
    /// What the media plane is doing, or `None` when nothing has been loaded.
    ///
    /// Tracked rather than assumed: `GET_STATUS` used to answer `PLAYING` unconditionally,
    /// so a sender that paused and asked was told playback had resumed, and a sender that
    /// asked before loading anything was told about media that did not exist.
    player_state: Option<messages::PlayerState>,
    /// Where the pipeline says playback has reached, as of the last time anyone asked.
    ///
    /// Pushed in by the actor rather than pulled from here, for the same reason DLNA does
    /// it: this module is a pure fold over its inputs, and a trait object it called into
    /// would make every status test depend on a live decoder.
    position: Option<castaway_core::PlaybackProgress>,
    id_counter: u64,
    auth: Option<Box<dyn DeviceAuthResponder>>,
    /// UDP port the actor pre-bound for mirroring RTP, if mirroring is enabled.
    mirror_port: Option<u16>,
    /// What is known about app ids this receiver does not serve natively.
    catalogue: messages::AppCatalogue,
}

impl CastSession {
    /// Create a session. `auth` handles the device-auth handshake; without it, the
    /// session answers challenges with `AuthError` (fine for local dev / tests).
    #[must_use]
    pub fn new(auth: Option<Box<dyn DeviceAuthResponder>>) -> Self {
        Self {
            receiver_id: "receiver-0".to_string(),
            app: None,
            volume: 1.0,
            muted: false,
            media_session_id: 1,
            player_state: None,
            position: None,
            id_counter: 0,
            auth,
            mirror_port: None,
            catalogue: messages::AppCatalogue::default(),
        }
    }

    /// Record what the receiver currently knows about app ids.
    ///
    /// Pushed in by the actor before each fold, exactly as
    /// [`CastSession::observe_progress`] pushes playback position: it is the answer to a
    /// network lookup, and a pure fold must not be able to make one.
    pub fn observe_catalogue(&mut self, catalogue: messages::AppCatalogue) {
        self.catalogue = catalogue;
    }

    /// Whether a hosted application currently owns `namespace`.
    fn hosted_owns(&self, namespace: &str) -> bool {
        // Transport is never the application's, whatever it declared. `CONNECTION` and
        // `HEARTBEAT` are how a sender reaches the session at all and `DEVICE_AUTH` is
        // how it trusts the device; an application that claimed one of them and was
        // given it would be able to end sessions it does not own, or answer a challenge
        // it cannot sign. The SDK already refuses to let a page open a bus on the system
        // namespace; this is the same rule applied from our side, where it is enforceable.
        if matches!(
            namespace,
            ns::CONNECTION | ns::HEARTBEAT | ns::DEVICE_AUTH | ns::RECEIVER
        ) {
            return false;
        }
        self.app
            .as_ref()
            .is_some_and(|app| app.namespaces.iter().any(|n| n == namespace))
    }

    /// Record where the pipeline says playback has reached.
    ///
    /// This is the whole of a sender's scrubber. `currentTime` used to be a hardcoded
    /// zero — knowingly, because nothing on the pipeline side reported a position — so the
    /// bar sat at the start for the length of every item.
    pub fn observe_progress(&mut self, progress: Option<castaway_core::PlaybackProgress>) {
        self.position = progress;
    }

    /// The pipeline finished with the item, or failed to play it: tell every sender.
    ///
    /// Returns the messages to write, which is the point — a Cast sender has no way at all
    /// to learn what became of the URL it handed us except by being told. Without this the
    /// status stayed `PLAYING` for the life of the connection, so a sender's queue never
    /// advanced and a fetch that failed was indistinguishable from a cast that was working.
    ///
    /// Broadcast rather than addressed: unsolicited status goes to every sender on the
    /// connection, and `requestId: 0` is how a sender knows it did not ask for this.
    pub fn media_ended(&mut self, end: &castaway_core::PlaybackEnd) -> Vec<CastMessage> {
        // Nothing was loaded, so nothing ended — a decode thread noticing on its own
        // schedule that it was torn down, after the session it belonged to had gone.
        if self.player_state.is_none() {
            return Vec::new();
        }
        let reason = if end.is_failure() {
            messages::IdleReason::Error
        } else {
            messages::IdleReason::Finished
        };
        self.player_state = Some(messages::PlayerState::Idle(reason));
        self.position = None;
        vec![self.media_status_msg(BROADCAST, 0)]
    }

    /// Apply a transport verb that came from the panel rather than from a sender, and
    /// return the status that tells every sender about it.
    ///
    /// The same reasoning as DLNA's: a finger on the glass and the phone that started the
    /// cast are two views of one session, and a receiver that moved for one and not the
    /// other leaves the sender's pause button toggling playback back on.
    pub fn apply_local_control(&mut self, txn: &ControlTxn) -> Vec<CastMessage> {
        match txn {
            ControlTxn::Play => self.player_state = Some(messages::PlayerState::Playing),
            ControlTxn::Pause => self.player_state = Some(messages::PlayerState::Paused),
            // Cast's media STOP unloads the item rather than pausing at zero, so the
            // session goes back to having no media at all — the same thing a sender's own
            // STOP does.
            ControlTxn::Stop => {
                self.player_state = None;
                self.position = None;
            }
            // `RECEIVER_STATUS` reports the sender's own scale back to it, so what is
            // stored is the slider position, not the amplitude behind it (#85).
            ControlTxn::Volume(level) => self.volume = level.position(),
            ControlTxn::Mute(muted) => self.muted = *muted,
            // A seek moves the position and nothing else; the position is read from the
            // pipeline, so there is nothing to store.
            ControlTxn::Seek(_) => {}
            other => {
                debug!(?other, "cast: a verb outside the panel's capability set");
                return Vec::new();
            }
        }
        vec![self.media_status_msg(BROADCAST, 0)]
    }

    /// Enable mirroring: the actor pre-binds a UDP socket and passes its `port` so the
    /// negotiator can put it in the `ANSWER`.
    #[must_use]
    pub fn with_mirror_port(mut self, port: u16) -> Self {
        self.mirror_port = Some(port);
        self
    }

    /// Point future ANSWERs at a different RTP port — or at nothing.
    ///
    /// The actor calls this when a session ends on a still-open connection: the old
    /// socket died with the mirror that used it, and an ANSWER naming a dead port is a
    /// sender streaming into a black hole.
    pub fn set_mirror_port(&mut self, port: Option<u16>) {
        self.mirror_port = port;
    }

    /// Fold one inbound message into a [`Reaction`].
    ///
    /// # Errors
    /// [`CastError`] on undecodable payloads.
    pub fn handle(&mut self, msg: &CastMessage) -> Result<Reaction, CastError> {
        // A hosted application owns the namespaces it declared, and that includes the
        // media namespace: with YouTube up, YouTube answers `LOAD`, not us. Answering
        // alongside it would put two receivers on one session — two `MEDIA_STATUS`
        // replies to one request, from two players that disagree.
        if self.hosted_owns(&msg.namespace) {
            let Some(data) = msg.payload_utf8.as_deref() else {
                // The application layer is JSON. A binary payload on an app namespace
                // is not something a receiver page can be handed.
                debug!(namespace = %msg.namespace, "dropping a binary payload bound for a hosted app");
                return Ok(Reaction::default());
            };
            return Ok(Reaction {
                to_page: vec![PageMessage {
                    namespace: msg.namespace.clone(),
                    sender: msg.source_id.clone(),
                    data: data.to_owned(),
                }],
                ..Reaction::default()
            });
        }
        match msg.namespace.as_str() {
            ns::DEVICE_AUTH => self.handle_device_auth(msg),
            ns::CONNECTION => Ok(self.handle_connection(msg)),
            ns::HEARTBEAT => Ok(self.handle_heartbeat(msg)),
            ns::RECEIVER => self.handle_receiver(msg),
            ns::MEDIA => self.handle_media(msg),
            crate::mirror::WEBRTC_NS => self.handle_webrtc(msg),
            other => {
                debug!(namespace = %other, "ignoring message on unknown namespace");
                Ok(Reaction::default())
            }
        }
    }

    fn handle_device_auth(&self, msg: &CastMessage) -> Result<Reaction, CastError> {
        let binary = msg
            .payload_binary
            .as_deref()
            .ok_or_else(|| CastError::Decode("deviceauth without binary payload".into()))?;
        let dam =
            DeviceAuthMessage::decode(binary).map_err(|e| CastError::Decode(e.to_string()))?;
        let Some(challenge) = dam.challenge else {
            return Ok(Reaction::default()); // response/error from us — ignore echoes
        };
        let reply = match self.auth.as_ref().map(|a| a.respond(&challenge)) {
            Some(Ok(response)) => DeviceAuthMessage {
                challenge: None,
                response: Some(response),
                error: None,
            },
            Some(Err(e)) => {
                warn!(error = %e, "device-auth signer failed");
                auth_err()
            }
            None => auth_err(),
        };
        let mut buf = Vec::new();
        reply
            .encode(&mut buf)
            .map_err(|e| CastError::Encode(e.to_string()))?;
        Ok(Reaction::reply(vec![CastMessage::binary(
            &msg.destination_id,
            &msg.source_id,
            ns::DEVICE_AUTH,
            buf,
        )]))
    }

    fn handle_connection(&mut self, msg: &CastMessage) -> Reaction {
        let ty = msg
            .payload_utf8
            .as_deref()
            .and_then(|p| Envelope::parse(p).ok())
            .map(|e| e.r#type)
            .unwrap_or_default();
        // CLOSE tears down one virtual connection, not the channel — several senders
        // multiplex over this socket (Chrome's platform sender, a page's Cast SDK
        // client, the one actually casting), and each opens and closes its own.
        // Only the session owner's departure ends the running app; anyone else's
        // CLOSE is that sender leaving, which is not our business. Ending on every
        // CLOSE meant a page probing the receiver could kill an unrelated mirror.
        if ty == "CLOSE"
            && self
                .app
                .as_ref()
                .is_some_and(|app| app.controller == msg.source_id)
        {
            self.app = None;
            self.player_state = None;
            return Reaction::reply_with(vec![], SessionEvent::End);
        }
        Reaction::default()
    }

    fn handle_heartbeat(&self, msg: &CastMessage) -> Reaction {
        let is_ping = msg
            .payload_utf8
            .as_deref()
            .and_then(|p| Envelope::parse(p).ok())
            .is_some_and(|e| e.r#type == "PING");
        if is_ping {
            Reaction::reply(vec![CastMessage::json(
                &msg.destination_id,
                &msg.source_id,
                ns::HEARTBEAT,
                messages::pong(),
            )])
        } else {
            Reaction::default()
        }
    }

    fn handle_receiver(&mut self, msg: &CastMessage) -> Result<Reaction, CastError> {
        let payload = msg
            .payload_utf8
            .as_deref()
            .ok_or_else(|| CastError::Json("receiver message without payload".into()))?;
        let env = Envelope::parse(payload)?;
        let sender = msg.source_id.clone();
        match env.r#type.as_str() {
            "GET_STATUS" => Ok(self.reply_receiver_status(&sender, env.request_id.unwrap_or(0))),
            "GET_APP_AVAILABILITY" => {
                let req: AppAvailabilityRequest = messages::parse_message(payload)?;
                let answers: Vec<(String, bool)> = req
                    .app_ids
                    .into_iter()
                    .map(|id| {
                        let can = self.can_host(App::classify(&id, &self.catalogue));
                        (id, can)
                    })
                    .collect();
                debug!(?answers, "answering app availability");
                Ok(Reaction::reply(vec![CastMessage::json(
                    &self.receiver_id,
                    &sender,
                    ns::RECEIVER,
                    messages::app_availability(req.request_id, &answers),
                )]))
            }
            "LAUNCH" => {
                let req: LaunchRequest = messages::parse_message(payload)?;
                let app = App::classify(&req.app_id, &self.catalogue);
                if app == App::Page {
                    self.id_counter += 1;
                    let n = self.id_counter;
                    // Accepted in principle and not answerable here: resolving the id is
                    // a lookup and hosting it needs a browser. The actor finishes it and
                    // comes back through `page_hosted`/`page_refused`; the sender waits,
                    // which is correct — a status claiming the app is running before its
                    // page exists is the failure this whole path is for.
                    return Ok(Reaction {
                        launch_page: Some(PendingLaunch {
                            app_id: req.app_id,
                            request_id: req.request_id,
                            sender,
                            session_id: format!("sess-{n}"),
                            transport_id: format!("transport-{n}"),
                        }),
                        ..Reaction::default()
                    });
                }
                if let Some(refusal) = self.refusal_for(app) {
                    // Saying "running" to a launch we cannot serve is the worst answer
                    // available: the sender opens a connection to a transport id that
                    // will never speak, and the room sees a connected phone and a black
                    // panel. Refuse in the sender's own vocabulary instead.
                    warn!(
                        app_id = %req.app_id,
                        reason = refusal.reason(),
                        "declining a LAUNCH for an app this receiver cannot host"
                    );
                    return Ok(Reaction::reply(vec![CastMessage::json(
                        &self.receiver_id,
                        &sender,
                        ns::RECEIVER,
                        messages::launch_error(req.request_id, refusal),
                    )]));
                }
                self.launch(&req, &sender);
                Ok(self.reply_receiver_status(&sender, req.request_id))
            }
            "SET_VOLUME" => {
                let req: SetVolumeRequest = messages::parse_message(payload)?;
                // Apply before replying: the `RECEIVER_STATUS` a sender reads back is how
                // its slider learns where it ended up, so echoing the old value makes the
                // control snap home and look broken on top of doing nothing.
                let mut events = Vec::new();
                if let Some(level) = req.volume.level {
                    self.volume = level.clamp(0.0, 1.0);
                    events.push(SessionEvent::Control(ControlTxn::Volume(
                        castaway_core::Volume::from_position(self.volume),
                    )));
                }
                if let Some(muted) = req.volume.muted {
                    self.muted = muted;
                    events.push(SessionEvent::Control(ControlTxn::Mute(muted)));
                }
                let mut r = self.reply_receiver_status(&sender, req.request_id);
                r.events = events;
                Ok(r)
            }
            "STOP" => {
                self.app = None;
                // The media session belonged to the application. Leaving the player state
                // behind would have a later GET_STATUS describe playback inside an app
                // that is no longer running.
                self.player_state = None;
                let mut r = self.reply_receiver_status(&sender, env.request_id.unwrap_or(0));
                r.events.push(SessionEvent::End);
                Ok(r)
            }
            other => {
                debug!(kind = %other, "unhandled receiver message");
                Ok(Reaction::default())
            }
        }
    }

    fn handle_media(&mut self, msg: &CastMessage) -> Result<Reaction, CastError> {
        let payload = msg
            .payload_utf8
            .as_deref()
            .ok_or_else(|| CastError::Json("media message without payload".into()))?;
        let env = Envelope::parse(payload)?;
        let sender = msg.source_id.clone();
        let request_id = env.request_id.unwrap_or(0);
        match env.r#type.as_str() {
            "LOAD" => {
                let req: LoadRequest = messages::parse_message(payload)?;
                let uri = MediaUri::parse(&req.media.content_id)
                    .map_err(|_| CastError::InvalidMedia(req.media.content_id.clone()))?;
                let start = req
                    .current_time
                    .filter(|t| *t > 0.0)
                    .map(std::time::Duration::from_secs_f64);
                self.player_state = Some(messages::PlayerState::Playing);
                Ok(Reaction::reply_with(
                    vec![self.media_status_msg(&sender, request_id)],
                    SessionEvent::Play { source: uri, start },
                ))
            }
            "PLAY" => Ok(self.media_control(
                &sender,
                request_id,
                Some(messages::PlayerState::Playing),
                ControlTxn::Play,
            )),
            "PAUSE" => Ok(self.media_control(
                &sender,
                request_id,
                Some(messages::PlayerState::Paused),
                ControlTxn::Pause,
            )),
            "STOP" => {
                // Cast's media STOP unloads the item rather than pausing at zero, so the
                // session goes back to having no media at all.
                let mut r = self.media_control(&sender, request_id, None, ControlTxn::Stop);
                r.events.push(SessionEvent::Control(ControlTxn::Stop));
                Ok(r)
            }
            "SEEK" => {
                let secs = serde_json::from_str::<serde_json::Value>(payload)
                    .ok()
                    .and_then(|v| v.get("currentTime").and_then(serde_json::Value::as_f64))
                    .unwrap_or(0.0);
                let txn = ControlTxn::Seek(std::time::Duration::from_secs_f64(secs.max(0.0)));
                // A seek does not resume a paused item; keep whatever state we were in.
                let state = self.player_state;
                Ok(self.media_control(&sender, request_id, state, txn))
            }
            "GET_STATUS" => Ok(Reaction::reply(vec![
                self.media_status_msg(&sender, request_id)
            ])),
            other => {
                debug!(kind = %other, "unhandled media message");
                Ok(Reaction::default())
            }
        }
    }

    fn handle_webrtc(&mut self, msg: &CastMessage) -> Result<Reaction, CastError> {
        let payload = msg
            .payload_utf8
            .as_deref()
            .ok_or_else(|| CastError::Json("webrtc message without payload".into()))?;
        let env = Envelope::parse(payload)?;
        if env.r#type != "OFFER" {
            debug!(kind = %env.r#type, "unhandled webrtc message");
            return Ok(Reaction::default());
        }
        let sender = msg.source_id.clone();
        let seq_num = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| v.get("seqNum").and_then(serde_json::Value::as_i64))
            .unwrap_or(0);
        let Some(port) = self.mirror_port else {
            // Mirroring not enabled: decline so the sender doesn't hang.
            let nack = serde_json::json!({
                "type": "ANSWER",
                "seqNum": seq_num,
                "result": "error",
                "error": { "code": 1, "description": "mirroring disabled" },
            })
            .to_string();
            return Ok(Reaction::reply(vec![CastMessage::json(
                &msg.destination_id,
                &sender,
                crate::mirror::WEBRTC_NS,
                nack,
            )]));
        };
        let (answer, config) = crate::mirror::negotiate(payload, port)?;
        Ok(Reaction {
            outgoing: vec![CastMessage::json(
                &msg.destination_id,
                &sender,
                crate::mirror::WEBRTC_NS,
                answer,
            )],
            events: Vec::new(),
            start_mirror: Some(config),
            ..Reaction::default()
        })
    }

    /// Whether this receiver can host `app` right now — the answer a sender's availability
    /// query gets, and the answer that decides whether it offers this device at all.
    fn can_host(&self, app: App) -> bool {
        self.refusal_for(app).is_none()
    }

    /// `None` when we can host it; otherwise why not.
    ///
    /// Mirroring is conditional on the actor having bound an RTP socket, and the
    /// distinction matters to whoever reads the log: "we do not have that app" and "we
    /// have it and could not bind a socket" are different faults with different fixes.
    fn refusal_for(&self, app: App) -> Option<LaunchRefusal> {
        match app {
            App::DefaultMedia => None,
            App::Streaming if self.mirror_port.is_some() => None,
            App::Streaming => Some(LaunchRefusal::SystemError),
            // Hosting is decided by the actor, which is the only party that can resolve
            // the id and open a browser. Reaching here means the caller asked whether we
            // *could*, which for a page is yes whenever hosting exists at all — the
            // narrowing happens when the lookup comes back.
            App::Page => None,
            App::Unhostable => Some(LaunchRefusal::NotFound),
        }
    }

    /// The page for `pending` is up: record the session and answer the waiting sender.
    ///
    /// `namespaces` is what the application itself declared over the platform channel,
    /// and it becomes what `RECEIVER_STATUS` reports — a sender reads that list to
    /// decide what it may send.
    pub fn page_hosted(
        &mut self,
        pending: &PendingLaunch,
        display_name: &str,
        namespaces: Vec<String>,
    ) -> Vec<CastMessage> {
        self.app = Some(RunningApp {
            app_id: pending.app_id.clone(),
            display_name: display_name.to_owned(),
            session_id: pending.session_id.clone(),
            transport_id: pending.transport_id.clone(),
            status_text: display_name.to_owned(),
            controller: pending.sender.clone(),
            namespaces,
        });
        // The media plane is the application's now, so whatever we were reporting about
        // our own is no longer this session's truth.
        self.player_state = None;
        self.position = None;
        self.reply_receiver_status(&pending.sender, pending.request_id)
            .outgoing
    }

    /// The page for `pending` could not be hosted: tell the sender in its own vocabulary.
    #[must_use]
    pub fn page_refused(
        &self,
        pending: &PendingLaunch,
        refusal: LaunchRefusal,
    ) -> Vec<CastMessage> {
        warn!(
            app_id = %pending.app_id,
            reason = refusal.reason(),
            "declining a LAUNCH the browser could not host"
        );
        vec![CastMessage::json(
            &self.receiver_id,
            &pending.sender,
            ns::RECEIVER,
            messages::launch_error(pending.request_id, refusal),
        )]
    }

    /// The device volume and mute, as the session currently believes them.
    ///
    /// A hosted application draws its own slider from this, so it has to be the level
    /// senders are told about rather than a fresh 1.0 — an app that comes up claiming
    /// full volume on a panel at a quarter is a control that lies before it is touched.
    #[must_use]
    pub const fn output_volume(&self) -> (f32, bool) {
        (self.volume, self.muted)
    }

    /// The session id a hosted page should be told, if one is running.
    ///
    /// The page echoes it to the vendor's own cloud, so it has to be the same string
    /// `RECEIVER_STATUS` reports rather than a second one invented for the platform.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.app.as_ref().map(|a| a.session_id.as_str())
    }

    /// An application's message for a sender, addressed from the running session.
    ///
    /// `*` is a broadcast — how a receiver tells every sender on the connection about a
    /// status change it did not ask for.
    #[must_use]
    pub fn from_page(&self, namespace: &str, sender: &str, data: &str) -> CastMessage {
        let source = self
            .app
            .as_ref()
            .map_or(self.receiver_id.as_str(), |a| a.transport_id.as_str());
        CastMessage::json(source, sender, namespace, data.to_owned())
    }

    fn launch(&mut self, req: &LaunchRequest, controller: &str) {
        self.id_counter += 1;
        let n = self.id_counter;
        self.app = Some(RunningApp {
            app_id: req.app_id.clone(),
            display_name: "castaway".to_string(),
            session_id: format!("sess-{n}"),
            transport_id: format!("transport-{n}"),
            status_text: "Ready To Cast".to_string(),
            controller: controller.to_string(),
            // Serving the session ourselves: the media namespace is ours, and
            // `namespaces` reports it because this list is empty.
            namespaces: Vec::new(),
        });
    }

    fn reply_receiver_status(&self, sender: &str, request_id: i64) -> Reaction {
        let json =
            messages::receiver_status(request_id, self.app.as_ref(), self.volume, self.muted);
        Reaction::reply(vec![CastMessage::json(
            &self.receiver_id,
            sender,
            ns::RECEIVER,
            json,
        )])
    }

    fn media_control(
        &mut self,
        sender: &str,
        request_id: i64,
        player_state: Option<messages::PlayerState>,
        txn: ControlTxn,
    ) -> Reaction {
        self.player_state = player_state;
        Reaction::reply_with(
            vec![self.media_status_msg(sender, request_id)],
            SessionEvent::Control(txn),
        )
    }

    fn media_status_msg(&self, sender: &str, request_id: i64) -> CastMessage {
        // Media status is sent from the transport id when an app is running.
        let source = self
            .app
            .as_ref()
            .map_or(self.receiver_id.as_str(), |a| a.transport_id.as_str());
        let json = match self.player_state {
            Some(state) => messages::media_status(
                request_id,
                self.media_session_id,
                state,
                self.position.map(|p| p.position),
                self.volume,
                self.muted,
            ),
            None => messages::media_status_empty(request_id),
        };
        CastMessage::json(source, sender, ns::MEDIA, json)
    }
}

/// The destination id an unsolicited message is addressed to.
///
/// Cast has no per-sender subscription list: a status nobody asked for goes to everybody
/// on the connection, and `*` is how that is spelled.
const BROADCAST: &str = "*";

fn auth_err() -> DeviceAuthMessage {
    DeviceAuthMessage {
        challenge: None,
        response: None,
        error: Some(AuthError {
            error_type: auth_error::ErrorType::InternalError as i32,
        }),
    }
}

/// Convenience: does this message carry a JSON payload of the given `type`?
#[must_use]
pub fn payload_is_type(msg: &CastMessage, ty: &str) -> bool {
    msg.payload_type == PayloadType::String as i32
        && msg
            .payload_utf8
            .as_deref()
            .and_then(|p| Envelope::parse(p).ok())
            .is_some_and(|e| e.r#type == ty)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn session() -> CastSession {
        CastSession::new(None)
    }

    fn recv_msg(ns: &str, source: &str, dest: &str, json: &str) -> CastMessage {
        CastMessage::json(source, dest, ns, json.to_string())
    }

    fn payload(msg: &CastMessage) -> serde_json::Value {
        serde_json::from_str(msg.payload_utf8.as_deref().unwrap()).unwrap()
    }

    fn receiver_request(json: &str) -> CastMessage {
        recv_msg(ns::RECEIVER, "sender-0", "receiver-0", json)
    }

    /// A session on a receiver that can host applications, with `app_id` already known
    /// to be a page — the state after one registry lookup.
    fn hosting_session(app_id: &str) -> CastSession {
        let mut catalogue = messages::AppCatalogue::new(true);
        catalogue.record(app_id, true);
        let mut s = session();
        s.observe_catalogue(catalogue);
        s
    }

    /// Launch `app_id` and take the receiver all the way to hosting its page.
    fn launched(app_id: &str, namespaces: &[&str]) -> CastSession {
        let mut s = hosting_session(app_id);
        let r = s
            .handle(&receiver_request(&format!(
                r#"{{"requestId":1,"type":"LAUNCH","appId":"{app_id}"}}"#
            )))
            .unwrap();
        let pending = r.launch_page.expect("a page launch");
        s.page_hosted(
            &pending,
            "The App",
            namespaces.iter().map(|n| (*n).to_string()).collect(),
        );
        s
    }

    /// The sender is left waiting on purpose. Resolving the id is a lookup and hosting
    /// it needs a browser, and a `RECEIVER_STATUS` sent before the page exists is
    /// exactly the "connected phone, black panel" failure #16 opens with.
    #[test]
    fn launching_a_page_defers_instead_of_answering() {
        let mut s = hosting_session("233637DE");
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":9,"type":"LAUNCH","appId":"233637DE"}"#,
            ))
            .unwrap();
        assert!(
            r.outgoing.is_empty(),
            "nothing may be claimed before the page exists: {:?}",
            r.outgoing
        );
        assert_eq!(
            r.launch_page,
            Some(PendingLaunch {
                app_id: "233637DE".into(),
                request_id: 9,
                sender: "sender-0".into(),
                session_id: "sess-1".into(),
                transport_id: "transport-1".into(),
            })
        );
    }

    #[test]
    fn a_hosted_page_is_reported_to_the_sender_that_launched_it() {
        let mut s = hosting_session("9AC194DC");
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":4,"type":"LAUNCH","appId":"9AC194DC"}"#,
            ))
            .unwrap();
        let pending = r.launch_page.unwrap();
        let out = s.page_hosted(&pending, "Plex", vec![ns::MEDIA.to_string()]);
        let status = payload(&out[0]);
        assert_eq!(status["type"], "RECEIVER_STATUS");
        assert_eq!(status["requestId"], 4);
        let app = &status["status"]["applications"][0];
        assert_eq!(app["appId"], "9AC194DC");
        assert_eq!(app["displayName"], "Plex");
        assert_eq!(s.session_id(), Some(app["sessionId"].as_str().unwrap()));
    }

    #[test]
    fn a_page_that_cannot_be_hosted_is_declined_in_the_senders_vocabulary() {
        let mut s = hosting_session("DEADBEEF");
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":5,"type":"LAUNCH","appId":"DEADBEEF"}"#,
            ))
            .unwrap();
        let pending = r.launch_page.unwrap();
        let out = s.page_refused(&pending, LaunchRefusal::NotFound);
        let error = payload(&out[0]);
        assert_eq!(error["type"], "LAUNCH_ERROR");
        assert_eq!(error["reason"], "NOT_FOUND");
        assert_eq!(error["requestId"], 5);
    }

    /// The media namespace changing hands is the point of hosting an app: with YouTube
    /// up, YouTube answers `LOAD`. Answering alongside it would put two receivers on one
    /// session, replying twice to one request from two players that disagree.
    #[test]
    fn a_hosted_app_takes_the_media_namespace_off_us() {
        let mut s = launched("233637DE", &[ns::MEDIA]);
        let load = recv_msg(
            ns::MEDIA,
            "sender-0",
            "transport-1",
            r#"{"requestId":7,"type":"LOAD","media":{"contentId":"http://x/v.mp4"}}"#,
        );
        let r = s.handle(&load).unwrap();
        assert!(
            r.outgoing.is_empty() && r.events.is_empty(),
            "we must not answer for the application: {r:?}"
        );
        assert_eq!(r.to_page.len(), 1);
        assert_eq!(r.to_page[0].namespace, ns::MEDIA);
        assert_eq!(r.to_page[0].sender, "sender-0");
        assert!(r.to_page[0].data.contains("v.mp4"));
    }

    /// With no application running, the media namespace is ours again — the same `LOAD`
    /// plays through our own pipeline.
    #[test]
    fn the_media_namespace_comes_back_when_no_app_is_hosted() {
        let mut s = session();
        let load = recv_msg(
            ns::MEDIA,
            "sender-0",
            "receiver-0",
            r#"{"requestId":7,"type":"LOAD","media":{"contentId":"http://x/v.mp4"}}"#,
        );
        let r = s.handle(&load).unwrap();
        assert!(r.to_page.is_empty());
        assert!(matches!(r.events[0], SessionEvent::Play { .. }));
    }

    /// Transport is never the application's, whatever it declared. An app given
    /// `CONNECTION` could end sessions it does not own; given `DEVICE_AUTH` it would be
    /// handed a challenge it cannot sign, and the sender would walk away.
    #[test]
    fn an_application_cannot_claim_the_transport_namespaces() {
        let mut s = launched(
            "233637DE",
            &[ns::MEDIA, ns::CONNECTION, ns::HEARTBEAT, ns::DEVICE_AUTH],
        );
        let ping = recv_msg(
            ns::HEARTBEAT,
            "sender-0",
            "receiver-0",
            r#"{"type":"PING"}"#,
        );
        let r = s.handle(&ping).unwrap();
        assert!(r.to_page.is_empty(), "heartbeat must stay ours");
        assert_eq!(payload(&r.outgoing[0])["type"], "PONG");

        let close = recv_msg(ns::CONNECTION, "other", "receiver-0", r#"{"type":"CLOSE"}"#);
        assert!(s.handle(&close).unwrap().to_page.is_empty());
    }

    /// The Plex failure, as a test. An app id nobody has resolved yet is answered
    /// available whenever hosting is possible — because the alternative makes the device
    /// vanish from the picker with nothing said anywhere.
    #[test]
    fn an_unresolved_app_id_is_offered_when_a_browser_can_host_it() {
        let mut s = session();
        s.observe_catalogue(messages::AppCatalogue::new(true));
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":1,"type":"GET_APP_AVAILABILITY","appId":["9AC194DC"]}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["availability"]["9AC194DC"],
            "APP_AVAILABLE"
        );
    }

    #[test]
    fn a_receiver_with_no_browser_says_so_rather_than_accepting_a_launch_it_cannot_serve() {
        let mut s = session();
        s.observe_catalogue(messages::AppCatalogue::new(false));
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":1,"type":"GET_APP_AVAILABILITY","appId":["9AC194DC"]}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["availability"]["9AC194DC"],
            "APP_UNAVAILABLE"
        );
    }

    /// A native app id that is not one of the mirroring ones stays unhostable even with
    /// a browser: the registry said it has no page, and a page is the only thing the
    /// browser can host.
    #[test]
    fn a_known_native_app_is_not_offered_to_a_browser() {
        let mut catalogue = messages::AppCatalogue::new(true);
        catalogue.record("AAAAAAAA", false);
        let mut s = session();
        s.observe_catalogue(catalogue);
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":1,"type":"GET_APP_AVAILABILITY","appId":["AAAAAAAA"]}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["availability"]["AAAAAAAA"],
            "APP_UNAVAILABLE"
        );
    }

    /// An application's answer goes back out addressed from the session's transport id,
    /// which is what the sender opened its virtual connection to.
    #[test]
    fn an_applications_answer_is_addressed_from_the_session() {
        let s = launched("233637DE", &[ns::MEDIA]);
        let out = s.from_page(ns::MEDIA, "sender-0", r#"{"type":"MEDIA_STATUS"}"#);
        assert_eq!(out.destination_id, "sender-0");
        assert_eq!(out.namespace, ns::MEDIA);
        assert!(out.source_id.starts_with("transport-"));
    }

    /// A sender asks what this device can run *before* offering it in a picker. Leaving
    /// this unanswered is not a missing feature but an invisible one: the query times out
    /// and the receiver simply never appears as somewhere to cast to.
    #[test]
    fn app_availability_is_answered_for_what_we_host() {
        let mut s = session().with_mirror_port(51_234);
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":548,"type":"GET_APP_AVAILABILITY","appId":["CC1AD845","0F5096E8"]}"#,
            ))
            .unwrap();

        let p = payload(&r.outgoing[0]);
        assert_eq!(p["requestId"], 548);
        // `responseType`, not `type` — a sender does not recognise it under the other key.
        assert_eq!(p["responseType"], "GET_APP_AVAILABILITY");
        assert_eq!(p["availability"]["CC1AD845"], "APP_AVAILABLE");
        assert_eq!(p["availability"]["0F5096E8"], "APP_AVAILABLE");
    }

    #[test]
    fn app_availability_declines_apps_we_cannot_host() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":1,"type":"GET_APP_AVAILABILITY","appId":["CA5E8412"]}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["availability"]["CA5E8412"],
            "APP_UNAVAILABLE"
        );
    }

    /// Mirroring availability is not a constant: the actor binds the RTP socket, and
    /// without one there is nowhere for the stream to land. Claiming it anyway buys a
    /// sender that starts a session and sees nothing.
    #[test]
    fn mirroring_is_unavailable_without_an_rtp_socket() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":2,"type":"GET_APP_AVAILABILITY","appId":["0F5096E8"]}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["availability"]["0F5096E8"],
            "APP_UNAVAILABLE"
        );
    }

    /// The G56 failure, from the sender's side: launching Netflix used to get a status
    /// saying it had started, a session id, and a transport id nothing was listening on.
    #[test]
    fn launching_an_app_we_cannot_host_is_refused_rather_than_faked() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":7,"type":"LAUNCH","appId":"CA5E8412"}"#,
            ))
            .unwrap();

        let p = payload(&r.outgoing[0]);
        assert_eq!(p["type"], "LAUNCH_ERROR");
        assert_eq!(p["reason"], "NOT_FOUND");
        assert_eq!(p["requestId"], 7);
        assert!(
            s.app.is_none(),
            "a refused launch must not leave an application running"
        );
    }

    /// Same refusal, different cause, and the sender is told which: we do have the
    /// streaming receiver, we just have nowhere to receive.
    #[test]
    fn launching_mirroring_without_a_socket_says_system_error() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":8,"type":"LAUNCH","appId":"85CDB22F"}"#,
            ))
            .unwrap();
        assert_eq!(payload(&r.outgoing[0])["reason"], "SYSTEM_ERROR");
    }

    #[test]
    fn launching_the_streaming_receiver_succeeds_when_mirroring_is_possible() {
        let mut s = session().with_mirror_port(51_234);
        let r = s
            .handle(&receiver_request(
                r#"{"requestId":9,"type":"LAUNCH","appId":"0F5096E8"}"#,
            ))
            .unwrap();
        assert!(payload_is_type(&r.outgoing[0], "RECEIVER_STATUS"));
        assert_eq!(s.app.as_ref().unwrap().app_id, "0F5096E8");
    }

    /// The Android and iOS streaming ids are the same feature as the desktop pair. A
    /// receiver that knows only some of them works from one sender and not another, for
    /// no reason anybody in the room could work out.
    #[test]
    fn every_streaming_app_id_is_recognised() {
        // With hosting on, so the streaming ids are being told apart from pages rather
        // than merely from a receiver that cannot host anything.
        let catalogue = messages::AppCatalogue::new(true);
        for id in [
            "0F5096E8", "85CDB22F", "674A0243", "8E6C866D", "96084372", "BFD92C23",
        ] {
            assert_eq!(
                App::classify(id, &catalogue),
                App::Streaming,
                "{id} not recognised"
            );
        }
        assert_eq!(App::classify("CC1AD845", &catalogue), App::DefaultMedia);
        // YouTube's own receiver is a page now, which is the whole of #16.
        assert_eq!(App::classify("233637DE", &catalogue), App::Page);
        // …and on a build with no browser it is honestly unhostable again.
        assert_eq!(
            App::classify("233637DE", &messages::AppCatalogue::new(false)),
            App::Unhostable
        );
    }

    /// Chrome's cast dialog has a volume slider. Unhandled, it moved and nothing happened
    /// — and the status the sender read back still said 1.0, so the control snapped home
    /// and looked broken on top of doing nothing.
    #[test]
    fn set_volume_reaches_the_pipeline_and_the_status_agrees() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"type":"SET_VOLUME","requestId":3,"volume":{"level":0.25}}"#,
            ))
            .unwrap();

        assert!(matches!(
            r.events.first(),
            Some(SessionEvent::Control(ControlTxn::Volume(v)))
                if *v == castaway_core::Volume::from_position(0.25)
        ));
        let p = payload(&r.outgoing[0]);
        assert!((p["status"]["volume"]["level"].as_f64().unwrap() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn set_volume_carries_mute_separately() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"type":"SET_VOLUME","requestId":4,"volume":{"muted":true}}"#,
            ))
            .unwrap();
        assert!(matches!(
            r.events.first(),
            Some(SessionEvent::Control(ControlTxn::Mute(true)))
        ));
        assert_eq!(payload(&r.outgoing[0])["status"]["volume"]["muted"], true);
    }

    /// The case that made `Reaction` carry a list: one message meaning two things. With a
    /// single slot the mute would have been dropped, and a mute that never arrives is a
    /// mute that never lifts.
    #[test]
    fn a_level_and_a_mute_in_one_message_both_reach_the_pipeline() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"type":"SET_VOLUME","requestId":5,"volume":{"level":0.5,"muted":true}}"#,
            ))
            .unwrap();
        assert_eq!(r.events.len(), 2, "{:?}", r.events);
    }

    #[test]
    fn a_volume_outside_the_range_is_clamped_not_forwarded() {
        let mut s = session();
        let r = s
            .handle(&receiver_request(
                r#"{"type":"SET_VOLUME","requestId":6,"volume":{"level":9.0}}"#,
            ))
            .unwrap();
        assert!(matches!(
            r.events.first(),
            Some(SessionEvent::Control(ControlTxn::Volume(v)))
                if *v == castaway_core::Volume::FULL
        ));
    }

    /// Nothing loaded is an empty status array, not a status object claiming to play.
    /// The old unconditional `PLAYING` told a sender's UI to show a transport bar for
    /// media that did not exist.
    #[test]
    fn media_status_with_nothing_loaded_is_empty() {
        let mut s = session();
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"GET_STATUS","requestId":1}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["status"].as_array().unwrap().len(),
            0
        );
    }

    /// And having paused, asking must not be told playback resumed.
    #[test]
    fn media_status_reports_the_state_we_are_actually_in() {
        let mut s = session();
        let load = r#"{"type":"LOAD","requestId":1,"media":{"contentId":"http://h/v.mp4"}}"#;
        s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
            .unwrap();
        s.handle(&recv_msg(
            ns::MEDIA,
            "sender-0",
            "receiver-0",
            r#"{"type":"PAUSE","requestId":2}"#,
        ))
        .unwrap();

        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"GET_STATUS","requestId":3}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["status"][0]["playerState"],
            "PAUSED"
        );
    }

    /// A sender's scrubber is drawn from `currentTime` and nothing else, and it was a
    /// hardcoded zero — knowingly, because nothing on the pipeline side reported a position.
    /// Something does now.
    #[test]
    fn the_position_a_sender_draws_its_scrubber_from_is_a_real_one() {
        let mut s = session();
        let load = r#"{"type":"LOAD","requestId":1,"media":{"contentId":"http://h/v.mp4"}}"#;
        s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
            .unwrap();

        s.observe_progress(Some(
            castaway_core::PlaybackProgress::at(std::time::Duration::from_millis(95_500))
                .of(std::time::Duration::from_secs(600)),
        ));
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"GET_STATUS","requestId":2}"#,
            ))
            .unwrap();
        let time = payload(&r.outgoing[0])["status"][0]["currentTime"]
            .as_f64()
            .unwrap();
        assert!((time - 95.5).abs() < 0.01, "currentTime was {time}");

        // Nothing playing is zero rather than absent: the field is not optional on the
        // wire, and a sender sees this only for the length of a fetch.
        s.observe_progress(None);
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"GET_STATUS","requestId":3}"#,
            ))
            .unwrap();
        assert_eq!(payload(&r.outgoing[0])["status"][0]["currentTime"], 0.0);
    }

    /// The gap Cast had and nobody logged: a sender is told `PLAYING` forever, so its queue
    /// never advances and a URL the box could not fetch is indistinguishable from a cast
    /// that is working.
    #[test]
    fn the_end_of_an_item_is_broadcast_with_the_reason_it_ended() {
        for (end, want) in [
            (castaway_core::PlaybackEnd::Finished, "FINISHED"),
            (
                castaway_core::PlaybackEnd::Failed("connection refused".into()),
                "ERROR",
            ),
        ] {
            let mut s = session();
            let load = r#"{"type":"LOAD","requestId":1,"media":{"contentId":"http://h/v.mp4"}}"#;
            s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
                .unwrap();

            let out = s.media_ended(&end);
            assert_eq!(out.len(), 1, "exactly one status, broadcast");
            // Unsolicited, so it goes to every sender on the connection and carries the
            // request id a sender reads as "you did not ask for this".
            assert_eq!(out[0].destination_id, "*");
            let status = &payload(&out[0])["status"][0];
            assert_eq!(status["playerState"], "IDLE");
            assert_eq!(status["idleReason"], want);
        }
    }

    /// A decode thread noticing on its own schedule that it was torn down arrives after
    /// the session it belonged to is gone. Announcing an item that was never loaded would
    /// have a sender show an error for a cast it never made.
    #[test]
    fn an_end_with_nothing_loaded_is_not_announced() {
        let mut s = session();
        assert!(s
            .media_ended(&castaway_core::PlaybackEnd::Finished)
            .is_empty());
    }

    /// `supportedMediaCommands` is a claim, and it was a false one: SEEK was advertised
    /// here while `RenderPipeline::control` refused it, so a sender drew a scrubber that
    /// did nothing. This holds the bitmask to the capability set the panel is built from,
    /// which is itself derived from what the pipeline honours.
    #[test]
    fn supported_commands_match_what_the_pipeline_honours() {
        use castaway_core::ControlCapabilities;
        // PAUSE | SEEK | STREAM_VOLUME | STREAM_MUTE, as Cast numbers them.
        const PAUSE: u32 = 1;
        const SEEK: u32 = 1 << 1;
        const STREAM_VOLUME: u32 = 1 << 2;
        const STREAM_MUTE: u32 = 1 << 3;

        let caps = crate::control::CastRemote::capabilities();
        let claimed = messages::SUPPORTED_MEDIA_COMMANDS;
        for (bit, name, txn) in [
            (PAUSE, "PAUSE", ControlTxn::Pause),
            (
                SEEK,
                "SEEK",
                ControlTxn::Seek(std::time::Duration::from_secs(1)),
            ),
            (
                STREAM_VOLUME,
                "STREAM_VOLUME",
                ControlTxn::Volume(castaway_core::Volume::from_position(0.5)),
            ),
            (STREAM_MUTE, "STREAM_MUTE", ControlTxn::Mute(true)),
        ] {
            assert_eq!(
                claimed & bit != 0,
                ControlCapabilities::supports(caps, &txn),
                "supportedMediaCommands claims {name} but the pipeline disagrees",
            );
        }
    }

    /// A `LOAD` that says where to start has to start there. Cast senders send this to
    /// resume, and it was extracted here and then ignored by the pipeline — so resuming a
    /// film restarted it.
    #[test]
    fn a_load_that_names_a_start_position_carries_it_to_the_pipeline() {
        let mut s = session();
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"LOAD","requestId":1,"currentTime":312.5,
                    "media":{"contentId":"http://h/film.mp4"}}"#,
            ))
            .unwrap();
        match r.events.first() {
            Some(SessionEvent::Play { start, .. }) => assert_eq!(
                *start,
                Some(std::time::Duration::from_millis(312_500)),
                "the resume point was dropped"
            ),
            other => panic!("expected a play, got {other:?}"),
        }
    }

    /// A press on the glass and the phone that started the cast are two views of one
    /// session. A receiver that moved for one and not the other leaves the sender's pause
    /// button toggling playback back on.
    #[test]
    fn a_control_from_the_panel_is_broadcast_to_the_senders() {
        let mut s = session();
        let load = r#"{"type":"LOAD","requestId":1,"media":{"contentId":"http://h/v.mp4"}}"#;
        s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
            .unwrap();

        let out = s.apply_local_control(&ControlTxn::Pause);
        assert_eq!(out[0].destination_id, "*");
        assert_eq!(payload(&out[0])["status"][0]["playerState"], "PAUSED");

        // Cast's STOP unloads rather than pausing at zero, so the status goes back to the
        // empty array that means "there is no media session here".
        let out = s.apply_local_control(&ControlTxn::Stop);
        assert_eq!(
            payload(&out[0])["status"].as_array().unwrap().len(),
            0,
            "a stopped Cast session has no media, not media that is stopped"
        );
    }

    /// A seek is not a resume. Reporting PLAYING for a seek while paused would have the
    /// sender's UI show playback running against a still picture.
    #[test]
    fn seeking_while_paused_stays_paused() {
        let mut s = session();
        let load = r#"{"type":"LOAD","requestId":1,"media":{"contentId":"http://h/v.mp4"}}"#;
        s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
            .unwrap();
        s.handle(&recv_msg(
            ns::MEDIA,
            "sender-0",
            "receiver-0",
            r#"{"type":"PAUSE","requestId":2}"#,
        ))
        .unwrap();
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"SEEK","requestId":3,"currentTime":30}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["status"][0]["playerState"],
            "PAUSED"
        );
    }

    /// Cast's media STOP unloads rather than pausing at zero, so what follows is a session
    /// with no media — an empty array, not `IDLE`, which the wire does not have a slot for.
    #[test]
    fn media_stop_leaves_no_media_session() {
        let mut s = session();
        let load = r#"{"type":"LOAD","requestId":1,"media":{"contentId":"http://h/v.mp4"}}"#;
        s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
            .unwrap();
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"STOP","requestId":2}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["status"].as_array().unwrap().len(),
            0
        );
    }

    /// Stopping the application ends the media session with it — otherwise a later
    /// GET_STATUS describes playback inside an app that is no longer running.
    #[test]
    fn stopping_the_app_ends_the_media_session_too() {
        let mut s = session();
        s.handle(&receiver_request(
            r#"{"requestId":1,"type":"LAUNCH","appId":"CC1AD845"}"#,
        ))
        .unwrap();
        let load = r#"{"type":"LOAD","requestId":2,"media":{"contentId":"http://h/v.mp4"}}"#;
        s.handle(&recv_msg(ns::MEDIA, "sender-0", "receiver-0", load))
            .unwrap();
        s.handle(&receiver_request(r#"{"requestId":3,"type":"STOP"}"#))
            .unwrap();

        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"GET_STATUS","requestId":4}"#,
            ))
            .unwrap();
        assert_eq!(
            payload(&r.outgoing[0])["status"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn ping_gets_pong() {
        let mut s = session();
        let msg = recv_msg(
            ns::HEARTBEAT,
            "sender-0",
            "receiver-0",
            r#"{"type":"PING"}"#,
        );
        let r = s.handle(&msg).unwrap();
        assert_eq!(r.outgoing.len(), 1);
        assert!(payload_is_type(&r.outgoing[0], "PONG"));
        assert_eq!(r.outgoing[0].destination_id, "sender-0");
    }

    #[test]
    fn get_status_reports_no_app_then_launch_runs_one() {
        let mut s = session();
        let r = s
            .handle(&recv_msg(
                ns::RECEIVER,
                "sender-0",
                "receiver-0",
                r#"{"type":"GET_STATUS","requestId":1}"#,
            ))
            .unwrap();
        assert!(r.outgoing[0]
            .payload_utf8
            .as_ref()
            .unwrap()
            .contains("\"applications\":[]"));

        let r = s
            .handle(&recv_msg(
                ns::RECEIVER,
                "sender-0",
                "receiver-0",
                r#"{"type":"LAUNCH","requestId":2,"appId":"CC1AD845"}"#,
            ))
            .unwrap();
        let status = r.outgoing[0].payload_utf8.as_ref().unwrap();
        assert!(status.contains("transport-1"));
        assert!(status.contains("CC1AD845"));
    }

    #[test]
    fn load_emits_play_event() {
        let mut s = session();
        // Launch first so media status is sourced from the transport id.
        s.handle(&recv_msg(
            ns::RECEIVER,
            "sender-0",
            "receiver-0",
            r#"{"type":"LAUNCH","requestId":1,"appId":"CC1AD845"}"#,
        ))
        .unwrap();
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "transport-1",
                r#"{"type":"LOAD","requestId":2,"media":{"contentId":"https://x/v.mp4","contentType":"video/mp4","streamType":"BUFFERED"}}"#,
            ))
            .unwrap();
        match r.events.first() {
            Some(SessionEvent::Play { source, .. }) => {
                assert_eq!(source.to_string(), "https://x/v.mp4");
            }
            _ => panic!("expected Play"),
        }
        assert_eq!(r.outgoing[0].source_id, "transport-1");
    }

    #[test]
    fn pause_emits_control() {
        let mut s = session();
        let r = s
            .handle(&recv_msg(
                ns::MEDIA,
                "sender-0",
                "receiver-0",
                r#"{"type":"PAUSE","requestId":5}"#,
            ))
            .unwrap();
        assert!(matches!(
            r.events.first(),
            Some(SessionEvent::Control(ControlTxn::Pause))
        ));
    }

    /// CLOSE tears down one virtual connection, and only the session owner's CLOSE
    /// ends the running app. Several senders share the socket — Chrome's platform
    /// sender, a page's Cast SDK client, the one actually casting — and a bystander
    /// closing its own connection must not take the session with it: a mirrored tab
    /// navigating to youtube.com does exactly that.
    #[test]
    fn only_the_controlling_senders_close_ends_the_session() {
        let mut s = session();
        s.handle(&recv_msg(
            ns::RECEIVER,
            "sender-1",
            "receiver-0",
            r#"{"requestId":1,"type":"LAUNCH","appId":"CC1AD845"}"#,
        ))
        .unwrap();

        // A bystander leaving is its own business.
        let r = s
            .handle(&recv_msg(
                ns::CONNECTION,
                "client-777",
                "receiver-0",
                r#"{"type":"CLOSE"}"#,
            ))
            .unwrap();
        assert!(r.events.is_empty());

        // The sender that launched the app leaving ends it.
        let r = s
            .handle(&recv_msg(
                ns::CONNECTION,
                "sender-1",
                "receiver-0",
                r#"{"type":"CLOSE"}"#,
            ))
            .unwrap();
        assert!(matches!(r.events.first(), Some(SessionEvent::End)));
    }

    #[test]
    fn a_close_with_nothing_running_ends_nothing() {
        let mut s = session();
        let r = s
            .handle(&recv_msg(
                ns::CONNECTION,
                "sender-0",
                "receiver-0",
                r#"{"type":"CLOSE"}"#,
            ))
            .unwrap();
        assert!(r.events.is_empty());
    }

    #[test]
    fn webrtc_offer_negotiates_and_starts_mirror() {
        let mut s = session().with_mirror_port(51000);
        let offer = r#"{"type":"OFFER","seqNum":9,"offer":{"supportedStreams":[
          {"index":0,"type":"video_source","codecName":"h264","rtpPayloadType":96,"ssrc":5,
           "aesKey":"000102030405060708090a0b0c0d0e0f","aesIvMask":"0f0e0d0c0b0a09080706050403020100"}]}}"#;
        let msg = recv_msg(crate::mirror::WEBRTC_NS, "sender-0", "receiver-0", offer);
        let r = s.handle(&msg).unwrap();
        assert!(r.start_mirror.is_some());
        assert!(r.outgoing[0]
            .payload_utf8
            .as_ref()
            .unwrap()
            .contains("\"result\":\"ok\""));
    }

    #[test]
    fn webrtc_offer_declined_when_mirroring_disabled() {
        let mut s = session(); // no mirror port
        let offer = r#"{"type":"OFFER","seqNum":9,"offer":{"supportedStreams":[
          {"index":0,"type":"video_source","codecName":"h264","ssrc":5,
           "aesKey":"000102030405060708090a0b0c0d0e0f","aesIvMask":"0f0e0d0c0b0a09080706050403020100"}]}}"#;
        let msg = recv_msg(crate::mirror::WEBRTC_NS, "sender-0", "receiver-0", offer);
        let r = s.handle(&msg).unwrap();
        assert!(r.start_mirror.is_none());
        assert!(r.outgoing[0]
            .payload_utf8
            .as_ref()
            .unwrap()
            .contains("\"result\":\"error\""));
    }

    #[test]
    fn device_auth_without_signer_returns_error() {
        let mut s = session();
        let dam = DeviceAuthMessage {
            challenge: Some(crate::proto::AuthChallenge::default()),
            response: None,
            error: None,
        };
        let mut buf = Vec::new();
        dam.encode(&mut buf).unwrap();
        let msg = CastMessage::binary("sender-0", "receiver-0", ns::DEVICE_AUTH, buf);
        let r = s.handle(&msg).unwrap();
        let reply =
            DeviceAuthMessage::decode(r.outgoing[0].payload_binary.as_deref().unwrap()).unwrap();
        assert!(reply.error.is_some());
    }
}
