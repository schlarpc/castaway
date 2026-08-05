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
    /// Somebody else's web receiver — YouTube, Plex, Netflix. Hosted by loading the
    /// vendor's page in the browser and speaking the platform protocol to it
    /// (`crate::platform`), with the vendor's own protocol staying in the vendor's JS.
    Page,
    /// Not something this receiver can run: a native application that is not one of the
    /// mirroring ids, or an id the registry does not have.
    Unhostable,
}

/// What the receiver currently knows about app ids it does not serve natively.
///
/// Pushed in by the actor before a fold rather than looked up during one, for the same
/// reason playback position is (see [`crate::session::CastSession::observe_progress`]):
/// resolving an app id is a network lookup, and a pure state machine that could make one
/// would put a third party in the path of every `GET_APP_AVAILABILITY` and make every
/// status test depend on the internet.
#[derive(Debug, Clone, Default)]
pub struct AppCatalogue {
    hosting: bool,
    known: std::collections::HashMap<String, bool>,
}

impl AppCatalogue {
    /// A catalogue for a receiver that can host pages, or one that cannot.
    ///
    /// `false` is a real configuration and not a degraded one: a build with no browser
    /// (`--no-default-features`) genuinely cannot host an application, and must say so
    /// rather than accept launches it will not serve.
    #[must_use]
    pub fn new(hosting: bool) -> Self {
        Self {
            hosting,
            known: std::collections::HashMap::new(),
        }
    }

    /// Record that `app_id` is, or is not, a web receiver.
    pub fn record(&mut self, app_id: &str, is_page: bool) {
        self.known.insert(app_id.to_ascii_uppercase(), is_page);
    }

    /// Whether a page could be hosted at all.
    #[must_use]
    pub const fn hosting(&self) -> bool {
        self.hosting
    }

    fn is_page(&self, app_id: &str) -> Option<bool> {
        self.known.get(&app_id.to_ascii_uppercase()).copied()
    }
}

impl App {
    /// Classify an `appId` from the wire against what the receiver knows.
    ///
    /// The interesting case is an id that has never been resolved, and it is answered
    /// **optimistically** — as a page — whenever hosting is possible at all. That is a
    /// deliberate asymmetry between two failures:
    ///
    /// - saying unavailable for an app we could have hosted makes the device *vanish
    ///   from the picker*. Nothing is shown, nothing is logged on the sender, and there
    ///   is no way for the person holding the phone to tell it apart from a network
    ///   fault. This is the failure Plex hit, and it is what #16 is about.
    /// - saying available for an app that turns out not to exist costs a launch that
    ///   fails with `NOT_FOUND` — the sender's own error, in its own words, in front of
    ///   somebody who just pressed a button.
    ///
    /// The second is recoverable and the first is invisible, so the guess goes that way.
    /// It is also usually right: an unknown eight-hex-digit id belongs to a web receiver
    /// far more often than to anything else, and one lookup makes it exact for good.
    #[must_use]
    pub fn classify(app_id: &str, catalogue: &AppCatalogue) -> Self {
        if app_id.eq_ignore_ascii_case(DEFAULT_MEDIA_RECEIVER_APP_ID) {
            Self::DefaultMedia
        } else if STREAMING_APP_IDS
            .iter()
            .any(|id| app_id.eq_ignore_ascii_case(id))
        {
            Self::Streaming
        } else if !catalogue.hosting {
            Self::Unhostable
        } else {
            match catalogue.is_page(app_id) {
                Some(true) | None => Self::Page,
                Some(false) => Self::Unhostable,
            }
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
        parse_message(payload)
    }
}

/// Parse a message payload, carrying the payload itself into the error.
///
/// A serde error names a line and column of a message nobody logged, which is a riddle,
/// not a diagnostic. Real senders diverge from the reference JSON constantly — that is
/// the DLNA lesson replayed on Cast — and the payload is the evidence the divergence is
/// diagnosed from.
///
/// # Errors
/// [`CastError::Json`] naming both the serde error and the payload.
pub fn parse_message<T: serde::de::DeserializeOwned>(payload: &str) -> Result<T, CastError> {
    serde_json::from_str(payload).map_err(|e| CastError::Json(format!("{e} in payload {payload}")))
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
    #[serde(default, deserialize_with = "lenient_bool")]
    pub muted: Option<bool>,
}

/// A boolean that also accepts its stringly form.
///
/// VLC's chromecast module sends `"autoplay":"false"` — the string — in its `LOAD`, and
/// a strict parse killed the whole connection, which read from the couch as "VLC sees
/// the device and nothing plays". The receiver's job at this boundary is the DLNA
/// conformance posture: parse what real senders actually send, exactly and tolerantly,
/// and reject only what is genuinely ambiguous.
fn lenient_bool<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<bool>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrText {
        Bool(bool),
        Text(String),
    }
    match Option::<BoolOrText>::deserialize(de)? {
        None => Ok(None),
        Some(BoolOrText::Bool(b)) => Ok(Some(b)),
        Some(BoolOrText::Text(t)) if t.eq_ignore_ascii_case("true") => Ok(Some(true)),
        Some(BoolOrText::Text(t)) if t.eq_ignore_ascii_case("false") => Ok(Some(false)),
        Some(BoolOrText::Text(t)) => Err(serde::de::Error::custom(format!(
            "neither a boolean nor \"true\"/\"false\": {t:?}"
        ))),
    }
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
    /// Whether to start immediately. Lenient: VLC sends the *string* `"false"`.
    #[serde(default, deserialize_with = "lenient_bool")]
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

/// The `namespaces` array for a running application.
///
/// Transport namespaces are always present — `CONNECTION` and `HEARTBEAT` are how a
/// sender reaches the session at all, and they stay ours even while somebody else's page
/// owns the media plane. Everything else comes from the application when there is one,
/// and defaults to the media namespace we serve ourselves when there is not.
fn namespaces(app: &RunningApp) -> serde_json::Value {
    let mut names: Vec<&str> = vec![ns::CONNECTION, ns::HEARTBEAT];
    if app.namespaces.is_empty() {
        names.push(ns::MEDIA);
    } else {
        names.extend(app.namespaces.iter().map(String::as_str));
    }
    names.sort_unstable();
    names.dedup();
    serde_json::Value::Array(
        names
            .into_iter()
            .map(|name| serde_json::json!({ "name": name }))
            .collect(),
    )
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
            "namespaces": namespaces(a),
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
/// The absence of a value — nothing *ever* loaded — is modelled as `Option<PlayerState>`
/// at the call site rather than as a variant, because the wire distinguishes them
/// structurally: nothing loaded is an *empty* status array, not a status saying idle.
///
/// [`Self::Idle`] is a different thing and not the same as that absence: it means an item
/// was loaded and is over, and it carries *why* — which is the whole of what a sender needs
/// to advance a queue or to show that a cast failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    /// Media is loaded and advancing.
    Playing,
    /// Media is loaded and held.
    Paused,
    /// The item is over, for the reason given.
    Idle(IdleReason),
}

impl PlayerState {
    /// The `playerState` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "PLAYING",
            Self::Paused => "PAUSED",
            Self::Idle(_) => "IDLE",
        }
    }

    /// The `idleReason`, present only when idle.
    #[must_use]
    pub const fn idle_reason(self) -> Option<&'static str> {
        match self {
            Self::Idle(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Why a media session went idle.
///
/// The three a receiver produces. `CANCELLED` and `INTERRUPTED` are the sender's own
/// vocabulary for a stop it asked for and a session another sender took, and neither is
/// something we conclude on our own — a stop we were told to do is answered where it was
/// asked for, and preemption is not this item ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleReason {
    /// The item played to its end.
    Finished,
    /// The fetch or the decode failed.
    Error,
}

impl IdleReason {
    /// The `idleReason` string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "FINISHED",
            Self::Error => "ERROR",
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

/// PAUSE | SEEK | STREAM_VOLUME | STREAM_MUTE.
///
/// A *claim*, and one this receiver got wrong for a while: SEEK was advertised here while
/// `RenderPipeline::control` refused it, so a sender drew a scrubber that did nothing. The
/// bit set has to track what the pipeline will honour, which is what
/// `supported_commands_match_what_the_pipeline_honours` holds it to.
pub const SUPPORTED_MEDIA_COMMANDS: u32 = 15;

/// Build a `MEDIA_STATUS` payload for a loaded media session.
///
/// `current_time` is where playback has actually reached. [`None`] means nothing knows yet
/// — before the first frame, or in a build with no decoder — and is rendered as zero
/// because the field is not optional on the wire. That is the one place this can still
/// mislead, and it is bounded: a sender sees it for the length of a fetch.
#[must_use]
pub fn media_status(
    request_id: i64,
    media_session_id: i64,
    player_state: PlayerState,
    current_time: Option<std::time::Duration>,
    volume_level: f32,
    muted: bool,
) -> String {
    let mut status = serde_json::json!({
        "mediaSessionId": media_session_id,
        "playbackRate": 1,
        "playerState": player_state.as_str(),
        "currentTime": current_time.map_or(0.0, |t| t.as_secs_f64()),
        "supportedMediaCommands": SUPPORTED_MEDIA_COMMANDS,
        // The session's volume, not a constant: a sender that reads 1.0 back after
        // setting 0.25 shows a slider that jumps home.
        "volume": { "level": volume_level, "muted": muted },
    });
    // Only when idle, and only then: `idleReason` on a PLAYING status is a field senders
    // are entitled to read as the item having ended.
    if let (Some(reason), Some(obj)) = (player_state.idle_reason(), status.as_object_mut()) {
        obj.insert("idleReason".into(), serde_json::json!(reason));
    }
    serde_json::json!({
        "type": "MEDIA_STATUS",
        "requestId": request_id,
        "status": [status],
    })
    .to_string()
}

/// A running application's identity, echoed in `RECEIVER_STATUS`.
#[derive(Debug, Clone)]
pub struct RunningApp {
    /// The namespaces a *hosted* application declared, or empty when we are serving the
    /// session ourselves.
    ///
    /// `RECEIVER_STATUS` reports this list and a sender reads it to decide what it may
    /// send — so a receiver that reported its own namespaces while somebody else's app
    /// was running would tell every sender the wrong thing, and the messages it invited
    /// would arrive on a namespace nothing is listening to.
    pub namespaces: Vec<String>,
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
    /// The sender that launched the app — the one whose departure ends the session.
    /// Any sender may STOP; only this one's CLOSE means "the session's owner left".
    pub controller: String,
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
            controller: "sender-0".into(),
            namespaces: Vec::new(),
        };
        let s = receiver_status(1, Some(&app), 1.0, false);
        assert!(s.contains("\"transportId\":\"transport-1\""));
        assert!(s.contains(ns::MEDIA));
    }

    /// While somebody else's application is running, the namespaces a sender is told
    /// about are *its* namespaces. Reporting ours would invite every sender to send on a
    /// namespace nothing is listening to.
    #[test]
    fn a_hosted_application_reports_its_own_namespaces() {
        let app = RunningApp {
            app_id: "233637DE".into(),
            display_name: "YouTube".into(),
            session_id: "sess-2".into(),
            transport_id: "transport-2".into(),
            status_text: "YouTube".into(),
            controller: "sender-0".into(),
            namespaces: vec!["urn:x-cast:com.google.youtube.mdx".into(), ns::MEDIA.into()],
        };
        let status = receiver_status(1, Some(&app), 1.0, false);
        assert!(
            status.contains("urn:x-cast:com.google.youtube.mdx"),
            "{status}"
        );
        // Transport stays ours whatever the application declared: `CONNECTION` and
        // `HEARTBEAT` are how a sender reaches the session at all.
        assert!(status.contains(ns::CONNECTION), "{status}");
        assert!(status.contains(ns::HEARTBEAT), "{status}");
    }

    #[test]
    fn pong_is_minimal() {
        assert_eq!(pong(), "{\"type\":\"PONG\"}");
    }

    #[test]
    fn vlcs_load_with_its_stringly_autoplay_parses() {
        // Captured verbatim from VLC 3.x's chromecast module (2026-07-29): `autoplay`
        // is the *string* "false". The strict parse rejected it and tore down the whole
        // connection — VLC listed the device and nothing ever played.
        let payload = r#"{"type":"LOAD","media":{"metadata":{ "metadataType":0,"title":"cast-test.mp4"},"contentId":"http://10.42.0.50:8010/chromecast/4357016178264/650812800/stream","streamType":"LIVE","contentType":"video/x-matroska"},"autoplay":"false","requestId":1}"#;
        let req: LoadRequest = parse_message(payload).expect("VLC's LOAD should parse");
        assert_eq!(req.autoplay, Some(false));
        assert_eq!(req.request_id, 1);
        assert!(req.media.content_id.starts_with("http://10.42.0.50:8010/"));
    }

    #[test]
    fn lenient_bools_accept_both_forms_and_refuse_nonsense() {
        let b: LoadRequest = parse_message(
            r#"{"requestId":1,"media":{"contentId":"u","contentType":"t"},"autoplay":true}"#,
        )
        .unwrap();
        assert_eq!(b.autoplay, Some(true));
        let t: LoadRequest = parse_message(
            r#"{"requestId":1,"media":{"contentId":"u","contentType":"t"},"autoplay":"TRUE"}"#,
        )
        .unwrap();
        assert_eq!(t.autoplay, Some(true));
        assert!(parse_message::<LoadRequest>(
            r#"{"requestId":1,"media":{"contentId":"u","contentType":"t"},"autoplay":"maybe"}"#,
        )
        .is_err());
        // And a parse failure names the payload, because a line/column of a message
        // nobody logged is a riddle.
        let err = parse_message::<LoadRequest>(r#"{"requestId":"x"}"#).unwrap_err();
        assert!(err.to_string().contains(r#"{"requestId":"x"}"#));
    }
}
