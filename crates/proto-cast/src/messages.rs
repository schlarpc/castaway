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

/// Build a `MEDIA_STATUS` payload for the current player state.
#[must_use]
pub fn media_status(request_id: i64, media_session_id: i64, player_state: &str) -> String {
    serde_json::json!({
        "type": "MEDIA_STATUS",
        "requestId": request_id,
        "status": [{
            "mediaSessionId": media_session_id,
            "playbackRate": 1,
            "playerState": player_state,
            "currentTime": 0,
            "supportedMediaCommands": 15,
            "volume": { "level": 1.0, "muted": false },
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
