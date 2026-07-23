//! The pure CASTv2 session state machine (ground rule 3): fold an incoming
//! [`CastMessage`] into `(outgoing messages, optional SessionEvent)`. No sockets, no
//! TLS, no timers — the actor drives it. This is what makes the wire-fixture tests
//! possible without a real Chrome sender.

use castaway_core::{ControlTxn, MediaUri, SessionEvent};
use prost::Message as _;
use tracing::{debug, warn};

use crate::error::CastError;
use crate::messages::{self, ns, Envelope, LaunchRequest, LoadRequest, RunningApp};
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
    /// A session event to forward to the session manager, if any.
    pub event: Option<SessionEvent>,
    /// A negotiated mirroring config the actor should start receiving on. The session
    /// can't emit [`SessionEvent::Mirror`] itself (that needs an I/O-fed frame channel),
    /// so it surfaces the config and the actor wires the RTP receiver + `FrameSource`.
    pub start_mirror: Option<crate::mirror::MirrorConfig>,
}

impl Reaction {
    /// A reaction that only writes messages back.
    fn reply(outgoing: Vec<CastMessage>) -> Self {
        Self {
            outgoing,
            event: None,
            start_mirror: None,
        }
    }

    /// A reaction that writes messages back and forwards a session event.
    fn reply_with(outgoing: Vec<CastMessage>, event: SessionEvent) -> Self {
        Self {
            outgoing,
            event: Some(event),
            start_mirror: None,
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
    id_counter: u64,
    auth: Option<Box<dyn DeviceAuthResponder>>,
    /// UDP port the actor pre-bound for mirroring RTP, if mirroring is enabled.
    mirror_port: Option<u16>,
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
            id_counter: 0,
            auth,
            mirror_port: None,
        }
    }

    /// Enable mirroring: the actor pre-binds a UDP socket and passes its `port` so the
    /// negotiator can put it in the `ANSWER`.
    #[must_use]
    pub fn with_mirror_port(mut self, port: u16) -> Self {
        self.mirror_port = Some(port);
        self
    }

    /// Fold one inbound message into a [`Reaction`].
    ///
    /// # Errors
    /// [`CastError`] on undecodable payloads.
    pub fn handle(&mut self, msg: &CastMessage) -> Result<Reaction, CastError> {
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
        // A CLOSE to the receiver ends the whole session; virtual-connection setup
        // (CONNECT) is silent.
        if ty == "CLOSE" && msg.destination_id == self.receiver_id {
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
            "LAUNCH" => {
                let req: LaunchRequest =
                    serde_json::from_str(payload).map_err(|e| CastError::Json(e.to_string()))?;
                self.launch(&req);
                Ok(self.reply_receiver_status(&sender, req.request_id))
            }
            "STOP" => {
                self.app = None;
                let mut r = self.reply_receiver_status(&sender, env.request_id.unwrap_or(0));
                r.event = Some(SessionEvent::End);
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
                let req: LoadRequest =
                    serde_json::from_str(payload).map_err(|e| CastError::Json(e.to_string()))?;
                let uri = MediaUri::parse(&req.media.content_id)
                    .map_err(|_| CastError::InvalidMedia(req.media.content_id.clone()))?;
                let start = req
                    .current_time
                    .filter(|t| *t > 0.0)
                    .map(std::time::Duration::from_secs_f64);
                Ok(Reaction::reply_with(
                    vec![self.media_status_msg(&sender, request_id, "PLAYING")],
                    SessionEvent::Play { source: uri, start },
                ))
            }
            "PLAY" => Ok(self.media_control(&sender, request_id, "PLAYING", ControlTxn::Play)),
            "PAUSE" => Ok(self.media_control(&sender, request_id, "PAUSED", ControlTxn::Pause)),
            "STOP" => {
                let mut r = self.media_control(&sender, request_id, "IDLE", ControlTxn::Stop);
                r.event = Some(SessionEvent::Control(ControlTxn::Stop));
                Ok(r)
            }
            "SEEK" => {
                let secs = serde_json::from_str::<serde_json::Value>(payload)
                    .ok()
                    .and_then(|v| v.get("currentTime").and_then(serde_json::Value::as_f64))
                    .unwrap_or(0.0);
                let txn = ControlTxn::Seek(std::time::Duration::from_secs_f64(secs.max(0.0)));
                Ok(self.media_control(&sender, request_id, "PLAYING", txn))
            }
            "GET_STATUS" => Ok(Reaction::reply(vec![
                self.media_status_msg(&sender, request_id, "PLAYING")
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
            event: None,
            start_mirror: Some(config),
        })
    }

    fn launch(&mut self, req: &LaunchRequest) {
        self.id_counter += 1;
        let n = self.id_counter;
        self.app = Some(RunningApp {
            app_id: req.app_id.clone(),
            display_name: "castaway".to_string(),
            session_id: format!("sess-{n}"),
            transport_id: format!("transport-{n}"),
            status_text: "Ready To Cast".to_string(),
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
        &self,
        sender: &str,
        request_id: i64,
        player_state: &str,
        txn: ControlTxn,
    ) -> Reaction {
        Reaction::reply_with(
            vec![self.media_status_msg(sender, request_id, player_state)],
            SessionEvent::Control(txn),
        )
    }

    fn media_status_msg(&self, sender: &str, request_id: i64, player_state: &str) -> CastMessage {
        // Media status is sent from the transport id when an app is running.
        let source = self
            .app
            .as_ref()
            .map_or(self.receiver_id.as_str(), |a| a.transport_id.as_str());
        let json = messages::media_status(request_id, self.media_session_id, player_state);
        CastMessage::json(source, sender, ns::MEDIA, json)
    }
}

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
        match r.event {
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
            r.event,
            Some(SessionEvent::Control(ControlTxn::Pause))
        ));
    }

    #[test]
    fn connection_close_to_receiver_ends_session() {
        let mut s = session();
        let r = s
            .handle(&recv_msg(
                ns::CONNECTION,
                "sender-0",
                "receiver-0",
                r#"{"type":"CLOSE"}"#,
            ))
            .unwrap();
        assert!(matches!(r.event, Some(SessionEvent::End)));
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
