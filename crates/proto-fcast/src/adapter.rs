//! The tokio shell around the pure core: a TCP listener on 46899, one actor per
//! connection feeding [`crate::session::Session`], a shared [`crate::player::Player`]
//! behind a sync lock, and a 1 Hz broadcast ticker that reads the pipeline's clock at
//! the boundary and fans `PlaybackUpdate`s out to every connected sender.
//!
//! Clocks are read here and passed inward (ground rule 6 / #208): `now` is monotonic
//! time since each connection opened, `wall_ms` is unix milliseconds for the wire's
//! `generationTime` fields. Nothing below this file sees either clock.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use castaway_core::{
    Advertisement, CoreError, PlaybackReport, ProtocolKind, SessionEvent, SessionSink,
    SourceAdapter,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::control::FCastRemote;
use crate::error::FCastError;
use crate::messages::PlayState;
use crate::player::{Applied, Player, Refusal};
use crate::session::{
    ReceiverIdentity, ReceiverUpdate, SenderCommand, Session, SessionContext, PROTOCOL_VERSION,
};
use crate::wire::{self, Frame};

/// The FCast TCP port. Fixed by convention across every published sender; there is
/// no SRV-record indirection senders actually honour for picking another.
pub const FCAST_PORT: u16 = 46899;

/// The DNS-SD service type senders browse for.
pub const FCAST_SERVICE_TYPE: &str = "_fcast._tcp";

/// How often the broadcast ticker reports playback progress while something plays.
/// The reference receiver's JSON-session cadence.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// Outbound frames queued per connection before we start dropping *updates*. A
/// sender 64 control frames behind is not reading its socket; the heartbeat will
/// declare it dead shortly, and dropping a snapshot it would never see beats
/// buffering without bound (rule 4: drop late, stay live).
const OUTBOUND_QUEUE: usize = 64;

/// The FCast receiver: advertises `_fcast._tcp` and terminates sender sessions.
pub struct FCastReceiver {
    listen: SocketAddr,
    /// Where the listener actually bound, set once by [`SourceAdapter::run`]. Only
    /// interesting when `listen` asked for port 0 (tests).
    bound: std::sync::OnceLock<SocketAddr>,
    shared: Arc<Shared>,
}

/// State shared between connections, the ticker, and the panel's remote.
pub(crate) struct Shared {
    pub(crate) identity: ReceiverIdentity,
    playback: Option<Arc<dyn PlaybackReport>>,
    pub(crate) inner: Mutex<Inner>,
}

/// Everything behind the lock. The lock is sync and never held across an await —
/// protocol work under it is pure and bounded.
pub(crate) struct Inner {
    pub(crate) player: Player,
    peers: HashMap<u64, Peer>,
    next_peer: u64,
}

struct Peer {
    session: Session,
    outbound: mpsc::Sender<Vec<u8>>,
}

impl FCastReceiver {
    /// A receiver advertising `friendly_name`, listening on the standard port.
    #[must_use]
    pub fn new(friendly_name: impl Into<String>) -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], FCAST_PORT)),
            bound: std::sync::OnceLock::new(),
            shared: Arc::new(Shared {
                identity: ReceiverIdentity {
                    display_name: friendly_name.into(),
                    app_name: "castaway".into(),
                    app_version: env!("CARGO_PKG_VERSION").into(),
                },
                playback: None,
                inner: Mutex::new(Inner {
                    player: Player::new(),
                    peers: HashMap::new(),
                    next_peer: 0,
                }),
            }),
        }
    }

    /// Attach the pipeline's progress report, so `PlaybackUpdate`s carry a live
    /// position instead of nothing.
    #[must_use]
    pub fn with_playback(mut self, report: Arc<dyn PlaybackReport>) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("with_playback is builder-time, before the adapter is shared");
        shared.playback = Some(report);
        self
    }

    /// Override the listen address (tests bind loopback on an ephemeral port).
    #[must_use]
    pub fn with_listen(mut self, listen: SocketAddr) -> Self {
        self.listen = listen;
        self
    }

    /// The port senders are told about.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.listen.port()
    }

    /// Where the listener actually bound, once [`SourceAdapter::run`] has. `None`
    /// before that. Lets a test bind port 0 and still find the socket.
    #[must_use]
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        self.bound.get().copied()
    }
}

impl Shared {
    /// Unix milliseconds, read once per boundary crossing.
    fn wall_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// The pipeline's current progress, read at the boundary.
    fn progress(&self) -> Option<castaway_core::PlaybackProgress> {
        self.playback.as_ref().and_then(|p| p.progress())
    }

    /// Fan updates out to every connected session, each in its own dialect.
    fn broadcast(inner: &mut Inner, wall_ms: u64, updates: &[ReceiverUpdate]) {
        for peer in inner.peers.values() {
            for update in updates {
                let Some(frame) = peer.session.frame_update(wall_ms, update) else {
                    continue;
                };
                send_frame(&peer.outbound, &frame);
            }
        }
    }

    /// Apply a sender command to the player. Returns the events to emit (after the
    /// lock is dropped) — refusals go back to the asking session alone, as the
    /// `PlaybackError` the protocol gives us for it.
    pub(crate) fn apply(
        &self,
        peer_id: Option<u64>,
        command: SenderCommand,
    ) -> Result<Vec<SessionEvent>, Refusal> {
        let wall_ms = Self::wall_ms();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let outcome = match command {
            SenderCommand::Load(play) => inner.player.load(*play),
            SenderCommand::Pause => Ok(inner.player.pause()),
            SenderCommand::Resume => Ok(inner.player.resume()),
            SenderCommand::Stop => Ok(inner.player.stop()),
            SenderCommand::Seek(target) => Ok(inner.player.seek(target)),
            SenderCommand::SetVolume(volume) => Ok(inner.player.set_volume(volume)),
            SenderCommand::SetSpeed(speed) => inner.player.set_speed(speed),
            SenderCommand::SetPlaylistItem(index) => inner.player.set_playlist_item(index),
        };
        match outcome {
            Ok(Applied { events, updates }) => {
                Self::broadcast(inner, wall_ms, &updates);
                Ok(events)
            }
            Err(refusal) => {
                if let Some(peer) = peer_id.and_then(|id| inner.peers.get(&id)) {
                    let update = ReceiverUpdate::Error(refusal.0.clone());
                    if let Some(frame) = peer.session.frame_update(wall_ms, &update) {
                        send_frame(&peer.outbound, &frame);
                    }
                }
                Err(refusal)
            }
        }
    }

    /// Step the playlist from the panel's next/previous buttons.
    pub(crate) fn step(&self, forward: bool) -> Result<Vec<SessionEvent>, Refusal> {
        let wall_ms = Self::wall_ms();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let Applied { events, updates } = inner.player.step(forward)?;
        Self::broadcast(inner, wall_ms, &updates);
        Ok(events)
    }

    /// The pipeline finished with an item (panel-side callback).
    pub(crate) fn media_ended(&self, end: &castaway_core::PlaybackEnd) -> Vec<SessionEvent> {
        let wall_ms = Self::wall_ms();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let Applied { events, updates } = inner.player.media_ended(end);
        Self::broadcast(inner, wall_ms, &updates);
        events
    }
}

/// Queue one frame, dropping it (with a note) when the peer has stopped reading.
fn send_frame(outbound: &mpsc::Sender<Vec<u8>>, frame: &Frame) {
    let Ok(bytes) = wire::encode(frame) else {
        // Our own outbound bodies are all far under the ceiling; a failure here is a
        // bug worth hearing about, not worth crashing a session over.
        warn!(opcode = ?frame.opcode, "outbound frame over the packet ceiling; dropped");
        return;
    };
    match outbound.try_send(bytes) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            debug!(opcode = ?frame.opcode, "sender not reading; update dropped");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// Emit events, splicing the control surface in immediately after a `Play` — the
/// session manager only accepts a surface from the source that holds the screen, so
/// it must follow the event that takes the screen (same reasoning as proto-cast).
async fn emit_all(sink: &SessionSink, shared: &Arc<Shared>, events: Vec<SessionEvent>) {
    for event in events {
        let begins = matches!(event, SessionEvent::Play { .. });
        if sink.emit(event).await.is_err() {
            return;
        }
        if begins {
            let remote = Arc::new(FCastRemote::new(Arc::clone(shared), sink.clone()));
            if sink
                .emit(SessionEvent::ControlSurface(remote))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

/// One connection's actor: read frames, feed the session, apply commands, write
/// whatever the session and the broadcasts queue up.
async fn serve(shared: Arc<Shared>, stream: TcpStream, peer_addr: SocketAddr, sink: SessionSink) {
    let started = tokio::time::Instant::now();
    let (mut reader, mut writer) = stream.into_split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE);

    // Register, and send the greeting both parties owe each other on connect.
    let peer_id = {
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (session, greeting) = Session::new();
        let id = guard.next_peer;
        guard.next_peer += 1;
        guard.peers.insert(
            id,
            Peer {
                session,
                outbound: outbound_tx.clone(),
            },
        );
        if let Some(peer) = guard.peers.get(&id) {
            send_frame(&peer.outbound, &greeting);
        }
        id
    };
    info!(peer = %peer_addr, "fcast: sender connected");

    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = outbound_rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    'conn: loop {
        tokio::select! {
            read = reader.read(&mut chunk) => {
                let n = match read {
                    Ok(0) => break 'conn,
                    Ok(n) => n,
                    Err(e) => {
                        debug!(peer = %peer_addr, error = %e, "fcast: read failed");
                        break 'conn;
                    }
                };
                buf.extend_from_slice(&chunk[..n]);
                loop {
                    let decoded = match wire::try_decode(&buf) {
                        Ok(Some(decoded)) => decoded,
                        Ok(None) => break,
                        Err(fault) => {
                            warn!(peer = %peer_addr, %fault, "fcast: framing fault; disconnecting");
                            break 'conn;
                        }
                    };
                    let (frame, consumed) = decoded;
                    buf.drain(..consumed);
                    match handle_frame(&shared, peer_id, started.elapsed(), &frame) {
                        Ok(events) => emit_all(&sink, &shared, events).await,
                        Err(fault) => {
                            warn!(peer = %peer_addr, %fault, "fcast: session fault; disconnecting");
                            break 'conn;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                let tick = {
                    let mut guard = shared
                        .inner
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(peer) = guard.peers.get_mut(&peer_id) else { break 'conn };
                    match peer.session.on_tick(started.elapsed()) {
                        Ok(ping) => {
                            if let Some(ping) = ping {
                                send_frame(&peer.outbound, &ping);
                            }
                            Ok(())
                        }
                        Err(fault) => Err(fault),
                    }
                };
                if let Err(fault) = tick {
                    info!(peer = %peer_addr, %fault, "fcast: disconnecting");
                    break 'conn;
                }
            }
        }
    }

    shared
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .peers
        .remove(&peer_id);
    writer_task.abort();
    info!(peer = %peer_addr, "fcast: sender disconnected");
}

/// Feed one frame through the session and the player. Pure work under the lock;
/// the returned events are emitted by the caller afterwards.
fn handle_frame(
    shared: &Arc<Shared>,
    peer_id: u64,
    now: Duration,
    frame: &Frame,
) -> Result<Vec<SessionEvent>, FCastError> {
    let wall_ms = Shared::wall_ms();
    let reaction = {
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let play_data = inner.player.play_data().cloned();
        let volume = inner.player.volume();
        let Some(peer) = inner.peers.get_mut(&peer_id) else {
            return Ok(Vec::new());
        };
        let ctx = SessionContext {
            wall_ms,
            receiver: &shared.identity,
            play_data: play_data.as_ref(),
            volume,
        };
        let reaction = peer.session.on_frame(now, &ctx, frame)?;
        for reply in &reaction.replies {
            send_frame(&peer.outbound, reply);
        }
        reaction
    };

    let Some(command) = reaction.command else {
        return Ok(Vec::new());
    };
    debug!(?command, "fcast: sender command");
    match shared.apply(Some(peer_id), command) {
        Ok(events) => Ok(events),
        Err(refusal) => {
            // The refusal already went back to the asking sender as a
            // `PlaybackError`; the connection stays up and whatever was playing
            // keeps playing.
            info!(reason = %refusal.0, "fcast: request refused");
            Ok(Vec::new())
        }
    }
}

/// The 1 Hz progress broadcast, shared by every connection: reads the pipeline
/// clock once per tick and fans the snapshot out.
async fn progress_ticker(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let progress = shared.progress();
        let wall_ms = Shared::wall_ms();
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        if inner.peers.is_empty() {
            continue;
        }
        let snapshot = inner.player.snapshot(progress);
        if snapshot.state == PlayState::Idle {
            continue;
        }
        Shared::broadcast(inner, wall_ms, &[ReceiverUpdate::Playback(snapshot)]);
    }
}

#[async_trait::async_trait]
impl SourceAdapter for FCastReceiver {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::FCast
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        vec![Advertisement::MdnsService {
            ty: FCAST_SERVICE_TYPE.to_string(),
            instance: self.shared.identity.display_name.clone(),
            port: self.port(),
            // The version, stated rather than implied (#241's scope note): protocol
            // v4 defines this key as "highest supported protocol version", and
            // stating 3 is what tells a v4-aware sender not to expect the TLS
            // upgrade. v3-era receivers advertise no TXT at all, so senders treat
            // the record as optional and older ones simply ignore it.
            txt: vec![("v".to_string(), PROTOCOL_VERSION.to_string())],
            subtypes: Vec::new(),
        }]
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "registered: the fcast/tcp 46899 entry in crates/app/src/surface.rs"
        )]
        let listener = TcpListener::bind(self.listen)
            .await
            .map_err(|e| CoreError::Adapter(format!("binding FCast on {}: {e}", self.listen)))?;
        if let Ok(addr) = listener.local_addr() {
            let _ = self.bound.set(addr);
        }
        info!(addr = %self.listen, "FCast listener ready");

        tokio::spawn(progress_ticker(Arc::clone(&self.shared)));

        match castaway_core::net::accept_loop(listener, sink, "fcast", move |stream, peer, sink| {
            let shared = Arc::clone(&self.shared);
            async move { serve(shared, stream, peer, sink).await }
        })
        .await {}
    }
}
