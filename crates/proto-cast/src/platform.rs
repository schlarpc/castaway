//! The Cast **receiver platform** protocol: what a hosted receiver page expects the
//! device underneath it to be (#16). Pure and socket-free (ground rule 3);
//! [`crate::platform_actor`] is the WebSocket shell that composes it with I/O.
//!
//! ## What this is, and how it is known
//!
//! A Cast application is a web page, and that page does not talk to senders directly. It
//! loads Google's receiver SDK, and the SDK talks to a *platform* over a local WebSocket:
//!
//! ```text
//! g.open = function(){ this.ga.open("ws://localhost:" + dc("port-for-web-server") + "/v2/ipc") };
//! g.send = function(a,b,c){ this.ga.send(JSON.stringify({namespace:a, senderId:b, data:c})) };
//! ```
//!
//! — `cast_receiver.js` v2.0.0, `cast.receiver.IpcChannel`. Every field here was read off
//! that bundle and off `caf_receiver_framework.js` v3, both pinned by
//! `nix/cast-receiver-sdk.nix`. The two SDK generations differ enormously above this
//! layer and are **identical at it**: same URL, same frame, same default port. So one
//! platform serves YouTube and Plex (which load v2) and the Default Media Receiver
//! (which loads CAF v3).
//!
//! ## The frame
//!
//! `{namespace, senderId, data}` in both directions, and `data` is always a **string** —
//! the SDK `JSON.stringify`s before sending and `JSON.parse`s on receipt, per namespace
//! (`g.Ie`/`g.je`). A frame carrying an object where a string belongs is silently dropped
//! by the page's own validator, which checks `a.namespace && a.senderId && a.data`.
//!
//! ## The handshake, which is a real gate
//!
//! The page speaks first. On socket open the SDK sends its own `ready`; the platform
//! answers with the `ready` that names the session. Until that answer arrives the SDK
//! warns "Application should not send requests before the system is ready (they will be
//! ignored)" and the app is inert. [`PlatformSession`] models the two states so that
//! sending app traffic to a page that has not identified itself does not typecheck.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::CastError;
use crate::ids::{AppId, SenderId, SessionId};

/// The namespace the platform control protocol lives on. Reserved: the SDK throws
/// `Protected namespace` if an application tries to open a bus on it.
pub const SYSTEM_NS: &str = "urn:x-cast:com.google.cast.system";

/// The sender id system traffic is addressed from and to. Not a real sender — the SDK
/// synthesises it for the platform's own side of the conversation.
pub const SYSTEM_SENDER: &str = "SystemSender";

/// The path the SDK opens. Not configurable on the page's side, so not configurable here.
pub const IPC_PATH: &str = "/v2/ipc";

/// The port the SDK reaches for when the platform injects no `__platform__`.
///
/// From the SDK's own default table (`ac["port-for-web-server"]="8008"`), which is also
/// the port a real Chromecast serves. Ours is bound on loopback only — see
/// [`crate::platform_actor`] for why that is not a detail.
pub const DEFAULT_PLATFORM_PORT: u16 = 8008;

/// One frame on the platform channel.
///
/// `data` is a `String` and not a `serde_json::Value` deliberately: it is a string on the
/// wire for every namespace, and for the ones this receiver merely relays it is *opaque*
/// — parsing it here would be inventing an interest in a vendor's private protocol that
/// #16 decided we must not take (`proto-cast` never learns YouTube exists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcFrame {
    /// The Cast namespace this belongs to.
    pub namespace: String,
    /// Which sender it is from, or for. [`SYSTEM_SENDER`] for platform control.
    #[serde(rename = "senderId")]
    pub sender_id: String,
    /// The payload, as a string. JSON namespaces carry JSON in it.
    pub data: String,
}

impl IpcFrame {
    /// A frame on an application namespace.
    #[must_use]
    pub fn app(
        namespace: impl Into<String>,
        sender_id: impl Into<String>,
        data: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            sender_id: sender_id.into(),
            data: data.into(),
        }
    }

    /// A platform control frame.
    fn system(data: String) -> Self {
        Self {
            namespace: SYSTEM_NS.to_owned(),
            sender_id: SYSTEM_SENDER.to_owned(),
            data,
        }
    }

    /// Whether this is platform control rather than application traffic.
    #[must_use]
    pub fn is_system(&self) -> bool {
        self.namespace == SYSTEM_NS
    }

    /// Encode for the socket.
    ///
    /// # Errors
    /// [`CastError::Encode`] if the frame will not serialise, which cannot happen for
    /// three owned strings but is not worth an `unwrap` to assert (ground rule 7).
    pub fn encode(&self) -> Result<String, CastError> {
        serde_json::to_string(self).map_err(|e| CastError::Encode(e.to_string()))
    }

    /// Decode one frame off the socket.
    ///
    /// # Errors
    /// [`CastError::Json`] if it is not a frame. The payload travels in the error: a
    /// receiver page that sends something unexpected is only diagnosable from what it
    /// actually sent.
    pub fn decode(text: &str) -> Result<Self, CastError> {
        serde_json::from_str(text).map_err(|e| CastError::Json(format!("{e} in frame {text}")))
    }
}

/// Why a sender's connection went away, in the SDK's own vocabulary.
///
/// The SDK maps these to its public enum and defaults anything else to `unknown`
/// (`$d`), so the set is closed and worth spelling out rather than passing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The sender closed its virtual connection — an ordinary "stop casting".
    ClosedByPeer,
    /// The transport carried something unusable.
    InvalidMessage,
    /// The connection went away without saying why (the socket dropped).
    Unknown,
}

impl DisconnectReason {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ClosedByPeer => "closed_by_peer",
            Self::InvalidMessage => "transport_invalid_message",
            Self::Unknown => "unknown",
        }
    }
}

/// What the platform tells the page about the device it is running on.
///
/// These reach the page as `deviceCapabilities` inside `ready`, and the page picks a
/// rendition from them. **Every field is a promise**, in exactly the sense
/// `proto-airplay`'s feature bits are: claim HDR here with no HDR panel and a receiver
/// negotiates a stream it cannot show. So the defaults are all `false` and the app crate
/// fills in what the panel actually is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DeviceCapabilities {
    /// The display can show HDR10.
    pub is_hdr_supported: bool,
    /// The display can show Dolby Vision.
    pub is_dv_supported: bool,
    /// The output can carry Dolby Atmos.
    pub is_dolby_atmos_supported: bool,
    /// The device is registered for development, which unlocks unpublished receivers.
    pub is_device_registered: bool,
    /// HLS with `cbcs` sample encryption is playable.
    pub is_cbcs_supported: bool,
}

/// The identity of the application session, as the page is told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIdentity {
    /// The app id the sender launched.
    pub application_id: AppId,
    /// The registry's name for it.
    pub application_name: String,
    /// The session id `RECEIVER_STATUS` reports for the same session. The page echoes
    /// it to its own cloud services, so the two must not drift.
    pub session_id: SessionId,
    /// The sender that launched it.
    pub launching_sender_id: SenderId,
    /// What the panel is, for the page's own telemetry.
    pub icon_url: Option<String>,
}

/// What the page said about itself when it came up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PageReady {
    /// Namespaces the application wants relayed. `RECEIVER_STATUS` has to report these,
    /// because a sender reads that list to decide what it may send.
    #[serde(rename = "activeNamespaces", default)]
    pub active_namespaces: Vec<String>,
    /// The status line the app wants shown, if it set one.
    #[serde(rename = "statusText", default)]
    pub status_text: Option<String>,
    /// The receiver SDK's version, for the log. Worth capturing: it is the one field
    /// that says which SDK generation is actually running.
    #[serde(default)]
    pub version: Option<String>,
}

/// Something the platform must act on outside this module.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PlatformEvent {
    /// The application identified itself and is ready to receive senders.
    AppReady(Box<PageReady>),
    /// The application asked for the device volume to move. Absolute, `0.0..=1.0`.
    SetVolume(f32),
    /// The application asked for the device to mute or unmute.
    SetMuted(bool),
    /// The application wants the status line changed.
    StatusText(String),
    /// The application asked to be pinged at least this often, and to be torn down if
    /// the platform stops answering.
    Heartbeat(Duration),
    /// The application asked for the sleep-timer overlay. Nothing on this panel draws
    /// one; surfaced rather than dropped so the log says it was asked for.
    SleepTimerOverlay,
    /// Diagnostic feedback the application wants recorded.
    Feedback(String),
}

/// Where a [`PlatformSession`] has got to.
///
/// Two states, and the transition is the handshake. Modelled as an enum rather than a
/// `bool` because the *data* differs: before the page identifies itself there are no
/// namespaces, so there is nothing to route by, and a relay attempted in that state is a
/// bug rather than a message to drop quietly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageState {
    /// The socket is open; the page has not sent its `ready` yet.
    Connecting,
    /// The page identified itself and declared what it speaks.
    Ready(PageReady),
}

/// The result of folding one input.
#[derive(Debug, Default, PartialEq)]
pub struct PlatformReaction {
    /// Frames to write to the page.
    pub to_page: Vec<IpcFrame>,
    /// What the rest of the receiver has to act on.
    pub events: Vec<PlatformEvent>,
}

impl PlatformReaction {
    fn frames(to_page: Vec<IpcFrame>) -> Self {
        Self {
            to_page,
            events: Vec::new(),
        }
    }

    fn event(event: PlatformEvent) -> Self {
        Self {
            to_page: Vec::new(),
            events: vec![event],
        }
    }
}

/// The platform side of one hosted application.
///
/// Owns nothing and blocks on nothing: every method is a fold from an input to frames
/// and events, which is what lets the whole protocol be tested against the real SDK's
/// own message shapes with no browser in the room.
#[derive(Debug)]
pub struct PlatformSession {
    app: AppIdentity,
    capabilities: DeviceCapabilities,
    state: PageState,
    /// Senders currently connected, so a page that comes up late still learns about the
    /// sender that launched it. Without this, the launching sender is invisible to the
    /// app: it connected before the page finished loading, and `senderconnected` is an
    /// edge rather than a state.
    senders: Vec<ConnectedSender>,
    volume: f32,
    muted: bool,
}

/// A sender the platform has told, or will tell, the page about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedSender {
    /// The sender's transport id.
    pub id: SenderId,
    /// Its user agent, if it gave one.
    pub user_agent: String,
}

impl PlatformSession {
    /// A session for `app`, with nothing connected and the page not yet up.
    #[must_use]
    pub fn new(app: AppIdentity, capabilities: DeviceCapabilities) -> Self {
        Self {
            app,
            capabilities,
            state: PageState::Connecting,
            senders: Vec::new(),
            volume: 1.0,
            muted: false,
        }
    }

    /// Set the volume the page will be told about, before it comes up.
    #[must_use]
    pub const fn with_volume(mut self, level: f32, muted: bool) -> Self {
        self.volume = level;
        self.muted = muted;
        self
    }

    /// The application this session hosts.
    #[must_use]
    pub const fn app(&self) -> &AppIdentity {
        &self.app
    }

    /// The namespaces the application declared, or nothing if it has not come up.
    ///
    /// This is what `RECEIVER_STATUS` must report while an app is running: a sender reads
    /// the list to decide what it may send, and a receiver that reports its *own*
    /// namespaces for somebody else's app tells every sender the wrong thing.
    #[must_use]
    pub fn namespaces(&self) -> &[String] {
        match &self.state {
            PageState::Connecting => &[],
            PageState::Ready(ready) => &ready.active_namespaces,
        }
    }

    /// Whether the page has identified itself.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, PageState::Ready(_))
    }

    /// Whether the application has claimed `namespace`.
    ///
    /// The question the media plane turns on: with a hosted app that speaks
    /// `urn:x-cast:com.google.cast.media`, the app owns media control and this
    /// receiver's own handler must step aside rather than answer alongside it.
    #[must_use]
    pub fn owns(&self, namespace: &str) -> bool {
        self.namespaces().iter().any(|n| n == namespace)
    }

    /// Fold one frame from the page.
    ///
    /// # Errors
    /// [`CastError::Json`] if a system frame's payload is not a system message. Frames on
    /// application namespaces are never parsed, so they cannot fail here.
    pub fn from_page(&mut self, frame: &IpcFrame) -> Result<PlatformReaction, CastError> {
        if !frame.is_system() {
            // Application traffic bound for a sender. Opaque by design.
            return Ok(PlatformReaction::default());
        }
        let msg: FromPage = serde_json::from_str(&frame.data)
            .map_err(|e| CastError::Json(format!("{e} in system message {}", frame.data)))?;
        Ok(self.system(msg))
    }

    fn system(&mut self, msg: FromPage) -> PlatformReaction {
        match msg {
            FromPage::Ready(ready) => self.page_ready(*ready),
            // The SDK refuses to send a level outside the range, so a value outside it
            // is not the SDK. Clamped rather than refused: the app asked for loud.
            FromPage::SetVolume {
                level: Some(level),
                muted: None,
            } => PlatformReaction::event(PlatformEvent::SetVolume(level.clamp(0.0, 1.0))),
            FromPage::SetVolume {
                level: None,
                muted: Some(muted),
            } => PlatformReaction::event(PlatformEvent::SetMuted(muted)),
            // `setvolume` carries exactly one of the two — the SDK has a separate method
            // per field (`Df`, `Ef`) and neither sets both. Something that sets both, or
            // neither, did not come from the SDK.
            FromPage::SetVolume { level, muted } => {
                tracing::debug!(
                    ?level,
                    ?muted,
                    "cast platform: a setvolume naming neither or both"
                );
                PlatformReaction::default()
            }
            FromPage::SetAppState { status_text, .. } => status_text
                .map(|t| PlatformReaction::event(PlatformEvent::StatusText(t)))
                .unwrap_or_default(),
            FromPage::StartHeartbeat { max_inactivity } => PlatformReaction::event(
                PlatformEvent::Heartbeat(Duration::from_secs_f64(max_inactivity.max(0.0))),
            ),
            FromPage::ShowSleepTimerOverlay => {
                PlatformReaction::event(PlatformEvent::SleepTimerOverlay)
            }
            FromPage::SendFeedbackMessage { message } => {
                PlatformReaction::event(PlatformEvent::Feedback(message))
            }
            // Speaker groups are a Chromecast Audio concept and this panel is not in one.
            // Answered as understood-and-ignored rather than left to time out.
            FromPage::AllowGroupChangeResponse { .. } => PlatformReaction::default(),
        }
    }

    /// The page identified itself: answer with the session, then catch it up on every
    /// sender that connected while it was loading.
    fn page_ready(&mut self, ready: PageReady) -> PlatformReaction {
        tracing::info!(
            app_id = %self.app.application_id,
            name = %self.app.application_name,
            sdk = ready.version.as_deref().unwrap_or("?"),
            namespaces = ready.active_namespaces.len(),
            "cast platform: the receiver page is up"
        );
        let status_text = ready.status_text.clone();
        self.state = PageState::Ready(ready.clone());

        let mut frames = vec![self.ready_frame()];
        // Order matters and this is the reason the sender list is kept: the launching
        // sender connected *before* the page finished loading, so it would never appear
        // as an edge. An app that never sees its own launcher never gets the first
        // message from it either, which is a session that connects and does nothing.
        for sender in &self.senders {
            frames.push(sender_connected_frame(sender));
        }
        frames.push(volume_changed_frame(self.volume, self.muted));

        let mut reaction = PlatformReaction::frames(frames);
        reaction
            .events
            .push(PlatformEvent::AppReady(Box::new(ready)));
        if let Some(text) = status_text {
            reaction.events.push(PlatformEvent::StatusText(text));
        }
        reaction
    }

    fn ready_frame(&self) -> IpcFrame {
        IpcFrame::system(
            serde_json::json!({
                "type": "ready",
                "applicationId": self.app.application_id,
                "applicationName": self.app.application_name,
                "sessionId": self.app.session_id,
                "launchingSenderId": self.app.launching_sender_id,
                // The SDK stores this verbatim and defaults it to "UNKNOWN"; a sender
                // launch is what this receiver has, and it is the honest value.
                "launchedFrom": "SENDER",
                "iconUrl": self.app.icon_url.clone().unwrap_or_default(),
                "deviceCapabilities": self.capabilities,
            })
            .to_string(),
        )
    }

    /// A sender connected. Returns the frame telling the page, if the page is up to
    /// hear it — otherwise it is remembered and replayed on `ready`.
    pub fn sender_connected(&mut self, id: &SenderId, user_agent: &str) -> PlatformReaction {
        let sender = ConnectedSender {
            id: id.clone(),
            user_agent: user_agent.to_owned(),
        };
        if self.senders.iter().any(|s| s.id == sender.id) {
            return PlatformReaction::default();
        }
        let frame = sender_connected_frame(&sender);
        self.senders.push(sender);
        if self.is_ready() {
            PlatformReaction::frames(vec![frame])
        } else {
            PlatformReaction::default()
        }
    }

    /// A sender went away.
    pub fn sender_disconnected(
        &mut self,
        id: &SenderId,
        reason: DisconnectReason,
    ) -> PlatformReaction {
        let before = self.senders.len();
        self.senders.retain(|s| s.id != *id);
        if self.senders.len() == before || !self.is_ready() {
            return PlatformReaction::default();
        }
        PlatformReaction::frames(vec![IpcFrame::system(
            serde_json::json!({
                "type": "senderdisconnected",
                "senderId": id,
                "reason": reason.as_str(),
            })
            .to_string(),
        )])
    }

    /// The device volume moved — because a sender set it, or because somebody touched
    /// the panel. Either way the page has to be told, or its own slider is a lie.
    pub fn volume_changed(&mut self, level: f32, muted: bool) -> PlatformReaction {
        self.volume = level;
        self.muted = muted;
        if !self.is_ready() {
            return PlatformReaction::default();
        }
        PlatformReaction::frames(vec![volume_changed_frame(level, muted)])
    }

    /// The page became visible, or stopped being. A receiver that is told it is hidden
    /// pauses rather than playing to nobody.
    pub fn visibility_changed(&self, visible: bool) -> PlatformReaction {
        if !self.is_ready() {
            return PlatformReaction::default();
        }
        PlatformReaction::frames(vec![IpcFrame::system(
            serde_json::json!({ "type": "visibilitychanged", "visible": visible }).to_string(),
        )])
    }

    /// The display went into, or came out of, standby.
    pub fn standby_changed(&self, standby: bool) -> PlatformReaction {
        if !self.is_ready() {
            return PlatformReaction::default();
        }
        PlatformReaction::frames(vec![IpcFrame::system(
            serde_json::json!({ "type": "standbychanged", "standby": standby }).to_string(),
        )])
    }

    /// Relay a sender's message on an application namespace to the page.
    ///
    /// [`None`] when the application has not claimed that namespace — including when the
    /// page has not come up at all, which is the case that matters: a message delivered
    /// then is dropped by the SDK with no trace, so holding it back and saying so is the
    /// difference between a diagnosable session and a silent one.
    #[must_use]
    pub fn relay_to_page(
        &self,
        namespace: &str,
        sender_id: &SenderId,
        data: &str,
    ) -> Option<IpcFrame> {
        if !self.owns(namespace) {
            return None;
        }
        Some(IpcFrame::app(namespace, sender_id.as_str(), data))
    }
}

fn sender_connected_frame(sender: &ConnectedSender) -> IpcFrame {
    IpcFrame::system(
        serde_json::json!({
            "type": "senderconnected",
            "senderId": sender.id,
            "userAgent": sender.user_agent,
        })
        .to_string(),
    )
}

fn volume_changed_frame(level: f32, muted: bool) -> IpcFrame {
    IpcFrame::system(
        serde_json::json!({ "type": "volumechanged", "level": level, "muted": muted }).to_string(),
    )
}

/// Every system message the receiver SDK sends a platform.
///
/// The complete set, taken from the SDK's own senders (`Xd`, `Df`, `Ef`, `Wd`, `Kd`,
/// `Jf`, `Hd`, `Gd`). Exhaustive on purpose: a `type` outside it is not the SDK, and a
/// non-exhaustive parse would let one through as a silent no-op.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FromPage {
    Ready(Box<PageReady>),
    SetVolume {
        level: Option<f32>,
        muted: Option<bool>,
    },
    SetAppState {
        #[serde(rename = "statusText")]
        status_text: Option<String>,
        #[serde(rename = "dialData")]
        #[allow(
            dead_code,
            reason = "parsed so the message is understood; DIAL state is proto-dial's"
        )]
        dial_data: Option<serde_json::Value>,
    },
    StartHeartbeat {
        #[serde(rename = "maxInactivity")]
        max_inactivity: f64,
    },
    ShowSleepTimerOverlay,
    SendFeedbackMessage {
        message: String,
    },
    AllowGroupChangeResponse {
        #[serde(rename = "requestId")]
        #[allow(
            dead_code,
            reason = "no speaker groups on this panel; parsed to be understood"
        )]
        request_id: Option<i64>,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn app() -> AppIdentity {
        AppIdentity {
            application_id: "233637DE".into(),
            application_name: "YouTube".into(),
            session_id: "sess-1".into(),
            launching_sender_id: "sender-0".into(),
            icon_url: None,
        }
    }

    fn session() -> PlatformSession {
        PlatformSession::new(app(), DeviceCapabilities::default())
    }

    /// The SDK's own `ready`, byte for byte in shape: what `Xd` builds and sends the
    /// moment the socket opens.
    const SDK_READY: &str = r#"{"type":"ready","statusText":"Ready to cast","activeNamespaces":["urn:x-cast:com.google.cast.media","urn:x-cast:com.google.youtube.mdx"],"version":"2.0.0.0157","messagesVersion":"1.0","sdkCapabilities":{"show_media_controls_supported":true,"group_capabilities_supported":true,"playback_device_status_supported":true}}"#;

    fn page_ready(session: &mut PlatformSession) -> PlatformReaction {
        session
            .from_page(&IpcFrame::system(SDK_READY.to_owned()))
            .unwrap()
    }

    fn parsed(frame: &IpcFrame) -> serde_json::Value {
        serde_json::from_str(&frame.data).unwrap()
    }

    #[test]
    fn a_frame_round_trips_through_the_wire_form_the_sdk_writes() {
        let frame = IpcFrame::app("urn:x-cast:com.example", "sender-1", r#"{"a":1}"#);
        let text = frame.encode().unwrap();
        // The three keys the SDK's own validator checks for, spelled as it spells them.
        assert!(text.contains("\"namespace\":"), "{text}");
        assert!(text.contains("\"senderId\":"), "{text}");
        assert!(text.contains("\"data\":"), "{text}");
        assert_eq!(IpcFrame::decode(&text).unwrap(), frame);
    }

    /// `data` is a string, not an object. A frame built the other way is dropped by the
    /// page with no error, which is the worst possible failure — so it is pinned here.
    #[test]
    fn data_is_carried_as_a_string_because_the_sdk_parses_it_itself() {
        let frame = IpcFrame::app("urn:x-cast:com.example", "s", r#"{"type":"X"}"#);
        let text = frame.encode().unwrap();
        assert!(
            text.contains(r#""data":"{\"type\":\"X\"}""#),
            "data must be an escaped string, got {text}"
        );
    }

    #[test]
    fn the_page_is_answered_with_the_session_it_asked_about() {
        let mut session = session();
        assert!(!session.is_ready());
        let reaction = page_ready(&mut session);

        let ready = parsed(&reaction.to_page[0]);
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["applicationId"], "233637DE");
        assert_eq!(ready["applicationName"], "YouTube");
        assert_eq!(ready["sessionId"], "sess-1");
        assert_eq!(ready["launchingSenderId"], "sender-0");
        assert_eq!(ready["launchedFrom"], "SENDER");
        assert!(session.is_ready());
    }

    /// What the app declares is what senders are told. Reporting our own namespaces for
    /// somebody else's app would have every sender sending into a void.
    #[test]
    fn the_pages_namespaces_become_the_sessions_namespaces() {
        let mut session = session();
        assert!(
            session.namespaces().is_empty(),
            "nothing is claimed before ready"
        );
        page_ready(&mut session);
        assert_eq!(
            session.namespaces(),
            [
                "urn:x-cast:com.google.cast.media",
                "urn:x-cast:com.google.youtube.mdx"
            ]
        );
        assert!(session.owns("urn:x-cast:com.google.youtube.mdx"));
        assert!(!session.owns("urn:x-cast:com.google.cast.webrtc"));
    }

    /// The media namespace changing hands is the whole point of hosting an app: with
    /// YouTube up, YouTube answers `LOAD`, not us.
    #[test]
    fn a_hosted_app_can_take_the_media_namespace() {
        let mut session = session();
        assert!(!session.owns(crate::messages::ns::MEDIA));
        page_ready(&mut session);
        assert!(session.owns(crate::messages::ns::MEDIA));
    }

    /// The bug this list exists for: the launching sender connects while the page is
    /// still loading, so it can never arrive as an edge.
    #[test]
    fn a_sender_that_connected_before_the_page_loaded_is_replayed_to_it() {
        let mut session = session();
        let early = session.sender_connected(&"sender-0".into(), "Chrome/125");
        assert!(
            early.to_page.is_empty(),
            "nothing may be written to a page that has not identified itself"
        );

        let reaction = page_ready(&mut session);
        let kinds: Vec<String> = reaction
            .to_page
            .iter()
            .map(|f| parsed(f)["type"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(kinds, ["ready", "senderconnected", "volumechanged"]);
        assert_eq!(parsed(&reaction.to_page[1])["senderId"], "sender-0");
    }

    #[test]
    fn a_sender_arriving_after_the_page_is_up_is_announced_immediately() {
        let mut session = session();
        page_ready(&mut session);
        let reaction = session.sender_connected(&"sender-9".into(), "CastVideos/1.0");
        assert_eq!(parsed(&reaction.to_page[0])["type"], "senderconnected");
        assert_eq!(parsed(&reaction.to_page[0])["userAgent"], "CastVideos/1.0");
    }

    #[test]
    fn the_same_sender_is_not_announced_twice() {
        let mut session = session();
        page_ready(&mut session);
        assert_eq!(session.sender_connected(&"s".into(), "ua").to_page.len(), 1);
        assert!(session
            .sender_connected(&"s".into(), "ua")
            .to_page
            .is_empty());
    }

    #[test]
    fn a_disconnect_names_the_reason_in_the_sdks_vocabulary() {
        let mut session = session();
        session.sender_connected(&"s".into(), "ua");
        page_ready(&mut session);
        let reaction = session.sender_disconnected(&"s".into(), DisconnectReason::ClosedByPeer);
        let msg = parsed(&reaction.to_page[0]);
        assert_eq!(msg["type"], "senderdisconnected");
        assert_eq!(msg["reason"], "closed_by_peer");
        // A sender that was never there is not a disconnection.
        assert!(session
            .sender_disconnected(&"ghost".into(), DisconnectReason::Unknown)
            .to_page
            .is_empty());
    }

    /// The two `setvolume` shapes the SDK produces, each from its own method, and never
    /// both at once.
    #[test]
    fn each_setvolume_shape_becomes_the_event_it_means() {
        let mut session = session();
        page_ready(&mut session);

        let level = session
            .from_page(&IpcFrame::system(
                r#"{"type":"setvolume","level":0.25}"#.into(),
            ))
            .unwrap();
        assert_eq!(level.events, [PlatformEvent::SetVolume(0.25)]);

        let muted = session
            .from_page(&IpcFrame::system(
                r#"{"type":"setvolume","muted":true}"#.into(),
            ))
            .unwrap();
        assert_eq!(muted.events, [PlatformEvent::SetMuted(true)]);
    }

    #[test]
    fn a_volume_outside_the_range_is_clamped_rather_than_refused() {
        let mut session = session();
        page_ready(&mut session);
        let loud = session
            .from_page(&IpcFrame::system(
                r#"{"type":"setvolume","level":9.0}"#.into(),
            ))
            .unwrap();
        assert_eq!(loud.events, [PlatformEvent::SetVolume(1.0)]);
    }

    #[test]
    fn the_heartbeat_request_carries_its_interval() {
        let mut session = session();
        page_ready(&mut session);
        let beat = session
            .from_page(&IpcFrame::system(
                r#"{"type":"startheartbeat","maxInactivity":30}"#.into(),
            ))
            .unwrap();
        assert_eq!(
            beat.events,
            [PlatformEvent::Heartbeat(Duration::from_secs(30))]
        );
    }

    #[test]
    fn a_status_line_from_the_app_reaches_the_receiver() {
        let mut session = session();
        page_ready(&mut session);
        let state = session
            .from_page(&IpcFrame::system(
                r#"{"type":"setappstate","statusText":"Big Buck Bunny"}"#.into(),
            ))
            .unwrap();
        assert_eq!(
            state.events,
            [PlatformEvent::StatusText("Big Buck Bunny".into())]
        );
    }

    /// Application traffic is never parsed. A page's private protocol is its own, and a
    /// receiver that reads it has taken on a maintenance burden #16 explicitly refused.
    #[test]
    fn traffic_on_an_application_namespace_is_not_parsed_at_all() {
        let mut session = session();
        page_ready(&mut session);
        let opaque = IpcFrame::app(
            "urn:x-cast:com.google.youtube.mdx",
            "sender-0",
            "this is not JSON and never has to be",
        );
        let reaction = session.from_page(&opaque).unwrap();
        assert_eq!(reaction, PlatformReaction::default());
    }

    #[test]
    fn a_system_frame_that_is_not_a_system_message_is_an_error_with_the_payload_in_it() {
        let mut session = session();
        let err = session
            .from_page(&IpcFrame::system(r#"{"type":"nonsense"}"#.into()))
            .unwrap_err();
        assert!(format!("{err}").contains("nonsense"), "{err}");
    }

    /// Relaying before the page is up would be delivered into the SDK's own drop path,
    /// where it leaves no trace at all.
    #[test]
    fn nothing_is_relayed_to_a_page_that_has_not_come_up() {
        let mut session = session();
        assert!(session
            .relay_to_page(crate::messages::ns::MEDIA, &"s".into(), "{}")
            .is_none());
        page_ready(&mut session);
        let frame = session
            .relay_to_page(
                crate::messages::ns::MEDIA,
                &"s".into(),
                r#"{"type":"LOAD"}"#,
            )
            .unwrap();
        assert_eq!(frame.sender_id, "s");
        assert_eq!(frame.data, r#"{"type":"LOAD"}"#);
    }

    #[test]
    fn a_namespace_the_app_never_claimed_is_not_relayed() {
        let mut session = session();
        page_ready(&mut session);
        assert!(session
            .relay_to_page("urn:x-cast:com.example.other", &"s".into(), "{}")
            .is_none());
    }

    /// Capabilities are promises. The default is a panel that claims nothing, so a
    /// receiver page cannot conclude HDR from silence.
    #[test]
    fn a_panel_claims_no_capability_it_was_not_given() {
        let mut session = session();
        let reaction = page_ready(&mut session);
        let caps = &parsed(&reaction.to_page[0])["deviceCapabilities"];
        assert_eq!(caps["is_hdr_supported"], false);
        assert_eq!(caps["is_dv_supported"], false);
        assert_eq!(caps["is_dolby_atmos_supported"], false);
    }

    #[test]
    fn declared_capabilities_reach_the_page() {
        let mut session = PlatformSession::new(
            app(),
            DeviceCapabilities {
                is_hdr_supported: true,
                ..DeviceCapabilities::default()
            },
        );
        let reaction = page_ready(&mut session);
        let caps = &parsed(&reaction.to_page[0])["deviceCapabilities"];
        assert_eq!(caps["is_hdr_supported"], true);
        assert_eq!(caps["is_dv_supported"], false);
    }

    #[test]
    fn volume_and_visibility_reach_a_page_that_is_up_and_nobody_otherwise() {
        let mut session = session();
        assert!(session.volume_changed(0.5, false).to_page.is_empty());
        assert!(session.visibility_changed(true).to_page.is_empty());
        assert!(session.standby_changed(false).to_page.is_empty());

        page_ready(&mut session);
        let volume = session.volume_changed(0.25, true);
        let msg = parsed(&volume.to_page[0]);
        assert_eq!(msg["type"], "volumechanged");
        assert_eq!(msg["muted"], true);

        assert_eq!(
            parsed(&session.visibility_changed(false).to_page[0])["visible"],
            false
        );
        assert_eq!(
            parsed(&session.standby_changed(true).to_page[0])["standby"],
            true
        );
    }

    /// The level the page is first told is the level the device is actually at — not
    /// 1.0, which is what an app would otherwise draw its slider at.
    #[test]
    fn the_first_volume_the_page_hears_is_the_one_the_device_is_at() {
        let mut session = session().with_volume(0.3, true);
        let reaction = page_ready(&mut session);
        let volume = parsed(reaction.to_page.last().unwrap());
        assert_eq!(volume["type"], "volumechanged");
        assert!((volume["level"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(volume["muted"], true);
    }
}
