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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use castaway_core::{FrameSource, OsdSink};
use castaway_core::{
    NowPlaying, PcmFrame, PlaybackState, RepeatMode, SessionEvent, SessionSink, SourceDescription,
};
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::dealer::protocol::Message;
use librespot_core::{Session, SessionConfig};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::config::{Bitrate, PlayerConfig};
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

/// Everything the panel needs in order to *present* this session, in one place.
///
/// A Connect session can end and come back without the login ending: the phone takes
/// playback back (`device became inactive` → [`SessionEvent::End`]) and then hands it over
/// again, and librespot asks for a fresh PCM channel rather than logging in afresh. That
/// second start is a new session as far as [`castaway_core::SessionManager`] is concerned
/// — `End` drops the source description, the control surface, the card and the queue,
/// because a session that has gone must not leave buttons on screen wired to a peer that
/// left with it.
///
/// So a restart has to say all of it again, and the bug this type exists to prevent is
/// that the restart path only said *some* of it: it emitted `Audio` and nothing else, and
/// the panel got a session with no name, no transport controls and no track — an empty
/// card with a queue eventually pasted into it. Bundling the facts with the emit that
/// republishes them is what makes "start a session and forget half of it" not a thing you
/// can write here (ground rule 1).
struct Presentation {
    /// Who is connected and over what. A `Mutex` because it grows: the login knows the
    /// account name, and the phone names *itself* only later, over a player event.
    description: std::sync::Mutex<SourceDescription>,
    /// The handle the panel drives the phone through. Fixed for the life of the login —
    /// it wraps `Spirc`, which outlives any one session.
    control: Arc<dyn castaway_core::RemoteControl>,
    /// The track as last known, and the queue behind it. `None`/empty before the cloud has
    /// said anything.
    now_playing: std::sync::Mutex<Option<NowPlaying>>,
    up_next: std::sync::Mutex<Vec<castaway_core::QueueItem>>,
}

impl Presentation {
    fn new(description: SourceDescription, control: Arc<dyn castaway_core::RemoteControl>) -> Self {
        Self {
            description: std::sync::Mutex::new(description),
            control,
            now_playing: std::sync::Mutex::new(None),
            up_next: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Read a field's current value, tolerating a poisoned lock: a session that cannot
    /// report its own title is still worth having, and nothing here is unsafe when stale.
    fn read<T: Clone>(lock: &std::sync::Mutex<T>) -> Option<T> {
        lock.lock().ok().map(|held| held.clone())
    }

    /// Merge in what a player event just taught us about the sender, and return the whole
    /// description so the caller can publish it.
    fn merge_description(&self, info: SourceDescription) -> Option<SourceDescription> {
        let mut held = self.description.lock().ok()?;
        *held = std::mem::take(&mut *held).merged(info);
        Some(held.clone())
    }

    /// Record the current track, so a restart can say what is playing without waiting for
    /// the next player event — which, for a track that never stopped, never comes.
    fn set_now_playing(&self, snapshot: &NowPlaying) {
        if let Ok(mut held) = self.now_playing.lock() {
            *held = Some(snapshot.clone());
        }
    }

    /// Record the queue, for the same reason: cluster updates announce *edits*, so a
    /// restart mid-playlist would otherwise show nothing coming up.
    fn set_up_next(&self, items: &[castaway_core::QueueItem]) {
        if let Ok(mut held) = self.up_next.lock() {
            held.clear();
            held.extend_from_slice(items);
        }
    }

    /// Open a session on `sink`: the audio plane, then everything needed to present it.
    ///
    /// Order is load-bearing. `Audio` is what makes this source the active one, and the
    /// session manager rejects metadata, controls and queues for a source that is not —
    /// so everything else has to follow it, not precede it.
    ///
    /// # Errors
    /// [`SpotifyError::SessionGone`] if the session manager has shut down.
    async fn open(
        &self,
        sink: &SessionSink,
        source: FrameSource,
        format: castaway_core::AudioFormat,
    ) -> Result<(), SpotifyError> {
        let emit = |event| async move {
            sink.emit(event)
                .await
                .map_err(|_| SpotifyError::SessionGone)
        };
        emit(SessionEvent::Audio {
            source,
            format,
            // PCM from librespot: nothing to configure a decoder with, because there is
            // no decoder.
            config: None,
        })
        .await?;
        if let Some(description) = Self::read(&self.description) {
            emit(SessionEvent::SourceInfo(description)).await?;
        }
        emit(SessionEvent::ControlSurface(Arc::clone(&self.control))).await?;
        if let Some(snapshot) = Self::read(&self.now_playing).flatten() {
            emit(SessionEvent::NowPlaying(snapshot)).await?;
        }
        let queue = Self::read(&self.up_next).unwrap_or_default();
        if !queue.is_empty() {
            emit(SessionEvent::UpNext(queue)).await?;
        }
        Ok(())
    }
}

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
    /// Stream quality in kbps. Anything other than 96/160/320 falls back to 320 with a
    /// warning — the set is librespot's, not ours, and a typo should not silently halve
    /// the bitrate.
    pub bitrate: u16,
    /// Apply Spotify's loudness normalisation, so a shared room does not get
    /// track-to-track volume jumps.
    pub normalisation: bool,
    /// Directories searched for tracks the user synced from their own files.
    ///
    /// Empty by default, and that default is a real position rather than an oversight: a
    /// receiver holds nobody's music library, so a playlist with local files is one whose
    /// local entries this device genuinely cannot play. What it must not do is *look*
    /// like it can — before this existed the card rendered a local track in full and then
    /// the player emitted `Unavailable` and skipped, which reads as the receiver dropping
    /// songs at random.
    ///
    /// Point it at a share the panel can reach and those tracks play like any other.
    pub local_file_directories: Vec<std::path::PathBuf>,
}

/// Who `getInfo` names as the active user, shared between the zeroconf endpoint and the
/// session runner.
///
/// It has to be shared because the two halves know different things. The endpoint knows a
/// pairing arrived and answers the phone immediately — it cannot wait for an AP handshake.
/// Only the runner knows whether the login behind that pairing actually worked, or whether
/// the session has since ended. Before this, the endpoint set the name and nothing ever
/// cleared it, so a failed login (a non-Premium account, a stale blob) left the device
/// claiming to be logged in as someone forever — and `getInfo` is exactly what a phone
/// reads back to decide whether this device is *theirs*.
#[derive(Debug, Clone, Default)]
pub struct ActiveUser(Arc<tokio::sync::Mutex<String>>);

impl ActiveUser {
    /// The name to report, empty if nobody is logged in.
    pub async fn get(&self) -> String {
        self.0.lock().await.clone()
    }

    /// Claim the device for `who`.
    pub async fn claim(&self, who: &str) {
        who.clone_into(&mut *self.0.lock().await);
    }

    /// Release the device, if `who` is still the one holding it.
    ///
    /// Conditional so a session ending late cannot evict whoever paired after it — the
    /// order those two arrive in is a race we do not control.
    async fn release(&self, who: &str) {
        let mut held = self.0.lock().await;
        if *held == who {
            held.clear();
        }
    }
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
    /// Serves the sink's requests for a fresh PCM channel after a preemption.
    reattach_task: JoinHandle<()>,
    /// Set when the *user* ended the session from their phone, as opposed to the session
    /// dying under us. The difference decides whether we reconnect or stay down, and
    /// nothing else in the session can tell them apart — see [`run`].
    hung_up: Arc<AtomicBool>,
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
    /// Resolves when this session is no longer live.
    ///
    /// `spirc_task` is the one worth watching: `SpircTask::run` loops
    /// `while !self.session.is_invalid() && !self.shutdown` and then simply *returns*, so
    /// its completion is the only signal that the device has left the picker. Nothing
    /// awaited it before, which is why a Wi-Fi blip was permanent.
    async fn ended(&mut self) {
        let _ = (&mut self.spirc_task).await;
    }

    /// Whether the user ended this session deliberately.
    fn was_hung_up(&self) -> bool {
        self.hung_up.load(Ordering::SeqCst)
    }

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
        self.reattach_task.abort();
    }
}

/// Start the runner. Returns the handle the HTTP endpoint pushes credentials into.
///
/// The runner is a task rather than something the caller drives, because pairing arrives
/// on an axum handler that must answer the phone promptly — starting a network login
/// inline would hold the HTTP response open for the length of an AP handshake.
#[must_use]
pub fn spawn(
    settings: ConnectSettings,
    sink: SessionSink,
    osd: Option<OsdSink>,
    active: ActiveUser,
) -> ConnectHandle {
    // Depth 1: pairings are human-paced, and if two arrive at once only the later one
    // matters — but it must not be dropped, so this is a send, not a try_send.
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(run(settings, sink, osd, rx, active));
    ConnectHandle { tx }
}

/// How long to wait before rebuilding a session that died, and the ceiling.
///
/// The common case is a Wi-Fi blip or an AP restart, where the first retry a couple of
/// seconds later just works. The uncommon case is the cloud being unreachable for an
/// afternoon, and there the interval has to grow — a login attempt every two seconds for
/// four hours is a way to get an account rate-limited.
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(120);

/// A session that stayed up this long counts as healthy, so the next failure starts over
/// at [`RECONNECT_MIN`] rather than inheriting a backoff from hours earlier.
const HEALTHY_SESSION: Duration = Duration::from_secs(120);

/// How many rebuilds to attempt before concluding the credentials are the problem.
///
/// A blob is a *reusable* credential, so it normally outlives any outage — but it can be
/// revoked, and an account whose password changed will fail every attempt forever.
/// Retrying to that horizon is worse than stopping and letting the person re-pair, which
/// takes them four seconds.
const RECONNECT_ATTEMPTS: u32 = 8;

/// The runner loop: one session at a time, replaced whenever someone new pairs.
///
/// Also the thing that keeps a session alive. librespot 0.8 does not re-establish the
/// access-point session after a keepalive timeout — `librespot-core/src/session.rs` says
/// so in a `// TODO` — so `SpircTask::run` returns and the device silently leaves every
/// phone's picker. That is the exact shape D30 exists to avoid ("every break lands as
/// silence on an unattended panel"), and delegating to librespot does not cover it,
/// because the reconnect is the part librespot does not do.
///
/// A dropped session and a deliberate hang-up look identical from here — both end
/// `spirc_task` — so `LiveSession::hung_up` carries the distinction back from the event
/// pump, which is the only place the difference is visible.
async fn run(
    settings: ConnectSettings,
    sink: SessionSink,
    osd: Option<OsdSink>,
    mut rx: mpsc::Receiver<PairedUser>,
    active: ActiveUser,
) {
    let mut current: Option<LiveSession> = None;
    /// Credentials worth reconnecting with, and how many times we have tried.
    struct Standing {
        user: PairedUser,
        attempts: u32,
        backoff: Duration,
        /// When the session these credentials belong to last came up.
        up_since: Option<std::time::Instant>,
    }
    let mut standing: Option<Standing> = None;

    loop {
        // Wait for whichever comes first: someone new pairing, or the live session
        // ending. With no session up there is nothing to watch, so this is just a recv.
        let paired = match &mut current {
            Some(live) => tokio::select! {
                user = rx.recv() => user,
                () = live.ended() => {
                    let live = current.take().unwrap_or_else(|| unreachable!("just matched"));
                    let deliberate = live.was_hung_up();
                    live.shutdown();
                    if deliberate {
                        // The user pressed Disconnect. Reconnecting would drag them back
                        // onto a device they just left, which is worse than useless on a
                        // shared panel — the next person's phone would find it occupied.
                        info!("spotify: the user disconnected");
                        if let Some(pending) = standing.take() {
                            active.release(&pending.user.user_name).await;
                        }
                    } else if let Some(pending) = standing.as_mut() {
                        // A session that stayed up for a good while was healthy; whatever
                        // just killed it is a new problem, not a continuation of an old
                        // one, so it gets a fresh budget.
                        if pending
                            .up_since
                            .is_some_and(|t| t.elapsed() >= HEALTHY_SESSION)
                        {
                            pending.attempts = 0;
                            pending.backoff = RECONNECT_MIN;
                        }
                        pending.up_since = None;
                        pending.attempts += 1;
                        if pending.attempts > RECONNECT_ATTEMPTS {
                            warn!(
                                attempts = pending.attempts,
                                "spotify: giving up on reconnecting; the credentials \
                                 are probably stale, so re-pair from the app"
                            );
                            if let Some(osd) = &osd {
                                osd.banner(
                                    "Spotify: disconnected — pair again to resume".to_owned(),
                                    Duration::from_secs(8),
                                );
                            }
                            if let Some(pending) = standing.take() {
                                active.release(&pending.user.user_name).await;
                            }
                            let _ = sink.emit(SessionEvent::End).await;
                        } else {
                            warn!(
                                attempt = pending.attempts,
                                retry_in = ?pending.backoff,
                                "spotify: session ended, reconnecting"
                            );
                        }
                    }
                    continue;
                }
            },
            None => match standing.as_ref() {
                // A reconnect is owed. Wait out the backoff, but let a fresh pairing
                // interrupt it — someone standing at the panel beats a retry timer.
                Some(pending) => {
                    let wait = pending.backoff;
                    tokio::select! {
                        user = rx.recv() => user,
                        () = tokio::time::sleep(wait) => {
                            let Some(pending) = standing.as_mut() else { continue };
                            pending.backoff = (pending.backoff * 2).min(RECONNECT_MAX);
                            Some(pending.user.clone())
                        }
                    }
                }
                None => rx.recv().await,
            },
        };

        let Some(user) = paired else { break };

        // A pairing that arrives while a reconnect is owed replaces it, whoever it is —
        // the person at the panel is the authority on whose music should play.
        let resuming = standing
            .as_ref()
            .is_some_and(|p| p.user.user_name == user.user_name && p.attempts > 0);

        // Retire the old session *before* starting the new one. Two Connect sessions on
        // one device id fight over the same registration, and the account that loses is
        // whichever the cloud saw last — which is not necessarily the person standing in
        // front of the panel.
        if let Some(previous) = current.take() {
            info!("spotify: replacing the active session");
            previous.shutdown();
        }

        let user_name = user.user_name.clone();
        let retained = user.clone();
        match start(&settings, user, &sink).await {
            Ok(live) => {
                info!(user = %user_name, resuming, "spotify: connect session up");
                // Only greet a genuinely new arrival. A banner on every reconnect would
                // turn a flaky uplink into a strobe light on the wall.
                if !resuming {
                    if let Some(osd) = &osd {
                        osd.banner(
                            format!("Spotify: {user_name} connected"),
                            Duration::from_secs(4),
                        );
                    }
                }
                current = Some(live);
                // The attempt counter is *not* reset here. A login that succeeds and then
                // dies again in ten seconds is a failing session, not a working one, and
                // resetting on connect would let it loop forever at the minimum interval.
                // `HEALTHY_SESSION` is what clears it — see the session-ended arm.
                let attempts = standing.as_ref().map_or(0, |p| p.attempts);
                let backoff = standing
                    .as_ref()
                    .map_or(RECONNECT_MIN, |p| p.backoff.min(RECONNECT_MAX));
                standing = Some(Standing {
                    user: retained,
                    attempts,
                    backoff,
                    up_since: Some(std::time::Instant::now()),
                });
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
                // A failed *login* is not a failed session: retry it on the same budget,
                // because "the AP refused us while the uplink was down" and "this account
                // cannot log in" are indistinguishable from one attempt.
                match standing.as_mut() {
                    Some(pending) if pending.user.user_name == user_name => {
                        pending.attempts += 1;
                        if pending.attempts > RECONNECT_ATTEMPTS {
                            let attempts = pending.attempts;
                            standing = None;
                            warn!(
                                attempts,
                                "spotify: giving up on logging in; the credentials \
                                 are probably stale, so re-pair from the app"
                            );
                            // Stop claiming to be logged in as someone we cannot log in
                            // as. The endpoint set the name optimistically when the
                            // pairing arrived, because it had to answer the phone before
                            // the login was attempted; this is where that optimism ends.
                            active.release(&user_name).await;
                            if let Some(osd) = &osd {
                                osd.banner(
                                    "Spotify: disconnected — pair again to resume".to_owned(),
                                    Duration::from_secs(8),
                                );
                            }
                            // The same ending as the session-ended give-up above, and the
                            // *common* one: a live session that dies non-deliberately
                            // sets attempts = 1 there, so every attempt after it fails
                            // inside `start()` and lands here instead. Without this the
                            // panel keeps a stale now-playing card and transport controls
                            // wired to an aborted Spirc, indefinitely — nothing else
                            // tears a Spotify session down, because the PCM path passes
                            // no failure callback and the manager's media-ended synthesis
                            // never fires for it. Harmless if the source is no longer
                            // active: the manager no-ops.
                            let _ = sink.emit(SessionEvent::End).await;
                        }
                    }
                    _ => {
                        standing = Some(Standing {
                            user: retained,
                            attempts: 1,
                            backoff: RECONNECT_MIN,
                            up_since: None,
                        });
                    }
                }
            }
        }
    }

    if let Some(previous) = current.take() {
        previous.shutdown();
    }
}

/// Turn a decrypted pairing into librespot credentials.
///
/// The decrypted zeroconf plaintext is not the credential bytes — it is the *base64
/// text* of a second, AES-ECB-encrypted layer keyed on the device id and username, and
/// [`Credentials::with_blob`] wants exactly that text. This function used to re-encode
/// it, adding a third layer: `with_blob` stripped one, "decrypted" ASCII base64 as if
/// it were ciphertext, and parsed the noise — surfacing on the panel as
/// `unknown authentication type 52`, a number sampled from garbage. Every pairing
/// failed that way; the phone showed the device and nothing ever logged in.
fn credentials_from_pairing(
    user: &PairedUser,
    device_id: &str,
) -> Result<Credentials, SpotifyError> {
    // Guard the length before handing the blob over. `with_blob` runs `0..len - 0x10`
    // over the *decoded* bytes, which underflows and panics below one AES block — and
    // the length is attacker-chosen: our own HMAC check only proves the sender
    // completed the Diffie-Hellman, not that the plaintext is well-formed. A hostile or
    // buggy sender on the LAN should get an error, not a panicking task. 24 characters
    // is the shortest padded base64 of 16 bytes; anything shorter cannot decode to a
    // full block, and anything malformed fails inside `with_blob` cleanly.
    const MIN_BLOB_B64: usize = 24;
    if user.blob.len() < MIN_BLOB_B64 {
        return Err(SpotifyError::Login(format!(
            "credential blob is {} bytes of base64, need at least {MIN_BLOB_B64}",
            user.blob.len()
        )));
    }

    Credentials::with_blob(&user.user_name, &user.blob, device_id)
        .map_err(|e| SpotifyError::Login(format!("credentials rejected: {e}")))
}

/// Bring up one session: log in, register as a Connect device, publish the audio path.
async fn start(
    settings: &ConnectSettings,
    user: PairedUser,
    sink: &SessionSink,
) -> Result<LiveSession, SpotifyError> {
    let credentials = credentials_from_pairing(&user, &settings.device_id)?;

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

    let (pcm_link, pcm_requests) = crate::sink::PcmLink::new();
    let pcm_rx = pcm_link.attach();
    let sink_link = Arc::clone(&pcm_link);
    // Both of these are *away* from librespot's defaults, deliberately. `Bitrate160` and
    // `normalisation: false` are what the struct default gives, and neither is what a
    // room wants: 160 on an account entitled to 320 is audible on a PA, and unnormalised
    // playback is the thing that has people reaching for the volume between tracks.
    let bitrate = match settings.bitrate {
        96 => Bitrate::Bitrate96,
        160 => Bitrate::Bitrate160,
        320 => Bitrate::Bitrate320,
        other => {
            warn!(other, "spotify: unknown bitrate, using 320");
            Bitrate::Bitrate320
        }
    };
    let player = Player::new(
        PlayerConfig {
            position_update_interval: Some(POSITION_INTERVAL),
            bitrate,
            normalisation: settings.normalisation,
            local_file_directories: settings.local_file_directories.clone(),
            ..PlayerConfig::default()
        },
        session.clone(),
        mixer.get_soft_volume(),
        move || Box::new(PcmSink::new(sink_link)),
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

    // One description of this session, published by whoever (re)opens it. Every task that
    // learns something about it updates this rather than only emitting it, so a restart
    // republishes what the room was already looking at.
    let presentation = Arc::new(Presentation::new(
        SourceDescription::new()
            .with_display_name(user.user_name.clone())
            .with_link(format!("Spotify Connect · {sample_rate} Hz · stereo")),
        Arc::new(SpotifyRemote::new(Arc::clone(&spirc))),
    ));

    presentation
        .open(sink, FrameSource::Pcm(pcm_rx), format)
        .await?;

    // Serve the sink's requests for a fresh channel. This is what makes preemption
    // survivable: the pipeline takes our audio away, and when someone presses play again
    // librespot's `start()` asks here, we reopen the session, and the session manager hands
    // Spotify the panel back — the same path as starting playback, because from the room's
    // point of view that is what just happened.
    let reattach_task = tokio::spawn(serve_reattach(
        pcm_requests,
        Arc::clone(&pcm_link),
        sink.clone(),
        format,
        Arc::clone(&presentation),
    ));

    let hung_up = Arc::new(AtomicBool::new(false));
    let events_task = tokio::spawn(pump_events(
        events,
        sink.clone(),
        session.clone(),
        Arc::clone(&hung_up),
        Arc::clone(&presentation),
    ));
    let queue_task = tokio::spawn(pump_queue(
        cluster_updates,
        sink.clone(),
        session.clone(),
        Arc::clone(&presentation),
    ));
    let spirc_task = tokio::spawn(spirc_task);

    Ok(LiveSession {
        spirc,
        spirc_task,
        events_task,
        queue_task,
        reattach_task,
        hung_up,
    })
}

/// Hand the pipeline a new PCM channel whenever the sink asks for one.
///
/// One request per preemption, not per block: the sink coalesces, and the channel it gets
/// back lasts until something takes the panel again.
///
/// Reopening is deliberately the *same* act a session start is — [`Presentation::open`],
/// not a bare `Audio` event. The session manager's arbitration is last-writer-wins, so
/// this preempts whoever holds the panel, which is the correct reading of "the person whose
/// phone this is just pressed play"; and because `End` wiped the description, the controls
/// and the card, saying only `Audio` here left the panel showing a nameless session with no
/// buttons and no track.
async fn serve_reattach(
    mut requests: mpsc::UnboundedReceiver<()>,
    link: Arc<crate::sink::PcmLink>,
    sink: SessionSink,
    format: castaway_core::AudioFormat,
    presentation: Arc<Presentation>,
) {
    while requests.recv().await.is_some() {
        // Coalesce anything that piled up while we were not looking, so a burst of failed
        // writes does not produce a burst of session hand-offs.
        while requests.try_recv().is_ok() {}
        info!("spotify: reattaching audio to the pipeline");
        let source = FrameSource::Pcm(link.attach());
        if presentation.open(&sink, source, format).await.is_err() {
            debug!("spotify: session manager gone; stopping the reattach server");
            return;
        }
    }
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
    hung_up: Arc<AtomicBool>,
    presentation: Arc<Presentation>,
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
                presentation.set_now_playing(&snapshot);
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
            PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. } => {
                // A jump someone caused. Worth publishing even though the card does not
                // draw a scrubber, because it is a discrete event rather than a tick.
                snapshot.position = Some(Duration::from_millis(u64::from(position_ms)));
                true
            }
            PlayerEvent::PositionChanged { position_ms, .. } => {
                // Republished: the transport strip (D33) draws a scrubber whose UI-side
                // clock re-anchors on every published position, so this once-a-second
                // reading is what keeps the strip honest against the real player —
                // without it the only position a track ever published was its opening 0,
                // and the clock free-ran on the OS clock from there (and carried its
                // anchor across a track change, since 0 equals 0).
                //
                // The old objection — a 33 MB card raster per second for a number the
                // card does not draw — is answered on the render side now: a card that
                // changed in nothing but position skips the raster (`visual_eq`) and
                // only the small strip repaints.
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
            PlayerEvent::SessionDisconnected { user_name, .. } => {
                // The user pressed Disconnect, or moved playback to their headphones.
                // This used to fall into the wildcard below, so the session manager was
                // never told: `active` stayed Spotify, the pipeline was never stopped,
                // the card never cleared, and the PCM thread kept the audio device — the
                // panel sat on a stale card until someone else cast over it.
                info!(user = %user_name, "spotify: the user ended the session");
                hung_up.store(true, Ordering::SeqCst);
                let _ = sink.emit(SessionEvent::End).await;
                return;
            }
            PlayerEvent::SessionClientChanged {
                client_name,
                client_brand_name,
                client_model_name,
                ..
            } => {
                // The only place the *phone* names itself. Much better than what the card
                // otherwise shows: `user_name` is a canonical Spotify id, which for any
                // account made since about 2015 is 25 random characters — rendered at
                // 28px on a 65-inch screen.
                let who = [client_name, client_brand_name, client_model_name]
                    .into_iter()
                    .find(|s| !s.is_empty());
                if let Some(who) = who {
                    debug!(%who, "spotify: controlling client");
                    // Merged into the session's own description as well as emitted, so a
                    // reopen names the phone rather than falling back to the account id.
                    let merged = presentation
                        .merge_description(SourceDescription::new().with_display_name(who));
                    if let Some(description) = merged {
                        if sink
                            .emit(SessionEvent::SourceInfo(description))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                false
            }
            PlayerEvent::ShuffleChanged { shuffle } => {
                snapshot.shuffle = Some(shuffle);
                true
            }
            PlayerEvent::RepeatChanged { context, track } => {
                // Spotify keeps two independent flags; `RepeatMode` is one answer. Track
                // wins when both are set, because that is what the listener will actually
                // hear — the current song, forever.
                snapshot.repeat = Some(if track {
                    RepeatMode::Track
                } else if context {
                    RepeatMode::Context
                } else {
                    RepeatMode::Off
                });
                true
            }
            // Everything else is either internal to spirc or does not change the card.
            _ => false,
        };

        // Recorded even when it is not republished — `SessionClientChanged` and
        // everything under the wildcard return `false` — because a session that reopens
        // should come back with the state it had. `PositionChanged` used to be the
        // example here and is not one any more: it returns `true` now, and the scrubber
        // re-anchoring documented on that arm depends on it.
        presentation.set_now_playing(&snapshot);
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
/// carries only the current item (#49). Subscribing a second listener to
/// the same dealer URI is supported — the dealer keeps a vector of subscribers per URI —
/// so this rides alongside spirc's own listener rather than replacing it.
///
/// Must be subscribed *before* `Spirc::new` connects, or the first cluster update (the
/// one that arrives with the transfer that started playback) is missed.
async fn pump_queue(
    mut updates: impl futures::Stream<Item = Result<ClusterUpdate, librespot_core::Error>> + Unpin,
    sink: SessionSink,
    session: Session,
    presentation: Arc<Presentation>,
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
        let items = names.resolve(&session, &tracks).await;
        // Cluster updates arrive for volume changes, device lists and playback position
        // — most of them leave the queue untouched, and re-rendering the card for each
        // would be a needless repaint on a screen people are looking at.
        if items == last {
            continue;
        }
        debug!(queued = items.len(), "spotify: queue changed");
        last.clone_from(&items);
        presentation.set_up_next(&items);
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
fn queue_tracks(update: &ClusterUpdate) -> Option<Vec<&ProvidedTrack>> {
    let state = update.cluster.as_ref()?.player_state.as_ref()?;

    // One line, once per change, naming the keys the cloud actually sent. The metadata
    // map is reverse-engineered surface (#49) and this is what tells us
    // whether `title`/`artist_name` are the right guesses without another session.
    if let Some(first) = state.next_tracks.first() {
        debug!(
            keys = ?first.metadata.keys().collect::<Vec<_>>(),
            uri = %first.uri,
            "spotify: queued track metadata"
        );
    }

    Some(
        state
            .next_tracks
            .iter()
            .filter(|t| !is_bookkeeping(t))
            .take(UP_NEXT_LIMIT)
            .collect(),
    )
}

/// Whether a queue entry is librespot's bookkeeping rather than a track.
///
/// A repeating context gets a hidden `spotify:delimiter` entry inserted where the list
/// wraps around (librespot `state/tracks.rs`). It is not a song, no lookup will ever
/// name it, and rendering it printed `spotify:delimiter` on the wall as though it were
/// one — reliably, after skipping around a playlist on repeat.
fn is_bookkeeping(track: &ProvidedTrack) -> bool {
    track.uri == "spotify:delimiter"
        || track
            .metadata
            .get("hidden")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
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
    /// What we know a URI is called, or how often asking has failed.
    seen: std::collections::HashMap<String, Known>,
    /// Insertion order, so the oldest entry can be dropped when the map is full.
    order: std::collections::VecDeque<String>,
}

/// What a lookup taught us about a URI.
#[derive(Debug, Clone)]
enum Known {
    /// It resolved; this is its name for the rest of the session.
    Named(castaway_core::QueueItem),
    /// It failed, this many times so far.
    ///
    /// A count rather than a verdict, because the two things a failure can be need
    /// opposite treatment: a *transient* one — a 5xx or timeout during exactly the
    /// skip-burst that makes the queue churn — must be retried, or a raw id sticks on
    /// the wall for the rest of the evening; a *permanent* one (region-locked, a local
    /// file) must eventually go quiet, or it spends the whole lookup budget on every
    /// volume change forever. The count is the line between them.
    Failed(u8),
}

/// Failures after which a URI stops being retried.
const FAILED_TRIES: u8 = 3;

/// How many resolved names to keep.
///
/// Unbounded before, and the map is keyed by whatever a room queues over an evening — so
/// it grew for as long as the session lasted. Generous enough that nothing is re-fetched
/// in practice; finite because a cache keyed on other people's choices should be.
const RESOLVE_CACHE: usize = 512;

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
        tracks: &[&ProvidedTrack],
    ) -> Vec<castaway_core::QueueItem> {
        let mut out = Vec::with_capacity(tracks.len());
        let mut fetched = 0usize;

        for &track in tracks {
            // A name is a name for the rest of the session; a failure is only final
            // once it has used up its retries (see [`Known::Failed`]).
            match self.seen.get(&track.uri) {
                Some(Known::Named(item)) => {
                    out.push(item.clone());
                    continue;
                }
                Some(Known::Failed(n)) if *n >= FAILED_TRIES => {
                    out.push(fallback_item(track));
                    continue;
                }
                Some(Known::Failed(_)) | None => {}
            }

            // Free path: the cloud already told us.
            if let Some(item) = named_from_metadata(track) {
                self.remember(&track.uri, Known::Named(item.clone()));
                out.push(item);
                continue;
            }

            // Paid path, and rationed. Past the limit the entry is only counted toward
            // "and N more", never drawn, so a lookup would buy nothing — and is not
            // recorded as a failure either, since it was never attempted.
            if fetched < RESOLVE_LIMIT {
                fetched += 1;
                match fetch_track_name(session, &track.uri).await {
                    Some(item) => {
                        self.remember(&track.uri, Known::Named(item.clone()));
                        out.push(item);
                        continue;
                    }
                    None => self.note_failure(&track.uri),
                }
            }

            out.push(fallback_item(track));
        }
        out
    }

    /// Record what a URI taught us, evicting the oldest entry when full.
    fn remember(&mut self, uri: &str, known: Known) {
        if self.seen.insert(uri.to_owned(), known).is_none() {
            self.order.push_back(uri.to_owned());
            while self.order.len() > RESOLVE_CACHE {
                if let Some(oldest) = self.order.pop_front() {
                    self.seen.remove(&oldest);
                }
            }
        }
    }

    /// One more failed lookup for `uri`.
    fn note_failure(&mut self, uri: &str) {
        match self.seen.get_mut(uri) {
            Some(Known::Failed(n)) => *n = n.saturating_add(1),
            Some(Known::Named(_)) => {}
            None => self.remember(uri, Known::Failed(1)),
        }
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
/// Several spellings because this is reverse-engineered surface (#49) and the set is not
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

/// The last resort for one `ProvidedTrack`: its id, formatted, so a queue row is never
/// blank.
///
/// The metadata-map reading this doc used to describe moved to [`named_from_metadata`]
/// when the two were split. What is left is the URI tail.
///
/// It is reached at drawn positions, not only past `RESOLVE_LIMIT`: [`QueueNames::resolve`]
/// falls back here for any entry whose lookup has failed `FAILED_TRIES` times, and for a
/// within-limit fetch that has just failed. So a raw `spotify:<id>` row really does go on
/// the wall — which [`Known::Failed`] says outright — and that is the intended outcome,
/// since the id is at least greppable against the log where an empty row is nothing.
fn fallback_item(track: &ProvidedTrack) -> castaway_core::QueueItem {
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
    // text hostage to a download (#50).
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

    /// A control surface that does nothing. `SpotifyRemote` wraps librespot's `Spirc`,
    /// which cannot be built without logging in; what these tests are about is *which
    /// events a session opening emits*, and that is indifferent to who answers them.
    #[derive(Debug)]
    struct StubRemote;

    #[async_trait::async_trait]
    impl castaway_core::RemoteControl for StubRemote {
        fn capabilities(&self) -> castaway_core::ControlCapabilities {
            castaway_core::ControlCapabilities::NONE
        }
        async fn issue_unchecked(
            &self,
            _txn: castaway_core::ControlTxn,
        ) -> Result<(), castaway_core::CoreError> {
            Ok(())
        }
    }

    fn presentation() -> Presentation {
        Presentation::new(
            SourceDescription::new().with_display_name("schlarpc"),
            Arc::new(StubRemote),
        )
    }

    fn pcm() -> FrameSource {
        let (_tx, rx) = std::sync::mpsc::channel();
        FrameSource::Pcm(rx)
    }

    fn format() -> castaway_core::AudioFormat {
        castaway_core::AudioFormat::from_hz(44100, 2).unwrap()
    }

    /// Which events, in order, an opening produced.
    async fn opened(presentation: &Presentation) -> Vec<SessionEvent> {
        let (tx, mut rx) = mpsc::channel(16);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Spotify, "http"), tx);
        presentation.open(&sink, pcm(), format()).await.unwrap();
        drop(sink);
        let mut events = Vec::new();
        while let Some(msg) = rx.recv().await {
            events.push(msg.event);
        }
        events
    }

    #[tokio::test]
    async fn a_session_opens_with_its_audio_first_and_its_identity_behind_it() {
        // Order is load-bearing: `Audio` is what makes this source active, and the session
        // manager rejects metadata and controls for a source that is not.
        let events = opened(&presentation()).await;
        assert!(
            matches!(events.first(), Some(SessionEvent::Audio { .. })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::SourceInfo(_))),
            "a session should say who it is: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::ControlSurface(_))),
            "…and hand over its controls: {events:?}"
        );
    }

    #[tokio::test]
    async fn reopening_republishes_the_track_and_the_queue_that_never_stopped() {
        // The reported bug. The phone takes playback back (`End` — which drops the
        // description, the controls, the card and the queue) and then hands it over again;
        // librespot asks for a fresh PCM channel but reports no new track, because the
        // track never changed. The reopening has to say all of it again, or the panel shows
        // a nameless session with an "up next" list pasted into an otherwise empty card —
        // which is exactly what it showed.
        let presentation = presentation();
        let mut track = NowPlaying::new(PlaybackState::Playing);
        track.title = Some("DONMAI".to_owned());
        presentation.set_now_playing(&track);
        presentation.set_up_next(&[castaway_core::QueueItem {
            title: "PSYCHO".to_owned(),
            artist: Some("Hakos Baelz".to_owned()),
        }]);

        let events = opened(&presentation).await;

        let titles: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::NowPlaying(n) => n.title.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(titles, vec!["DONMAI".to_owned()], "{events:?}");
        let queued: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::UpNext(items) => Some(items.len()),
                _ => None,
            })
            .collect();
        assert_eq!(queued, vec![1], "{events:?}");
    }

    #[tokio::test]
    async fn a_reopening_names_the_phone_rather_than_the_account_id() {
        // `SessionClientChanged` is the only place the phone names itself, and it arrives
        // once, mid-session. Merging it into the session's own description is what makes a
        // reopen say "iPhone" instead of falling back to a 25-character Spotify id.
        let presentation = presentation();
        presentation.merge_description(SourceDescription::new().with_display_name("iPhone"));
        let events = opened(&presentation).await;
        let named = events.iter().any(|e| match e {
            SessionEvent::SourceInfo(info) => info.to_string().contains("iPhone"),
            _ => false,
        });
        assert!(named, "{events:?}");
    }

    /// Build the inner credential blob exactly as the phone does: the librespot
    /// `Credentials::with_blob` decode, run in reverse. Payload framing, then the
    /// rolling XOR, then AES-192-ECB under the pbkdf2(sha1(device_id), username) key,
    /// then base64 — whose *text bytes* are what the zeroconf plaintext carries.
    fn phone_inner_blob(username: &str, device_id: &str, auth_data: &[u8]) -> Vec<u8> {
        use aes::cipher::generic_array::GenericArray;
        use aes::cipher::{BlockEncrypt as _, BlockSizeUser as _, KeyInit as _};
        use base64::Engine as _;
        use sha1::{Digest as _, Sha1};

        assert!(auth_data.len() < 0x80, "test framing writes 1-byte varints");
        // read_u8; read_bytes (ignored); read_u8; auth_type varint; read_u8; auth_data.
        let mut payload = vec![0x00, 0x00, 0x00];
        payload.push(0x01); // AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS
        payload.push(0x00);
        payload.push(u8::try_from(auth_data.len()).expect("test credential fits a byte"));
        payload.extend_from_slice(auth_data);
        // Pad to whole AES blocks; the parser stops after auth_data and never looks.
        while payload.len() % 16 != 0 {
            payload.push(0x00);
        }

        // The decode XORs data[j] ^= data[j - 0x10] walking backwards, reading the
        // still-encrypted lower block; encode is therefore the forward walk over the
        // ciphertext being built.
        for j in 0x10..payload.len() {
            let prev = payload[j - 0x10];
            payload[j] ^= prev;
        }

        let secret = Sha1::digest(device_id.as_bytes());
        let mut key = [0u8; 24];
        pbkdf2::pbkdf2_hmac::<Sha1>(&secret, username.as_bytes(), 0x100, &mut key[0..20]);
        let hash = Sha1::digest(&key[..20]);
        key[..20].copy_from_slice(&hash);
        key[20..].copy_from_slice(&20u32.to_be_bytes());

        let cipher = aes::Aes192::new(GenericArray::from_slice(&key));
        for chunk in payload.chunks_exact_mut(aes::Aes192::block_size()) {
            cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
        }

        base64::engine::general_purpose::STANDARD
            .encode(&payload)
            .into_bytes()
    }

    #[test]
    fn a_phone_shaped_pairing_becomes_credentials_without_extra_encoding() {
        // The regression that broke every pairing: the zeroconf plaintext already *is*
        // the base64 text `with_blob` wants, and re-encoding it made librespot decrypt
        // ASCII as ciphertext — parsed as a nonsense auth type ("unknown authentication
        // type 52" on the panel, the number sampled from noise).
        let auth_data = b"stored-credential-bytes";
        let user = PairedUser {
            user_name: "alice".into(),
            blob: phone_inner_blob("alice", "deadbeef", auth_data),
        };
        let creds = credentials_from_pairing(&user, "deadbeef").expect("pairing should decode");
        assert_eq!(creds.username.as_deref(), Some("alice"));
        assert_eq!(creds.auth_data, auth_data);
    }

    #[test]
    fn a_runt_blob_is_an_error_not_a_panic() {
        // `with_blob` underflows on anything decoding below one AES block, and the
        // length is attacker-chosen — the HMAC only proves the sender finished the DH.
        let user = PairedUser {
            user_name: "alice".into(),
            blob: b"AAAA".to_vec(),
        };
        assert!(matches!(
            credentials_from_pairing(&user, "deadbeef"),
            Err(SpotifyError::Login(_))
        ));
    }

    fn settings() -> ConnectSettings {
        ConnectSettings {
            device_name: "castaway".into(),
            device_id: "deadbeef".into(),
            initial_volume: 0.5,
            bitrate: 320,
            normalisation: true,
            local_file_directories: Vec::new(),
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
        // Past the length guard, into librespot's parser. This is the #48 shape: our
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
            queue_tracks(&cluster_update(Some(Vec::new()))).map(|t| t.len()),
            Some(0)
        );
    }

    /// The free half of `QueueNames::resolve` — everything that needs no session.
    fn named(tracks: &[&ProvidedTrack]) -> Vec<castaway_core::QueueItem> {
        tracks
            .iter()
            .map(|&t| named_from_metadata(t).unwrap_or_else(|| fallback_item(t)))
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
        let items = named(&queue_tracks(&update).unwrap());
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
        // The keys are reverse-engineered and not contractual (#49). A blank row would be
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
    fn a_failed_lookup_is_retried_a_few_times_then_goes_quiet() {
        // Both halves matter. A failure remembered *forever* stuck a raw id on the wall
        // for the rest of the evening when the failure was a 5xx during exactly the
        // skip-burst that churns the queue. A failure never remembered spent six awaited
        // round trips per cluster update — and those arrive on volume changes — for a
        // name that was never going to come. The count is the line between them.
        let mut names = QueueNames::default();
        for tries in 1..=FAILED_TRIES {
            names.note_failure("spotify:track:gone");
            let Some(Known::Failed(n)) = names.seen.get("spotify:track:gone") else {
                panic!("a failure must be recorded as a failure");
            };
            assert_eq!(*n, tries);
        }
        // Under the limit a failure reads as "worth another try"…
        assert!(matches!(
            names.seen.get("spotify:track:gone"),
            Some(Known::Failed(n)) if *n >= FAILED_TRIES
        ));
        // …and a later success replaces nothing (a name, once known, is final).
        names.remember(
            "spotify:track:ok",
            Known::Named(castaway_core::QueueItem::new("A Name".to_owned())),
        );
        names.note_failure("spotify:track:ok");
        assert!(matches!(
            names.seen.get("spotify:track:ok"),
            Some(Known::Named(_))
        ));
    }

    #[test]
    fn bookkeeping_entries_never_reach_the_card() {
        // librespot inserts a hidden `spotify:delimiter` where a repeating context wraps
        // around. It is not a song, and it rendered as one — literally the string
        // "spotify:delimiter" in Up next — after skipping around a playlist on repeat.
        let update = cluster_update(Some(vec![
            provided("spotify:delimiter", &[("hidden", "true")]),
            provided("spotify:track:aaa", &[("title", "Alkatraz")]),
            provided("spotify:track:hid", &[("title", "x"), ("hidden", "true")]),
        ]));
        let tracks = queue_tracks(&update).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].uri, "spotify:track:aaa");
    }

    #[test]
    fn the_name_cache_does_not_grow_for_the_life_of_the_session() {
        // Keyed by whatever a room queues over an evening, and unbounded — so it grew for
        // as long as the panel stayed up.
        let mut names = QueueNames::default();
        for i in 0..(RESOLVE_CACHE + 50) {
            names.remember(&format!("spotify:track:{i}"), Known::Failed(FAILED_TRIES));
        }
        assert_eq!(names.seen.len(), RESOLVE_CACHE);
        assert_eq!(names.order.len(), RESOLVE_CACHE);
        assert!(
            !names.seen.contains_key("spotify:track:0"),
            "the oldest entry should have been evicted first"
        );
        assert!(
            names
                .seen
                .contains_key(&format!("spotify:track:{}", RESOLVE_CACHE + 49)),
            "and the newest kept"
        );
    }

    #[test]
    fn remembering_a_uri_twice_does_not_grow_the_eviction_queue() {
        // A re-resolved URI must not push a second entry, or the queue outgrows the map
        // and eviction starts dropping live entries.
        let mut names = QueueNames::default();
        names.remember("spotify:track:a", Known::Failed(FAILED_TRIES));
        names.remember("spotify:track:a", Known::Failed(FAILED_TRIES));
        assert_eq!(names.order.len(), 1);
        assert_eq!(names.seen.len(), 1);
    }

    #[tokio::test]
    async fn releasing_the_device_does_not_evict_whoever_paired_next() {
        // The order a dying session and a fresh pairing arrive in is a race we do not
        // control. An unconditional release would let alice's session, ending late, hand
        // bob's phone a device that reports nobody is on it — while bob is playing.
        let active = ActiveUser::default();
        active.claim("alice").await;
        active.claim("bob").await;
        active.release("alice").await;
        assert_eq!(active.get().await, "bob");
        active.release("bob").await;
        assert_eq!(active.get().await, "");
    }

    #[test]
    fn the_initial_volume_covers_the_scale() {
        assert_eq!(volume_to_spotify(0.0), 0);
        assert_eq!(volume_to_spotify(1.0), u16::MAX);
        assert_eq!(volume_to_spotify(2.0), u16::MAX);
    }

    /// An `AudioItem` the way the metadata layer hands one over, minus the parts the
    /// card does not read.
    ///
    /// The issue's own point, and it holds: this needs no `Session`. Every field is
    /// public and the two that are not trivially constructible — `track_id` and
    /// `availability` — take a parsed URI and a `Result<(), _>` (#199).
    fn audio_item(name: &str, duration_ms: u32, unique_fields: UniqueFields) -> AudioItem {
        AudioItem {
            track_id: librespot_core::SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC")
                .expect("a real Spotify track URI"),
            uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".to_owned(),
            files: librespot_metadata::audio::AudioFiles::default(),
            name: name.to_owned(),
            covers: Vec::new(),
            language: Vec::new(),
            duration_ms,
            is_explicit: false,
            availability: Ok(()),
            alternatives: None,
            unique_fields,
        }
    }

    fn artist(name: &str) -> librespot_metadata::artist::ArtistWithRole {
        librespot_metadata::artist::ArtistWithRole {
            id: librespot_core::SpotifyUri::from_uri("spotify:artist:4tZwfgrHOc3mvqYlEYSvVi")
                .expect("a real Spotify artist URI"),
            name: name.to_owned(),
            role: librespot_metadata::artist::ArtistRole::ARTIST_ROLE_MAIN_ARTIST,
        }
    }

    /// The three shapes a Spotify item comes in, folded onto the card.
    ///
    /// `apply_track` had no test. It is the whole of what a person reads while music is
    /// playing, and every branch of it encodes a decision that is wrong in a specific,
    /// silent way if reversed (#199).
    #[test]
    fn every_kind_of_item_folds_onto_the_card_the_way_it_should_read() {
        // A track. Several artists is the common case rather than the exception, and a
        // card showing only the first is wrong for most of a playlist.
        let mut card = NowPlaying::default();
        apply_track(
            &mut card,
            &audio_item(
                "Windowlicker",
                366_000,
                UniqueFields::Track {
                    artists: librespot_metadata::artist::ArtistsWithRole(vec![
                        artist("Aphex Twin"),
                        artist("AFX"),
                    ]),
                    album: "Windowlicker".to_owned(),
                    album_artists: Vec::new(),
                    popularity: 50,
                    number: 1,
                    disc_number: 1,
                },
            ),
        );
        assert_eq!(card.title.as_deref(), Some("Windowlicker"));
        assert_eq!(card.artist.as_deref(), Some("Aphex Twin, AFX"));
        assert_eq!(card.album.as_deref(), Some("Windowlicker"));
        assert_eq!(card.duration, Some(Duration::from_secs(366)));
        assert_eq!(card.track, Some((1, None)));
        // A new track starts at zero, not wherever the last one got to — the position
        // ticks that blank the card are what this field's history is about.
        assert_eq!(card.position, Some(Duration::ZERO));
        // Artwork is a URL at this point, not bytes. Leaving it absent rather than
        // holding the text hostage to a download is the decision in #50, and the failure
        // it prevents is a card that appears a second late for every track.
        assert!(card.artwork.is_none());

        // `number == 0` means "not known", not "track zero". A card that printed `0` for
        // every single would look like a bug in the metadata rather than an absence.
        let mut card = NowPlaying::default();
        apply_track(
            &mut card,
            &audio_item(
                "Untitled",
                1000,
                UniqueFields::Track {
                    artists: librespot_metadata::artist::ArtistsWithRole(vec![artist("Someone")]),
                    album: "Album".to_owned(),
                    album_artists: Vec::new(),
                    popularity: 0,
                    number: 0,
                    disc_number: 1,
                },
            ),
        );
        assert_eq!(card.track, None);

        // A podcast has no artist and no album. The show name is the closest honest
        // mapping, and it is what a listener would call the thing playing.
        let mut card = NowPlaying::default();
        apply_track(
            &mut card,
            &audio_item(
                "Episode 12",
                3_600_000,
                UniqueFields::Episode {
                    description: "…".to_owned(),
                    publish_time: librespot_core::date::Date::from_timestamp_ms(0).unwrap(),
                    show_name: "A Podcast".to_owned(),
                },
            ),
        );
        assert_eq!(card.title.as_deref(), Some("Episode 12"));
        assert_eq!(card.artist.as_deref(), Some("A Podcast"));
        assert_eq!(card.album, None);
        assert_eq!(card.track, None);

        // A local file the user synced. librespot cannot split the artist string safely,
        // so it goes through exactly as the file's metadata had it — including the case
        // where the file had none at all.
        let mut card = NowPlaying::default();
        apply_track(
            &mut card,
            &audio_item(
                "A File",
                1000,
                UniqueFields::Local {
                    artists: Some("Some; Body".to_owned()),
                    album: None,
                    album_artists: None,
                    number: Some(0),
                    disc_number: None,
                    path: std::path::PathBuf::from("/music/a.mp3"),
                },
            ),
        );
        assert_eq!(card.artist.as_deref(), Some("Some; Body"));
        assert_eq!(card.album, None);
        // …and `0` means unknown here too, which is a separate code path from the track
        // branch above and was equally untested.
        assert_eq!(card.track, None);
    }

    /// The cover art chosen is the biggest one offered.
    ///
    /// Spotify sends several sizes and the card scales down, so picking the smallest
    /// gives a visibly soft image on a 4K panel — and picking "the first" gives whichever
    /// order the metadata happened to arrive in.
    #[test]
    fn the_cover_chosen_is_the_largest_one_offered() {
        use librespot_metadata::audio::item::CoverImage;
        use librespot_metadata::image::ImageSize;

        let cover = |url: &str, width: i32| CoverImage {
            url: url.to_owned(),
            size: ImageSize::DEFAULT,
            width,
            height: width,
        };

        let mut item = audio_item(
            "x",
            1000,
            UniqueFields::Episode {
                description: String::new(),
                publish_time: librespot_core::date::Date::from_timestamp_ms(0).unwrap(),
                show_name: "s".to_owned(),
            },
        );
        assert_eq!(best_cover(&item), None, "no covers is not a panic");

        // Deliberately not in size order: "the largest" and "the last" must not be the
        // same answer, or the test would pass on a `.last()`.
        item.covers = vec![
            cover("https://i.scdn.co/small", 64),
            cover("https://i.scdn.co/large", 640),
            cover("https://i.scdn.co/medium", 300),
        ];
        assert_eq!(
            best_cover(&item).as_deref(),
            Some("https://i.scdn.co/large")
        );
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
