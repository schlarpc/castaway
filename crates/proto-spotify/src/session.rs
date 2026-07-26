//! The live Connect session: what happens after a phone pairs.
//!
//! [`discovery`] gets us credentials without anyone typing a password; this module turns
//! those credentials into a device that actually plays. The Spotify half — access point,
//! login5, the dealer WebSocket, connect-state, audio keys, CDN fetch and Vorbis decode —
//! is librespot's (DECISION-LOG D30). What lives here is the seam: credentials in,
//! [`SessionEvent`]s out, and one session at a time.
//!
//! Threading follows ground rule 4. `Spirc` and the player-event pump are ordinary tokio
//! tasks; librespot's player runs its decode on its own thread and reaches us through the
//! [`PcmSink`] channel, so nothing blocking lands on a runtime worker.
//!
//! [`discovery`]: crate::discovery

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use castaway_core::{FrameSource, OsdSink};
use castaway_core::{
    NowPlaying, PcmFrame, PlaybackState, SessionEvent, SessionSink, SourceDescription,
};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::dealer::protocol::Message;
use librespot_core::{Session, SessionConfig};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use librespot_protocol::connect::ClusterUpdate;
use librespot_protocol::player::ProvidedTrack;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::control::SpotifyRemote;
use crate::error::SpotifyError;
use crate::sink::PcmSink;

/// How often the player reports where it is in the track.
///
/// Only used to keep the now-playing card's progress honest. One second is the coarsest
/// interval that still looks like it is moving.
const POSITION_INTERVAL: Duration = Duration::from_secs(1);

/// Who paired, and the credential blob they handed over.
///
/// The blob is still the *outer*-decrypted form from [`crate::discovery::add_user`];
/// unwrapping the inner reusable credential is librespot's
/// [`Credentials::with_blob`], which needs the same `device_id` we advertised.
#[derive(Debug, Clone)]
pub struct PairedUser {
    /// The Spotify account name the sender named in `addUser`.
    pub user_name: String,
    /// The decrypted credentials blob.
    pub blob: Vec<u8>,
}

/// Fixed identity of the Connect device we present.
#[derive(Debug, Clone)]
pub struct ConnectSettings {
    /// Name shown in the Spotify device picker.
    pub device_name: String,
    /// Stable device id. Must be byte-identical to the one `getInfo` advertised, because
    /// it keys the blob decryption — a mismatch fails login with nothing that looks like
    /// a cause.
    pub device_id: String,
    /// Volume the device comes up at, as a fraction of full scale.
    pub initial_volume: f32,
}

/// A handle the zeroconf endpoint uses to hand freshly paired credentials to the runner.
#[derive(Debug, Clone)]
pub struct ConnectHandle {
    tx: mpsc::Sender<PairedUser>,
}

impl ConnectHandle {
    /// Hand over a newly paired user. Replaces whatever session is running.
    ///
    /// # Errors
    /// [`SpotifyError::SessionGone`] if the runner task has stopped.
    pub async fn paired(&self, user: PairedUser) -> Result<(), SpotifyError> {
        self.tx
            .send(user)
            .await
            .map_err(|_| SpotifyError::SessionGone)
    }
}

/// Everything one live Connect session owns, so dropping it stops the session.
struct LiveSession {
    spirc: Arc<Spirc>,
    spirc_task: JoinHandle<()>,
    events_task: JoinHandle<()>,
    queue_task: JoinHandle<()>,
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Spirc` is an opaque command sender; the tasks are the only useful state.
        f.debug_struct("LiveSession")
            .field("spirc_task", &self.spirc_task)
            .field("events_task", &self.events_task)
            .field("queue_task", &self.queue_task)
            .finish_non_exhaustive()
    }
}

impl LiveSession {
    /// Stop the session and release the account.
    fn shutdown(self) {
        // Ask politely first: this pauses playback and tells the cloud we are no longer
        // the active device, so the user's phone stops showing castaway as playing.
        if let Err(e) = self.spirc.shutdown() {
            debug!(error = %e, "spotify: spirc shutdown failed, aborting instead");
        }
        self.spirc_task.abort();
        self.events_task.abort();
        self.queue_task.abort();
    }
}

/// Start the runner. Returns the handle the HTTP endpoint pushes credentials into.
///
/// The runner is a task rather than something the caller drives, because pairing arrives
/// on an axum handler that must answer the phone promptly — starting a network login
/// inline would hold the HTTP response open for the length of an AP handshake.
#[must_use]
pub fn spawn(settings: ConnectSettings, sink: SessionSink, osd: Option<OsdSink>) -> ConnectHandle {
    // Depth 1: pairings are human-paced, and if two arrive at once only the later one
    // matters — but it must not be dropped, so this is a send, not a try_send.
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(run(settings, sink, osd, rx));
    ConnectHandle { tx }
}

/// The runner loop: one session at a time, replaced whenever someone new pairs.
async fn run(
    settings: ConnectSettings,
    sink: SessionSink,
    osd: Option<OsdSink>,
    mut rx: mpsc::Receiver<PairedUser>,
) {
    let mut current: Option<LiveSession> = None;

    while let Some(user) = rx.recv().await {
        // Retire the old session *before* starting the new one. Two Connect sessions on
        // one device id fight over the same registration, and the account that loses is
        // whichever the cloud saw last — which is not necessarily the person standing in
        // front of the panel.
        if let Some(previous) = current.take() {
            info!("spotify: replacing the active session");
            previous.shutdown();
        }

        let user_name = user.user_name.clone();
        match start(&settings, user, &sink).await {
            Ok(live) => {
                info!(user = %user_name, "spotify: connect session up");
                if let Some(osd) = &osd {
                    osd.banner(
                        format!("Spotify: {user_name} connected"),
                        Duration::from_secs(4),
                    );
                }
                current = Some(live);
            }
            Err(e) => {
                // The overwhelmingly likely causes are a non-Premium account or a stale
                // blob, and both look identical from the phone — it just silently fails
                // to appear. Say which one on the panel.
                warn!(user = %user_name, error = %e, "spotify: connect session failed");
                if let Some(osd) = &osd {
                    osd.banner(
                        format!("Spotify: {user_name} — {e}"),
                        Duration::from_secs(8),
                    );
                }
            }
        }
    }

    if let Some(previous) = current.take() {
        previous.shutdown();
    }
}

/// Bring up one session: log in, register as a Connect device, publish the audio path.
async fn start(
    settings: &ConnectSettings,
    user: PairedUser,
    sink: &SessionSink,
) -> Result<LiveSession, SpotifyError> {
    // Guard the length before handing the blob over. `Credentials::with_blob` ends with
    // `for i in 0..len - 0x10`, which underflows and panics on anything shorter than one
    // AES block — and the length is attacker-chosen: our own HMAC check only proves the
    // sender completed the Diffie-Hellman, not that the plaintext is well-formed. A
    // hostile or buggy sender on the LAN should get an error, not a panicking task.
    const MIN_BLOB: usize = 16;
    if user.blob.len() < MIN_BLOB {
        return Err(SpotifyError::Login(format!(
            "credential blob is {} bytes, need at least {MIN_BLOB}",
            user.blob.len()
        )));
    }

    let credentials = Credentials::with_blob(
        &user.user_name,
        base64::engine::general_purpose::STANDARD.encode(&user.blob),
        &settings.device_id,
    )
    .map_err(|e| SpotifyError::Login(format!("credentials rejected: {e}")))?;

    let session = Session::new(
        SessionConfig {
            device_id: settings.device_id.clone(),
            ..SessionConfig::default()
        },
        // No cache. A hackerspace panel logs in as whoever walked up, and a credential
        // cache would silently re-log-in the *last* person after a restart.
        None,
    );

    let mixer = Arc::new(
        SoftMixer::open(MixerConfig::default())
            .map_err(|e| SpotifyError::Login(format!("mixer: {e}")))?,
    );

    let (pcm_tx, pcm_rx) = PcmSink::channel();
    let player = Player::new(
        PlayerConfig {
            position_update_interval: Some(POSITION_INTERVAL),
            ..PlayerConfig::default()
        },
        session.clone(),
        mixer.get_soft_volume(),
        move || Box::new(PcmSink::new(pcm_tx)),
    );
    let events = player.get_player_event_channel();

    // Subscribe to cluster updates *before* Spirc connects, or the first one — the update
    // that accompanies the transfer that starts playback, and so the first queue we could
    // show — is gone before anyone is listening.
    let cluster_updates = session
        .dealer()
        .listen_for("hm://connect-state/v1/cluster", |msg| {
            Message::from_raw::<ClusterUpdate>(msg)
        })
        .map_err(|e| SpotifyError::Login(format!("cluster subscription: {e}")))?;

    // This is where the network actually happens: AP handshake, login5, dealer connect,
    // and the connect-state registration that makes us visible in the picker. Anything
    // wrong with the account surfaces here, before we have claimed the audio output.
    let (spirc, spirc_task) = Spirc::new(
        ConnectConfig {
            name: settings.device_name.clone(),
            initial_volume: volume_to_spotify(settings.initial_volume),
            ..ConnectConfig::default()
        },
        session.clone(),
        credentials,
        player,
        mixer,
    )
    .await
    .map_err(|e| SpotifyError::Login(login_reason(&e)))?;

    let spirc = Arc::new(spirc);

    // Only now claim the pipeline. `format` is what the adapter negotiated, and every
    // block restates it — see `PcmSink::format`.
    let (sample_rate, channels) = PcmSink::format();
    let format = castaway_core::AudioFormat::from_hz(sample_rate, channels)
        .ok_or(SpotifyError::Crypto("librespot named an impossible format"))?;
    sink.emit(SessionEvent::Audio {
        source: FrameSource::Pcm(pcm_rx),
        format,
    })
    .await
    .map_err(|_| SpotifyError::SessionGone)?;

    sink.emit(SessionEvent::SourceInfo(
        SourceDescription::new()
            .with_display_name(user.user_name.clone())
            .with_link(format!("Spotify Connect · {sample_rate} Hz · stereo")),
    ))
    .await
    .map_err(|_| SpotifyError::SessionGone)?;

    sink.emit(SessionEvent::ControlSurface(Arc::new(SpotifyRemote::new(
        Arc::clone(&spirc),
    ))))
    .await
    .map_err(|_| SpotifyError::SessionGone)?;

    let events_task = tokio::spawn(pump_events(events, sink.clone(), session.clone()));
    let queue_task = tokio::spawn(pump_queue(cluster_updates, sink.clone(), session.clone()));
    let spirc_task = tokio::spawn(spirc_task);

    Ok(LiveSession {
        spirc,
        spirc_task,
        events_task,
        queue_task,
    })
}

/// Translate librespot's player events into the now-playing surface.
///
/// Deliberately a *fold* rather than a straight map: [`NowPlaying`] is specified as a
/// full snapshot re-emitted whenever any part changes, but librespot reports the track
/// and the position in separate events. Without keeping the last track here, every
/// position tick would blank the card's text.
///
/// Cover art arrives on its own schedule too. librespot hands over image *URLs*, so the
/// bytes need a fetch, and the text must not wait for it — a card that appears a second
/// late is much worse than one whose art fills in a second late. So the fetch is spawned
/// and its result folded in when it lands, exactly the case `NowPlaying` was documented
/// to expect ("artwork arriving after the text").
async fn pump_events(
    mut events: mpsc::UnboundedReceiver<PlayerEvent>,
    sink: SessionSink,
    session: Session,
) {
    let mut snapshot = NowPlaying::new(PlaybackState::Stopped);
    // Which track the current snapshot describes, so late artwork for a track that has
    // already been skipped past is dropped rather than pasted onto its successor.
    let mut current_uri: Option<String> = None;
    let (art_tx, mut art_rx) = mpsc::channel::<(String, castaway_core::Artwork)>(2);

    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Some(event) => event,
                None => break,
            },
            Some((uri, artwork)) = art_rx.recv() => {
                if current_uri.as_deref() != Some(uri.as_str()) {
                    debug!(%uri, "spotify: cover art arrived for a track we have left");
                    continue;
                }
                snapshot.artwork = Some(artwork);
                if sink
                    .emit(SessionEvent::NowPlaying(snapshot.clone()))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };

        let changed = match event {
            PlayerEvent::TrackChanged { audio_item } => {
                apply_track(&mut snapshot, &audio_item);
                current_uri = Some(audio_item.uri.clone());
                if let Some(url) = best_cover(&audio_item) {
                    let (session, art_tx, uri) =
                        (session.clone(), art_tx.clone(), audio_item.uri.clone());
                    tokio::spawn(async move {
                        if let Some(art) = fetch_cover(&session, &url).await {
                            let _ = art_tx.send((uri, art)).await;
                        }
                    });
                }
                true
            }
            PlayerEvent::Playing { position_ms, .. } => {
                snapshot.state = PlaybackState::Playing;
                snapshot.position = Some(Duration::from_millis(u64::from(position_ms)));
                true
            }
            PlayerEvent::Paused { position_ms, .. } => {
                snapshot.state = PlaybackState::Paused;
                snapshot.position = Some(Duration::from_millis(u64::from(position_ms)));
                true
            }
            PlayerEvent::PositionChanged { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. } => {
                snapshot.position = Some(Duration::from_millis(u64::from(position_ms)));
                true
            }
            PlayerEvent::Stopped { .. } => {
                snapshot = NowPlaying::new(PlaybackState::Stopped);
                current_uri = None;
                true
            }
            PlayerEvent::Unavailable { track_id, .. } => {
                // Region-locked or pulled from the catalogue. Spirc will skip on, but the
                // card would otherwise sit on a track that is never going to play.
                warn!(track = %track_id, "spotify: track unavailable");
                snapshot.state = PlaybackState::Error;
                true
            }
            // Everything else is either internal to spirc or does not change the card.
            _ => false,
        };

        if changed
            && sink
                .emit(SessionEvent::NowPlaying(snapshot.clone()))
                .await
                .is_err()
        {
            debug!("spotify: session manager gone, stopping event pump");
            return;
        }
    }
}

/// How many queued tracks to forward to the card.
///
/// The card shows three and counts the rest, so a handful past that is enough to render
/// "and N more" honestly without shipping a 500-track playlist through the session bus on
/// every cluster update.
const UP_NEXT_LIMIT: usize = 24;

/// Watch the cloud's cluster updates and publish the queue.
///
/// This is the one piece of Connect state we read *ourselves* rather than through
/// `Spirc`, because `Spirc` exposes no accessor for the track list and `PlayerEvent`
/// carries only the current item (OPEN-QUESTIONS Q38). Subscribing a second listener to
/// the same dealer URI is supported — the dealer keeps a vector of subscribers per URI —
/// so this rides alongside spirc's own listener rather than replacing it.
///
/// Must be subscribed *before* `Spirc::new` connects, or the first cluster update (the
/// one that arrives with the transfer that started playback) is missed.
async fn pump_queue(
    mut updates: impl futures::Stream<Item = Result<ClusterUpdate, librespot_core::Error>> + Unpin,
    sink: SessionSink,
    session: Session,
) {
    use futures::StreamExt as _;

    let mut last: Vec<castaway_core::QueueItem> = Vec::new();
    let mut names = QueueNames::default();
    while let Some(update) = updates.next().await {
        let Ok(update) = update else { continue };
        // An update that carries no player state says nothing about the queue. Treating
        // that as "the queue is empty" is what blanked a 24-track queue 27 seconds after
        // it arrived, on an update that was about something else entirely.
        let Some(tracks) = queue_tracks(&update) else {
            debug!("spotify: cluster update with no player state, queue unchanged");
            continue;
        };
        let items = names.resolve(&session, tracks).await;
        // Cluster updates arrive for volume changes, device lists and playback position
        // — most of them leave the queue untouched, and re-rendering the card for each
        // would be a needless repaint on a screen people are looking at.
        if items == last {
            continue;
        }
        debug!(queued = items.len(), "spotify: queue changed");
        last.clone_from(&items);
        if sink.emit(SessionEvent::UpNext(items)).await.is_err() {
            return;
        }
    }
}

/// Extract the upcoming tracks from a cluster update.
///
/// `None` means "this update tells us nothing about the queue" — it had no player state,
/// because it was about volume, or the device list, or a session hand-off. That is
/// emphatically not the same as an empty queue, and conflating the two blanks the panel.
/// `Some(vec![])` is the real thing: a player state that genuinely has nothing queued.
fn queue_tracks(update: &ClusterUpdate) -> Option<&[ProvidedTrack]> {
    let state = update.cluster.as_ref()?.player_state.as_ref()?;

    // One line, once per change, naming the keys the cloud actually sent. The metadata
    // map is reverse-engineered surface (OPEN-QUESTIONS Q38) and this is what tells us
    // whether `title`/`artist_name` are the right guesses without another session.
    if let Some(first) = state.next_tracks.first() {
        debug!(
            keys = ?first.metadata.keys().collect::<Vec<_>>(),
            uri = %first.uri,
            "spotify: queued track metadata"
        );
    }

    let end = state.next_tracks.len().min(UP_NEXT_LIMIT);
    Some(&state.next_tracks[..end])
}

/// How many queued tracks are worth a metadata lookup when the cloud did not name them.
///
/// The card shows three and counts the rest, so anything past a small margin is never
/// read. Each miss is one request, and they are cached for the life of the session.
const RESOLVE_LIMIT: usize = 6;

/// Names for queued tracks, remembered across cluster updates.
///
/// Needed because cluster updates are frequent — volume, position, device list — and
/// re-resolving the same queue on each one would be a burst of requests per minute for a
/// list that has not changed.
#[derive(Default)]
struct QueueNames {
    seen: std::collections::HashMap<String, castaway_core::QueueItem>,
}

impl QueueNames {
    /// Build the display list, asking Spotify only for the tracks it did not name itself.
    ///
    /// Spotify hydrates a queue lazily: the first entries arrive with a full `metadata`
    /// map and the rest with almost nothing, and which is which changes as the user
    /// scrolls their queue. So "use the map, and go ask when it is missing" is the only
    /// version of this that does not sometimes print raw ids on the wall.
    async fn resolve(
        &mut self,
        session: &Session,
        tracks: &[ProvidedTrack],
    ) -> Vec<castaway_core::QueueItem> {
        let mut out = Vec::with_capacity(tracks.len());
        let mut fetched = 0usize;

        for track in tracks {
            if let Some(hit) = self.seen.get(&track.uri) {
                out.push(hit.clone());
                continue;
            }

            // Free path: the cloud already told us.
            if let Some(item) = named_from_metadata(track) {
                self.seen.insert(track.uri.clone(), item.clone());
                out.push(item);
                continue;
            }

            // Paid path, and rationed. Past the limit the entry is only counted toward
            // "and N more", never drawn, so a lookup would buy nothing.
            if fetched < RESOLVE_LIMIT {
                fetched += 1;
                if let Some(item) = fetch_track_name(session, &track.uri).await {
                    self.seen.insert(track.uri.clone(), item.clone());
                    out.push(item);
                    continue;
                }
            }

            out.push(fallback_item(track));
        }
        out
    }
}

/// Build an item from the `metadata` map, if it carries a usable title.
fn named_from_metadata(track: &ProvidedTrack) -> Option<castaway_core::QueueItem> {
    let title = track
        .metadata
        .get("title")
        .filter(|t| !t.is_empty())?
        .clone();
    // An artist name if the map carries one under any spelling we have seen; the album is
    // the closest honest second line when it carries only `artist_uri`, and absent both
    // the title stands alone.
    let mut item = castaway_core::QueueItem::new(title);
    if let Some(artist) = ARTIST_KEYS
        .iter()
        .filter_map(|k| track.metadata.get(*k))
        .find(|v| !v.is_empty())
    {
        item = item.with_artist(artist.clone());
    } else if let Some(album) = track.metadata.get("album_title").filter(|a| !a.is_empty()) {
        item = item.with_artist(album.clone());
    }
    Some(item)
}

/// Metadata keys that have carried an artist name.
///
/// Several spellings because this is reverse-engineered surface (Q38) and the set is not
/// contractual — the `queued track metadata` debug line exists to tell us which one the
/// cloud actually sent. Dropping these in favour of the album alone was a regression: a
/// queue that used to name the artist started naming the record instead.
const ARTIST_KEYS: &[&str] = &["artist_name", "artist", "album_artist_name"];

/// Ask Spotify what a track is called. `None` on any failure — a queue row is not worth
/// failing anything over.
async fn fetch_track_name(session: &Session, uri: &str) -> Option<castaway_core::QueueItem> {
    let parsed = librespot_core::SpotifyUri::from_uri(uri).ok()?;
    let item = AudioItem::get_file(session, parsed).await.ok()?;
    let mut queued = castaway_core::QueueItem::new(item.name.clone());
    // Unlike the metadata map, this has real artist names.
    if let UniqueFields::Track { artists, .. } = &item.unique_fields {
        let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
        if !names.is_empty() {
            queued = queued.with_artist(names.join(", "));
        }
    }
    debug!(%uri, title = %item.name, "spotify: resolved a queued track");
    Some(queued)
}

/// Turn one `ProvidedTrack` into something worth putting on a wall.
///
/// The names come from the track's `metadata` map rather than from a metadata lookup per
/// URI: `Track::get` would be one round trip per queued item, and its `artists` are URIs
/// that would each need another. The map is what the cloud already sent us.
///
/// Keys are checked in several spellings because this is reverse-engineered surface and
/// the exact set is not contractual; the URI is the last resort so a queue entry is never
/// blank.
fn fallback_item(track: &ProvidedTrack) -> castaway_core::QueueItem {
    // Only reached for entries past `RESOLVE_LIMIT`, which the card counts but never
    // draws. Still better than an empty row: the id is greppable against the log.
    castaway_core::QueueItem::new(
        track
            .uri
            .rsplit(':')
            .next()
            .map_or_else(|| "Unknown track".to_owned(), |id| format!("spotify:{id}")),
    )
}

/// Pick the cover worth fetching: the widest one offered.
///
/// Chosen by pixel width rather than by librespot's `ImageSize` enum because the panel is
/// 4K and the art square is roughly a third of its height — every size Spotify offers is
/// smaller than the space, so "largest available" is always the right answer and needs no
/// mapping from an enum that could gain a variant.
fn best_cover(item: &AudioItem) -> Option<String> {
    item.covers
        .iter()
        .max_by_key(|c| c.width)
        .map(|c| c.url.clone())
}

/// Fetch cover bytes over librespot's HTTP client.
///
/// Returns `None` on any failure, deliberately: a missing cover is a cosmetic gap the
/// card already draws an empty panel for, and nothing here is worth failing a session or
/// retrying over.
async fn fetch_cover(session: &Session, url: &str) -> Option<castaway_core::Artwork> {
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(url)
        .body(bytes::Bytes::new())
        .ok()?;
    match session.http_client().request_body(request).await {
        Ok(body) if !body.is_empty() => {
            debug!(bytes = body.len(), %url, "spotify: cover art fetched");
            // The declared format is a hint the card does not trust — it sniffs the bytes
            // — but Spotify serves JPEG, so name it honestly.
            Some(castaway_core::Artwork::new(
                castaway_core::ImageFormat::Jpeg,
                body,
            ))
        }
        Ok(_) => {
            debug!(%url, "spotify: cover art was empty");
            None
        }
        Err(e) => {
            debug!(error = %e, %url, "spotify: cover art fetch failed");
            None
        }
    }
}

/// Fill the identifying half of the snapshot from a track.
fn apply_track(snapshot: &mut NowPlaying, item: &AudioItem) {
    snapshot.title = Some(item.name.clone());
    snapshot.duration = Some(Duration::from_millis(u64::from(item.duration_ms)));
    snapshot.position = Some(Duration::ZERO);

    match &item.unique_fields {
        UniqueFields::Track {
            artists,
            album,
            number,
            ..
        } => {
            // Several artists is the common case, not the exception, and a card showing
            // only the first is wrong for most of a playlist.
            let names: Vec<&str> = artists.iter().map(|a| a.name.as_str()).collect();
            snapshot.artist = (!names.is_empty()).then(|| names.join(", "));
            snapshot.album = Some(album.clone());
            // Spotify numbers tracks from 1; a 0 means "not known", not "track zero".
            snapshot.track = (*number > 0).then_some((*number, None));
        }
        UniqueFields::Local {
            artists,
            album,
            number,
            ..
        } => {
            // A local file the user synced. librespot cannot split the artist string
            // safely, so it is passed through exactly as the file's metadata had it.
            snapshot.artist = artists.clone();
            snapshot.album = album.clone();
            snapshot.track = number.filter(|n| *n > 0).map(|n| (n, None));
        }
        UniqueFields::Episode { show_name, .. } => {
            // A podcast has no artist or album; the show is the closest honest mapping.
            snapshot.artist = Some(show_name.clone());
            snapshot.album = None;
            snapshot.track = None;
        }
    }

    // Artwork arrives as a URL, not bytes, and `NowPlaying::artwork` wants the encoded
    // image. Fetching it is a separate step — leave it absent rather than holding the
    // text hostage to a download (OPEN-QUESTIONS Q39).
    snapshot.artwork = None;
}

/// Turn a librespot login failure into something a person standing at the panel can act on.
fn login_reason(error: &librespot_core::Error) -> String {
    let text = error.to_string();
    if text.contains("PremiumAccountRequired") {
        "Spotify Connect needs Premium".to_owned()
    } else if text.contains("BadCredentials") || text.contains("CouldNotValidateCredentials") {
        "pairing expired, try again from the Spotify app".to_owned()
    } else {
        text
    }
}

/// Map a `0.0..=1.0` level onto Spotify's 16-bit volume scale.
fn volume_to_spotify(level: f32) -> u16 {
    let clamped = level.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (f32::from(u16::MAX) * clamped).round() as u16;
    scaled
}

/// The PCM channel type this module publishes, named for the pipeline's benefit.
pub type PcmReceiver = mpsc::Receiver<PcmFrame>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use castaway_core::{ProtocolKind, SourceId};

    fn settings() -> ConnectSettings {
        ConnectSettings {
            device_name: "castaway".into(),
            device_id: "deadbeef".into(),
            initial_volume: 0.5,
        }
    }

    #[tokio::test]
    async fn a_handle_whose_runner_is_gone_reports_it_instead_of_hanging() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handle = ConnectHandle { tx };
        let err = handle
            .paired(PairedUser {
                user_name: "alice".into(),
                blob: vec![1, 2, 3],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, SpotifyError::SessionGone));
    }

    #[tokio::test]
    async fn a_short_blob_is_refused_instead_of_panicking_inside_librespot() {
        // Not hypothetical: `Credentials::with_blob` ends with `0..len - 0x10`, which
        // underflows below one AES block. Our HMAC check proves only that the sender did
        // the Diffie-Hellman — the plaintext length is still theirs to choose, so this is
        // reachable by anyone on the LAN.
        let (tx, _rx) = mpsc::channel(4);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Spotify, "t"), tx);
        for len in [0usize, 1, 15] {
            let err = start(
                &settings(),
                PairedUser {
                    user_name: "alice".into(),
                    blob: vec![0u8; len],
                },
                &sink,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, SpotifyError::Login(_)), "len {len}: {err:?}");
        }
    }

    #[tokio::test]
    async fn a_well_sized_blob_that_is_not_a_credential_still_fails_cleanly() {
        // Past the length guard, into librespot's parser. This is the Q10 shape: our
        // crypto can succeed on bytes librespot then rejects. It must not reach the
        // network, and it must not panic.
        let (tx, _rx) = mpsc::channel(4);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Spotify, "t"), tx);
        let err = start(
            &settings(),
            PairedUser {
                user_name: "alice".into(),
                blob: vec![0u8; 32],
            },
            &sink,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SpotifyError::Login(_)), "got {err:?}");
    }

    fn provided(uri: &str, meta: &[(&str, &str)]) -> ProvidedTrack {
        let mut t = ProvidedTrack::new();
        t.uri = uri.to_owned();
        for (k, v) in meta {
            t.metadata.insert((*k).to_owned(), (*v).to_owned());
        }
        t
    }

    fn cluster_update(state: Option<Vec<ProvidedTrack>>) -> ClusterUpdate {
        use librespot_protocol::connect::Cluster;
        use librespot_protocol::player::PlayerState;

        let mut update = ClusterUpdate::new();
        let mut cluster = Cluster::new();
        if let Some(tracks) = state {
            let mut player = PlayerState::new();
            player.next_tracks = tracks;
            cluster.player_state = Some(player).into();
        }
        update.cluster = Some(cluster).into();
        update
    }

    #[test]
    fn an_update_with_no_player_state_says_nothing_about_the_queue() {
        // The bug this exists to prevent, observed live: a 24-track queue arrived, then
        // 27 seconds later an unrelated cluster update blanked the panel. Volume changes
        // and device-list churn must not be read as "the queue is empty".
        assert!(queue_tracks(&cluster_update(None)).is_none());
    }

    #[test]
    fn a_player_state_with_no_next_tracks_really_is_an_empty_queue() {
        // The other half: when the state *is* present and says nothing is queued, that is
        // authoritative and the card should clear.
        assert_eq!(
            queue_tracks(&cluster_update(Some(Vec::new()))).map(<[_]>::len),
            Some(0)
        );
    }

    /// The free half of `QueueNames::resolve` — everything that needs no session.
    fn named(tracks: &[ProvidedTrack]) -> Vec<castaway_core::QueueItem> {
        tracks
            .iter()
            .map(|t| named_from_metadata(t).unwrap_or_else(|| fallback_item(t)))
            .collect()
    }

    #[test]
    fn queued_tracks_keep_their_order_and_their_names() {
        let update = cluster_update(Some(vec![
            provided(
                "spotify:track:aaa",
                &[("title", "Alkatraz"), ("artist_name", "DEMONDICE")],
            ),
            provided("spotify:track:bbb", &[("title", "Second")]),
        ]));
        let items = named(queue_tracks(&update).unwrap());
        assert_eq!(items[0].title, "Alkatraz");
        assert_eq!(items[0].artist.as_deref(), Some("DEMONDICE"));
        assert_eq!(items[1].title, "Second");
        assert_eq!(items[1].artist, None);
    }

    #[test]
    fn an_artist_name_is_preferred_over_the_album() {
        // The regression this exists to catch: a queue that named the artist started
        // naming the record, because the album became the only second line considered.
        let track = provided(
            "spotify:track:ccc",
            &[
                ("title", "Alkatraz"),
                ("album_title", "Cyber Sex Kitten"),
                ("artist_name", "DEMONDICE"),
            ],
        );
        let item = named_from_metadata(&track).unwrap();
        assert_eq!(item.artist.as_deref(), Some("DEMONDICE"));
    }

    #[test]
    fn the_album_stands_in_when_the_cloud_names_no_artist() {
        let track = provided(
            "spotify:track:ddd",
            &[("title", "Alkatraz"), ("album_title", "Cyber Sex Kitten")],
        );
        let item = named_from_metadata(&track).unwrap();
        assert_eq!(item.artist.as_deref(), Some("Cyber Sex Kitten"));
    }

    #[test]
    fn a_track_whose_metadata_has_no_title_still_renders_as_something() {
        // The keys are reverse-engineered and not contractual (Q38). A blank row would be
        // indistinguishable from a rendering bug; the id at least identifies the track.
        let track = provided("spotify:track:1240iIrz36c", &[]);
        assert!(
            named_from_metadata(&track).is_none(),
            "no title means the metadata map cannot name it"
        );
        assert_eq!(fallback_item(&track).title, "spotify:1240iIrz36c");
    }

    #[test]
    fn a_very_long_queue_is_bounded_before_it_reaches_the_session_bus() {
        let tracks = (0..200)
            .map(|i| provided(&format!("spotify:track:{i}"), &[("title", "x")]))
            .collect();
        let update = cluster_update(Some(tracks));
        assert_eq!(queue_tracks(&update).unwrap().len(), UP_NEXT_LIMIT);
    }

    #[test]
    fn the_initial_volume_covers_the_scale() {
        assert_eq!(volume_to_spotify(0.0), 0);
        assert_eq!(volume_to_spotify(1.0), u16::MAX);
        assert_eq!(volume_to_spotify(2.0), u16::MAX);
    }

    #[test]
    fn a_premium_refusal_is_explained_rather_than_dumped() {
        // The single most likely reason a pairing "just does nothing".
        let reason = login_reason(&librespot_core::Error::permission_denied(
            "Login failed with reason: PremiumAccountRequired",
        ));
        assert!(reason.contains("Premium"), "got {reason}");
        assert!(!reason.contains("permission_denied"), "got {reason}");
    }
}
