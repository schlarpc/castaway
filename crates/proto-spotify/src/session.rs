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
use librespot_core::{Session, SessionConfig};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
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
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Spirc` is an opaque command sender; the tasks are the only useful state.
        f.debug_struct("LiveSession")
            .field("spirc_task", &self.spirc_task)
            .field("events_task", &self.events_task)
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

    // This is where the network actually happens: AP handshake, login5, dealer connect,
    // and the connect-state registration that makes us visible in the picker. Anything
    // wrong with the account surfaces here, before we have claimed the audio output.
    let (spirc, spirc_task) = Spirc::new(
        ConnectConfig {
            name: settings.device_name.clone(),
            initial_volume: volume_to_spotify(settings.initial_volume),
            ..ConnectConfig::default()
        },
        session,
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

    let events_task = tokio::spawn(pump_events(events, sink.clone()));
    let spirc_task = tokio::spawn(spirc_task);

    Ok(LiveSession {
        spirc,
        spirc_task,
        events_task,
    })
}

/// Translate librespot's player events into the now-playing surface.
///
/// Deliberately a *fold* rather than a straight map: [`NowPlaying`] is specified as a
/// full snapshot re-emitted whenever any part changes, but librespot reports the track
/// and the position in separate events. Without keeping the last track here, every
/// position tick would blank the card's text.
async fn pump_events(mut events: mpsc::UnboundedReceiver<PlayerEvent>, sink: SessionSink) {
    let mut snapshot = NowPlaying::new(PlaybackState::Stopped);

    while let Some(event) = events.recv().await {
        let changed = match event {
            PlayerEvent::TrackChanged { audio_item } => {
                apply_track(&mut snapshot, &audio_item);
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
