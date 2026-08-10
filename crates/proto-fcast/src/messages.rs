//! FCast v1-v3 message bodies: UTF-8 JSON, converted to rich types at the boundary
//! (ground rule 1) and back to per-version JSON on the way out.
//!
//! The shapes come from the published spec (`docs/docs/protocol/{v1,v2,v3}.md` in the
//! FCast repository) cross-checked against the reference implementation's own serde
//! (`crates/fcast-protocol/src/{v1,v2,v3}.rs`) and the transcripts captured from the
//! reference sender in `tests/fixtures/`. Field names are camelCase on the wire.
//!
//! Outbound messages that exist in more than one version get one struct *per
//! version*, because the versions disagree about which fields exist and which are
//! required: v2's `PlaybackUpdateMessage` requires `duration` and `speed`, v3 makes
//! them nullable, v1 has neither. One struct with options would let us send a v2
//! sender a body its parser rejects.

use std::collections::HashMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::error::FCastError;
use crate::wire::{Frame, Opcode};

/// The playback state table shared by every protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayState {
    /// Nothing loaded, or stopped.
    #[default]
    Idle,
    /// Media is playing.
    Playing,
    /// Media is paused.
    Paused,
}

impl PlayState {
    /// The wire number (0/1/2).
    #[must_use]
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Playing => 1,
            Self::Paused => 2,
        }
    }
}

impl Serialize for PlayState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.to_wire())
    }
}

impl<'de> Deserialize<'de> for PlayState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u8::deserialize(deserializer)? {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Playing),
            2 => Ok(Self::Paused),
            other => Err(D::Error::custom(format!("unknown playback state {other}"))),
        }
    }
}

/// v3 `GenericMediaMetadata` — advisory display data attached to a play request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaMetadata {
    /// Display title.
    pub title: Option<String>,
    /// Cover art / thumbnail URL.
    pub thumbnail_url: Option<String>,
    /// Sender-defined extra data, passed through untouched.
    pub custom: Option<Value>,
}

/// Deserialize the v3 `MetadataObject`: an integer-tagged union with one published
/// variant (`type: 0`, Generic).
///
/// A metadata object of an *unknown* type deserializes to `None` rather than failing
/// the whole `Play`: metadata is advisory (a title on the screen), and refusing to
/// play media over unreadable decoration would be disproportionate. Unknown *opcodes*
/// are declined (#241's scope note); unknown decoration is dropped. Explicit `null`
/// for a field means the same as absent, which is what the reference sender emits.
fn de_metadata<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<MediaMetadata>, D::Error> {
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let Value::Object(map) = value else {
        return Err(D::Error::custom("metadata is not an object"));
    };
    if map.get("type").and_then(Value::as_u64) != Some(0) {
        return Ok(None);
    }
    let text = |key: &str| {
        map.get(key)
            .filter(|v| !v.is_null())
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Ok(Some(MediaMetadata {
        title: text("title"),
        thumbnail_url: text("thumbnailUrl"),
        custom: map.get("custom").filter(|v| !v.is_null()).cloned(),
    }))
}

/// Serialize [`MediaMetadata`] back into the tagged wire object.
fn ser_metadata<S: Serializer>(
    metadata: &Option<MediaMetadata>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let Some(metadata) = metadata else {
        // Unreachable through the skip attribute on the fields below; serialize
        // something honest anyway rather than panicking.
        return serializer.serialize_none();
    };
    let mut map = serde_json::Map::new();
    map.insert("type".into(), Value::from(0u64));
    if let Some(title) = &metadata.title {
        map.insert("title".into(), Value::from(title.clone()));
    }
    if let Some(url) = &metadata.thumbnail_url {
        map.insert("thumbnailUrl".into(), Value::from(url.clone()));
    }
    if let Some(custom) = &metadata.custom {
        map.insert("custom".into(), custom.clone());
    }
    map.serialize(serializer)
}

/// `VersionMessage` — both directions, v2+ (and implicitly how v1 is detected: a v1
/// sender never sends one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionMessage {
    /// The highest protocol version the peer speaks.
    pub version: u64,
}

/// `PlayMessage` — the v3 superset. v1 and v2 bodies parse into it too, since every
/// field they lack is optional here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayMessage {
    /// The MIME type of the content (`video/mp4`, `application/json`, ...).
    pub container: String,
    /// The URL to load. This receiver's supported path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Inline content (a DASH manifest, playlist JSON, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Start position in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    /// Desired volume, 0.0-1.0 (v3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    /// Playback speed factor (v2+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// HTTP request headers for the media fetch (v2+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Advisory display metadata (v3).
    #[serde(
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_metadata",
        serialize_with = "ser_metadata"
    )]
    pub metadata: Option<MediaMetadata>,
}

/// `SeekMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct SeekMessage {
    /// Absolute target position in seconds.
    pub time: f64,
}

/// `SetVolumeMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct SetVolumeMessage {
    /// Desired volume, 0.0-1.0.
    pub volume: f64,
}

/// `SetSpeedMessage` (v2+).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct SetSpeedMessage {
    /// Desired playback speed factor.
    pub speed: f64,
}

/// `SetPlaylistItemMessage` (v3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPlaylistItemMessage {
    /// Zero-based playlist index to jump to.
    pub item_index: u64,
}

/// `InitialSenderMessage` (v3) — who connected.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitialSenderMessage {
    /// Human-readable device name.
    pub display_name: Option<String>,
    /// Sending application.
    pub app_name: Option<String>,
    /// Sending application version.
    pub app_version: Option<String>,
}

/// The events a v3 sender can subscribe to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventSubscription {
    /// A media item started.
    MediaItemStart,
    /// A media item finished.
    MediaItemEnd,
    /// The current media item changed.
    MediaItemChange,
    /// Keys went down on the receiver. This panel has no keys, so the subscription is
    /// accepted and never fires.
    KeyDown(Vec<String>),
    /// Keys came up on the receiver. As above.
    KeyUp(Vec<String>),
}

impl<'de> Deserialize<'de> for EventSubscription {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let ty = value
            .get("type")
            .and_then(Value::as_u64)
            .ok_or_else(|| D::Error::custom("event subscription has no integer `type`"))?;
        let keys = || -> Result<Vec<String>, D::Error> {
            value
                .get("keys")
                .and_then(Value::as_array)
                .ok_or_else(|| D::Error::custom("key event subscription has no `keys` array"))?
                .iter()
                .map(|k| {
                    k.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| D::Error::custom("`keys` entry is not a string"))
                })
                .collect()
        };
        match ty {
            0 => Ok(Self::MediaItemStart),
            1 => Ok(Self::MediaItemEnd),
            2 => Ok(Self::MediaItemChange),
            3 => Ok(Self::KeyDown(keys()?)),
            4 => Ok(Self::KeyUp(keys()?)),
            other => Err(D::Error::custom(format!("unknown event type {other}"))),
        }
    }
}

/// `SubscribeEventMessage` / `UnsubscribeEventMessage` (v3) share one body shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EventSubscriptionMessage {
    /// Which event to (un)subscribe.
    pub event: EventSubscription,
}

/// One entry of a v3 playlist (`MediaItem`).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MediaItem {
    /// The MIME type of this item.
    pub container: String,
    /// The URL to load.
    pub url: Option<String>,
    /// Inline content — unsupported here, refused at load (see `player`).
    pub content: Option<String>,
    /// Start position in seconds.
    pub time: Option<f64>,
    /// Desired volume, 0.0-1.0.
    pub volume: Option<f64>,
    /// Playback speed factor.
    pub speed: Option<f64>,
    /// Whether the receiver should preload this item. Advisory; this receiver
    /// fetches on demand.
    pub cache: Option<bool>,
    /// How long an image item stays on screen, in seconds.
    pub show_duration: Option<f64>,
    /// HTTP request headers for the media fetch.
    pub headers: Option<HashMap<String, String>>,
    /// Advisory display metadata.
    #[serde(deserialize_with = "de_metadata")]
    pub metadata: Option<MediaMetadata>,
}

/// v3 `PlaylistContent`: what a `Play` with `container: application/json` carries.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistContent {
    /// The content type tag. Only `0` (playlist) is published; anything else fails
    /// [`PlaylistContent::from_json`].
    pub content_type: u64,
    /// The items, in play order.
    pub items: Vec<MediaItem>,
    /// Index of the first item to play.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Desired volume for the whole playlist, 0.0-1.0.
    #[serde(default)]
    pub volume: Option<f64>,
    /// Playback speed factor for the whole playlist.
    #[serde(default)]
    pub speed: Option<f64>,
    /// Preload hint: items to cache ahead. Advisory.
    #[serde(default)]
    pub forward_cache: Option<u64>,
    /// Preload hint: items to cache behind. Advisory.
    #[serde(default)]
    pub backward_cache: Option<u64>,
    /// Advisory display metadata for the playlist itself.
    #[serde(default, deserialize_with = "de_metadata")]
    pub metadata: Option<MediaMetadata>,
}

impl PlaylistContent {
    /// Parse playlist JSON out of a `Play` body's `content` field.
    ///
    /// # Errors
    /// [`FCastError::MalformedBody`] when the JSON does not parse or the tag is not
    /// the published playlist tag.
    pub fn from_json(content: &str) -> Result<Self, FCastError> {
        let playlist: Self =
            serde_json::from_str(content).map_err(|e| FCastError::MalformedBody {
                opcode: Opcode::Play,
                detail: format!("playlist content: {e}"),
            })?;
        if playlist.content_type != 0 {
            return Err(FCastError::MalformedBody {
                opcode: Opcode::Play,
                detail: format!("unknown contentType {}", playlist.content_type),
            });
        }
        Ok(playlist)
    }
}

// ---------------------------------------------------------------------------
// Outbound bodies, one per version where the versions disagree.
// ---------------------------------------------------------------------------

/// v1 `PlaybackUpdateMessage`: time and state only.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlaybackUpdateV1 {
    /// Position in seconds.
    pub time: f64,
    /// Playback state.
    pub state: PlayState,
}

/// v2 `PlaybackUpdateMessage`: every field required.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackUpdateV2 {
    /// When this snapshot was taken (unix milliseconds).
    pub generation_time: u64,
    /// Position in seconds.
    pub time: f64,
    /// Duration in seconds.
    pub duration: f64,
    /// Playback state.
    pub state: PlayState,
    /// Playback speed factor.
    pub speed: f64,
}

/// v3 `PlaybackUpdateMessage`: `generationTime` and `state` required, the rest
/// nullable.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackUpdateV3 {
    /// When this snapshot was taken (unix milliseconds).
    pub generation_time: u64,
    /// Playback state.
    pub state: PlayState,
    /// Position in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    /// Duration in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Playback speed factor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// Playlist index currently playing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_index: Option<u64>,
}

/// v1 `VolumeUpdateMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct VolumeUpdateV1 {
    /// Current volume, 0.0-1.0.
    pub volume: f64,
}

/// v2+ `VolumeUpdateMessage`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeUpdateV2 {
    /// When this snapshot was taken (unix milliseconds).
    pub generation_time: u64,
    /// Current volume, 0.0-1.0.
    pub volume: f64,
}

/// `PlaybackErrorMessage` (v2+).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaybackErrorMessage {
    /// Human-readable description of what failed.
    pub message: String,
}

/// `InitialReceiverMessage` (v3) — who we are, and what is already playing so a
/// sender that joins mid-session starts in sync.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitialReceiverMessage {
    /// The advertised receiver name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// This application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_name: Option<String>,
    /// This application's version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    /// The currently loaded content, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_data: Option<PlayMessage>,
}

/// `PlayUpdateMessage` (v3) — broadcast when any sender changes what is loaded.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayUpdateMessage {
    /// When the change happened (unix milliseconds).
    pub generation_time: u64,
    /// The newly loaded content (`None` after a stop).
    pub play_data: Option<PlayMessage>,
}

/// Which media-item event fired (v3 `EventType`, media rows only — this panel has no
/// keys, so key events can never occur).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaItemEventKind {
    /// `MediaItemStart` (0).
    Start,
    /// `MediaItemEnd` (1).
    End,
    /// `MediaItemChange` (2).
    Change,
}

impl MediaItemEventKind {
    /// The wire event-type number.
    #[must_use]
    pub const fn to_wire(self) -> u64 {
        match self {
            Self::Start => 0,
            Self::End => 1,
            Self::Change => 2,
        }
    }
}

/// Build a v3 `EventMessage` frame for a media-item event.
///
/// The item is echoed as the spec's `MediaItemEvent { type, item }`, rebuilt from the
/// play request that started it.
#[must_use]
pub fn media_item_event_frame(
    generation_time: u64,
    kind: MediaItemEventKind,
    item: &PlayMessage,
) -> Frame {
    let event = serde_json::json!({
        "generationTime": generation_time,
        "event": {
            "type": kind.to_wire(),
            "item": item,
        },
    });
    json_frame(Opcode::Event, &event)
}

/// Frame a serializable body under an opcode.
///
/// Serialization of our own outbound types cannot fail (no maps with non-string keys,
/// no non-finite floats reachable — positions and durations come from the pipeline's
/// clock), so this is total; a frame over the packet ceiling would be a bug in our
/// own body construction and is clamped by `wire::encode` at the actor.
#[must_use]
pub fn json_frame<T: Serialize>(opcode: Opcode, body: &T) -> Frame {
    match serde_json::to_vec(body) {
        Ok(bytes) => Frame::with_body(opcode, bytes),
        // Defensive: emit an empty body rather than panicking in a library crate.
        Err(_) => Frame::bare(opcode),
    }
}

/// Parse a frame's body as `T`, faulting with the opcode attached.
///
/// # Errors
/// [`FCastError::BodyNotUtf8`] or [`FCastError::MalformedBody`].
pub fn parse_body<T: for<'de> Deserialize<'de>>(frame: &Frame) -> Result<T, FCastError> {
    let text = std::str::from_utf8(&frame.body).map_err(|_| FCastError::BodyNotUtf8)?;
    serde_json::from_str(text).map_err(|e| FCastError::MalformedBody {
        opcode: frame.opcode,
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The exact `Play` body the reference sender emitted for a URL cast
    /// (`tests/fixtures/play-url.jsonl`).
    const CAPTURED_PLAY_URL: &str = r#"{"container":"video/mp4","url":"http://example.com/media/BigBuckBunny.mp4","time":10.0,"speed":1.25,"headers":{"Authorization":"Bearer sekrit"}}"#;

    #[test]
    fn the_reference_senders_play_body_parses() {
        let play: PlayMessage = serde_json::from_str(CAPTURED_PLAY_URL).unwrap();
        assert_eq!(play.container, "video/mp4");
        assert_eq!(
            play.url.as_deref(),
            Some("http://example.com/media/BigBuckBunny.mp4")
        );
        assert_eq!(play.time, Some(10.0));
        assert_eq!(play.speed, Some(1.25));
        assert_eq!(
            play.headers.unwrap().get("Authorization").unwrap(),
            "Bearer sekrit"
        );
        assert_eq!(play.content, None);
        assert_eq!(play.metadata, None);
    }

    /// A v1 sender's minimal body — only `container` and `url` — must parse into the
    /// same superset type.
    #[test]
    fn a_v1_play_body_parses_into_the_superset() {
        let play: PlayMessage =
            serde_json::from_str(r#"{"container":"video/mp4","url":"http://h/v.mp4"}"#).unwrap();
        assert_eq!(play.time, None);
        assert_eq!(play.speed, None);
    }

    /// v2 requires every `PlaybackUpdate` field; the golden bytes here are what the
    /// reference receiver's own serde produces for the same values.
    #[test]
    fn v2_playback_update_serializes_every_field() {
        let update = PlaybackUpdateV2 {
            generation_time: 1_754_700_000_000,
            time: 12.5,
            duration: 596.5,
            state: PlayState::Playing,
            speed: 1.0,
        };
        assert_eq!(
            serde_json::to_string(&update).unwrap(),
            r#"{"generationTime":1754700000000,"time":12.5,"duration":596.5,"state":1,"speed":1.0}"#
        );
    }

    /// v3 omits what it does not know rather than inventing zeros: an idle update
    /// carries no position, duration, speed or index.
    #[test]
    fn v3_playback_update_omits_what_it_does_not_know() {
        let update = PlaybackUpdateV3 {
            generation_time: 7,
            state: PlayState::Idle,
            time: None,
            duration: None,
            speed: None,
            item_index: None,
        };
        assert_eq!(
            serde_json::to_string(&update).unwrap(),
            r#"{"generationTime":7,"state":0}"#
        );
    }

    /// The reference sender subscribes to `MediaItemEnd` on every connection
    /// (`tests/fixtures/*.jsonl` row 4); its exact body must parse.
    #[test]
    fn the_reference_senders_subscription_parses() {
        let msg: EventSubscriptionMessage =
            serde_json::from_str(r#"{"event":{"type":1}}"#).unwrap();
        assert_eq!(msg.event, EventSubscription::MediaItemEnd);
    }

    #[test]
    fn key_subscriptions_carry_their_key_lists() {
        let msg: EventSubscriptionMessage =
            serde_json::from_str(r#"{"event":{"type":3,"keys":["ArrowLeft","Enter"]}}"#).unwrap();
        assert_eq!(
            msg.event,
            EventSubscription::KeyDown(vec!["ArrowLeft".into(), "Enter".into()])
        );
    }

    /// The reference repository ships a playlist example
    /// (`senders/terminal/video_playlist_example.json`); the captured `Play` that
    /// carries it is replayed end-to-end in `tests/real_sender_transcripts.rs`. Here:
    /// the content-level parse.
    #[test]
    fn a_playlist_with_a_tag_we_do_not_know_is_refused() {
        assert!(matches!(
            PlaylistContent::from_json(r#"{"contentType":9,"items":[]}"#),
            Err(FCastError::MalformedBody { .. })
        ));
        let playlist =
            PlaylistContent::from_json(r#"{"contentType":0,"items":[{"container":"video/mp4","url":"http://h/a.mp4"}],"offset":1}"#)
                .unwrap();
        assert_eq!(playlist.items.len(), 1);
        assert_eq!(playlist.offset, Some(1));
    }

    /// Metadata is advisory: an unknown metadata type drops to `None` instead of
    /// failing the play, while the published Generic type parses fully, treating
    /// explicit `null` as absent the way the reference sender writes it.
    #[test]
    fn metadata_is_advisory_and_null_tolerant() {
        let play: PlayMessage = serde_json::from_str(
            r#"{"container":"video/mp4","url":"http://h/v.mp4","metadata":{"type":0,"title":"Big Buck Bunny","thumbnailUrl":null}}"#,
        )
        .unwrap();
        let metadata = play.metadata.unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Big Buck Bunny"));
        assert_eq!(metadata.thumbnail_url, None);

        let play: PlayMessage = serde_json::from_str(
            r#"{"container":"video/mp4","url":"http://h/v.mp4","metadata":{"type":42,"weird":true}}"#,
        )
        .unwrap();
        assert_eq!(play.metadata, None);
    }

    /// `playData` inside `InitialReceiverMessage` round-trips the play request,
    /// including metadata, so a second sender joining mid-session sees what the first
    /// one loaded.
    #[test]
    fn initial_receiver_echoes_play_data() {
        let play: PlayMessage = serde_json::from_str(
            r#"{"container":"video/mp4","url":"http://h/v.mp4","metadata":{"type":0,"title":"T"}}"#,
        )
        .unwrap();
        let initial = InitialReceiverMessage {
            display_name: Some("dma.space/screen".into()),
            app_name: Some("castaway".into()),
            app_version: Some("0.1.0".into()),
            play_data: Some(play.clone()),
        };
        let json = serde_json::to_string(&initial).unwrap();
        let echoed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(echoed["displayName"], "dma.space/screen");
        assert_eq!(echoed["playData"]["url"], "http://h/v.mp4");
        assert_eq!(echoed["playData"]["metadata"]["type"], 0);
        assert_eq!(echoed["playData"]["metadata"]["title"], "T");
    }

    #[test]
    fn media_item_events_carry_the_item_and_the_tag() {
        let play: PlayMessage =
            serde_json::from_str(r#"{"container":"video/mp4","url":"http://h/v.mp4"}"#).unwrap();
        let frame = media_item_event_frame(9, MediaItemEventKind::End, &play);
        assert_eq!(frame.opcode, Opcode::Event);
        let value: Value = serde_json::from_slice(&frame.body).unwrap();
        assert_eq!(value["generationTime"], 9);
        assert_eq!(value["event"]["type"], 1);
        assert_eq!(value["event"]["item"]["url"], "http://h/v.mp4");
    }
}
