//! The YouTube Lounge (MDX) bind channel. After DIAL launches our "YouTube app", we
//! register a screen with YouTube's Lounge server and subscribe to a BrowserChannel-
//! style long-poll. The server pushes length-prefixed JSON command arrays; this module
//! is the pure parser + command→[`SessionEvent`] mapping (ground rule 3). The HTTP
//! long-poll client (`gsessionid`/`RID`/`AID`/`SID`) is the actor.

pub mod sender;

use castaway_core::{ControlTxn, MediaUri, SessionEvent};
use serde_json::Value;

use crate::error::DialError;

/// One command from the bind channel: `[aid, [name, payload?]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct LoungeCommand {
    /// The array id (monotonic; used as `AID` to resume the channel).
    pub aid: i64,
    /// The command name (`setPlaylist`, `play`, `seekTo`, …).
    pub name: String,
    /// The command payload object (may be null).
    pub payload: Value,
}

/// Parse a BrowserChannel response body into its commands.
///
/// Framing: repeated `<char-length>\n<json-array>` chunks, where each JSON array holds
/// `[aid, [name, payload]]` entries.
///
/// # Errors
/// [`DialError::MalformedChunk`] on a bad length prefix or invalid JSON.
pub fn parse_chunks(text: &str) -> Result<Vec<LoungeCommand>, DialError> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        // Skip stray whitespace between chunks.
        while i < chars.len() && chars[i].is_whitespace() && chars[i] != '\n' {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return Err(DialError::MalformedChunk("expected length prefix"));
        }
        let len: usize = chars[start..i]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| DialError::MalformedChunk("bad length"))?;
        if i < chars.len() && chars[i] == '\n' {
            i += 1;
        } else {
            return Err(DialError::MalformedChunk("length not followed by newline"));
        }
        if i + len > chars.len() {
            return Err(DialError::MalformedChunk("chunk truncated"));
        }
        let json: String = chars[i..i + len].iter().collect();
        i += len;

        let val: Value =
            serde_json::from_str(&json).map_err(|_| DialError::MalformedChunk("bad json"))?;
        if let Some(entries) = val.as_array() {
            for entry in entries {
                if let Some(cmd) = parse_entry(entry) {
                    out.push(cmd);
                }
            }
        }
    }
    Ok(out)
}

fn parse_entry(entry: &Value) -> Option<LoungeCommand> {
    let arr = entry.as_array()?;
    let aid = arr.first()?.as_i64()?;
    let inner = arr.get(1)?.as_array()?;
    let name = inner.first()?.as_str()?.to_string();
    let payload = inner.get(1).cloned().unwrap_or(Value::Null);
    Some(LoungeCommand { aid, name, payload })
}

/// Map a Lounge command to a session event, if it drives playback. Returns `None` for
/// informational commands (`getNowPlaying`, `onStateChange`, …) the actor answers with
/// status rather than a pipeline action.
#[must_use]
pub fn to_event(cmd: &LoungeCommand) -> Option<SessionEvent> {
    match cmd.name.as_str() {
        "setPlaylist" | "updatePlaylist" => set_playlist(&cmd.payload),
        "play" => Some(SessionEvent::Control(ControlTxn::Play)),
        "pause" => Some(SessionEvent::Control(ControlTxn::Pause)),
        "stopVideo" => Some(SessionEvent::Control(ControlTxn::Stop)),
        "next" => Some(SessionEvent::Control(ControlTxn::Next)),
        "previous" => Some(SessionEvent::Control(ControlTxn::Previous)),
        "seekTo" => {
            let secs = cmd.payload.get("newTime").and_then(Value::as_f64)?;
            Some(SessionEvent::Control(ControlTxn::Seek(
                std::time::Duration::from_secs_f64(secs.max(0.0)),
            )))
        }
        "setVolume" => {
            let vol = cmd.payload.get("volume").and_then(Value::as_f64)?;
            #[allow(clippy::cast_possible_truncation)]
            Some(SessionEvent::Control(ControlTxn::Volume(
                (vol / 100.0).clamp(0.0, 1.0) as f32,
            )))
        }
        _ => None,
    }
}

fn set_playlist(payload: &Value) -> Option<SessionEvent> {
    let ids: Vec<String> = match payload.get("videoIds").and_then(Value::as_str) {
        Some(csv) if !csv.is_empty() => csv.split(',').map(str::to_string).collect(),
        _ => payload
            .get("videoId")
            .and_then(Value::as_str)
            .map(|id| vec![id.to_string()])
            .unwrap_or_default(),
    };
    let items: Vec<MediaUri> = ids.iter().filter_map(|id| video_url(id)).collect();
    if items.is_empty() {
        return None;
    }
    let start_index = payload
        .get("currentIndex")
        .and_then(Value::as_i64)
        .and_then(|i| usize::try_from(i).ok())
        .unwrap_or(0)
        .min(items.len().saturating_sub(1));
    Some(SessionEvent::Control(ControlTxn::SetQueue {
        items,
        start_index,
    }))
}

/// Build a watch URL for a YouTube video id (the pipeline / yt-dlp resolves the stream).
fn video_url(video_id: &str) -> Option<MediaUri> {
    MediaUri::parse(&format!("https://www.youtube.com/watch?v={video_id}")).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_a_two_command_chunk() {
        // One chunk containing two commands: onStateChange then play.
        let json = r#"[[7,["onStateChange",{"state":"1"}]],[8,["play"]]]"#;
        let text = format!("{}\n{}", json.chars().count(), json);
        let cmds = parse_chunks(&text).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "onStateChange");
        assert_eq!(cmds[1].aid, 8);
        assert_eq!(cmds[1].name, "play");
    }

    #[test]
    fn parses_multiple_chunks() {
        let c1 = r#"[[1,["play"]]]"#;
        let c2 = r#"[[2,["pause"]]]"#;
        let text = format!(
            "{}\n{}{}\n{}",
            c1.chars().count(),
            c1,
            c2.chars().count(),
            c2
        );
        let cmds = parse_chunks(&text).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "play");
        assert_eq!(cmds[1].name, "pause");
    }

    #[test]
    fn set_playlist_maps_to_queue_of_watch_urls() {
        let cmd = LoungeCommand {
            aid: 1,
            name: "setPlaylist".into(),
            payload: serde_json::json!({
                "videoIds": "abc123,def456,ghi789",
                "currentIndex": 1,
                "currentTime": 0
            }),
        };
        match to_event(&cmd).unwrap() {
            SessionEvent::Control(ControlTxn::SetQueue { items, start_index }) => {
                assert_eq!(items.len(), 3);
                assert_eq!(
                    items[0].to_string(),
                    "https://www.youtube.com/watch?v=abc123"
                );
                assert_eq!(start_index, 1);
            }
            _ => panic!("expected SetQueue"),
        }
    }

    #[test]
    fn single_video_id_playlist() {
        let cmd = LoungeCommand {
            aid: 1,
            name: "setPlaylist".into(),
            payload: serde_json::json!({ "videoId": "solo" }),
        };
        match to_event(&cmd).unwrap() {
            SessionEvent::Control(ControlTxn::SetQueue { items, .. }) => {
                assert_eq!(items.len(), 1);
            }
            _ => panic!("expected SetQueue"),
        }
    }

    #[test]
    fn transport_commands_map_to_control() {
        let mk = |name: &str, payload: Value| LoungeCommand {
            aid: 1,
            name: name.into(),
            payload,
        };
        assert!(matches!(
            to_event(&mk("pause", Value::Null)),
            Some(SessionEvent::Control(ControlTxn::Pause))
        ));
        assert!(matches!(
            to_event(&mk("seekTo", serde_json::json!({"newTime": 42.5}))),
            Some(SessionEvent::Control(ControlTxn::Seek(d))) if d == std::time::Duration::from_secs_f64(42.5)
        ));
        assert!(matches!(
            to_event(&mk("setVolume", serde_json::json!({"volume": 50}))),
            Some(SessionEvent::Control(ControlTxn::Volume(v))) if (v - 0.5).abs() < 1e-6
        ));
        assert!(to_event(&mk("getNowPlaying", Value::Null)).is_none());
    }

    #[test]
    fn rejects_bad_length_prefix() {
        assert!(parse_chunks("xx\n[]").is_err());
    }
}
