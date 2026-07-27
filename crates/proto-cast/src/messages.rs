//! CASTv2 JSON payloads and namespace constants. Incoming requests are parsed into
//! typed structs (parse-don't-validate); responses are built with `serde_json`.

use serde::Deserialize;

use crate::error::CastError;

/// The CASTv2 protocol namespaces we handle.
pub mod ns {
    /// Virtual-connection control (`CONNECT`/`CLOSE`).
    pub const CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
    /// Keepalive (`PING`/`PONG`).
    pub const HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
    /// Receiver control (`LAUNCH`/`STOP`/`GET_STATUS`).
    pub const RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
    /// Device authentication (binary protobuf).
    pub const DEVICE_AUTH: &str = "urn:x-cast:com.google.cast.tp.deviceauth";
    /// Media control (`LOAD`/`PLAY`/`PAUSE`/`SEEK`/`STOP`).
    pub const MEDIA: &str = "urn:x-cast:com.google.cast.media";
}

/// The Default Media Receiver application id — the app senders launch to LOAD media
/// URLs. Advertising support for it is what makes the "Cast a video" button work.
pub const DEFAULT_MEDIA_RECEIVER_APP_ID: &str = "CC1AD845";

/// The Cast Streaming receiver app ids — the apps a sender launches when it intends to
/// *mirror* to us rather than hand us a URL.
///
/// From openscreen's `cast/common/public/cast_streaming_app_ids.h`, which is where the
/// senders get them too. All six are listed rather than the desktop pair alone because a
/// receiver that recognises only some of them is a receiver that works from Chrome and
/// not from a phone, for no reason a person in the room could deduce.
const STREAMING_APP_IDS: [&str; 6] = [
    "0F5096E8", // audio + video
    "85CDB22F", // audio only
    "674A0243", // Android mirroring, audio + video
    "8E6C866D", // Android mirroring, audio only
    "96084372", // Android app streaming
    "BFD92C23", // iOS app streaming
];

/// What this receiver can do with an `appId` a sender asks about or launches.
///
/// An enum rather than a boolean because the two things we support are not the same
/// thing: one is a media URL we play ourselves, the other is an RTP stream we terminate,
/// and a third category exists that we must decline. Modelling it this way is what makes
/// the decline exhaustive — a new app id has to be classified before it can be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    /// The Default Media Receiver: the sender hands us a media URL and our own pipeline
    /// plays it.
    DefaultMedia,
    /// A Cast Streaming receiver: the sender mirrors to us over RTP.
    Streaming,
    /// Somebody else's web receiver — Netflix, Spotify, YouTube's own Cast app. Hosting
    /// these means running the vendor's receiver page and speaking the Cast receiver
    /// SDK's platform protocol to it, which is GAPS.md G56 and is not built.
    Unhostable,
}

impl App {
    /// Classify an `appId` from the wire.
    #[must_use]
    pub fn classify(app_id: &str) -> Self {
        if app_id.eq_ignore_ascii_case(DEFAULT_MEDIA_RECEIVER_APP_ID) {
            Self::DefaultMedia
        } else if STREAMING_APP_IDS
            .iter()
            .any(|id| app_id.eq_ignore_ascii_case(id))
        {
            Self::Streaming
        } else {
            Self::Unhostable
        }
    }
}

/// Why a `LAUNCH` was refused, in the sender's own vocabulary
/// (`cast/common/channel/message_util.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRefusal {
    /// This receiver does not have that application at all.
    NotFound,
    /// We do host it, but cannot right now — mirroring asked for with no RTP socket.
    SystemError,
}

impl LaunchRefusal {
    /// The `reason` string a sender expects.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::SystemError => "SYSTEM_ERROR",
        }
    }
}

/// Peek the `type` and `requestId` of any JSON payload without full typing.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    /// The message type discriminator (`PING`, `LAUNCH`, `LOAD`, …).
    pub r#type: String,
    /// The request id to echo in the response, if present.
    #[serde(rename = "requestId")]
    pub request_id: Option<i64>,
}

impl Envelope {
    /// Parse just the envelope fields from a JSON payload.
    ///
    /// # Errors
    /// [`CastError::Json`] if the payload isn't an object with a string `type`.
    pub fn parse(payload: &str) -> Result<Self, CastError> {
        serde_json::from_str(payload).map_err(|e| CastError::Json(e.to_string()))
    }
}

/// A `LAUNCH` request on the receiver namespace.
#[derive(Debug, Clone, Deserialize)]
pub struct LaunchRequest {
    /// Request id.
    #[serde(rename = "requestId")]
    pub request_id: i64,
    /// The app id the sender wants launched.
    #[serde(rename = "appId")]
    pub app_id: String,
}

/// A `GET_APP_AVAILABILITY` request on the receiver namespace.
///
/// `appId` is an array here and a bare string in `LAUNCH` — same key, different shape,
/// which is why this is its own type rather than a field reused from [`LaunchRequest`].
#[derive(Debug, Clone, Deserialize)]
pub struct AppAvailabilityRequest {
    /// Request id.
    #[serde(rename = "requestId")]
    pub request_id: i64,
    /// The app ids the sender is asking about.
    #[serde(rename = "appId")]
    pub app_ids: Vec<String>,
}

/// A `SET_VOLUME` request on the receiver namespace.
///
/// Both fields are optional and senders really do send them separately — a slider drag
/// carries `level`, the mute button carries `muted` — so this is deliberately lenient
/// rather than a tagged union. Note that openscreen's *receiver* does not implement
/// `SET_VOLUME` at all (it reports a hardcoded level 1.0), so unlike the rest of this
/// module the shape here is taken from sender behaviour rather than from a reference
/// receiver, and parses anything either form can produce.
#[derive(Debug, Clone, Deserialize)]
pub struct SetVolumeRequest {
    /// Request id.
    #[serde(rename = "requestId")]
    pub request_id: i64,
    /// The change being asked for.
    pub volume: VolumeChange,
}

/// The `volume` object inside a `SET_VOLUME`.
#[derive(Debug, Clone, Deserialize)]
pub struct VolumeChange {
    /// New output level, `0.0..=1.0`.
    pub level: Option<f32>,
    /// New mute state.
    pub muted: Option<bool>,
}

/// A `LOAD` request on the media namespace.
#[derive(Debug, Clone, Deserialize)]
pub struct LoadRequest {
    /// Request id.
    #[serde(rename = "requestId")]
    pub request_id: i64,
    /// The media descriptor.
    pub media: MediaInfo,
    /// Start position, seconds.
    #[serde(rename = "currentTime")]
    pub current_time: Option<f64>,
    /// Whether to start immediately.
    pub autoplay: Option<bool>,
}

/// The `media` object inside a `LOAD`.
#[derive(Debug, Clone, Deserialize)]
pub struct MediaInfo {
    /// The content URL/id.
    #[serde(rename = "contentId")]
    pub content_id: String,
    /// MIME type, e.g. `video/mp4`.
    #[serde(rename = "contentType")]
    pub content_type: Option<String>,
    /// `BUFFERED`, `LIVE`, or `NONE`.
    #[serde(rename = "streamType")]
    pub stream_type: Option<String>,
}

/// Build a `PONG` heartbeat reply.
#[must_use]
pub fn pong() -> String {
    "{\"type\":\"PONG\"}".to_string()
}

/// Build the `GET_APP_AVAILABILITY` reply for the app ids asked about.
///
/// Note the key is `responseType`, not `type` — a sender matches this reply by request id
/// and would not recognise it under the wrong key. Shape taken from openscreen's
/// `ApplicationAgent::HandleGetAppAvailability`.
#[must_use]
pub fn app_availability(request_id: i64, availability: &[(String, bool)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = availability
        .iter()
        .map(|(id, available)| {
            let value = if *available {
                "APP_AVAILABLE"
            } else {
                "APP_UNAVAILABLE"
            };
            (id.clone(), serde_json::Value::String(value.to_string()))
        })
        .collect();
    serde_json::json!({
        "requestId": request_id,
        "responseType": "GET_APP_AVAILABILITY",
        "availability": map,
    })
    .to_string()
}

/// Build a `LAUNCH_ERROR` payload — the answer to a launch we will not perform.
#[must_use]
pub fn launch_error(request_id: i64, refusal: LaunchRefusal) -> String {
    serde_json::json!({
        "requestId": request_id,
        "type": "LAUNCH_ERROR",
        "reason": refusal.reason(),
    })
    .to_string()
}

/// Build a `RECEIVER_STATUS` payload. `app` is `Some` when an application is running.
#[must_use]
pub fn receiver_status(
    request_id: i64,
    app: Option<&RunningApp>,
    volume_level: f32,
    muted: bool,
) -> String {
    let applications = match app {
        Some(a) => serde_json::json!([{
            "appId": a.app_id,
            "displayName": a.display_name,
            "sessionId": a.session_id,
            "transportId": a.transport_id,
            "statusText": a.status_text,
            "isIdleScreen": false,
            "namespaces": [
                {"name": ns::MEDIA},
                {"name": ns::CONNECTION},
                {"name": ns::HEARTBEAT},
            ],
        }]),
        None => serde_json::json!([]),
    };
    serde_json::json!({
        "type": "RECEIVER_STATUS",
        "requestId": request_id,
        "status": {
            "applications": applications,
            "volume": { "level": volume_level, "muted": muted },
        },
    })
    .to_string()
}

/// What the media plane is doing, in the vocabulary a sender's `MEDIA_STATUS` uses.
///
/// The absence of a value — nothing loaded — is modelled as `Option<PlayerState>` at the
/// call site rather than as an `Idle` variant, because the wire distinguishes them
/// structurally: nothing loaded is an *empty* status array, not a status saying idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    /// Media is loaded and advancing.
    Playing,
    /// Media is loaded and held.
    Paused,
}

impl PlayerState {
    /// The `playerState` string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "PLAYING",
            Self::Paused => "PAUSED",
        }
    }
}

/// Build a `MEDIA_STATUS` for a session with nothing loaded.
///
/// An empty `status` array is the answer, not a status object saying `IDLE`: a sender
/// reads the array's emptiness as "there is no media session here". Reporting `PLAYING`
/// with nothing loaded — which this receiver used to do for every `GET_STATUS` — tells a
/// sender's UI to show a transport bar for media that does not exist.
#[must_use]
pub fn media_status_empty(request_id: i64) -> String {
    serde_json::json!({
        "type": "MEDIA_STATUS",
        "requestId": request_id,
        "status": [],
    })
    .to_string()
}

/// Build a `MEDIA_STATUS` payload for a loaded media session.
#[must_use]
pub fn media_status(
    request_id: i64,
    media_session_id: i64,
    player_state: &str,
    volume_level: f32,
    muted: bool,
) -> String {
    serde_json::json!({
        "type": "MEDIA_STATUS",
        "requestId": request_id,
        "status": [{
            "mediaSessionId": media_session_id,
            "playbackRate": 1,
            "playerState": player_state,
            // Always zero, and knowingly so: nothing on the pipeline side reports a
            // playback position yet, so a sender's scrubber sits at the start for the
            // whole item. Reporting a made-up position would be worse — the scrubber
            // would move and mean nothing.
            "currentTime": 0,
            // PAUSE | SEEK | STREAM_VOLUME | STREAM_MUTE. This is a claim about what we
            // answer, so it has to track what `handle_media` and `SET_VOLUME` really do.
            "supportedMediaCommands": 15,
            // The session's volume, not a constant: a sender that reads 1.0 back after
            // setting 0.25 shows a slider that jumps home.
            "volume": { "level": volume_level, "muted": muted },
        }],
    })
    .to_string()
}

/// A running application's identity, echoed in `RECEIVER_STATUS`.
#[derive(Debug, Clone)]
pub struct RunningApp {
    /// The launched app id.
    pub app_id: String,
    /// Human-readable app name.
    pub display_name: String,
    /// The session id we assigned.
    pub session_id: String,
    /// The transport (virtual-connection) id media messages address.
    pub transport_id: String,
    /// Status line text.
    pub status_text: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn envelope_extracts_type_and_request_id() {
        let e = Envelope::parse(r#"{"type":"GET_STATUS","requestId":7}"#).unwrap();
        assert_eq!(e.r#type, "GET_STATUS");
        assert_eq!(e.request_id, Some(7));
    }

    #[test]
    fn parses_load_media() {
        let json = r#"{"requestId":3,"type":"LOAD","media":{"contentId":"http://x/v.mp4","contentType":"video/mp4","streamType":"BUFFERED"},"autoplay":true}"#;
        let load: LoadRequest = serde_json::from_str(json).unwrap();
        assert_eq!(load.media.content_id, "http://x/v.mp4");
        assert_eq!(load.autoplay, Some(true));
    }

    #[test]
    fn receiver_status_includes_running_app() {
        let app = RunningApp {
            app_id: DEFAULT_MEDIA_RECEIVER_APP_ID.into(),
            display_name: "Default Media Receiver".into(),
            session_id: "sess-1".into(),
            transport_id: "transport-1".into(),
            status_text: "Ready".into(),
        };
        let s = receiver_status(1, Some(&app), 1.0, false);
        assert!(s.contains("\"transportId\":\"transport-1\""));
        assert!(s.contains(ns::MEDIA));
    }

    #[test]
    fn pong_is_minimal() {
        assert_eq!(pong(), "{\"type\":\"PONG\"}");
    }
}
