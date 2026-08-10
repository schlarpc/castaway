//! The receiver-side player model shared by every FCast connection: what is loaded,
//! the playlist and its index, the playback state, and the volume.
//!
//! Pure (ground rule 3): commands come in, `SessionEvent`s for the pipeline and
//! [`ReceiverUpdate`]s for the sessions come out. The actor owns the lock and the
//! clock; nothing here does I/O. This is where FCast's *semantics* live — the
//! playlist is owned here, not by the pipeline, because advancing it means issuing a
//! fresh `Play` per item and telling every connected sender which index is up
//! (`itemIndex`), both of which are protocol facts (rule 2: each protocol owns its
//! own state machine).

use std::time::Duration;

use castaway_core::{
    ControlTxn, MediaUri, NowPlaying, PlaybackEnd, PlaybackProgress, QueueItem, SessionEvent,
    Volume,
};

use crate::messages::{MediaItemEventKind, PlayMessage, PlayState, PlaylistContent};
use crate::session::{PlaybackSnapshot, ReceiverUpdate};

/// A load or control request the player refuses, with the message the asking sender
/// is shown (as a `PlaybackError`). A refusal changes nothing: whatever was playing
/// keeps playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl Refusal {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// What applying a command produced: events for the pipeline (via the sink) and
/// updates for every connected session.
#[derive(Debug, Default)]
pub struct Applied {
    /// Session events, in emission order.
    pub events: Vec<SessionEvent>,
    /// Broadcasts for every session ([`crate::session::Session::frame_update`]
    /// translates each per version and subscription).
    pub updates: Vec<ReceiverUpdate>,
}

/// One resolved, playable entry.
#[derive(Debug, Clone)]
struct ResolvedItem {
    uri: MediaUri,
    start: Option<Duration>,
    /// The item in `PlayMessage` shape, for media-item events and `playData` echoes.
    echo: PlayMessage,
    title: Option<String>,
}

impl ResolvedItem {
    /// Resolve one play request or playlist entry. `what` names the entry in a
    /// refusal message.
    fn resolve(play: PlayMessage, what: &str) -> Result<Self, Refusal> {
        if play.url.is_none() && play.content.is_some() {
            // Inline content (a DASH manifest pushed as text) needs somewhere to be
            // fetched *from* — hosting it is issue #249. Refused rather than guessed
            // at: a sender told "unsupported" can fall back to its URL path.
            return Err(Refusal::new(format!(
                "{what}: inline content is not supported by this receiver; send a URL"
            )));
        }
        let Some(url) = play.url.as_deref() else {
            return Err(Refusal::new(format!("{what}: no URL to play")));
        };
        let uri = MediaUri::parse(url).map_err(|e| Refusal::new(format!("{what}: {e}")))?;
        let start = play
            .time
            .filter(|&t| t > 0.0)
            .and_then(|t| Duration::try_from_secs_f64(t).ok());
        let title = play.metadata.as_ref().and_then(|m| m.title.clone());
        Ok(Self {
            uri,
            start,
            title,
            echo: play,
        })
    }
}

#[derive(Debug)]
struct Loaded {
    /// The original request, echoed as `playData` to late-joining v3 senders.
    play: PlayMessage,
    queue: Vec<ResolvedItem>,
    index: usize,
    /// Whether this came in as a playlist — a single URL has no `itemIndex`.
    is_playlist: bool,
}

/// The shared player model. One per adapter, behind the actor's lock.
#[derive(Debug)]
pub struct Player {
    loaded: Option<Loaded>,
    state: PlayState,
    volume: f64,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    /// An idle player at full volume.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            loaded: None,
            state: PlayState::Idle,
            volume: 1.0,
        }
    }

    /// The current load, for `InitialReceiverMessage.playData`.
    #[must_use]
    pub fn play_data(&self) -> Option<&PlayMessage> {
        self.loaded.as_ref().map(|l| &l.play)
    }

    /// The current volume (0.0-1.0).
    #[must_use]
    pub const fn volume(&self) -> f64 {
        self.volume
    }

    /// The playback snapshot senders are told about, joined with the position the
    /// pipeline reported — the clock is read at the actor boundary, not here.
    #[must_use]
    pub fn snapshot(&self, progress: Option<PlaybackProgress>) -> PlaybackSnapshot {
        let progress = progress.filter(|_| self.state != PlayState::Idle);
        PlaybackSnapshot {
            state: self.state,
            time: progress.map(|p| p.position.as_secs_f64()),
            duration: progress.and_then(|p| p.duration).map(|d| d.as_secs_f64()),
            // This pipeline has no rate control (issue #250); updates say so
            // honestly rather than echoing a speed nothing is playing at.
            speed: 1.0,
            item_index: self.item_index(),
        }
    }

    fn item_index(&self) -> Option<u64> {
        self.loaded
            .as_ref()
            .filter(|l| l.is_playlist)
            .map(|l| l.index as u64)
    }

    /// Load new content: a single URL, or a playlist when the container is
    /// `application/json`.
    ///
    /// # Errors
    /// A [`Refusal`] naming what could not be played. Nothing changes on refusal.
    pub fn load(&mut self, play: PlayMessage) -> Result<Applied, Refusal> {
        let (queue, index, is_playlist) = if play.container == "application/json" {
            let Some(content) = play.content.as_deref() else {
                // A playlist *URL* would need fetching before we know its items —
                // deliberately not guessed at (issue #249 covers hosted/fetched
                // content in both directions).
                return Err(Refusal::new(
                    "playlist by URL is not supported by this receiver; send the playlist inline",
                ));
            };
            let playlist = PlaylistContent::from_json(content)
                .map_err(|e| Refusal::new(format!("playlist: {e}")))?;
            if playlist.items.is_empty() {
                return Err(Refusal::new("playlist: no items"));
            }
            let items: Vec<ResolvedItem> = playlist
                .items
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    let play = PlayMessage {
                        container: item.container,
                        url: item.url,
                        content: item.content,
                        time: item.time,
                        volume: item.volume,
                        speed: item.speed,
                        headers: item.headers,
                        metadata: item.metadata,
                    };
                    ResolvedItem::resolve(play, &format!("playlist item {i}"))
                })
                .collect::<Result<_, _>>()?;
            let index = usize::try_from(playlist.offset.unwrap_or(0))
                .unwrap_or(usize::MAX)
                .min(items.len() - 1);
            (items, index, true)
        } else {
            (
                vec![ResolvedItem::resolve(play.clone(), "play request")?],
                0,
                false,
            )
        };

        if let Some(volume) = play.volume.filter(|v| (0.0..=1.0).contains(v)) {
            self.volume = volume;
        }
        self.loaded = Some(Loaded {
            play: play.clone(),
            queue,
            index,
            is_playlist,
        });
        self.state = PlayState::Playing;

        let mut applied = self.start_current();
        applied
            .updates
            .insert(0, ReceiverUpdate::PlayChanged(Some(play)));
        applied.updates.push(ReceiverUpdate::Volume(self.volume));
        applied
            .events
            .push(SessionEvent::Control(ControlTxn::Volume(
                #[allow(clippy::cast_possible_truncation)]
                Volume::from_position(self.volume as f32),
            )));
        Ok(applied)
    }

    /// Events and updates for starting (or restarting) the current queue item.
    ///
    /// The event kind follows the reference receiver's rule, which `fast`'s
    /// `cast_simple_playlist` case pins: a *single* load fires `MediaItemStart`, a
    /// *playlist* item fires `MediaItemChange` — on every item start, the first
    /// included. Never both.
    fn start_current(&self) -> Applied {
        let Some(loaded) = &self.loaded else {
            return Applied::default();
        };
        let item = &loaded.queue[loaded.index];
        let mut now_playing = NowPlaying::new(castaway_core::PlaybackState::Playing);
        now_playing.title = item.title.clone();
        now_playing.duration = None;
        let up_next: Vec<QueueItem> = loaded.queue[loaded.index + 1..]
            .iter()
            .map(|next| QueueItem::new(next.title.clone().unwrap_or_else(|| next.uri.to_string())))
            .collect();

        let mut events = vec![
            SessionEvent::Play {
                source: item.uri.clone(),
                start: item.start,
            },
            SessionEvent::NowPlaying(now_playing),
        ];
        if !up_next.is_empty() {
            events.push(SessionEvent::UpNext(up_next));
        }
        let updates = vec![
            ReceiverUpdate::MediaItem {
                kind: if loaded.is_playlist {
                    MediaItemEventKind::Change
                } else {
                    MediaItemEventKind::Start
                },
                item: item.echo.clone(),
            },
            ReceiverUpdate::Playback(self.snapshot(None)),
        ];
        Applied { events, updates }
    }

    /// Pause. A no-op when nothing is playing.
    pub fn pause(&mut self) -> Applied {
        if self.state != PlayState::Playing {
            return Applied::default();
        }
        self.state = PlayState::Paused;
        Applied {
            events: vec![SessionEvent::Control(ControlTxn::Pause)],
            updates: vec![ReceiverUpdate::Playback(self.snapshot(None))],
        }
    }

    /// Resume. A no-op unless paused.
    pub fn resume(&mut self) -> Applied {
        if self.state != PlayState::Paused {
            return Applied::default();
        }
        self.state = PlayState::Playing;
        Applied {
            events: vec![SessionEvent::Control(ControlTxn::Play)],
            updates: vec![ReceiverUpdate::Playback(self.snapshot(None))],
        }
    }

    /// Stop and unload — the spec's "clear any playback related state".
    pub fn stop(&mut self) -> Applied {
        if self.loaded.is_none() {
            return Applied::default();
        }
        self.loaded = None;
        self.state = PlayState::Idle;
        Applied {
            events: vec![SessionEvent::Control(ControlTxn::Stop)],
            updates: vec![
                ReceiverUpdate::PlayChanged(None),
                ReceiverUpdate::Playback(self.snapshot(None)),
            ],
        }
    }

    /// Seek. The transport state does not move — a paused, scrubbed session stays
    /// paused.
    pub fn seek(&mut self, target: Duration) -> Applied {
        if self.loaded.is_none() {
            return Applied::default();
        }
        Applied {
            events: vec![SessionEvent::Control(ControlTxn::Seek(target))],
            updates: vec![ReceiverUpdate::Playback(
                self.snapshot(Some(PlaybackProgress::at(target))),
            )],
        }
    }

    /// Set the volume (already clamped by the session) and tell everyone.
    pub fn set_volume(&mut self, volume: f64) -> Applied {
        self.volume = volume;
        Applied {
            events: vec![SessionEvent::Control(ControlTxn::Volume(
                #[allow(clippy::cast_possible_truncation)]
                Volume::from_position(volume as f32),
            ))],
            updates: vec![ReceiverUpdate::Volume(volume)],
        }
    }

    /// Set playback speed — refused, because the pipeline has no rate control
    /// (issue #250) and reporting a speed nothing plays at would be faking.
    ///
    /// # Errors
    /// Always, until #250: the [`Refusal`] the asking sender sees.
    pub fn set_speed(&mut self, speed: f64) -> Result<Applied, Refusal> {
        if (speed - 1.0).abs() < f64::EPSILON {
            // Asking for 1.0 is asking for what is already true.
            return Ok(Applied::default());
        }
        Err(Refusal::new(format!(
            "playback speed {speed} is not supported by this receiver (plays at 1.0)"
        )))
    }

    /// Jump to a playlist index.
    ///
    /// # Errors
    /// A [`Refusal`] when nothing is loaded, the load is not a playlist, or the
    /// index is out of range.
    pub fn set_playlist_item(&mut self, index: u64) -> Result<Applied, Refusal> {
        let Some(loaded) = &mut self.loaded else {
            return Err(Refusal::new("no playlist is loaded"));
        };
        if !loaded.is_playlist {
            return Err(Refusal::new("the loaded content is not a playlist"));
        }
        let Some(index) = usize::try_from(index)
            .ok()
            .filter(|&i| i < loaded.queue.len())
        else {
            return Err(Refusal::new(format!(
                "playlist item {index} is out of range (0..{})",
                loaded.queue.len()
            )));
        };
        loaded.index = index;
        self.state = PlayState::Playing;
        Ok(self.start_current())
    }

    /// Step to the adjacent playlist item (the panel's next/previous buttons).
    ///
    /// # Errors
    /// A [`Refusal`] at either end of the playlist, or when there is none.
    pub fn step(&mut self, forward: bool) -> Result<Applied, Refusal> {
        let Some(loaded) = &self.loaded else {
            return Err(Refusal::new("no playlist is loaded"));
        };
        let index = if forward {
            loaded.index as u64 + 1
        } else {
            u64::try_from(loaded.index)
                .ok()
                .and_then(|i| i.checked_sub(1))
                .ok_or_else(|| Refusal::new("already at the first playlist item"))?
        };
        self.set_playlist_item(index)
    }

    /// The pipeline finished with the current item: advance the playlist, or go
    /// idle. `Failed` also tells every sender what went wrong.
    pub fn media_ended(&mut self, end: &PlaybackEnd) -> Applied {
        let Some(loaded) = &mut self.loaded else {
            return Applied::default();
        };
        let ended = loaded.queue[loaded.index].echo.clone();
        let mut updates = vec![ReceiverUpdate::MediaItem {
            kind: MediaItemEventKind::End,
            item: ended,
        }];

        match end {
            PlaybackEnd::Finished if loaded.index + 1 < loaded.queue.len() => {
                loaded.index += 1;
                self.state = PlayState::Playing;
                let mut applied = self.start_current();
                updates.append(&mut applied.updates);
                Applied {
                    events: applied.events,
                    updates,
                }
            }
            PlaybackEnd::Finished => {
                self.state = PlayState::Idle;
                updates.push(ReceiverUpdate::Playback(self.snapshot(None)));
                Applied {
                    events: Vec::new(),
                    updates,
                }
            }
            PlaybackEnd::Failed(message) => {
                self.state = PlayState::Idle;
                updates.push(ReceiverUpdate::Error(format!("playback failed: {message}")));
                updates.push(ReceiverUpdate::Playback(self.snapshot(None)));
                Applied {
                    events: Vec::new(),
                    updates,
                }
            }
            // `PlaybackEnd` is non_exhaustive; a new way for media to end is at
            // least an end.
            _ => {
                self.state = PlayState::Idle;
                updates.push(ReceiverUpdate::Playback(self.snapshot(None)));
                Applied {
                    events: Vec::new(),
                    updates,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn play_url(url: &str) -> PlayMessage {
        PlayMessage {
            container: "video/mp4".into(),
            url: Some(url.into()),
            ..PlayMessage::default()
        }
    }

    fn playlist_json(urls: &[&str], offset: Option<u64>) -> PlayMessage {
        let items: Vec<serde_json::Value> = urls
            .iter()
            .map(|u| serde_json::json!({"container": "video/mp4", "url": u}))
            .collect();
        let mut content = serde_json::json!({"contentType": 0, "items": items});
        if let Some(offset) = offset {
            content["offset"] = offset.into();
        }
        PlayMessage {
            container: "application/json".into(),
            content: Some(content.to_string()),
            ..PlayMessage::default()
        }
    }

    fn played_uri(applied: &Applied) -> String {
        applied
            .events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Play { source, .. } => Some(source.url().to_string()),
                _ => None,
            })
            .expect("a Play event")
    }

    #[test]
    fn a_url_load_plays_and_tells_everyone() {
        let mut player = Player::new();
        let applied = player.load(play_url("http://h/v.mp4")).unwrap();
        assert_eq!(played_uri(&applied), "http://h/v.mp4");
        assert!(applied
            .updates
            .iter()
            .any(|u| matches!(u, ReceiverUpdate::PlayChanged(Some(_)))));
        assert!(applied.updates.iter().any(|u| matches!(
            u,
            ReceiverUpdate::Playback(PlaybackSnapshot {
                state: PlayState::Playing,
                ..
            })
        )));
        assert!(player.play_data().is_some());
    }

    /// A refusal changes nothing: the first load keeps playing, `playData` still
    /// answers with it, and the refusing message names the problem.
    #[test]
    fn a_refused_load_leaves_the_current_session_alone() {
        let mut player = Player::new();
        player.load(play_url("http://h/ok.mp4")).unwrap();
        let refusal = player
            .load(PlayMessage {
                container: "video/mp4".into(),
                content: Some("<manifest/>".into()),
                ..PlayMessage::default()
            })
            .unwrap_err();
        assert!(refusal.0.contains("inline content"), "{}", refusal.0);
        assert_eq!(
            player.play_data().unwrap().url.as_deref(),
            Some("http://h/ok.mp4")
        );

        let refusal = player.load(play_url("ftp://h/no.mp4")).unwrap_err();
        assert!(refusal.0.contains("unsupported scheme"), "{}", refusal.0);
    }

    /// The playlist is owned here: finishing an item plays the next one and the
    /// `itemIndex` in the snapshot moves, without any sender doing anything.
    #[test]
    fn finishing_an_item_advances_the_playlist() {
        let mut player = Player::new();
        let applied = player
            .load(playlist_json(&["http://h/a.mp4", "http://h/b.mp4"], None))
            .unwrap();
        assert_eq!(played_uri(&applied), "http://h/a.mp4");
        assert_eq!(player.snapshot(None).item_index, Some(0));
        // A playlist item fires `MediaItemChange` — the reference's rule, pinned by
        // fast's cast_simple_playlist, and it fires for the FIRST item too. `Start`
        // is the single-load event and must not fire here.
        assert!(applied.updates.iter().any(|u| matches!(
            u,
            ReceiverUpdate::MediaItem {
                kind: MediaItemEventKind::Change,
                ..
            }
        )));
        assert!(!applied.updates.iter().any(|u| matches!(
            u,
            ReceiverUpdate::MediaItem {
                kind: MediaItemEventKind::Start,
                ..
            }
        )));

        let applied = player.media_ended(&PlaybackEnd::Finished);
        assert_eq!(played_uri(&applied), "http://h/b.mp4");
        assert_eq!(player.snapshot(None).item_index, Some(1));
        // The end of the old item and the change to the new both fire.
        assert!(applied.updates.iter().any(|u| matches!(
            u,
            ReceiverUpdate::MediaItem {
                kind: MediaItemEventKind::End,
                ..
            }
        )));
        assert!(applied.updates.iter().any(|u| matches!(
            u,
            ReceiverUpdate::MediaItem {
                kind: MediaItemEventKind::Change,
                ..
            }
        )));

        // The last item ends: idle, nothing more to play.
        let applied = player.media_ended(&PlaybackEnd::Finished);
        assert!(applied.events.is_empty());
        assert_eq!(player.snapshot(None).state, PlayState::Idle);
    }

    #[test]
    fn the_playlist_offset_picks_the_first_item() {
        let mut player = Player::new();
        let applied = player
            .load(playlist_json(
                &["http://h/a.mp4", "http://h/b.mp4"],
                Some(1),
            ))
            .unwrap();
        assert_eq!(played_uri(&applied), "http://h/b.mp4");
        // An overshooting offset clamps to the last item rather than refusing the
        // whole playlist.
        let applied = player
            .load(playlist_json(
                &["http://h/a.mp4", "http://h/b.mp4"],
                Some(9),
            ))
            .unwrap();
        assert_eq!(played_uri(&applied), "http://h/b.mp4");
    }

    #[test]
    fn set_playlist_item_jumps_and_out_of_range_is_refused() {
        let mut player = Player::new();
        player
            .load(playlist_json(&["http://h/a.mp4", "http://h/b.mp4"], None))
            .unwrap();
        let applied = player.set_playlist_item(1).unwrap();
        assert_eq!(played_uri(&applied), "http://h/b.mp4");
        assert!(player.set_playlist_item(2).is_err());

        // A single URL is not a playlist; jumping in it is refused, not index 0.
        let mut single = Player::new();
        single.load(play_url("http://h/v.mp4")).unwrap();
        assert!(single.set_playlist_item(0).is_err());
    }

    /// Pause/resume follow the state machine: pausing what isn't playing and
    /// resuming what isn't paused are no-ops, not errors — the reference receiver
    /// treats them the same way.
    #[test]
    fn pause_and_resume_follow_the_state() {
        let mut player = Player::new();
        assert!(player.pause().events.is_empty());
        player.load(play_url("http://h/v.mp4")).unwrap();
        assert!(matches!(
            player.pause().events[..],
            [SessionEvent::Control(ControlTxn::Pause)]
        ));
        assert!(player.pause().events.is_empty(), "already paused");
        assert!(matches!(
            player.resume().events[..],
            [SessionEvent::Control(ControlTxn::Play)]
        ));
        assert!(player.resume().events.is_empty(), "already playing");
    }

    /// Stop clears the load (the spec's own wording), so a late-joining sender sees
    /// no `playData` and every connected one sees `PlayUpdate(None)` and idle.
    #[test]
    fn stop_unloads() {
        let mut player = Player::new();
        player.load(play_url("http://h/v.mp4")).unwrap();
        let applied = player.stop();
        assert!(applied
            .updates
            .iter()
            .any(|u| matches!(u, ReceiverUpdate::PlayChanged(None))));
        assert_eq!(player.play_data(), None);
        assert!(player.stop().events.is_empty(), "stop when idle is a no-op");
    }

    /// Speed is refused, not faked (#250): the snapshot always reports the 1.0 the
    /// pipeline actually plays at, and asking for 1.0 succeeds as a no-op.
    #[test]
    fn speed_is_refused_not_faked() {
        let mut player = Player::new();
        player.load(play_url("http://h/v.mp4")).unwrap();
        assert!(player.set_speed(2.0).is_err());
        assert!(player.set_speed(1.0).is_ok());
        assert!((player.snapshot(None).speed - 1.0).abs() < f64::EPSILON);
    }

    /// A failed fetch reaches every sender as a `PlaybackError` and the state goes
    /// idle — the sender's UI shows the failure instead of a progress bar frozen at
    /// zero.
    #[test]
    fn a_failed_item_reports_and_goes_idle() {
        let mut player = Player::new();
        player.load(play_url("http://h/gone.mp4")).unwrap();
        let applied = player.media_ended(&PlaybackEnd::Failed("404".into()));
        assert!(applied
            .updates
            .iter()
            .any(|u| matches!(u, ReceiverUpdate::Error(e) if e.contains("404"))));
        assert_eq!(player.snapshot(None).state, PlayState::Idle);
    }

    /// `Play.volume` (v3) applies before the media starts, and out-of-range values
    /// are ignored rather than clamped — a volume of 5.0 is a confused sender, not a
    /// request for maximum.
    #[test]
    fn play_volume_applies_when_sane() {
        let mut player = Player::new();
        let mut play = play_url("http://h/v.mp4");
        play.volume = Some(0.25);
        player.load(play).unwrap();
        assert!((player.volume() - 0.25).abs() < f64::EPSILON);

        let mut play = play_url("http://h/v.mp4");
        play.volume = Some(5.0);
        player.load(play).unwrap();
        assert!((player.volume() - 0.25).abs() < f64::EPSILON, "unchanged");
    }
}
