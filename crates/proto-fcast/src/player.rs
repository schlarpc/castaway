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
    ControlTxn, MediaRequest, MediaUri, NowPlaying, PlaybackEnd, PlaybackProgress, QueueItem,
    RequestHeader, SessionEvent, Volume,
};

use crate::messages::{MediaItemEventKind, PlayMessage, PlayState, PlaylistContent};
use crate::session::{PlaybackSnapshot, ReceiverUpdate};

/// A load or control request the player refuses, with the message a v1-v3 sender
/// is shown (as a `PlaybackError`) and the typed kind a v4 sender gets (as
/// `Error {{ kind }}`). A refusal changes nothing: whatever was playing keeps
/// playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The human-readable reason (v1-v3's error surface).
    pub message: String,
    /// The typed kind (v4's error surface).
    pub kind: fcast_flatbuf::flat::ErrorKind,
}

impl Refusal {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: fcast_flatbuf::flat::ErrorKind::Internal,
        }
    }

    pub(crate) fn kinded(message: impl Into<String>, kind: fcast_flatbuf::flat::ErrorKind) -> Self {
        Self {
            message: message.into(),
            kind,
        }
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
    /// The fetch, headers and all (#251). FCast is the one protocol here whose senders
    /// say how to open the media as well as where it is — Grayjay puts an
    /// `Authorization: Bearer …` on an auth-gated source — and before this seam existed
    /// the URL was kept and the headers dropped.
    request: MediaRequest,
    start: Option<Duration>,
    /// The item in `PlayMessage` shape, for media-item events and `playData` echoes.
    echo: PlayMessage,
    title: Option<String>,
}

impl ResolvedItem {
    /// Resolve one play request or playlist entry. `what` names the entry in a
    /// refusal message.
    fn resolve(play: PlayMessage, what: &str) -> Result<Self, Refusal> {
        use fcast_flatbuf::flat::ErrorKind;
        if play.url.is_none() && play.content.is_some() {
            // Inline content needs somewhere to be fetched *from*, and the adapter has
            // already published it on the shared host when there is one
            // (`resolve_sources`, #249). Reaching here means there is not — a build with
            // no HTTP surface mounted — and a sender told "unsupported" can fall back to
            // its URL path, which is better than a load that hangs.
            return Err(Refusal::kinded(
                format!("{what}: inline content is not supported by this receiver; send a URL"),
                ErrorKind::UnsupportedFormat,
            ));
        }
        let Some(url) = play.url.as_deref() else {
            return Err(Refusal::kinded(
                format!("{what}: no URL to play"),
                ErrorKind::MalformedBody,
            ));
        };
        let uri = MediaUri::parse(url)
            .map_err(|e| Refusal::kinded(format!("{what}: {e}"), ErrorKind::UnsupportedFormat))?;
        let headers = request_headers(
            play.headers
                .iter()
                .flatten()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            what,
        )?;
        let start = play
            .time
            .filter(|&t| t > 0.0)
            .and_then(|t| Duration::try_from_secs_f64(t).ok());
        let title = play.metadata.as_ref().and_then(|m| m.title.clone());
        Ok(Self {
            request: MediaRequest::with_headers(uri, headers),
            start,
            title,
            echo: play,
        })
    }

    /// Resolve one v4 item (#248).
    ///
    /// `fcomp://` — FCompanion's scheme, media the *sender* serves — has already been
    /// rewritten to the local proxy by the actor when a host is configured (#249). It
    /// reaches here only in a build with no HTTP surface, where `ResourceNotFound` is the
    /// truthful answer: the resource exists, and this receiver cannot get at it.
    fn resolve_v4(item: &crate::v4msg::V4MediaItem, what: &str) -> Result<Self, Refusal> {
        use fcast_flatbuf::flat::ErrorKind;
        let uri = MediaUri::parse(&item.source_url).map_err(|e| {
            let kind = if item.source_url.starts_with("fcomp://") {
                ErrorKind::ResourceNotFound
            } else {
                ErrorKind::UnsupportedFormat
            };
            Refusal::kinded(format!("{what}: {e}"), kind)
        })?;
        let echo = PlayMessage {
            container: item.container.clone(),
            url: Some(item.source_url.clone()),
            content: None,
            time: item.start_time.map(|d| d.as_secs_f64()),
            volume: item.volume.map(f64::from),
            speed: item.speed.map(f64::from),
            headers: (!item.headers.is_empty()).then(|| item.headers.iter().cloned().collect()),
            metadata: item
                .title
                .clone()
                .map(|title| crate::messages::MediaMetadata {
                    title: Some(title),
                    thumbnail_url: item.thumbnail_url.clone(),
                    custom: None,
                }),
        };
        let headers = request_headers(
            item.headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
            what,
        )?;
        Ok(Self {
            request: MediaRequest::with_headers(uri, headers),
            start: item.start_time.filter(|d| !d.is_zero()),
            title: item.title.clone(),
            echo,
        })
    }
}

/// Turn what a sender sent into headers we will actually put on a fetch.
///
/// Sorted by name, which the wire is not: v1-v3 carries them in a JSON object and
/// `serde_json` hands those back in hash order, so the *same* `play` produced a different
/// header block on each run and no fixture could pin it. v4's vector is ordered, and
/// sorting it too costs nothing and makes the two dialects produce one block.
///
/// A header we will not send is a refusal rather than a silent drop: a sender that puts
/// `Authorization: Bearer …\r\nHost: elsewhere` on an item is either broken or hostile,
/// and playing the media without the header it said was required would look like success.
fn request_headers<'a>(
    headers: impl Iterator<Item = (&'a str, &'a str)>,
    what: &str,
) -> Result<Vec<RequestHeader>, Refusal> {
    use fcast_flatbuf::flat::ErrorKind;
    let mut parsed = headers
        .map(|(name, value)| {
            RequestHeader::new(name, value)
                .map_err(|e| Refusal::kinded(format!("{what}: {e}"), ErrorKind::MalformedBody))
        })
        .collect::<Result<Vec<_>, _>>()?;
    parsed.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(parsed)
}

#[derive(Debug)]
struct Loaded {
    /// The original request, echoed as `playData` to late-joining v3 senders.
    play: PlayMessage,
    queue: Vec<ResolvedItem>,
    index: usize,
    /// Whether this came in as a playlist — a single URL has no `itemIndex`.
    is_playlist: bool,
    /// Whether finishing an item plays the next. v3 playlists always advance
    /// (their protocol has no flag); v4 queues carry it explicitly.
    autoplay: bool,
    /// The raw v4 `Load` when this was a v4 *single* — replayed (stripped) to a
    /// sender that joins mid-session, exactly the slice the reference replays.
    v4_single_raw: Option<Vec<u8>>,
    /// Whether v4 queue mutation applies (`QueueInsert` needs a *queue*, not a
    /// single, and not a v3 playlist whose senders couldn't see the change).
    v4_queue: bool,
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
                // The items are not known until the playlist has been fetched, and this
                // is a pure function. The adapter fetches it at the boundary when there is
                // a host configured (#249); reaching here means the fetch was not possible
                // or did not succeed, and the sender is told so rather than left waiting.
                return Err(Refusal::kinded(
                    "playlist by URL is not supported by this receiver; send the playlist inline",
                    fcast_flatbuf::flat::ErrorKind::UnsupportedFormat,
                ));
            };
            let playlist = PlaylistContent::from_json(content).map_err(|e| {
                Refusal::kinded(
                    format!("playlist: {e}"),
                    fcast_flatbuf::flat::ErrorKind::MalformedBody,
                )
            })?;
            if playlist.items.is_empty() {
                return Err(Refusal::kinded(
                    "playlist: no items",
                    fcast_flatbuf::flat::ErrorKind::MalformedBody,
                ));
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
            // v3 playlists have no autoplay flag; the protocol's receivers have
            // always advanced, and so do we.
            autoplay: true,
            v4_single_raw: None,
            v4_queue: false,
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

    /// Load v4 content (#248): a single item, or a queue with the reference's
    /// exact acceptance rules (256-item cap as `MalformedBody`, a start index
    /// past the end as `QueuePositionOutOfRange`).
    ///
    /// # Errors
    /// A typed [`Refusal`]; nothing changes on refusal.
    pub fn load_v4(
        &mut self,
        source: crate::v4msg::LoadSource,
        raw: Vec<u8>,
    ) -> Result<Applied, Refusal> {
        use crate::v4msg::LoadSource;
        use fcast_flatbuf::flat::ErrorKind;

        let (queue, index, is_playlist, autoplay, v4_single_raw, v4_queue) = match source {
            LoadSource::Single(item) => {
                let resolved = ResolvedItem::resolve_v4(&item, "play request")?;
                (vec![resolved], 0, false, true, Some(raw.clone()), false)
            }
            LoadSource::Queue {
                items,
                start_index,
                autoplay,
            } => {
                if items.is_empty() {
                    return Err(Refusal::kinded("queue: no items", ErrorKind::MalformedBody));
                }
                if items.len() > 256 {
                    return Err(Refusal::kinded(
                        format!("queue of {} items exceeds the 256-item cap", items.len()),
                        ErrorKind::MalformedBody,
                    ));
                }
                let index = usize::from(start_index.unwrap_or(0));
                if index >= items.len() {
                    return Err(Refusal::kinded(
                        format!("start index {index} outside 0..{}", items.len()),
                        ErrorKind::QueuePositionOutOfRange,
                    ));
                }
                let resolved: Vec<ResolvedItem> = items
                    .iter()
                    .enumerate()
                    .map(|(i, (item, _show))| {
                        ResolvedItem::resolve_v4(item, &format!("queue item {i}"))
                    })
                    .collect::<Result<_, _>>()?;
                (resolved, index, true, autoplay, None, true)
            }
        };

        if let Some(volume) = queue[index].echo.volume.filter(|v| (0.0..=1.0).contains(v)) {
            self.volume = volume;
        }
        let play_echo = queue[index].echo.clone();
        self.loaded = Some(Loaded {
            play: play_echo.clone(),
            queue,
            index,
            is_playlist,
            autoplay,
            v4_single_raw,
            v4_queue,
        });
        self.state = PlayState::Playing;

        let mut applied = self.start_current();
        // v4 peers get the stripped raw relay; v1-v3 peers the PlayMessage echo.
        applied
            .updates
            .insert(0, ReceiverUpdate::PlayChanged(Some(play_echo)));
        applied.updates.insert(1, ReceiverUpdate::V4Load { raw });
        applied.updates.push(ReceiverUpdate::Volume(self.volume));
        applied
            .events
            .push(SessionEvent::Control(ControlTxn::Volume(
                #[allow(clippy::cast_possible_truncation)]
                Volume::from_position(self.volume as f32),
            )));
        Ok(applied)
    }

    /// The raw v4 `Load` currently playing, when it was a v4 single — replayed
    /// (stripped) to late-joining v4 senders, the slice the reference replays.
    #[must_use]
    pub fn v4_single_raw(&self) -> Option<&[u8]> {
        self.loaded
            .as_ref()
            .and_then(|l| l.v4_single_raw.as_deref())
    }

    /// Resolve a v4 queue position against the current queue length.
    fn position_index(
        loaded: &Loaded,
        position: crate::v4msg::QueuePosition,
        insert: bool,
    ) -> Result<usize, Refusal> {
        use crate::v4msg::QueuePosition;
        use fcast_flatbuf::flat::ErrorKind;
        let len = loaded.queue.len();
        let index = match position {
            QueuePosition::Index(i) => usize::from(i),
            QueuePosition::Front => 0,
            // For an insert, `Back` means append; for select/remove, the last item.
            QueuePosition::Back => {
                if insert {
                    len
                } else {
                    len.saturating_sub(1)
                }
            }
        };
        let limit = if insert { len } else { len.saturating_sub(1) };
        if index > limit {
            return Err(Refusal::kinded(
                format!("position {index} outside the queue of {len}"),
                ErrorKind::QueuePositionOutOfRange,
            ));
        }
        Ok(index)
    }

    fn v4_queue_mut(&mut self) -> Result<&mut Loaded, Refusal> {
        use fcast_flatbuf::flat::ErrorKind;
        self.loaded
            .as_mut()
            .filter(|l| l.v4_queue)
            .ok_or_else(|| Refusal::kinded("no queue is loaded", ErrorKind::InvalidState))
    }

    /// Insert a v4 queue item (#248).
    ///
    /// # Errors
    /// `InvalidState` without a queue, `QueueFull` at the 256 cap,
    /// `QueuePositionOutOfRange` past the end.
    pub fn queue_insert_v4(
        &mut self,
        item: &crate::v4msg::V4MediaItem,
        position: crate::v4msg::QueuePosition,
        raw: Vec<u8>,
    ) -> Result<Applied, Refusal> {
        use fcast_flatbuf::flat::ErrorKind;
        let resolved = ResolvedItem::resolve_v4(item, "inserted item")?;
        let loaded = self.v4_queue_mut()?;
        if loaded.queue.len() >= 256 {
            return Err(Refusal::kinded(
                "the queue already holds 256 items",
                ErrorKind::QueueFull,
            ));
        }
        let index = Self::position_index(loaded, position, true)?;
        if index <= loaded.index {
            loaded.index += 1;
        }
        loaded.queue.insert(index, resolved);
        Ok(Applied {
            events: vec![SessionEvent::UpNext(Self::up_next(loaded))],
            updates: vec![
                ReceiverUpdate::QueueInsertRelay { raw },
                ReceiverUpdate::Playback(self.snapshot(None)),
            ],
        })
    }

    /// Remove a v4 queue item (#248). The playing item cannot be removed.
    ///
    /// # Errors
    /// `InvalidState`, `QueuePositionOutOfRange`, or `QueueRemovePlayingItem`.
    pub fn queue_remove_v4(
        &mut self,
        position: crate::v4msg::QueuePosition,
    ) -> Result<Applied, Refusal> {
        use fcast_flatbuf::flat::ErrorKind;
        let loaded = self.v4_queue_mut()?;
        let index = Self::position_index(loaded, position, false)?;
        if index == loaded.index {
            return Err(Refusal::kinded(
                "cannot remove the playing item",
                ErrorKind::QueueRemovePlayingItem,
            ));
        }
        if index < loaded.index {
            loaded.index -= 1;
        }
        loaded.queue.remove(index);
        Ok(Applied {
            events: vec![SessionEvent::UpNext(Self::up_next(loaded))],
            updates: vec![
                ReceiverUpdate::QueueRemoveRelay(position),
                ReceiverUpdate::Playback(self.snapshot(None)),
            ],
        })
    }

    /// Jump to a v4 queue item (#248).
    ///
    /// # Errors
    /// `InvalidState` or `QueuePositionOutOfRange`.
    pub fn queue_select_v4(
        &mut self,
        position: crate::v4msg::QueuePosition,
    ) -> Result<Applied, Refusal> {
        let loaded = self.v4_queue_mut()?;
        let index = Self::position_index(loaded, position, false)?;
        loaded.index = index;
        self.state = PlayState::Playing;
        let mut applied = self.start_current();
        applied.updates.push(ReceiverUpdate::QueueSelectRelay {
            position,
            initiated_by_receiver: false,
        });
        Ok(applied)
    }

    fn up_next(loaded: &Loaded) -> Vec<QueueItem> {
        loaded.queue[loaded.index + 1..]
            .iter()
            .map(|next| {
                QueueItem::new(
                    next.title
                        .clone()
                        .unwrap_or_else(|| next.request.to_string()),
                )
            })
            .collect()
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
            .map(|next| {
                QueueItem::new(
                    next.title
                        .clone()
                        .unwrap_or_else(|| next.request.to_string()),
                )
            })
            .collect();

        let mut events = vec![
            SessionEvent::Play {
                source: item.request.clone(),
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
            updates: vec![
                ReceiverUpdate::Playback(self.snapshot(Some(PlaybackProgress::at(target)))),
                ReceiverUpdate::Progress {
                    position: target,
                    duration: None,
                },
            ],
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
        Err(Refusal::kinded(
            format!("playback speed {speed} is not supported by this receiver (plays at 1.0)"),
            fcast_flatbuf::flat::ErrorKind::RateOutOfRange,
        ))
    }

    /// Jump to a playlist index.
    ///
    /// # Errors
    /// A [`Refusal`] when nothing is loaded, the load is not a playlist, or the
    /// index is out of range.
    pub fn set_playlist_item(&mut self, index: u64) -> Result<Applied, Refusal> {
        let Some(loaded) = &mut self.loaded else {
            return Err(Refusal::kinded(
                "no playlist is loaded",
                fcast_flatbuf::flat::ErrorKind::InvalidState,
            ));
        };
        if !loaded.is_playlist {
            return Err(Refusal::kinded(
                "the loaded content is not a playlist",
                fcast_flatbuf::flat::ErrorKind::InvalidState,
            ));
        }
        let Some(index) = usize::try_from(index)
            .ok()
            .filter(|&i| i < loaded.queue.len())
        else {
            return Err(Refusal::kinded(
                format!(
                    "playlist item {index} is out of range (0..{})",
                    loaded.queue.len()
                ),
                fcast_flatbuf::flat::ErrorKind::QueuePositionOutOfRange,
            ));
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
            return Err(Refusal::kinded(
                "no playlist is loaded",
                fcast_flatbuf::flat::ErrorKind::InvalidState,
            ));
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
            PlaybackEnd::Finished if loaded.autoplay && loaded.index + 1 < loaded.queue.len() => {
                loaded.index += 1;
                let next = loaded.index;
                let announce_v4 = loaded.v4_queue;
                self.state = PlayState::Playing;
                let mut applied = self.start_current();
                updates.append(&mut applied.updates);
                if announce_v4 {
                    // The receiver moved the pointer, so *every* sender hears
                    // it — the originator distinction only exists for
                    // sender-driven selections.
                    #[allow(clippy::cast_possible_truncation)]
                    updates.push(ReceiverUpdate::QueueSelectRelay {
                        position: crate::v4msg::QueuePosition::Index(next as u8),
                        initiated_by_receiver: true,
                    });
                }
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
                updates.push(ReceiverUpdate::Error {
                    message: format!("playback failed: {message}"),
                    kind: fcast_flatbuf::flat::ErrorKind::Internal,
                });
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
                SessionEvent::Play { source, .. } => Some(source.uri().to_string()),
                _ => None,
            })
            .expect("a Play event")
    }

    fn played_headers(applied: &Applied) -> Vec<(String, String)> {
        applied
            .events
            .iter()
            .find_map(|e| match e {
                SessionEvent::Play { source, .. } => Some(
                    source
                        .headers()
                        .iter()
                        .map(|h| (h.name().to_owned(), h.value().to_owned()))
                        .collect(),
                ),
                _ => None,
            })
            .expect("a Play event")
    }

    /// #251: the captured fixture has `Authorization: Bearer …` on a plain `play`, and
    /// the sender did tell us how to fetch the media. It now rides the `Play`.
    #[test]
    fn a_v1_to_v3_play_carries_its_request_headers() {
        let mut player = Player::new();
        let play = PlayMessage {
            headers: Some(
                [
                    ("Authorization".to_string(), "Bearer sekrit".to_string()),
                    ("Referer".to_string(), "https://h/".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
            ..play_url("http://h/v.mp4")
        };
        let applied = player.load(play).unwrap();
        // Sorted, because the wire here is a JSON object and serde hands those back in
        // hash order — the same `play` must produce the same fetch on every run.
        assert_eq!(
            played_headers(&applied),
            [
                ("Authorization".to_string(), "Bearer sekrit".to_string()),
                ("Referer".to_string(), "https://h/".to_string()),
            ]
        );
    }

    /// A header we would not send is a typed refusal, not a silent drop: fetching
    /// without the credential the sender said was required would look like success and
    /// fail at the server.
    #[test]
    fn a_header_that_would_forge_a_request_is_refused() {
        let mut player = Player::new();
        let play = PlayMessage {
            headers: Some(
                [(
                    "Authorization".to_string(),
                    "Bearer a\r\nHost: elsewhere".to_string(),
                )]
                .into_iter()
                .collect(),
            ),
            ..play_url("http://h/v.mp4")
        };
        let refusal = player.load(play).unwrap_err();
        assert_eq!(refusal.kind, fcast_flatbuf::flat::ErrorKind::MalformedBody);
        assert!(player.play_data().is_none(), "a refusal changes nothing");
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
        assert!(
            refusal.message.contains("inline content"),
            "{}",
            refusal.message
        );
        assert_eq!(
            player.play_data().unwrap().url.as_deref(),
            Some("http://h/ok.mp4")
        );

        let refusal = player.load(play_url("ftp://h/no.mp4")).unwrap_err();
        assert!(
            refusal.message.contains("unsupported scheme"),
            "{}",
            refusal.message
        );
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
        assert!(applied.updates.iter().any(
            |u| matches!(u, ReceiverUpdate::Error { message, .. } if message.contains("404"))
        ));
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
