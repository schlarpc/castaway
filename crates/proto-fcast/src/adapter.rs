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
    Advertisement, CoreError, MirrorBackend, PlaybackReport, ProtocolKind, SessionEvent,
    SessionSink, SourceAdapter,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::control::FCastRemote;
use crate::error::FCastError;
use crate::identity::V4Identity;
use crate::messages::PlayState;
use crate::player::{Applied, Player, Refusal};
use crate::session::{
    ReceiverIdentity, ReceiverUpdate, SenderCommand, Session, SessionContext, PROTOCOL_VERSION,
};
use crate::session_v4::{SessionV4, V4Command, V4Reaction};
use crate::v4msg;
use crate::wire::{self, Frame, Opcode};

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
    /// Protocol v4 (#248): the TLS identity, and whether the hello announces 4.
    /// Coupled deliberately — the sender SDK quits on `fp`-without-v4 (insecure
    /// downgrade) and on v4-without-`fp` (nothing to pin) — so both flip together.
    v4: Option<V4>,
    /// The WebRTC plane a v4 sender's screen arrives on (#248), when this build has
    /// one. Its presence *is* the advertised `mirroring` capability: a receiver that
    /// says it mirrors and then answers `InvalidState` is worse than one that says it
    /// does not, because the sender has already told someone it is casting.
    mirror: Option<Arc<dyn MirrorBackend>>,
    pub(crate) inner: Mutex<Inner>,
}

struct V4 {
    identity: V4Identity,
    announce: bool,
}

/// Everything behind the lock. The lock is sync and never held across an await —
/// protocol work under it is pure and bounded.
pub(crate) struct Inner {
    pub(crate) player: Player,
    peers: HashMap<u64, Peer>,
    next_peer: u64,
    /// Active FCompanion provider ids; the lowest free id is reused, like the
    /// reference.
    providers: std::collections::BTreeSet<u16>,
}

/// One connection's protocol state: which dialect it negotiated, and its pipe.
enum PeerSession {
    Json(Session),
    V4(SessionV4),
}

struct Peer {
    session: PeerSession,
    outbound: mpsc::Sender<Vec<u8>>,
    /// When this peer last got a progress frame (v4's per-session cadence).
    last_progress: tokio::time::Instant,
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
                v4: None,
                mirror: None,
                inner: Mutex::new(Inner {
                    player: Player::new(),
                    peers: HashMap::new(),
                    next_peer: 0,
                    providers: std::collections::BTreeSet::new(),
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

    /// Enable protocol v4 (#248) with this TLS identity. `announce` flips the
    /// hello to `Version {{4}}` *and* the TXT record to `v=4` + `fp` — they are
    /// one switch because the sender SDK quits on either half alone, and the
    /// upgrade itself only ever runs when **we** announced 4: a sender that said
    /// 4 but heard our 3 continues in plaintext JSON (measured — the SDK's
    /// `Version {{4}}` goes out before it reads ours), and treating its next
    /// JSON frame as a ClientHello would kill a healthy session. With
    /// `announce = false` the identity is carried but v4 never engages.
    #[must_use]
    pub fn with_v4(mut self, identity: V4Identity, announce: bool) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("with_v4 is builder-time, before the adapter is shared");
        shared.v4 = Some(V4 { identity, announce });
        self
    }

    /// Offer WebRTC screen mirroring to v4 senders, over `backend` (#248).
    ///
    /// Without one the receiver introduces itself with `mirroring: false` and refuses a
    /// `StartMirroringSession` typed, which is what it did before this existed: a
    /// build with no media plane (`--no-default-features`, the headless one) genuinely
    /// cannot show a picture, and saying so is the honest answer.
    #[must_use]
    pub fn with_mirroring(mut self, backend: Arc<dyn MirrorBackend>) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("with_mirroring is builder-time, before the adapter is shared");
        shared.mirror = Some(backend);
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

    /// The `fcast://r/…` connection URL to render as a QR code (#248), or `None`
    /// when v4 is not armed. `addresses` are the LAN IPs the panel should advertise —
    /// loopback and the wildcard bind are the caller's to resolve.
    ///
    /// `None` in two cases, and the second is the interesting one. Without an identity
    /// there is no `fp` to pin, so the QR carries nothing mDNS does not. And with
    /// `announce = false` the identity exists but v4 never engages — while the document
    /// says `v=4` unconditionally, which is a *promise*: a sender that reads it refuses
    /// to fall back to plaintext, and would then meet our `Version {{3}}` hello and give
    /// up. A QR that cannot be honoured is worse than no QR, so the announce switch
    /// gates this too.
    #[must_use]
    pub fn connection_url(&self, addresses: Vec<String>) -> Option<String> {
        let v4 = self.shared.v4.as_ref().filter(|v4| v4.announce)?;
        crate::connect_url::ConnectionInfo::v4(
            self.shared.identity.display_name.clone(),
            addresses,
            self.port(),
            v4.identity.fingerprint(),
        )
        .to_url()
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
    ///
    /// `initiator` scopes the v4 relays: a sender-driven `Load`/queue mutation is
    /// echoed to *other* senders only (the originator's own UI already moved),
    /// while state broadcasts — volume, playback state, speed, receiver-driven
    /// queue advances — go to everyone, originator included. The reference's
    /// relay table exactly.
    fn broadcast(
        inner: &mut Inner,
        wall_ms: u64,
        updates: &[ReceiverUpdate],
        initiator: Option<u64>,
    ) {
        for (id, peer) in &inner.peers {
            for update in updates {
                let excluded = Some(*id) == initiator
                    && matches!(
                        update,
                        ReceiverUpdate::V4Load { .. }
                            | ReceiverUpdate::QueueInsertRelay { .. }
                            | ReceiverUpdate::QueueRemoveRelay(_)
                            | ReceiverUpdate::QueueSelectRelay {
                                initiated_by_receiver: false,
                                ..
                            }
                    );
                if excluded {
                    continue;
                }
                let frame = match &peer.session {
                    PeerSession::Json(session) => session.frame_update(wall_ms, update),
                    PeerSession::V4(_) => v4_update_frame(update),
                };
                if let Some(frame) = frame {
                    send_frame(&peer.outbound, &frame);
                }
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
            SenderCommand::Seek(target) => {
                let duration = self.progress().and_then(|p| p.duration);
                let clamped = match duration {
                    Some(duration) if target > duration => duration,
                    _ => target,
                };
                Ok(inner.player.seek(clamped))
            }
            SenderCommand::SetVolume(volume) => Ok(inner.player.set_volume(volume)),
            SenderCommand::SetSpeed(speed) => inner.player.set_speed(speed),
            SenderCommand::SetPlaylistItem(index) => inner.player.set_playlist_item(index),
        };
        match outcome {
            Ok(Applied { events, updates }) => {
                Self::broadcast(inner, wall_ms, &updates, peer_id);
                Ok(events)
            }
            Err(refusal) => {
                if let Some(peer) = peer_id.and_then(|id| inner.peers.get(&id)) {
                    send_refusal(peer, wall_ms, &refusal, None);
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
        Self::broadcast(inner, wall_ms, &updates, None);
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
        Self::broadcast(inner, wall_ms, &updates, None);
        events
    }

    /// Apply a v4 command (#248). Same shape as [`Shared::apply`], with typed
    /// refusals and the v4-only verbs.
    fn apply_v4(&self, peer_id: u64, command: V4Command) -> Result<Vec<SessionEvent>, Refusal> {
        use fcast_flatbuf::flat::ErrorKind;
        let wall_ms = Self::wall_ms();
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let outcome = match command {
            V4Command::Load { source, raw } => inner.player.load_v4(source, raw),
            V4Command::Seek(target) => {
                // Clamp against the duration the pipeline reports, with the
                // typed error the spec defines for it. No duration known (or a
                // build with no pipeline) seeks unclamped — the pipeline is the
                // authority and answers its own way.
                let duration = self.progress().and_then(|p| p.duration);
                let clamped = match duration {
                    Some(duration) if target > duration => {
                        if let Some(peer) = inner.peers.get(&peer_id) {
                            send_frame(
                                &peer.outbound,
                                &v4msg::error_frame(ErrorKind::SeekOutOfRange, None),
                            );
                        }
                        duration
                    }
                    _ => target,
                };
                Ok(inner.player.seek(clamped))
            }
            V4Command::SetVolume(volume) => Ok(inner.player.set_volume(volume)),
            V4Command::SetSpeed(speed) => inner.player.set_speed(speed).map(|mut applied| {
                // Whatever was asked, 1.0 is what plays (#250); v4 senders are
                // told the truth as a SpeedChanged broadcast.
                applied.updates.push(ReceiverUpdate::SpeedActual(1.0));
                applied
            }),
            V4Command::Pause => Ok(inner.player.pause()),
            V4Command::Resume => Ok(inner.player.resume()),
            V4Command::Stop => Ok(inner.player.stop()),
            V4Command::QueueInsert {
                item,
                position,
                raw,
                playback_duration: _,
            } => inner.player.queue_insert_v4(&item, position, raw),
            V4Command::QueueRemove(position) => inner.player.queue_remove_v4(position),
            V4Command::QueueSelect(position) => inner.player.queue_select_v4(position),
            V4Command::CompanionHello => {
                // Lowest free provider id, reused after disconnect — the
                // reference's allocator. The resource plane itself is #249; a
                // load naming an fcomp URL is refused typed until it lands.
                let id = (0..=u16::MAX)
                    .find(|id| !inner.providers.contains(id))
                    .unwrap_or(0);
                inner.providers.insert(id);
                if let Some(peer) = inner.peers.get_mut(&peer_id) {
                    if let PeerSession::V4(session) = &mut peer.session {
                        session.set_companion_provider(id);
                    }
                    send_frame(&peer.outbound, &v4msg::companion_hello_response_frame(id));
                }
                Ok(Applied::default())
            }
            // A session that starts one is accepted only when there is a media
            // plane to answer with; the introduction said which. Refusing here
            // rather than leaving the sender waiting for an answer SDP that will
            // never come is the whole point of the typed error.
            V4Command::StartMirroring(_) => {
                if self.mirror.is_some() {
                    Ok(Applied::default())
                } else {
                    Err(Refusal::kinded(
                        "mirroring is not offered by this receiver",
                        ErrorKind::InvalidState,
                    ))
                }
            }
            // Answering an offer is a DTLS handshake and an ICE gather — I/O, and
            // seconds of it. It cannot happen here, under the lock that every other
            // connection's protocol work also takes (ground rule 3), so the actor
            // peels this command off before it ever reaches `apply_v4`.
            V4Command::MirroringOffer { .. } => Err(Refusal::kinded(
                "a mirroring offer is answered off the lock",
                ErrorKind::Internal,
            )),
        };
        match outcome {
            Ok(Applied { events, updates }) => {
                Self::broadcast(inner, wall_ms, &updates, Some(peer_id));
                Ok(events)
            }
            Err(refusal) => {
                if let Some(peer) = inner.peers.get(&peer_id) {
                    send_refusal(peer, wall_ms, &refusal, None);
                }
                Err(refusal)
            }
        }
    }

    /// Queue one frame to a peer, if it is still connected.
    fn send_to(&self, peer_id: u64, frame: &Frame) {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(peer) = guard.peers.get(&peer_id) {
            send_frame(&peer.outbound, frame);
        }
    }

    /// Tell one peer its request was refused, in its own dialect.
    fn refuse_v4(&self, peer_id: u64, refusal: &Refusal) {
        let wall_ms = Self::wall_ms();
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(peer) = guard.peers.get(&peer_id) {
            send_refusal(peer, wall_ms, refusal, None);
        }
    }

    /// Release a disconnecting peer's companion provider id.
    fn release_provider(&self, provider: Option<u16>) {
        if let Some(id) = provider {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .providers
                .remove(&id);
        }
    }
}

/// A refusal, spoken in the asking session's dialect: `PlaybackError` text for
/// JSON senders, `Error {{ kind, packet_num }}` for v4.
fn send_refusal(peer: &Peer, wall_ms: u64, refusal: &Refusal, packet_num: Option<u32>) {
    match &peer.session {
        PeerSession::Json(session) => {
            let update = ReceiverUpdate::Error {
                message: refusal.message.clone(),
                kind: refusal.kind,
            };
            if let Some(frame) = session.frame_update(wall_ms, &update) {
                send_frame(&peer.outbound, &frame);
            }
        }
        PeerSession::V4(_) => {
            send_frame(
                &peer.outbound,
                &v4msg::error_frame(refusal.kind, packet_num),
            );
        }
    }
}

/// Translate one receiver update into v4's dialect, or `None` when v4 has no
/// message for it (media-item events; the v3 `PlayUpdate` shape).
fn v4_update_frame(update: &ReceiverUpdate) -> Option<Frame> {
    match update {
        ReceiverUpdate::Playback(snapshot) => {
            Some(v4msg::playback_state_frame(v4_state(snapshot.state)))
        }
        #[allow(clippy::cast_possible_truncation)]
        ReceiverUpdate::Volume(volume) => Some(v4msg::volume_changed_frame(*volume as f32)),
        #[allow(clippy::cast_possible_truncation)]
        ReceiverUpdate::SpeedActual(speed) => Some(v4msg::speed_changed_frame(*speed as f32)),
        // A playback failure is broadcast typed; per-command refusals go to the
        // asking peer through `send_refusal` instead and never reach here.
        ReceiverUpdate::Error { kind, .. } => Some(v4msg::error_frame(*kind, None)),
        ReceiverUpdate::PlayChanged(None) => Some(v4msg::stop_playback_frame()),
        // v4 peers hear loads through the stripped raw relay below.
        ReceiverUpdate::PlayChanged(Some(_)) | ReceiverUpdate::MediaItem { .. } => None,
        ReceiverUpdate::V4Load { raw } | ReceiverUpdate::QueueInsertRelay { raw } => {
            v4msg::stripped_relay_frame(raw)
        }
        ReceiverUpdate::QueueRemoveRelay(position) => Some(v4msg::queue_remove_frame(*position)),
        ReceiverUpdate::QueueSelectRelay { position, .. } => {
            Some(v4msg::queue_select_frame(*position))
        }
        ReceiverUpdate::Progress { position, duration } => {
            Some(v4msg::progress_frame(*position, *duration))
        }
    }
}

/// Our playback state in v4's vocabulary.
fn v4_state(state: PlayState) -> fcast_flatbuf::flat::PlaybackState {
    match state {
        PlayState::Idle => fcast_flatbuf::flat::PlaybackState::Idle,
        PlayState::Playing => fcast_flatbuf::flat::PlaybackState::Playing,
        PlayState::Paused => fcast_flatbuf::flat::PlaybackState::Paused,
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

/// A stream that replays already-read bytes before the socket — the TLS
/// ClientHello prefix that got coalesced into the same read as the plaintext
/// `Version` packet (#248). The reference's `PrefixedRead`, in miniature.
struct PrefixedStream {
    prefix: Vec<u8>,
    pos: usize,
    inner: TcpStream,
}

impl tokio::io::AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// How long the TLS handshake may take — the reference's own bound.
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(5);

/// One connection's actor. Runs the plaintext hello phase, then whichever
/// session loop the negotiation picked.
async fn serve(shared: Arc<Shared>, stream: TcpStream, peer_addr: SocketAddr, sink: SessionSink) {
    // Every connection speaks as ONE logical source. FCast senders share the
    // receiver's single session by design — the terminal sender opens a fresh TCP
    // connection per verb, and a phone may pause what another phone played — so a
    // per-connection source tag would make the session manager refuse the pause as
    // coming from a source that never played (found on the bench, not in review:
    // play landed and every later verb was silently dropped). Who a session is
    // *for* still reaches the screen, via `SourceInfo`.
    let sink = sink.with_instance("player");
    let started = tokio::time::Instant::now();
    info!(peer = %peer_addr, "fcast: sender connected");

    match plaintext_phase(&shared, stream, peer_addr).await {
        Ok(Some(Negotiated::Json {
            stream,
            first_frame,
            buffered,
        })) => {
            serve_json(
                &shared,
                stream,
                peer_addr,
                &sink,
                started,
                first_frame,
                buffered,
            )
            .await;
        }
        Ok(Some(Negotiated::V4 { stream })) => {
            serve_v4(&shared, stream, peer_addr, &sink, started).await;
        }
        Ok(None) => {}
        Err(fault) => {
            warn!(peer = %peer_addr, %fault, "fcast: connection failed before a session existed");
        }
    }
    info!(peer = %peer_addr, "fcast: sender disconnected");
}

/// What the plaintext phase concluded.
// Boxing the TLS stream would only shuffle which variant is big; this enum
// exists for exactly one call step.
#[allow(clippy::large_enum_variant)]
enum Negotiated {
    /// A v1-v3 session: the socket back, the first frame to replay through the
    /// JSON session, and any bytes read past it.
    Json {
        stream: TcpStream,
        first_frame: Frame,
        buffered: Vec<u8>,
    },
    /// A v4 session, TLS already up.
    V4 {
        stream: tokio_rustls::server::TlsStream<PrefixedStream>,
    },
}

/// Read the first frame in plaintext, greeting first — both parties owe their
/// `Version` on connect. Returns `None` on a clean pre-session EOF.
async fn plaintext_phase(
    shared: &Arc<Shared>,
    mut stream: TcpStream,
    peer_addr: SocketAddr,
) -> Result<Option<Negotiated>, FCastError> {
    let announce_v4 = shared.v4.as_ref().is_some_and(|v4| v4.announce);
    let greeting = crate::messages::json_frame(
        Opcode::Version,
        &crate::messages::VersionMessage {
            version: if announce_v4 { 4 } else { PROTOCOL_VERSION },
        },
    );
    let bytes = wire::encode(&greeting)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|e| FCastError::Tls(format!("writing the greeting: {e}")))?;

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let (first_frame, consumed) = loop {
        // The ceiling during negotiation is the JSON one; a v4 sender's first
        // frame is a tiny Version either way.
        if let Some((frame, consumed)) = wire::try_decode(&buf)? {
            break (frame, consumed);
        }
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| FCastError::Tls(format!("reading the hello: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let buffered = buf.split_off(consumed);

    // The upgrade needs both parties at 4: ours is `announce_v4`, theirs is the
    // hello we just read. A sender that said 4 but heard our 3 continues in
    // plaintext JSON (its Version left before it read ours), so sender-said-4
    // alone must not upgrade.
    let sender_v4 = first_frame.opcode == Opcode::Version
        && crate::messages::parse_body::<crate::messages::VersionMessage>(&first_frame)
            .is_ok_and(|msg| msg.version >= 4);
    if !(announce_v4 && sender_v4) {
        return Ok(Some(Negotiated::Json {
            stream,
            first_frame,
            buffered,
        }));
    }

    // In-place TLS 1.3: the bytes already read past the Version are the front
    // of the ClientHello.
    let v4 = shared
        .v4
        .as_ref()
        .expect("announce_v4 implied the identity exists");
    let prefixed = PrefixedStream {
        prefix: buffered,
        pos: 0,
        inner: stream,
    };
    let stream = tokio::time::timeout(TLS_UPGRADE_TIMEOUT, v4.identity.acceptor().accept(prefixed))
        .await
        .map_err(|_| FCastError::Tls("handshake timed out".into()))?
        .map_err(|e| FCastError::Tls(format!("handshake: {e}")))?;
    debug!(peer = %peer_addr, "fcast: v4 TLS up");
    Ok(Some(Negotiated::V4 { stream }))
}

/// The v1-v3 session loop (the original actor), with the first frame replayed.
async fn serve_json(
    shared: &Arc<Shared>,
    stream: TcpStream,
    peer_addr: SocketAddr,
    sink: &SessionSink,
    started: tokio::time::Instant,
    first_frame: Frame,
    mut buf: Vec<u8>,
) {
    let (mut reader, mut writer) = stream.into_split();
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE);

    let peer_id = {
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The greeting already went out in the plaintext phase.
        let (session, _greeting) = Session::new();
        let id = guard.next_peer;
        guard.next_peer += 1;
        guard.peers.insert(
            id,
            Peer {
                session: PeerSession::Json(session),
                outbound: outbound_tx.clone(),
                last_progress: started,
            },
        );
        id
    };

    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = outbound_rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut chunk = [0u8; 4096];
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // The frame the plaintext phase already read.
    match handle_frame(shared, peer_id, peer_addr, started.elapsed(), &first_frame) {
        Ok(events) => emit_all(sink, shared, events).await,
        Err(fault) => {
            warn!(peer = %peer_addr, %fault, "fcast: session fault; disconnecting");
            finish_peer(shared, peer_id);
            writer_task.abort();
            return;
        }
    }

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
                    match handle_frame(shared, peer_id, peer_addr, started.elapsed(), &frame) {
                        Ok(events) => emit_all(sink, shared, events).await,
                        Err(fault) => {
                            warn!(peer = %peer_addr, %fault, "fcast: session fault; disconnecting");
                            break 'conn;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(fault) = tick_peer(shared, peer_id, started.elapsed()) {
                    info!(peer = %peer_addr, %fault, "fcast: disconnecting");
                    break 'conn;
                }
            }
        }
    }

    finish_peer(shared, peer_id);
    writer_task.abort();
}

/// The v4 session loop (#248): raw-opcode decode at the 512 KiB ceiling, typed
/// error answers, and the connect sequence senders rely on — introduction,
/// volume seed, and the current single `Load` replayed stripped.
async fn serve_v4(
    shared: &Arc<Shared>,
    stream: tokio_rustls::server::TlsStream<PrefixedStream>,
    peer_addr: SocketAddr,
    sink: &SessionSink,
    started: tokio::time::Instant,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Vec<u8>>(OUTBOUND_QUEUE);

    let peer_id = {
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let id = inner.next_peer;
        inner.next_peer += 1;
        // The plaintext Version was inbound packet 0.
        inner.peers.insert(
            id,
            Peer {
                session: PeerSession::V4(SessionV4::new(1, started.elapsed())),
                outbound: outbound_tx.clone(),
                last_progress: started,
            },
        );

        // The connect sequence, in the reference's order: who we are, the
        // volume (it otherwise only broadcasts on change), and — when a v4
        // single is playing — the load and its state, so a sender joining
        // mid-session starts in sync.
        let peer = inner.peers.get(&id).expect("inserted immediately above");
        send_frame(
            &peer.outbound,
            &v4msg::receiver_introduction_frame(
                &shared.identity.display_name,
                &shared.identity.app_name,
                &shared.identity.app_version,
                &v4msg::Capabilities {
                    mirroring: shared.mirror.is_some(),
                },
            ),
        );
        #[allow(clippy::cast_possible_truncation)]
        send_frame(
            &peer.outbound,
            &v4msg::volume_changed_frame(inner.player.volume() as f32),
        );
        if let Some(raw) = inner.player.v4_single_raw() {
            if let Some(replay) = v4msg::stripped_relay_frame(raw) {
                send_frame(&peer.outbound, &replay);
                send_frame(
                    &peer.outbound,
                    &v4msg::playback_state_frame(v4_state(inner.player.snapshot(None).state)),
                );
            }
        }
        id
    };

    let writer_task = tokio::spawn(async move {
        while let Some(bytes) = outbound_rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    'conn: loop {
        tokio::select! {
            read = reader.read(&mut chunk) => {
                let n = match read {
                    Ok(0) => break 'conn,
                    Ok(n) => n,
                    Err(e) => {
                        debug!(peer = %peer_addr, error = %e, "fcast: v4 read failed");
                        break 'conn;
                    }
                };
                buf.extend_from_slice(&chunk[..n]);
                loop {
                    let raw = match wire::try_decode_raw(&buf, v4msg::MAX_PACKET_V4) {
                        Ok(Some(raw)) => raw,
                        Ok(None) => break,
                        Err(fault) => {
                            warn!(peer = %peer_addr, %fault, "fcast: v4 framing fault; disconnecting");
                            break 'conn;
                        }
                    };
                    buf.drain(..raw.consumed);
                    match handle_v4_frame(shared, peer_id, peer_addr, started.elapsed(), &raw).await {
                        Ok(events) => emit_all(sink, shared, events).await,
                        Err(fault) => {
                            warn!(peer = %peer_addr, %fault, "fcast: v4 session fault; disconnecting");
                            break 'conn;
                        }
                    }
                }
            }
            _ = ticker.tick() => {
                if let Err(fault) = tick_peer(shared, peer_id, started.elapsed()) {
                    info!(peer = %peer_addr, %fault, "fcast: disconnecting");
                    break 'conn;
                }
            }
        }
    }

    finish_peer(shared, peer_id);
    writer_task.abort();
}

/// Heartbeat one peer, whichever dialect it speaks.
fn tick_peer(shared: &Arc<Shared>, peer_id: u64, now: Duration) -> Result<(), FCastError> {
    let mut guard = shared
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(peer) = guard.peers.get_mut(&peer_id) else {
        return Err(FCastError::HeartbeatTimeout);
    };
    let ping = match &mut peer.session {
        PeerSession::Json(session) => session.on_tick(now)?,
        PeerSession::V4(session) => session.on_tick(now)?,
    };
    if let Some(ping) = ping {
        send_frame(&peer.outbound, &ping);
    }
    Ok(())
}

/// Deregister a peer, releasing whatever it held.
fn finish_peer(shared: &Arc<Shared>, peer_id: u64) {
    let provider = {
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.peers.remove(&peer_id).and_then(|peer| {
            if let PeerSession::V4(session) = &peer.session {
                session.companion_provider()
            } else {
                None
            }
        })
    };
    shared.release_provider(provider);
}

/// Feed one frame through the JSON session and the player. Pure work under the
/// lock; the returned events are emitted by the caller afterwards.
fn handle_frame(
    shared: &Arc<Shared>,
    peer_id: u64,
    peer_addr: SocketAddr,
    now: Duration,
    frame: &Frame,
) -> Result<Vec<SessionEvent>, FCastError> {
    let wall_ms = Shared::wall_ms();
    let (reaction, sender_identity) = {
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
        let PeerSession::Json(session) = &mut peer.session else {
            return Ok(Vec::new());
        };
        let ctx = SessionContext {
            wall_ms,
            receiver: &shared.identity,
            play_data: play_data.as_ref(),
            volume,
        };
        let reaction = session.on_frame(now, &ctx, frame)?;
        for reply in &reaction.replies {
            send_frame(&peer.outbound, reply);
        }
        let identity = session.peer().cloned();
        (reaction, identity)
    };

    let Some(command) = reaction.command else {
        return Ok(Vec::new());
    };
    debug!(?command, "fcast: sender command");
    let was_load = matches!(command, SenderCommand::Load(_));
    match shared.apply(Some(peer_id), command) {
        Ok(mut events) => {
            if was_load {
                events.push(SessionEvent::SourceInfo(source_description(
                    peer_addr,
                    sender_identity
                        .as_ref()
                        .and_then(|i| i.display_name.clone().or_else(|| i.app_name.clone())),
                )));
            }
            Ok(events)
        }
        Err(refusal) => {
            // The refusal already went back to the asking sender; the connection
            // stays up and whatever was playing keeps playing.
            info!(reason = %refusal.message, "fcast: request refused");
            Ok(Vec::new())
        }
    }
}

/// Feed one raw frame through a v4 session (#248).
///
/// Async only for the mirroring offer, which is a DTLS handshake and an ICE gather and
/// therefore cannot be answered under the lock the rest of the protocol work takes. Every
/// other command is still the pure step it was.
async fn handle_v4_frame(
    shared: &Arc<Shared>,
    peer_id: u64,
    peer_addr: SocketAddr,
    now: Duration,
    raw: &wire::RawFrame,
) -> Result<Vec<SessionEvent>, FCastError> {
    let (reaction, sender_identity) = {
        let mut guard = shared
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(peer) = guard.peers.get_mut(&peer_id) else {
            return Ok(Vec::new());
        };
        let PeerSession::V4(session) = &mut peer.session else {
            return Ok(Vec::new());
        };
        let reaction: V4Reaction = session.on_frame(now, raw)?;
        for reply in &reaction.replies {
            send_frame(&peer.outbound, reply);
        }
        let identity = session.peer().cloned();
        (reaction, identity)
    };

    let Some(command) = reaction.command else {
        return Ok(Vec::new());
    };
    debug!(?command, "fcast: v4 sender command");
    if let V4Command::MirroringOffer { session_id, sdp } = &command {
        return Ok(answer_mirroring(shared, peer_id, *session_id, sdp).await);
    }
    let was_load = matches!(command, V4Command::Load { .. });
    match shared.apply_v4(peer_id, command) {
        Ok(mut events) => {
            if was_load {
                events.push(SessionEvent::SourceInfo(source_description(
                    peer_addr,
                    sender_identity
                        .as_ref()
                        .and_then(|i| i.display_name.clone().or_else(|| i.app_name.clone())),
                )));
            }
            Ok(events)
        }
        Err(refusal) => {
            info!(reason = %refusal.message, "fcast: v4 request refused");
            Ok(Vec::new())
        }
    }
}

/// Answer a sender's mirroring offer and take the screen (#248).
///
/// Off the lock, because building the peer connection is I/O: a DTLS handshake and an ICE
/// gather, seconds of it in the bad case. Holding the player lock across that would stall
/// every other sender's `pause`.
///
/// The answer goes back on the connection that asked, and the frames go to the session
/// manager as a `Mirror` — which is what preempts whatever was playing. A backend that
/// refuses is a typed error to the sender rather than silence: it has already told
/// somebody it is casting, and a sender waiting for an answer SDP that never comes has
/// nothing to show and nothing to say.
async fn answer_mirroring(
    shared: &Arc<Shared>,
    peer_id: u64,
    session_id: u16,
    offer: &str,
) -> Vec<SessionEvent> {
    use fcast_flatbuf::flat::ErrorKind;
    let Some(backend) = shared.mirror.clone() else {
        shared.refuse_v4(
            peer_id,
            &Refusal::kinded(
                "mirroring is not offered by this receiver",
                ErrorKind::InvalidState,
            ),
        );
        return Vec::new();
    };
    match backend.answer(offer).await {
        Ok(answer) => {
            info!(session_id, "fcast: mirroring session answered");
            shared.send_to(
                peer_id,
                &v4msg::mirroring_answer_frame(session_id, &answer.sdp),
            );
            vec![SessionEvent::Mirror {
                video: answer.video,
                audio: answer.audio,
            }]
        }
        Err(e) => {
            warn!(session_id, error = %e, "fcast: could not answer the mirroring offer");
            shared.refuse_v4(
                peer_id,
                &Refusal::kinded(format!("mirroring: {e}"), ErrorKind::Internal),
            );
            Vec::new()
        }
    }
}

fn source_description(
    peer_addr: SocketAddr,
    name: Option<String>,
) -> castaway_core::SourceDescription {
    let mut description =
        castaway_core::SourceDescription::new().with_address(peer_addr.to_string());
    if let Some(name) = name {
        description = description.with_display_name(name);
    }
    description
}

/// The progress broadcast, shared by every connection: reads the pipeline clock
/// once per tick and fans position out — at [`PROGRESS_INTERVAL`] to JSON
/// sessions, and at each v4 session's own negotiated cadence (default 500 ms,
/// `SetProgressUpdateInterval` to change it).
async fn progress_ticker(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = tokio::time::Instant::now();
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
        let update = ReceiverUpdate::Playback(snapshot);
        for peer in inner.peers.values_mut() {
            let (due, frame) = match &peer.session {
                PeerSession::Json(session) => (
                    now.duration_since(peer.last_progress) >= PROGRESS_INTERVAL,
                    session.frame_update(wall_ms, &update),
                ),
                PeerSession::V4(session) => (
                    now.duration_since(peer.last_progress) >= session.progress_interval(),
                    snapshot.time.map(|secs| {
                        v4msg::progress_frame(
                            Duration::from_secs_f64(secs.max(0.0)),
                            snapshot.duration.map(Duration::from_secs_f64),
                        )
                    }),
                ),
            };
            if !due {
                continue;
            }
            if let Some(frame) = frame {
                peer.last_progress = now;
                send_frame(&peer.outbound, &frame);
            }
        }
    }
}

#[async_trait::async_trait]
impl SourceAdapter for FCastReceiver {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::FCast
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        // The version, stated rather than implied (#241's scope note): protocol
        // v4 defines `v` as "highest supported protocol version". `fp` rides
        // along exactly when the hello will say 4 — the pair is atomic, because
        // the sender SDK quits on either half alone (`fp` present + a v3 answer
        // is its insecure-downgrade refusal; a v4 answer with no `fp` gives it
        // nothing to pin and it never sends the ClientHello).
        let announced = self.shared.v4.as_ref().filter(|v4| v4.announce);
        let mut txt = vec![(
            "v".to_string(),
            announced.map_or(PROTOCOL_VERSION, |_| 4).to_string(),
        )];
        if let Some(v4) = announced {
            txt.push(("fp".to_string(), v4.identity.fingerprint().to_string()));
        }
        vec![Advertisement::MdnsService {
            ty: FCAST_SERVICE_TYPE.to_string(),
            instance: self.shared.identity.display_name.clone(),
            port: self.port(),
            txt,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::identity::V4Identity;

    /// The QR is a promise, and it is only made when it can be kept.
    ///
    /// The connection document says `v=4` unconditionally, and a sender that reads one
    /// refuses to fall back to plaintext. With `announce = false` the receiver still says
    /// `Version {3}` and greets in JSON, so a QR drawn from it would send a sender into a
    /// session it has just promised not to accept. Both halves of #248's coupled switch
    /// are therefore one switch here as well.
    #[test]
    fn the_connection_url_follows_the_announce_switch() {
        let addresses = vec!["10.0.0.5".to_string()];

        // No identity at all: nothing to pin, so the QR would carry nothing mDNS does not.
        let plain = FCastReceiver::new("Panel");
        assert_eq!(plain.connection_url(addresses.clone()), None);

        let (identity, _) = V4Identity::generate().unwrap();
        let carried = FCastReceiver::new("Panel").with_v4(identity, false);
        assert_eq!(
            carried.connection_url(addresses.clone()),
            None,
            "the identity is carried but v4 never engages"
        );

        let (identity, _) = V4Identity::generate().unwrap();
        let fingerprint = identity.fingerprint().to_string();
        let announced = FCastReceiver::new("Panel").with_v4(identity, true);
        let url = announced.connection_url(addresses).expect("a QR payload");
        assert!(url.starts_with("fcast://r/"));
        let json = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE,
            url.strip_prefix("fcast://r/").unwrap(),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["name"], "Panel");
        assert_eq!(value["addresses"][0], "10.0.0.5");
        assert_eq!(value["services"][0]["port"], u64::from(FCAST_PORT));
        assert_eq!(
            value["txt"]["fp"], fingerprint,
            "the QR pins the key that actually answers"
        );
    }
}
