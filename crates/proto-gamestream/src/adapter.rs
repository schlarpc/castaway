//! The tokio actor: discovery, pairing state, and the session lifecycle.
//!
//! This is the shell (ground rule 3) — it owns the mDNS browse, the persisted pairing
//! files, and the channels, and makes no protocol decisions of its own.
//!
//! The shape differs from every other adapter here, because the role does. A receiver
//! advertises and waits; this one browses and dials. Two consequences:
//!
//! - [`SourceAdapter::advertisements`] is empty. There is nothing to advertise — we are
//!   the client. The app's advertisement bridge is never asked for anything.
//! - `run` cannot be the only entry point. A receiver learns what to do from the sender
//!   that connected; a client has to be *told*, so the adapter is built with a
//!   [`GameStreamCommand`] receiver. Today the only sender of those commands is the
//!   app's config (`autostart`); the channel exists so the panel's own chooser can
//!   become the second one without changing this file.

use std::path::PathBuf;
use std::sync::Arc;

use castaway_core::{
    Advertisement, CoreError, ProtocolKind, SessionEvent, SessionSink, SourceAdapter,
    SourceDescription,
};
use substrate_mdns::{BrowseEvent, MdnsResponder};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::client::{generate_session_keys, GameStreamClient};
use crate::discovery::HostCandidate;
use crate::error::GameStreamError;
use crate::identity::ClientIdentity;
use crate::nvhttp::{LaunchParams, UniqueId};
use crate::pairing::PairedServer;

/// What the adapter can be asked to do. The seam a chooser would drive.
#[derive(Debug, Clone)]
pub enum GameStreamCommand {
    /// Pair with a host. The PIN is typed into the *host's* UI, not ours, so this only
    /// carries what identifies the host.
    Pair {
        /// Host address or mDNS instance name.
        host: String,
        /// The PIN the person will type on the host.
        pin: String,
    },
    /// Start streaming an app. `app` matches an app title case-insensitively; `None`
    /// takes whatever the host lists first, which on Sunshine is the desktop.
    Start {
        /// Host address or mDNS instance name.
        host: String,
        /// The app title to launch.
        app: Option<String>,
    },
    /// End the session and tell the host to stop.
    Stop,
}

/// Where the client identity and per-host pairings live between boots.
///
/// The identity *is* the credential — a host trusts one certificate, so losing this
/// directory means re-pairing with every host. Point it somewhere persistent and
/// readable only by the service user.
#[derive(Debug, Clone)]
pub struct PairingStore {
    dir: PathBuf,
}

impl PairingStore {
    /// A store rooted at `dir`, created on first write.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Load the client identity, generating and persisting one on first run.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] if the files exist but do not parse, or cannot be
    /// written.
    pub fn load_identity(&self) -> Result<ClientIdentity, GameStreamError> {
        let cert_path = self.dir.join("client.crt");
        let key_path = self.dir.join("client.key");
        if let (Ok(cert), Ok(key)) = (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
        ) {
            return ClientIdentity::from_pem(&cert, &key);
        }
        // RSA-2048 keygen is slow in a debug build; this happens once, ever.
        info!("generating a GameStream client identity (first run)");
        let identity = ClientIdentity::generate()?;
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            GameStreamError::Identity(format!("creating {}: {e}", self.dir.display()))
        })?;
        write_private(&key_path, identity.key_pem())?;
        std::fs::write(&cert_path, identity.cert_pem()).map_err(|e| {
            GameStreamError::Identity(format!("writing {}: {e}", cert_path.display()))
        })?;
        Ok(identity)
    }

    /// The stable client id every request carries, generated once and persisted.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] if it cannot be written.
    pub fn load_unique_id(&self) -> Result<UniqueId, GameStreamError> {
        let path = self.dir.join("uniqueid");
        if let Ok(id) = std::fs::read_to_string(&path) {
            let id = id.trim();
            if !id.is_empty() {
                return Ok(UniqueId::new(id));
            }
        }
        let id = UniqueId::generate();
        std::fs::create_dir_all(&self.dir).map_err(|e| {
            GameStreamError::Identity(format!("creating {}: {e}", self.dir.display()))
        })?;
        std::fs::write(&path, id.as_str())
            .map_err(|e| GameStreamError::Identity(format!("writing {}: {e}", path.display())))?;
        Ok(id)
    }

    /// The pinned certificate for a host, if we have paired with it.
    #[must_use]
    pub fn load_pairing(&self, host: &str) -> Option<PairedServer> {
        let pem = std::fs::read_to_string(self.host_path(host)).ok()?;
        let der = crate::pairing::pem_to_der(&pem)?;
        Some(PairedServer {
            server_cert_pem: pem,
            server_cert_der: der,
        })
    }

    /// Persist a completed pairing.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] if it cannot be written.
    pub fn save_pairing(&self, host: &str, server: &PairedServer) -> Result<(), GameStreamError> {
        std::fs::create_dir_all(self.dir.join("hosts"))
            .map_err(|e| GameStreamError::Identity(format!("creating the host store: {e}")))?;
        let path = self.host_path(host);
        std::fs::write(&path, &server.server_cert_pem)
            .map_err(|e| GameStreamError::Identity(format!("writing {}: {e}", path.display())))
    }

    fn host_path(&self, host: &str) -> PathBuf {
        // Host addresses contain dots and colons; neither is a path separator, but a
        // stray one would be, so anything unexpected becomes an underscore.
        let safe: String = host
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.dir.join("hosts").join(format!("{safe}.crt"))
    }
}

/// Write a file only the owner can read. The client key is the whole credential.
fn write_private(path: &std::path::Path, contents: &str) -> Result<(), GameStreamError> {
    std::fs::write(path, contents)
        .map_err(|e| GameStreamError::Identity(format!("writing {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort: a store on a filesystem without modes is not a reason to refuse
        // to run, but it is a reason to say so.
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            warn!(path = %path.display(), error = %e, "could not restrict the client key's permissions");
        }
    }
    Ok(())
}

/// How a session should be requested from the host.
#[derive(Debug, Clone)]
pub struct SessionPreferences {
    /// Stream width in pixels.
    pub width: u32,
    /// Stream height in pixels.
    pub height: u32,
    /// Frame rate.
    pub fps: u32,
    /// Video bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Let the host change the game's own resolution to match.
    pub optimize_settings: bool,
    /// Also play audio on the host's speakers.
    pub play_audio_on_host: bool,
    /// Allow HEVC when the host offers it.
    pub allow_hevc: bool,
}

impl Default for SessionPreferences {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            optimize_settings: false,
            play_audio_on_host: false,
            allow_hevc: false,
        }
    }
}

/// The GameStream client adapter.
pub struct GameStreamAdapter {
    identity: Arc<ClientIdentity>,
    unique_id: UniqueId,
    store: PairingStore,
    prefs: SessionPreferences,
    commands: Mutex<Option<mpsc::Receiver<GameStreamCommand>>>,
    /// Hosts seen on the LAN, keyed by mDNS fullname.
    hosts: Mutex<Vec<HostCandidate>>,
    /// The live streaming session, if any. Owning it is what keeps it running —
    /// dropping it stops the stream and frees the C library's singleton.
    #[cfg(feature = "stream")]
    session: Mutex<Option<crate::stream::StreamSession>>,
    /// The host the live session is talking to, so `Stop` can tell it to stop.
    active_host: Mutex<Option<String>>,
}

impl GameStreamAdapter {
    /// Build the adapter, loading (or creating) the persisted identity.
    ///
    /// # Errors
    /// [`GameStreamError::Identity`] if the store is unusable.
    pub fn new(
        store: PairingStore,
        prefs: SessionPreferences,
        commands: mpsc::Receiver<GameStreamCommand>,
    ) -> Result<Self, GameStreamError> {
        let identity = Arc::new(store.load_identity()?);
        let unique_id = store.load_unique_id()?;
        Ok(Self {
            identity,
            unique_id,
            store,
            prefs,
            commands: Mutex::new(Some(commands)),
            hosts: Mutex::new(Vec::new()),
            #[cfg(feature = "stream")]
            session: Mutex::new(None),
            active_host: Mutex::new(None),
        })
    }

    /// Hosts discovered so far — what a chooser would render.
    pub async fn hosts(&self) -> Vec<HostCandidate> {
        self.hosts.lock().await.clone()
    }

    /// Resolve a name from a command to something dialable: an exact address, or a
    /// discovered instance name.
    async fn resolve(&self, name: &str) -> Option<(String, u16)> {
        let hosts = self.hosts.lock().await;
        if let Some(host) = hosts
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name) || h.address.to_string() == name)
        {
            return Some((host.address.to_string(), host.http_port));
        }
        // Not discovered: treat it as an address the operator gave us. Sunshine on a
        // different subnet is invisible to mDNS but perfectly reachable.
        if name.is_empty() {
            return None;
        }
        Some((name.to_string(), crate::nvhttp::DEFAULT_HTTP_PORT))
    }

    /// Build a client for a host, restoring any persisted pairing.
    async fn client_for(&self, name: &str) -> Result<GameStreamClient, GameStreamError> {
        let (address, port) = self
            .resolve(name)
            .await
            .ok_or_else(|| GameStreamError::NotPaired { host: name.into() })?;
        let mut client = GameStreamClient::new(
            Arc::clone(&self.identity),
            self.unique_id.clone(),
            address.clone(),
            port,
        );
        if let Some(server) = self.store.load_pairing(&address) {
            // The TLS port comes from the host, not from us, so ask first.
            let probe = GameStreamClient::new(
                Arc::clone(&self.identity),
                self.unique_id.clone(),
                address.clone(),
                port,
            );
            let https_port = probe
                .server_info()
                .await
                .map_or(crate::nvhttp::DEFAULT_HTTPS_PORT, |i| i.https_port);
            client = client.with_pairing(server, https_port);
        }
        Ok(client)
    }

    /// Run the pairing handshake with a host and persist the result.
    ///
    /// Public for the same reason [`Self::apps_for`] is (D38): the panel's shell is a
    /// second caller, and unlike the config-driven [`GameStreamCommand::Pair`] it needs
    /// the *verdict* — a screen has to change on success and say why on failure, which
    /// a fire-and-forget command cannot carry.
    ///
    /// Blocks for as long as the host's side takes: Sunshine parks the first response
    /// until someone types the PIN into its web UI, so this returns when a human acts.
    /// There is deliberately no timeout here (docs/gamestream-protocol-notes.md §3 —
    /// "a wait to allow, not a timeout to tune"); a caller with a screen to keep honest
    /// owns its own.
    ///
    /// # Errors
    /// [`GameStreamError::WrongPin`] when the digits typed on the host were not these;
    /// [`GameStreamError::Pairing`] when a trust check failed; transport errors as the
    /// client reports them.
    pub async fn pair(&self, host: &str, pin: &str) -> Result<(), GameStreamError> {
        let (address, port) = self
            .resolve(host)
            .await
            .ok_or_else(|| GameStreamError::NotPaired { host: host.into() })?;
        let mut client = GameStreamClient::new(
            Arc::clone(&self.identity),
            self.unique_id.clone(),
            address.clone(),
            port,
        );
        let info = client.server_info().await?;
        info!(host = %address, name = %info.hostname, "pairing with GameStream host");
        client.pair(pin, info.https_port).await?;
        if let Some(server) = client.pairing() {
            self.store.save_pairing(&address, server)?;
        }
        Ok(())
    }

    /// What a host offers, for the panel's picker (D38).
    ///
    /// Read-only and safe to call from a UI press: it opens no session and starts
    /// nothing. A host we have not paired with answers [`GameStreamError::NotPaired`]
    /// rather than an empty list, because "no apps" and "we are not allowed to ask" are
    /// different things to show someone.
    ///
    /// # Errors
    /// [`GameStreamError`] as the NVHTTP client reports it — the host's own words where
    /// there are any.
    pub async fn apps_for(&self, host: &str) -> Result<Vec<crate::nvhttp::App>, GameStreamError> {
        let client = self.client_for(host).await?;
        let info = client.server_info().await?;
        if !info.paired {
            return Err(GameStreamError::NotPaired {
                host: client.host().to_string(),
            });
        }
        client.apps().await
    }

    /// Pair-if-needed, launch, and hand the streams to the session manager.
    async fn handle_start(
        &self,
        host: &str,
        app: Option<&str>,
        sink: &SessionSink,
    ) -> Result<(), GameStreamError> {
        let client = self.client_for(host).await?;
        let info = client.server_info().await?;
        if !info.paired {
            return Err(GameStreamError::NotPaired {
                host: client.host().to_string(),
            });
        }

        let apps = client.apps().await?;
        let chosen = match app {
            Some(wanted) => apps
                .iter()
                .find(|a| a.title.eq_ignore_ascii_case(wanted))
                .ok_or_else(|| GameStreamError::Nvhttp {
                    code: 404,
                    message: format!(
                        "host has no app named {wanted:?}; it offers: {}",
                        apps.iter()
                            .map(|a| a.title.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                })?,
            None => apps.first().ok_or_else(|| GameStreamError::Nvhttp {
                code: 404,
                message: "host lists no apps to launch".into(),
            })?,
        };

        let (ri_key, ri_key_iv, ri_key_id) = generate_session_keys();
        let params = LaunchParams {
            app_id: chosen.id,
            // A host with something already running takes /resume, not /launch —
            // /launch would be refused with "an app is already running".
            resume: info.current_game != 0,
            width: self.prefs.width,
            height: self.prefs.height,
            fps: self.prefs.fps,
            optimize_settings: self.prefs.optimize_settings,
            play_audio_on_host: self.prefs.play_audio_on_host,
            // Stereo: (channelMask << 16) | channelCount.
            surround_audio_info: 196_610,
            ri_key,
            ri_key_id,
        };
        info!(app = %chosen.title, host = %client.host(), "launching GameStream app");
        let launched = client.launch(&params).await?;
        *self.active_host.lock().await = Some(client.host().to_string());

        sink.emit(SessionEvent::SourceInfo(
            SourceDescription::default()
                .with_display_name(format!("{} on {}", chosen.title, info.hostname))
                .with_address(client.host().to_string()),
        ))
        .await
        .map_err(|_: CoreError| GameStreamError::SinkClosed)?;

        self.stream(
            &client,
            &info,
            &launched.session_url,
            ri_key,
            ri_key_iv,
            sink,
        )
        .await
    }

    /// End any live session: drop it (which stops the linked core) and ask the host to
    /// stop too, so a game is not left running on an unattended PC.
    async fn stop_session(&self) {
        #[cfg(feature = "stream")]
        {
            // Dropped before the host is told, so the library has already torn its
            // sockets down when the host stops sending.
            drop(self.session.lock().await.take());
        }
        let host = self.active_host.lock().await.take();
        if let Some(host) = host {
            if let Ok(client) = self.client_for(&host).await {
                client.cancel().await;
            }
        }
    }

    /// Hand the launched session to the linked core and publish its streams.
    #[cfg(feature = "stream")]
    async fn stream(
        &self,
        client: &GameStreamClient,
        info: &crate::nvhttp::ServerInfo,
        session_url: &str,
        ri_key: [u8; 16],
        ri_key_iv: [u8; 16],
        sink: &SessionSink,
    ) -> Result<(), GameStreamError> {
        let config = crate::stream::StreamConfig {
            width: self.prefs.width,
            height: self.prefs.height,
            fps: self.prefs.fps,
            bitrate_kbps: self.prefs.bitrate_kbps,
            ri_key,
            ri_key_iv,
            allow_hevc: self.prefs.allow_hevc,
        };
        let info = info.clone();
        let address = client.host().to_string();
        let session_url = session_url.to_string();
        // LiStartConnection blocks through the whole handshake.
        let (session, video, audio) = tokio::task::spawn_blocking(move || {
            crate::stream::StreamSession::start(&info, &address, &session_url, &config)
        })
        .await
        .map_err(|e| GameStreamError::Stream {
            stage: format!("starting the streaming core ({e})"),
            code: 0,
        })
        .and_then(|r| r)?;

        sink.emit(SessionEvent::Mirror {
            video,
            audio: Some(audio),
        })
        .await
        .map_err(|_: CoreError| GameStreamError::SinkClosed)?;
        // Held, not leaked: dropping a `StreamSession` calls LiStopConnection and
        // releases the library's process-wide singleton, so the adapter has to own it
        // until `Stop` or shutdown. A previous session in the slot is dropped here,
        // which is also what makes starting a second one work.
        *self.session.lock().await = Some(session);
        Ok(())
    }

    /// Without the `stream` feature the client can discover, pair, and launch, but has
    /// nothing to decode the result with — so it says so and stops the host rather than
    /// leaving a session running against a black panel.
    #[cfg(not(feature = "stream"))]
    async fn stream(
        &self,
        client: &GameStreamClient,
        _info: &crate::nvhttp::ServerInfo,
        _session_url: &str,
        _ri_key: [u8; 16],
        _ri_key_iv: [u8; 16],
        _sink: &SessionSink,
    ) -> Result<(), GameStreamError> {
        client.cancel().await;
        Err(GameStreamError::Stream {
            stage: "streaming".into(),
            code: 0,
        })
    }
}

#[async_trait::async_trait]
impl SourceAdapter for GameStreamAdapter {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::GameStream
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        // Nothing. We are the client here — the host is the one advertising, and this
        // adapter browses for it in `run`.
        Vec::new()
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        let mut commands = self
            .commands
            .lock()
            .await
            .take()
            .ok_or_else(|| CoreError::Adapter("GameStream adapter was already run".into()))?;

        // Its own responder: this adapter browses, and the app's shared responder is
        // built for advertising. One extra daemon is cheaper than threading a browse
        // handle through every call site that does not need one.
        let responder = MdnsResponder::new()
            .map_err(|e| CoreError::Adapter(format!("GameStream mDNS: {e}")))?;
        let mut browser = responder
            .browse(crate::NVSTREAM_SERVICE_TYPE)
            .map_err(|e| CoreError::Adapter(format!("GameStream browse: {e}")))?;
        info!(
            "browsing for GameStream hosts ({})",
            crate::NVSTREAM_SERVICE_TYPE
        );

        loop {
            tokio::select! {
                event = browser.next() => match event {
                    Some(BrowseEvent::Resolved(service)) => {
                        if let Some(host) = HostCandidate::from_resolved(&service) {
                            let mut hosts = self.hosts.lock().await;
                            if let Some(slot) =
                                hosts.iter_mut().find(|h| h.fullname == host.fullname)
                            {
                                *slot = host;
                            } else {
                                info!(name = %host.name, address = %host.address, "GameStream host found");
                                hosts.push(host);
                            }
                        }
                    }
                    Some(BrowseEvent::Removed { fullname }) => {
                        let mut hosts = self.hosts.lock().await;
                        hosts.retain(|h| h.fullname != fullname);
                        debug!(%fullname, "GameStream host gone");
                    }
                    // The daemon shut down; nothing more will be discovered, but a
                    // configured address still works, so keep serving commands.
                    None => break,
                },
                command = commands.recv() => match command {
                    Some(GameStreamCommand::Pair { host, pin }) => {
                        if let Err(e) = self.pair(&host, &pin).await {
                            warn!(%host, error = %e, "GameStream pairing failed");
                        }
                    }
                    Some(GameStreamCommand::Start { host, app }) => {
                        if let Err(e) = self.handle_start(&host, app.as_deref(), &sink).await {
                            warn!(%host, error = %e, "could not start the GameStream session");
                        }
                    }
                    Some(GameStreamCommand::Stop) => {
                        self.stop_session().await;
                        let _ = sink.emit(SessionEvent::End).await;
                    }
                    // The command channel closed: the app is shutting down.
                    None => return Ok(()),
                },
            }
        }

        // Discovery ended but commands may still arrive.
        while let Some(command) = commands.recv().await {
            if let GameStreamCommand::Start { host, app } = command {
                if let Err(e) = self.handle_start(&host, app.as_deref(), &sink).await {
                    warn!(%host, error = %e, "could not start the GameStream session");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn temp_store() -> (PairingStore, tempdir::Guard) {
        let guard = tempdir::Guard::new();
        (PairingStore::new(guard.path()), guard)
    }

    /// A minimal scoped temporary directory — the workspace has no dev-dependency for
    /// one, and this needs exactly two operations.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Guard(PathBuf);

        impl Guard {
            pub fn new() -> Self {
                let mut dir = std::env::temp_dir();
                // Unique without a clock or an RNG dependency: the address of a fresh
                // allocation is distinct for as long as it is alive.
                let boxed = Box::new(0u8);
                dir.push(format!("castaway-gs-test-{:p}", &*boxed));
                std::fs::create_dir_all(&dir).unwrap();
                Self(dir)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn the_identity_is_generated_once_and_then_reloaded() {
        // The failure this catches is quiet and total: a client that regenerates its
        // certificate on every boot is unpaired from every host on every boot, and the
        // only symptom is a 401 that says nothing about why.
        let (store, _guard) = temp_store();
        let first = store.load_identity().unwrap();
        let second = store.load_identity().unwrap();
        assert_eq!(first.cert_der(), second.cert_der());
        assert_eq!(first.key_pem(), second.key_pem());
    }

    #[test]
    fn the_unique_id_survives_a_restart() {
        let (store, _guard) = temp_store();
        let first = store.load_unique_id().unwrap();
        let second = store.load_unique_id().unwrap();
        assert_eq!(first.as_str(), second.as_str());
    }

    #[test]
    fn pairings_round_trip_per_host() {
        let (store, _guard) = temp_store();
        let identity = ClientIdentity::generate().unwrap();
        let server = PairedServer {
            server_cert_pem: identity.cert_pem().to_string(),
            server_cert_der: identity.cert_der().to_vec(),
        };
        assert!(store.load_pairing("10.0.0.7").is_none());
        store.save_pairing("10.0.0.7", &server).unwrap();
        let loaded = store.load_pairing("10.0.0.7").unwrap();
        assert_eq!(loaded.server_cert_der, server.server_cert_der);
        // Per host: pairing with one must not make us look paired with another.
        assert!(store.load_pairing("10.0.0.8").is_none());
    }

    #[test]
    fn a_host_name_cannot_escape_the_store_directory() {
        let (store, guard) = temp_store();
        let identity = ClientIdentity::generate().unwrap();
        let server = PairedServer {
            server_cert_pem: identity.cert_pem().to_string(),
            server_cert_der: identity.cert_der().to_vec(),
        };
        // A host name arrives from mDNS, which is to say from the network.
        store.save_pairing("../../etc/evil", &server).unwrap();
        let escaped = guard.path().join("../../etc/evil.crt");
        assert!(!escaped.exists(), "a host name escaped the store directory");
    }

    #[test]
    fn the_client_adapter_advertises_nothing() {
        // Not an oversight: a client has nothing to announce, and an adapter that
        // advertised would put a service on the LAN claiming to be a receiver we are
        // not.
        let (store, _guard) = temp_store();
        let (_tx, rx) = mpsc::channel(1);
        let adapter = GameStreamAdapter::new(store, SessionPreferences::default(), rx).unwrap();
        assert!(adapter.advertisements().is_empty());
        assert_eq!(adapter.kind(), ProtocolKind::GameStream);
    }
}
