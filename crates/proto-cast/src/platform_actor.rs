//! The receiver-platform socket actor: the thin async shell around
//! [`crate::platform::PlatformSession`]. It owns the WebSocket the hosted page dials and
//! makes no protocol decisions of its own (ground rule 3).
//!
//! ## Why this listener is loopback-only, and why that is not a detail
//!
//! The platform channel is the *inside* of the receiver. Anything that can open it can
//! impersonate the device to the application: set its volume, claim a sender connected,
//! feed it messages as though they came from a phone. The shared HTTP host on 8080 is
//! bound `0.0.0.0` because DIAL and DLNA have to be reachable from the LAN; this must be
//! the opposite, so it gets its own listener on `127.0.0.1` rather than a route on that
//! one. The page reaching it is in our own browser process on the same host.
//!
//! ## One page at a time
//!
//! There is one panel and one browser, so there is one hosted application. A second
//! connection replaces the first — which is what a `LAUNCH` arriving while another app
//! runs actually means — rather than being multiplexed, and the displaced page is told
//! nothing because it is already being navigated away from.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use crate::error::CastError;
use crate::platform::{
    AppIdentity, DeviceCapabilities, DisconnectReason, IpcFrame, PageReady, PlatformEvent,
    PlatformSession, DEFAULT_PLATFORM_PORT, IPC_PATH,
};

/// How long to wait for a page to dial in after a `LAUNCH`.
///
/// Generous, because it covers a cold browser navigating to a third-party receiver over
/// whatever the hackerspace uplink is doing. What it protects against is the case where
/// the page never comes at all: without a bound, a sender sits on a `LAUNCH` forever and
/// the panel shows a blank browser with no way to know it failed.
pub const PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// What the Cast session asks the platform to do.
#[derive(Debug)]
enum HostCommand {
    Start {
        app: Box<AppIdentity>,
        volume: (f32, bool),
        events: mpsc::Sender<HostEvent>,
        reply: oneshot::Sender<()>,
    },
    Stop,
    SenderConnected {
        id: String,
        user_agent: String,
    },
    SenderDisconnected {
        id: String,
        reason: DisconnectReason,
    },
    VolumeChanged {
        level: f32,
        muted: bool,
    },
    VisibilityChanged {
        visible: bool,
    },
    /// A sender's message on an application namespace, for the page.
    ToPage {
        namespace: String,
        sender_id: String,
        data: String,
    },
}

/// What the platform tells the Cast session.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HostEvent {
    /// The page identified itself and declared its namespaces.
    Ready(Box<PageReady>),
    /// The application is talking to a sender. Routed back out over CASTv2 verbatim —
    /// this is the payload of #16's "forward app-namespace messages *opaquely*".
    ToSender {
        /// The namespace it belongs to.
        namespace: String,
        /// Which sender it is for. `*` addresses every sender on the connection, which
        /// is how a receiver broadcasts its status.
        sender_id: String,
        /// The payload, untouched.
        data: String,
    },
    /// The application asked the device for something.
    Platform(PlatformEvent),
    /// The page's socket closed. The application is gone whatever the browser shows.
    PageGone,
}

/// A handle on the running platform server.
///
/// Cloneable and cheap: every CASTv2 connection task holds one, because any of them may
/// be the one that launches an app.
#[derive(Clone)]
pub struct PlatformHost {
    tx: mpsc::Sender<HostCommand>,
    port: u16,
}

impl std::fmt::Debug for PlatformHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformHost")
            .field("port", &self.port)
            .finish()
    }
}

impl PlatformHost {
    /// The port the page must be pointed at.
    ///
    /// Handed to the browser as `__platform__.queryPlatformValue("port-for-web-server")`
    /// so the two cannot disagree — the SDK falls back to a hardcoded 8008 when no
    /// `__platform__` is injected, and a platform on a different port with no shim is a
    /// page that dials nothing and an app that never starts.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Begin hosting `app`: the next page to dial in is this application.
    ///
    /// Returns once the server has the session, not once the page has arrived — the page
    /// cannot arrive until somebody navigates the browser, which is the caller's next
    /// move.
    ///
    /// # Errors
    /// [`CastError::Io`] if the platform server has stopped.
    pub async fn start(
        &self,
        app: AppIdentity,
        volume: (f32, bool),
        events: mpsc::Sender<HostEvent>,
    ) -> Result<(), CastError> {
        let (reply, wait) = oneshot::channel();
        self.send(HostCommand::Start {
            app: Box::new(app),
            volume,
            events,
            reply,
        })
        .await?;
        wait.await
            .map_err(|_| CastError::Io("the platform server dropped a start".into()))
    }

    /// Stop hosting. Idempotent.
    pub async fn stop(&self) {
        let _ = self.send(HostCommand::Stop).await;
    }

    /// Tell the application a sender connected.
    pub async fn sender_connected(&self, id: &str, user_agent: &str) {
        let _ = self
            .send(HostCommand::SenderConnected {
                id: id.to_owned(),
                user_agent: user_agent.to_owned(),
            })
            .await;
    }

    /// Tell the application a sender went away.
    pub async fn sender_disconnected(&self, id: &str, reason: DisconnectReason) {
        let _ = self
            .send(HostCommand::SenderDisconnected {
                id: id.to_owned(),
                reason,
            })
            .await;
    }

    /// Tell the application the device volume moved.
    pub async fn volume_changed(&self, level: f32, muted: bool) {
        let _ = self.send(HostCommand::VolumeChanged { level, muted }).await;
    }

    /// Tell the application whether it is on screen.
    pub async fn visibility_changed(&self, visible: bool) {
        let _ = self.send(HostCommand::VisibilityChanged { visible }).await;
    }

    /// Relay a sender's message on an application namespace to the page.
    pub async fn to_page(&self, namespace: &str, sender_id: &str, data: &str) {
        let _ = self
            .send(HostCommand::ToPage {
                namespace: namespace.to_owned(),
                sender_id: sender_id.to_owned(),
                data: data.to_owned(),
            })
            .await;
    }

    async fn send(&self, command: HostCommand) -> Result<(), CastError> {
        self.tx
            .send(command)
            .await
            .map_err(|_| CastError::Io("the platform server has stopped".into()))
    }
}

/// The platform server: a loopback listener plus the task that owns the session.
pub struct PlatformServer {
    listen: SocketAddr,
    capabilities: DeviceCapabilities,
}

impl PlatformServer {
    /// A server on the SDK's default port, loopback only.
    #[must_use]
    pub fn new(capabilities: DeviceCapabilities) -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PLATFORM_PORT),
            capabilities,
        }
    }

    /// Serve on `port` instead. `0` takes whatever the OS gives, which is what tests use.
    #[must_use]
    pub const fn with_port(mut self, port: u16) -> Self {
        self.listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        self
    }

    /// Bind, and return the handle plus the task that serves it.
    ///
    /// Split rather than spawning internally so the caller owns the task's lifetime and
    /// the port is known before anything is spawned — the browser has to be told the
    /// port, and a `0` bind only resolves it here.
    ///
    /// # Errors
    /// [`CastError::Io`] if the loopback port cannot be bound. That is fatal to app
    /// hosting and to nothing else: media-URL casting and mirroring do not use it.
    pub async fn bind(
        self,
    ) -> Result<(PlatformHost, impl std::future::Future<Output = ()> + Send), CastError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "registered: the cast-platform/tcp loopback entry in crates/app/src/surface.rs"
        )]
        let listener = tokio::net::TcpListener::bind(self.listen)
            .await
            .map_err(|e| {
                CastError::Io(format!("binding the Cast platform on {}: {e}", self.listen))
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| CastError::Io(e.to_string()))?
            .port();

        let (tx, rx) = mpsc::channel(64);
        let state = Arc::new(ServerState {
            session: Mutex::new(None),
            capabilities: self.capabilities,
        });

        let router = axum::Router::new()
            .route(IPC_PATH, get(upgrade))
            .with_state(Arc::clone(&state));

        let serving = async move {
            info!(
                port,
                path = IPC_PATH,
                "Cast receiver platform listening on loopback"
            );
            if let Err(e) = axum::serve(listener, router).await {
                warn!(error = %e, "the Cast platform listener stopped");
            }
        };
        let commands = drive(state, rx);

        let task = async move {
            tokio::join!(serving, commands);
        };
        Ok((PlatformHost { tx, port }, task))
    }
}

/// What a connected page is attached to.
struct Live {
    session: PlatformSession,
    /// Frames to write to the page. `None` until the socket connects.
    to_page: Option<mpsc::Sender<IpcFrame>>,
    events: mpsc::Sender<HostEvent>,
}

struct ServerState {
    session: Mutex<Option<Live>>,
    capabilities: DeviceCapabilities,
}

/// Fold host commands into the live session.
async fn drive(state: Arc<ServerState>, mut rx: mpsc::Receiver<HostCommand>) {
    while let Some(command) = rx.recv().await {
        match command {
            HostCommand::Start {
                app,
                volume,
                events,
                reply,
            } => {
                let session =
                    PlatformSession::new(*app, state.capabilities).with_volume(volume.0, volume.1);
                info!(
                    app_id = %session.app().application_id,
                    name = %session.app().application_name,
                    "cast platform: hosting an application; waiting for its page"
                );
                *state.session.lock().await = Some(Live {
                    session,
                    to_page: None,
                    events,
                });
                let _ = reply.send(());
            }
            HostCommand::Stop => {
                if state.session.lock().await.take().is_some() {
                    debug!("cast platform: stopped hosting");
                }
            }
            other => apply(&state, other).await,
        }
    }
}

/// Apply one command that needs a live session, writing whatever it produces.
async fn apply(state: &Arc<ServerState>, command: HostCommand) {
    let mut guard = state.session.lock().await;
    let Some(live) = guard.as_mut() else {
        return;
    };
    let frames = match command {
        HostCommand::SenderConnected { id, user_agent } => {
            live.session.sender_connected(&id, &user_agent).to_page
        }
        HostCommand::SenderDisconnected { id, reason } => {
            live.session.sender_disconnected(&id, reason).to_page
        }
        HostCommand::VolumeChanged { level, muted } => {
            live.session.volume_changed(level, muted).to_page
        }
        HostCommand::VisibilityChanged { visible } => {
            live.session.visibility_changed(visible).to_page
        }
        HostCommand::ToPage {
            namespace,
            sender_id,
            data,
        } => match live.session.relay_to_page(&namespace, &sender_id, &data) {
            Some(frame) => vec![frame],
            None => {
                // Not a mistake worth an error, but worth saying: a sender is talking on
                // a namespace the running application never claimed, which from the room
                // looks like a cast that connects and does nothing.
                debug!(
                    %namespace,
                    ready = live.session.is_ready(),
                    "cast platform: nothing to relay a sender's message to"
                );
                Vec::new()
            }
        },
        HostCommand::Start { .. } | HostCommand::Stop => Vec::new(),
    };
    write_all(live, frames).await;
}

/// Write frames to the page, if it is there to receive them.
async fn write_all(live: &Live, frames: Vec<IpcFrame>) {
    let Some(to_page) = live.to_page.as_ref() else {
        return;
    };
    for frame in frames {
        if to_page.send(frame).await.is_err() {
            debug!("cast platform: the page went away mid-write");
            return;
        }
    }
}

/// The HTTP upgrade the SDK performs on `/v2/ipc`.
async fn upgrade(State(state): State<Arc<ServerState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| serve_page(state, socket))
}

/// Serve one page connection to completion.
async fn serve_page(state: Arc<ServerState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (to_page, mut outgoing) = mpsc::channel::<IpcFrame>(64);

    {
        let mut guard = state.session.lock().await;
        let Some(live) = guard.as_mut() else {
            // A page dialled in with nothing being hosted. That is a stale browser tab
            // from an application that already stopped, and it must not be able to
            // start one: the session is created by a `LAUNCH`, never by a connection.
            warn!("cast platform: a page connected with no application being hosted; closing");
            drop(guard);
            // Closed properly rather than dropped. A page whose socket is reset retries;
            // one that is told the conversation is over stops.
            let _ = sink.send(Message::Close(None)).await;
            return;
        };
        live.to_page = Some(to_page);
    }
    info!("cast platform: the receiver page connected");

    // The writer half. Split off so a slow page cannot block the command path — the
    // Cast connection that feeds it is answering a sender that is also waiting.
    let writer = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            let text = match frame.encode() {
                Ok(text) => text,
                Err(e) => {
                    warn!(error = %e, "cast platform: unencodable frame");
                    continue;
                }
            };
            tracing::trace!(%text, "cast platform: frame out");
            if sink.send(Message::text(text)).await.is_err() {
                return;
            }
        }
    });

    while let Some(message) = stream.next().await {
        let text = match message {
            Ok(Message::Text(text)) => text,
            // The SDK sends text and nothing else. Pings are answered by axum.
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        tracing::trace!(%text, "cast platform: frame in");
        if let Err(e) = handle_from_page(&state, &text).await {
            warn!(error = %e, "cast platform: a frame from the page was refused");
        }
    }

    writer.abort();
    let mut guard = state.session.lock().await;
    if let Some(live) = guard.as_mut() {
        live.to_page = None;
        let _ = live.events.send(HostEvent::PageGone).await;
    }
    info!("cast platform: the receiver page disconnected");
}

/// Fold one inbound frame and dispatch whatever it produced.
async fn handle_from_page(state: &Arc<ServerState>, text: &str) -> Result<(), CastError> {
    let frame = IpcFrame::decode(text)?;
    let mut guard = state.session.lock().await;
    let Some(live) = guard.as_mut() else {
        return Ok(());
    };

    if !frame.is_system() {
        // The application is answering a sender. Straight back out over CASTv2, with
        // nothing here reading it.
        let _ = live
            .events
            .send(HostEvent::ToSender {
                namespace: frame.namespace,
                sender_id: frame.sender_id,
                data: frame.data,
            })
            .await;
        return Ok(());
    }

    let reaction = live.session.from_page(&frame)?;
    write_all(live, reaction.to_page).await;
    for event in reaction.events {
        let out = match event {
            PlatformEvent::AppReady(ready) => HostEvent::Ready(ready),
            other => HostEvent::Platform(other),
        };
        let _ = live.events.send(out).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tokio_tungstenite::tungstenite;

    fn app() -> AppIdentity {
        AppIdentity {
            application_id: "4F8B3483".into(),
            application_name: "CastVideos".into(),
            session_id: "sess-7".into(),
            launching_sender_id: "sender-0".into(),
            icon_url: None,
        }
    }

    /// A page's side of the conversation, over a real WebSocket.
    struct Page {
        socket: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    }

    impl Page {
        async fn connect(port: u16) -> Self {
            let url = format!("ws://127.0.0.1:{port}{IPC_PATH}");
            let (socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            Self { socket }
        }

        async fn send(&mut self, frame: &IpcFrame) {
            self.socket
                .send(tungstenite::Message::text(frame.encode().unwrap()))
                .await
                .unwrap();
        }

        /// The next frame, or a panic after a bounded wait — a hang here would be a
        /// test that never finishes rather than one that fails.
        async fn next(&mut self) -> IpcFrame {
            let message = tokio::time::timeout(Duration::from_secs(5), self.socket.next())
                .await
                .expect("the platform sent nothing")
                .expect("the socket closed")
                .unwrap();
            IpcFrame::decode(message.to_text().unwrap()).unwrap()
        }

        async fn next_system(&mut self) -> serde_json::Value {
            let frame = self.next().await;
            assert!(frame.is_system(), "{frame:?}");
            serde_json::from_str(&frame.data).unwrap()
        }
    }

    const SDK_READY: &str = r#"{"type":"ready","activeNamespaces":["urn:x-cast:com.google.cast.media"],"version":"2.0.0.0157","messagesVersion":"1.0"}"#;

    async fn hosted() -> (PlatformHost, mpsc::Receiver<HostEvent>, Page) {
        let (host, task) = PlatformServer::new(DeviceCapabilities::default())
            .with_port(0)
            .bind()
            .await
            .unwrap();
        tokio::spawn(task);
        let (events_tx, events) = mpsc::channel(32);
        host.start(app(), (0.5, false), events_tx).await.unwrap();
        let page = Page::connect(host.port()).await;
        (host, events, page)
    }

    #[tokio::test]
    async fn a_page_that_says_ready_is_answered_with_its_session() {
        let (_host, mut events, mut page) = hosted().await;
        page.send(&IpcFrame {
            namespace: crate::platform::SYSTEM_NS.into(),
            sender_id: crate::platform::SYSTEM_SENDER.into(),
            data: SDK_READY.into(),
        })
        .await;

        let ready = page.next_system().await;
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["applicationId"], "4F8B3483");
        assert_eq!(ready["sessionId"], "sess-7");

        // And the receiver learns the app is up, with what it declared.
        match events.recv().await.unwrap() {
            HostEvent::Ready(page_ready) => {
                assert_eq!(
                    page_ready.active_namespaces,
                    ["urn:x-cast:com.google.cast.media"]
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// The round trip #16 is actually about: a sender's message goes in one side and
    /// comes out at the page, and the page's answer comes back out at the sender —
    /// with nothing in between parsing either.
    #[tokio::test]
    async fn a_senders_message_reaches_the_page_and_the_answer_comes_back() {
        let (host, mut events, mut page) = hosted().await;
        page.send(&IpcFrame {
            namespace: crate::platform::SYSTEM_NS.into(),
            sender_id: crate::platform::SYSTEM_SENDER.into(),
            data: SDK_READY.into(),
        })
        .await;
        let _ready = page.next_system().await;
        let _volume = page.next_system().await;
        let _ = events.recv().await;

        host.to_page(
            crate::messages::ns::MEDIA,
            "sender-42",
            r#"{"type":"LOAD","media":{"contentId":"http://x/v.mp4"}}"#,
        )
        .await;
        let relayed = page.next().await;
        assert_eq!(relayed.namespace, crate::messages::ns::MEDIA);
        assert_eq!(relayed.sender_id, "sender-42");
        assert!(relayed.data.contains("v.mp4"));

        page.send(&IpcFrame::app(
            crate::messages::ns::MEDIA,
            "sender-42",
            r#"{"type":"MEDIA_STATUS","status":[]}"#,
        ))
        .await;
        match events.recv().await.unwrap() {
            HostEvent::ToSender {
                namespace,
                sender_id,
                data,
            } => {
                assert_eq!(namespace, crate::messages::ns::MEDIA);
                assert_eq!(sender_id, "sender-42");
                assert_eq!(data, r#"{"type":"MEDIA_STATUS","status":[]}"#);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_app_can_move_the_device_volume() {
        let (_host, mut events, mut page) = hosted().await;
        page.send(&IpcFrame {
            namespace: crate::platform::SYSTEM_NS.into(),
            sender_id: crate::platform::SYSTEM_SENDER.into(),
            data: SDK_READY.into(),
        })
        .await;
        let _ = page.next_system().await;
        let _ = page.next_system().await;
        let _ = events.recv().await;

        page.send(&IpcFrame {
            namespace: crate::platform::SYSTEM_NS.into(),
            sender_id: crate::platform::SYSTEM_SENDER.into(),
            data: r#"{"type":"setvolume","level":0.42}"#.into(),
        })
        .await;
        match events.recv().await.unwrap() {
            HostEvent::Platform(PlatformEvent::SetVolume(level)) => {
                assert!((level - 0.42).abs() < 1e-6);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A stale tab must not be able to start an application. The session is created by
    /// a `LAUNCH` and by nothing else.
    /// A stale tab is told the conversation is over, rather than having its socket
    /// reset: a page that sees a reset retries, and one that is closed stops.
    #[tokio::test]
    async fn a_page_that_connects_with_nothing_hosted_is_closed() {
        let (host, task) = PlatformServer::new(DeviceCapabilities::default())
            .with_port(0)
            .bind()
            .await
            .unwrap();
        tokio::spawn(task);

        let mut page = Page::connect(host.port()).await;
        let closed = tokio::time::timeout(Duration::from_secs(5), page.socket.next()).await;
        match closed {
            Ok(Some(Ok(tungstenite::Message::Close(_)))) => {}
            other => panic!("the page should have been told the socket is closing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_page_going_away_is_reported() {
        let (_host, mut events, mut page) = hosted().await;
        page.send(&IpcFrame {
            namespace: crate::platform::SYSTEM_NS.into(),
            sender_id: crate::platform::SYSTEM_SENDER.into(),
            data: SDK_READY.into(),
        })
        .await;
        let _ = events.recv().await;
        page.socket.close(None).await.unwrap();

        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await {
                    Some(HostEvent::PageGone) => return true,
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await
        .unwrap();
        assert!(gone);
    }

    /// The listener is on loopback. Reachable from the LAN it would let anything on the
    /// network drive a hosted application as though it were the device.
    #[tokio::test]
    async fn the_platform_is_not_reachable_from_anywhere_but_this_host() {
        let (host, task) = PlatformServer::new(DeviceCapabilities::default())
            .with_port(0)
            .bind()
            .await
            .unwrap();
        tokio::spawn(task);

        // A non-loopback address on this machine must not have the port open. Asking
        // the routing table for one is what makes this a real check rather than a
        // restatement of the bind argument.
        let external = local_non_loopback_addr();
        let Some(addr) = external else {
            // Nothing but loopback on this machine; the claim is vacuously true and
            // there is nothing to test against.
            return;
        };
        let attempt = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(SocketAddr::new(addr, host.port())),
        )
        .await;
        assert!(
            !matches!(attempt, Ok(Ok(_))),
            "the platform accepted a connection on {addr}, which is not loopback"
        );
    }

    /// Any address on this host that is not loopback, if there is one.
    fn local_non_loopback_addr() -> Option<IpAddr> {
        #[expect(
            clippy::disallowed_methods,
            reason = "a test asking the routing table which address it has; not a listener"
        )]
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        // No packet is sent; connect on UDP only picks a route and a source address.
        socket.connect("192.0.2.1:9").ok()?;
        let addr = socket.local_addr().ok()?.ip();
        (!addr.is_loopback() && !addr.is_unspecified()).then_some(addr)
    }
}
