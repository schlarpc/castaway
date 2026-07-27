//! The castaway binary. Composes the enabled protocol adapters into one session
//! manager driving one pipeline, behind one shared HTTP host, one SSDP responder, and
//! one mDNS responder. This is the only crate that uses `anyhow` (ground rule 7).
//!
//! Two run modes:
//! - default: [`pipeline::NullPipeline`] (logs + drains) — proves the whole stack with
//!   no GPU/codec, and stays on a plain tokio runtime.
//! - `render` feature: [`pipeline::RenderPipeline`] (ffmpeg decode → wgpu compositor) with
//!   the winit fullscreen kiosk on the **main thread**; tokio runs the servers on a
//!   spawned runtime (the three-thread model, architecture §6).

mod bluetooth;
mod config;
mod screen;
// Reading the screen is pure and always compiled; the actor that drives it needs a page
// to drive, so it exists only in the browser build (D27).
mod sponsorblock;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use castaway_core::{
    osd_channel, Advertisement, DisplayControl, ProtocolKind, SessionConfig, SessionManager,
    SessionSink, SourceAdapter, SourceId, SourceMessage,
};
use control_display::NullDisplay;
use crypto_cast_auth::CastDeviceSigner;
use proto_airplay::AirPlayReceiver;
use proto_cast::{CastReceiver, TlsIdentity};
use proto_dial::DialService;
use proto_dlna::DlnaService;
use proto_spotify::SpotifyService;
use substrate_mdns::MdnsResponder;
use substrate_ssdp::{Responder, ResponderConfig, SsdpDevice};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::config::Config;

/// The mDNS host label every advertisement resolves to (`castaway.local.`). One name for
/// the box, however many services it publishes.
const MDNS_HOST: &str = "castaway";

fn main() -> anyhow::Result<()> {
    // Tracing first, and *before* CEF bootstrap on purpose. A CEF subprocess re-execs this
    // binary and then spends its entire life inside `execute_process` — so a subscriber
    // installed after bootstrap is installed only in the browser process, and everything
    // the render process logs (scriptlet injection, list reloads) goes nowhere. That is
    // not a small gap: injection *only* happens over there.
    //
    // The ordering rule this bends is real but narrower than it looked: CEF wants
    // `execute_process` early so a subprocess does no needless work and sees the original
    // argv. Installing a subscriber touches neither — no argv, no env, no threads — and it
    // is the whole reason the renderer is observable at all.
    init_tracing();

    // CEF is multi-process: subprocesses re-exec this same binary, so bootstrap comes
    // before config and the tokio runtime. `None` means this invocation *was* a subprocess
    // and has already run to completion.
    #[cfg(feature = "cef")]
    let cef = match pipeline::Cef::bootstrap().map_err(|e| anyhow::anyhow!("cef bootstrap: {e}"))? {
        Some(cef) => cef,
        None => return Ok(()),
    };

    let config = Config::from_env().context("loading config")?;
    // A category name that is not one of SponsorBlock's parses to "unknown" rather than
    // failing, which for a *response* is the point and for *config* is a silent typo.
    if config.unknown_sponsorblock_categories() > 0 {
        warn!(
            count = config.unknown_sponsorblock_categories(),
            "castaway.toml lists SponsorBlock categories this build does not know; \
             they will never be skipped. Valid: sponsor, selfpromo, interaction, intro, \
             outro, preview, music_offtopic, filler, exclusive_access"
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let (event_tx, event_rx) = mpsc::channel::<SourceMessage>(64);
    let shutdown = Arc::new(Notify::new());
    // A fullscreen kiosk has no window chrome, so ctrl-c must also stop the winit loop;
    // it polls this flag every iteration.
    let kiosk_exit = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // The OSD channel: the session manager posts "Now casting from …", and any other
    // source (here, the app itself) can post to the same overlay by cloning `osd`.
    let (osd, osd_rx) = osd_channel();

    // A welcome banner, injected by the app (a non-session OSD producer) — demonstrates
    // that OSD messages can come from anywhere holding an `OsdSink`.
    osd.banner(
        format!("{} ready", config.friendly_name),
        Duration::from_secs(6),
    );

    // The browser lives on the main thread and DIAL lives on the runtime, so a page that
    // has given up tells DIAL over a channel rather than holding it. Only the `cef` build
    // has a browser to give up.
    #[cfg_attr(not(feature = "cef"), allow(unused_variables))]
    let (abandoned_tx, abandoned_rx) = mpsc::unbounded_channel::<()>();

    #[cfg(feature = "render")]
    {
        use pipeline::{OsdController, RenderPipeline};
        let (render_pipeline, rx) = RenderPipeline::new(3);
        let shot_handle = render_pipeline.screenshot_handle();

        // DIAL launch → navigate the main-thread CEF browser to YouTube leanback with
        // the sender's pairing params, so the phone binds to this screen; DIAL stop →
        // hide it. Without CEF there is no launch target, and DIAL goes unadvertised.
        #[cfg(feature = "cef")]
        let (nav_tx, nav_rx) = std::sync::mpsc::channel::<pipeline::BrowserCommand>();

        // Said once, at startup, because the alternative is discovering it from a
        // leanback console line while standing in front of the panel. nixpkgs' CDM is
        // Linux-only (`meta.platforms`), so the Windows deploy artifact reaches here —
        // and the failure it produces is a video that silently does not start.
        #[cfg(feature = "cef")]
        if !pipeline::cef_browser::has_widevine() {
            warn!(
                "no Widevine CDM in this build: DRM-protected video (rentals, some \
                 higher-tier streams) will not play. Everything else is unaffected."
            );
        }

        // Whoever casts next gets the panel. Nothing but DIAL `DELETE` used to dismiss
        // the leanback page, and nothing sends `DELETE` (D28) — so the first YouTube cast
        // owned the screen for the rest of the process, with later DLNA/Cast video
        // decoding underneath it and Spotify playing under YouTube's own audio.
        #[cfg(feature = "cef")]
        {
            let release_tx = nav_tx.clone();
            render_pipeline.set_screen_release(Arc::new(move || {
                let _ = release_tx.send(pipeline::BrowserCommand::Hide);
            }));
        }

        let display: Box<dyn DisplayControl> = Box::new(NullDisplay);
        let manager = SessionManager::new(render_pipeline, Some(display), SessionConfig::default())
            .with_osd(osd.clone());
        let remote = manager.remote_handle();
        runtime.spawn(manager.run(event_rx));

        #[cfg(feature = "cef")]
        let on_dial = move |event: proto_dial::DialEvent| match event {
            proto_dial::DialEvent::Launched(params) => {
                let url = params.leanback_url();
                info!(%url, "DIAL launch: navigating kiosk browser");
                let _ = nav_tx.send(pipeline::BrowserCommand::Navigate(url));
            }
            proto_dial::DialEvent::Stopped => {
                info!("DIAL stop: hiding kiosk browser");
                let _ = nav_tx.send(pipeline::BrowserCommand::Hide);
            }
        };
        #[cfg(feature = "cef")]
        let on_dial = Some(on_dial);
        // Rendering but browser-less: there is a screen, and still nothing to put YouTube
        // on it with.
        #[cfg(not(feature = "cef"))]
        let on_dial: Option<NoLauncher> = None;

        let serve_cfg = config.clone();
        let serve_tx = event_tx.clone();
        let serve_shutdown = shutdown.clone();
        let serve_osd = osd.clone();
        // Taken before the pipeline is handed to the session manager: after that, nothing
        // out here holds it, and an HTTP handler has no business owning a pipeline anyway.
        let shot = Some(shot_handle);
        runtime.spawn(async move {
            if let Err(e) = serve(
                serve_cfg,
                serve_tx,
                serve_shutdown,
                serve_osd,
                on_dial,
                shot,
                abandoned_rx,
            )
            .await
            {
                warn!(error = %e, "service layer exited with error");
            }
        });

        // CEF setup happens on the main thread (which will also pump it): TV user agent
        // so youtube.com/tv serves the leanback UI, EasyList (fetch → cache → built-in
        // fallback) for the ad blocker, then cef_initialize.
        #[cfg(feature = "cef")]
        let browser_host = {
            let mut cef = cef;
            cef.set_user_agent(pipeline::TV_USER_AGENT);
            // EasyList *and* uBlock Origin's list, plus the scriptlet bundle uBO's
            // `##+js(...)` rules need bodies from. Written to a cache the render
            // processes read, which is how injection reaches the page.
            let list_cache = pipeline::filterlists::CachePaths::default();
            // Whatever is already on disk, or the built-in list — instantly. Fetching
            // here used to block the main thread for up to ~110 s before CEF even
            // initialised, while `serve()` was already telling senders the app was
            // `running`; the refresh thread does it off the startup path and swaps the
            // engine in behind the shared cell when it lands.
            cef.set_adblock(
                pipeline::filterlists::load_cached_only(&list_cache)
                    .unwrap_or_else(pipeline::cef_adblock::AdBlocker::with_defaults),
            );
            // The lists change on the order of days and this box stays up for weeks, so
            // re-fetch daily rather than running whatever it booted with until someone
            // restarts it. The renderers notice by the cache changing under them.
            pipeline::filterlists::spawn_daily_refresh(list_cache, cef.adblock_handle());
            cef.initialize()
                .map_err(|e| anyhow::anyhow!("cef initialize: {e}"))?;
            let host = pipeline::BrowserHost::new(cef, nav_rx);
            // If the page dies and will not come back, stop telling senders it is there.
            // A crashed renderer we could not recover leaves nothing on screen, and DIAL
            // answering `running` with a published screen id invites a phone to attach to
            // it — which is the half of a browser crash a sender can actually see.
            let host = {
                let tx = abandoned_tx.clone();
                host.on_recovery_failed(Arc::new(move || {
                    let _ = tx.send(());
                }))
            };
            // The same browser does double duty: a live widget in the idle screen's card
            // until a cast takes it fullscreen, then back to the widget on DIAL stop.
            match &config.attract_widget_url {
                Some(url) => host.with_attract_widget(url),
                None => host,
            }
        };

        // Registered after `cef.initialize()` on purpose: Chromium installs its own
        // SIGINT handler during init, which would silently replace an earlier one.
        spawn_ctrl_c(&runtime, &shutdown, &kiosk_exit, remote);

        info!("kiosk: opening fullscreen output (close the window or ctrl-c to stop)");
        let attract = build_attract(&config);
        // No size here on purpose: the controller rasterizes each banner for whatever the
        // surface measures at the time, so it follows the panel and any resize.
        let osd_controller = OsdController::new(osd_rx);
        // The winit event loop MUST own the main thread; with CEF it doubles as the
        // browser's message pump and shuts CEF down when the loop exits.
        #[cfg(feature = "cef")]
        pipeline::kiosk::run_with_browser(
            rx,
            attract,
            Some(osd_controller),
            Some(kiosk_exit),
            browser_host,
        )
        .map_err(|e| anyhow::anyhow!("kiosk: {e}"))?;
        #[cfg(not(feature = "cef"))]
        pipeline::kiosk::run(rx, attract, Some(osd_controller), Some(kiosk_exit))
            .map_err(|e| anyhow::anyhow!("kiosk: {e}"))?;
        shutdown.notify_waiters();
    }

    #[cfg(not(feature = "render"))]
    {
        use pipeline::NullPipeline;
        let display: Box<dyn DisplayControl> = Box::new(NullDisplay);
        let manager =
            SessionManager::new(NullPipeline::new(), Some(display), SessionConfig::default())
                .with_osd(osd.clone());
        let remote = manager.remote_handle();
        runtime.spawn(manager.run(event_rx));
        // Headless: no renderer, so drain the OSD channel to the log.
        std::thread::spawn(move || drain_osd_to_log(&osd_rx));
        spawn_ctrl_c(&runtime, &shutdown, &kiosk_exit, remote);
        // Headless: no renderer at all, so certainly no browser to launch YouTube in.
        let on_dial: Option<NoLauncher> = None;
        runtime.block_on(serve(
            config,
            event_tx,
            shutdown,
            osd,
            on_dial,
            None,
            abandoned_rx,
        ))?;
    }

    Ok(())
}

/// A stable per-protocol device UUID, derived from the receiver's configured one.
///
/// Two UPnP root devices on one host must not share a UUID, and DLNA and DIAL did. Every
/// `SsdpDevice` advertises `upnp:rootdevice` and the bare `uuid:` target, so an `M-SEARCH`
/// for `ssdp:all` or `upnp:rootdevice` drew two `200 OK`s with an identical
/// `USN: uuid:…::upnp:rootdevice` and *different* `LOCATION`s. Control points key on USN
/// and most dedupe on it, so one description won arbitrarily — and if DLNA's won, the
/// response carried no `Application-URL` and DIAL was simply invisible. Only the targeted
/// `ST: urn:dial-multiscreen-org:service:dial:1` search was unaffected, which is exactly
/// why nothing caught it.
///
/// Derived rather than random so it survives a restart: a sender that remembers a device
/// by UUID should find the same one tomorrow. v5 over the configured UUID's own namespace
/// keeps that property without needing a second value in the config file.
fn device_uuid(base: &str, protocol: &str) -> String {
    let namespace = uuid::Uuid::parse_str(base).unwrap_or(uuid::Uuid::NAMESPACE_URL);
    uuid::Uuid::new_v5(&namespace, protocol.as_bytes()).to_string()
}

/// ctrl-c triggers the same shutdown as a kiosk window close: stop the services and
/// tell the winit loop to exit.
fn spawn_ctrl_c(
    runtime: &tokio::runtime::Runtime,
    shutdown: &Arc<Notify>,
    kiosk_exit: &Arc<std::sync::atomic::AtomicBool>,
    remote: castaway_core::RemoteHandle,
) {
    let shutdown = shutdown.clone();
    let kiosk_exit = kiosk_exit.clone();
    runtime.spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("ctrl-c: shutting down");
        // Tell whoever is sending to stop, before the thing they are sending to goes
        // away. A phone streaming A2DP into a receiver that has exited does not find out
        // quickly — it keeps encoding into a link that is gone, and the person holding it
        // sees playback running with no sound. Best-effort and bounded: shutdown must not
        // wait on a peer that has stopped answering.
        if let Some(remote) = remote.get() {
            let asked = tokio::time::timeout(
                Duration::from_secs(2),
                remote.issue(castaway_core::ControlTxn::Pause),
            )
            .await;
            match asked {
                Ok(Ok(())) => info!("told the active sender to pause"),
                Ok(Err(e)) => debug!(error = %e, "the active sender would not pause"),
                Err(_) => debug!("the active sender did not answer in time"),
            }
        }
        kiosk_exit.store(true, std::sync::atomic::Ordering::Relaxed);
        shutdown.notify_waiters();
    });
}

/// Headless OSD consumer: log each banner instead of drawing it.
#[cfg(not(feature = "render"))]
fn drain_osd_to_log(rx: &castaway_core::OsdReceiver) {
    use castaway_core::OsdCommand;
    while let Some(cmd) = rx.recv() {
        match cmd {
            OsdCommand::Show(m) => info!(osd = %m.text, "OSD"),
            OsdCommand::Clear => info!("OSD clear"),
        }
    }
}

/// The `on_dial` a build with no kiosk browser has: none. Spelled out as a type so the
/// `None` at the call site has something to be `None` of.
#[cfg(not(feature = "cef"))]
type NoLauncher = fn(proto_dial::DialEvent);

/// Stand up the shared HTTP host, SSDP responder, and mDNS responder for the enabled
/// protocols, and run until `shutdown` is signalled. `osd` is cloned to each adapter so
/// they can surface their own status on the overlay; DIAL launch/stop events are handed
/// to `on_dial` (the kiosk browser navigation hook). `on_dial: None` means this build has
/// nowhere to launch YouTube, and DIAL is then neither mounted nor advertised.
async fn serve(
    config: Config,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
    osd: castaway_core::OsdSink,
    on_dial: Option<impl Fn(proto_dial::DialEvent) + Send + 'static>,
    screenshot: Option<Screenshot>,
    // Signalled when the kiosk browser has given up on the launched page.
    mut abandoned: mpsc::UnboundedReceiver<()>,
) -> anyhow::Result<()> {
    let iface = config.resolved_interface();
    info!(
        name = %config.friendly_name,
        interface = %iface,
        http_port = config.http_port,
        "castaway services starting"
    );

    let mut http = Router::new();
    let mut ssdp_devices: Vec<(SsdpDevice, String)> = Vec::new();
    let mut mdns = MdnsResponder::new().context("creating mDNS responder")?;

    if config.enable.dlna {
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "http"), event_tx.clone());
        let dlna = DlnaService::new(
            config.advertised_name(ProtocolKind::Dlna),
            &config.uuid,
            sink,
        )
        .with_osd(osd.clone());
        http = http.merge(dlna.router());
        ssdp_devices.push((dlna.ssdp_device(), dlna.description_path().to_string()));
        info!("enabled: DLNA MediaRenderer");
    }

    if config.enable.spotify {
        let sink = SessionSink::new(
            SourceId::new(ProtocolKind::Spotify, "http"),
            event_tx.clone(),
        );
        let spotify = SpotifyService::new(
            config.advertised_name(ProtocolKind::Spotify),
            spotify_device_id(&config),
        )
        // Order matters: the runner clones the overlay sink when it starts, so the OSD
        // has to be attached first or session-level messages never reach the screen.
        .with_osd(osd.clone())
        .with_playback(
            sink,
            proto_spotify::PlaybackQuality {
                initial_volume: config.spotify.initial_volume,
                bitrate: config.spotify.bitrate,
                normalisation: config.spotify.normalisation,
            },
        );
        http = http.merge(spotify.router());
        mdns.advertise(&spotify.mdns_service(config.http_port, MDNS_HOST))
            .context("advertising Spotify")?;
        info!("enabled: Spotify Connect (pairing + playback)");
    }

    let (dial_tx, mut dial_rx) = mpsc::channel(8);
    // DIAL is launch-only: it carries no media, so everything a YouTube sender does after
    // the launch happens between the phone, YouTube's Lounge servers, and the *page* we
    // are supposed to have opened. A build with no browser has nothing to open, so the
    // pairing code is never registered and the phone binds to nothing — it sees `running`
    // and then browses a session that will never play. D16's rule ("advertising a service
    // with no listener only frustrates senders") is the same rule here, and a missing
    // launch target is a missing listener.
    match (config.enable.dial, on_dial) {
        (true, None) => warn!(
            "DIAL disabled: this build has no kiosk browser to launch YouTube in \
             (build with `--features cef`)"
        ),
        (false, _) => {}
        (true, Some(on_dial)) => {
            let dial = DialService::new(
                config.advertised_name(ProtocolKind::YouTubeLounge),
                config.http_base_url(),
                device_uuid(&config.uuid, "dial"),
                dial_tx.clone(),
            )
            .with_osd(osd.clone());
            http = http.merge(dial.router());
            ssdp_devices.push((dial.ssdp_device(), dial.description_path().to_string()));
            info!("enabled: DIAL → YouTube leanback (launch/stop)");
            // Whatever the launched page becomes, a sender arriving later needs to be
            // able to find it. The routes clear this slot themselves on launch and stop;
            // filling it is a network lookup, so it happens out here.
            let screen = dial.screen_slot();
            // The same screen id, put to a second use: SponsorBlock attaches to our own
            // page as a remote control and seeks past sponsors. Only where there is a
            // page to attach to — this arm is the `cef` build by construction (D27).
            #[cfg(feature = "cef")]
            if config.sponsorblock.enabled {
                tokio::spawn(sponsorblock::run(
                    config.sponsorblock.clone(),
                    screen.clone(),
                    osd.clone(),
                ));
            }
            // A page that crashed past recovery is not running, whatever DIAL last said.
            // Left saying `running` with a screen id published, it invites a phone to
            // attach to a surface that no longer exists.
            {
                let dial = dial.clone();
                tokio::spawn(async move {
                    while abandoned.recv().await.is_some() {
                        dial.abandoned().await;
                    }
                });
            }
            tokio::spawn(async move {
                // At most one resolver at a time. Each launch used to spawn one with no
                // handle, so a relaunch inside the ~60 s budget left the *old* task
                // polling the *old* pairing code — and whichever finished last won the
                // slot. A stale writer could overwrite the fresh screen id, or refill a
                // slot the stop route had just cleared, which reproduces the exact D28
                // symptom the slot exists to prevent: connected, and unable to queue.
                let mut resolver: Option<tokio::task::JoinHandle<()>> = None;
                while let Some(event) = dial_rx.recv().await {
                    match &event {
                        proto_dial::DialEvent::Launched(params) => {
                            if let Some(task) = resolver.take() {
                                task.abort();
                            }
                            if let Some(code) = params.pairing_code.clone() {
                                resolver = Some(tokio::spawn(screen::publish_screen_id(
                                    code,
                                    screen.clone(),
                                )));
                            }
                        }
                        // The page is going away, so a resolver still hunting for its id
                        // is hunting for a screen that will not exist.
                        proto_dial::DialEvent::Stopped => {
                            if let Some(task) = resolver.take() {
                                task.abort();
                            }
                        }
                    }
                    on_dial(event);
                }
            });
        }
    }

    // Cast is the first protocol whose adapter owns a real listener, so it advertises
    // itself: what goes in the TXT record comes from the same object that answers the
    // port, and the two can't drift.
    let mut adapter_handles = Vec::new();
    if config.enable.cast {
        adapter_handles
            .push(spawn_cast(&config, &mut mdns, event_tx.clone(), shutdown.clone()).await?);
    }
    if config.enable.airplay {
        adapter_handles.push(spawn_airplay(
            &config,
            &mut mdns,
            event_tx.clone(),
            shutdown.clone(),
        ));
    }
    if config.enable.bluetooth {
        // Bluetooth is its own discovery layer — inquiry scan and SDP records, no mDNS —
        // so nothing is registered with the responder here.
        //
        // A missing or unclaimable controller is logged and skipped rather than fatal: a
        // receiver that can still do AirPlay and Cast should not refuse to start because
        // someone unplugged a dongle.
        match bluetooth::spawn(&config, event_tx.clone(), shutdown.clone()).await {
            Ok(handle) => adapter_handles.push(handle),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "Bluetooth sink unavailable; continuing without it")
            }
        }
    }

    // SSDP responder.
    let ssdp_handle = {
        let mut responder = Responder::new(ResponderConfig {
            interface: iface,
            http_port: config.http_port,
            server: "castaway/0.1 UPnP/1.0".to_string(),
            max_age: 1800,
        });
        for (device, path) in ssdp_devices {
            responder = responder.advertise(device, path);
        }
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = responder
                .run(async move { shutdown.notified().await })
                .await
            {
                warn!(error = %e, "SSDP responder exited");
            }
        })
    };

    // HTTP host.
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.http_port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding HTTP host on {addr}"))?;
    // Mounted last so it cannot be shadowed by a protocol router, and with the handle as
    // state. A build with no renderer still answers — saying it cannot draw, rather than
    // 404ing, because "no such endpoint" and "this binary has no compositor" are
    // different problems and only one is worth chasing.
    let http = http.route(
        "/screenshot.png",
        axum::routing::get(screenshot_route).with_state(screenshot),
    );

    info!(%addr, "HTTP host listening");
    let http_shutdown = shutdown.clone();
    let http_handle = tokio::spawn(async move {
        let served = axum::serve(listener, http)
            .with_graceful_shutdown(async move { http_shutdown.notified().await })
            .await;
        if let Err(e) = served {
            warn!(error = %e, "HTTP host exited");
        }
    });

    info!("castaway services running");
    shutdown.notified().await;
    info!("services: shutting down (SSDP byebye, unregister mDNS)");

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = ssdp_handle.await;
        let _ = http_handle.await;
        for handle in adapter_handles {
            let _ = handle.await;
        }
    })
    .await;
    drop(mdns);
    Ok(())
}

/// Stand up the CASTv2 TLS listener, advertise what it asks for, and run it until
/// `shutdown`. Returns the actor's join handle so shutdown can wait on it.
async fn spawn_cast(
    config: &Config,
    mdns: &mut MdnsResponder,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    // RSA-2048 keygen takes seconds; it belongs on a blocking thread, not stalling the
    // runtime while the other adapters are trying to come up (ground rule 4).
    let signer = tokio::task::spawn_blocking(CastDeviceSigner::generate_dev)
        .await
        .context("joining Cast device-key generation")?
        .context("generating the Cast device key")?;
    warn!(
        "Cast device auth uses a self-generated dev key; senders that verify the Google \
         chain will reject it (Q2/Q11)"
    );

    let identity = TlsIdentity::self_signed(&["castaway.local".to_string()])
        .context("generating the Cast TLS identity")?;
    let receiver = CastReceiver::new(
        proto_cast::actor::default_listen_addr(),
        config.advertised_name(ProtocolKind::Cast).as_str(),
        config.uuid.replace('-', ""),
        &identity,
    )
    .context("building the CASTv2 receiver")?
    .with_signer(Arc::new(signer));

    advertise_adapter(&receiver, mdns);
    info!("enabled: Google Cast (CASTv2 media-URL LOAD)");

    // The listener adapter's own tag; each accepted sender is retagged with its peer.
    let sink = SessionSink::new(SourceId::new(ProtocolKind::Cast, "listener"), event_tx);
    let adapter = Arc::new(receiver);
    Ok(tokio::spawn(async move {
        tokio::select! {
            res = adapter.run(sink) => {
                if let Err(e) = res {
                    warn!(error = %e, "Cast adapter exited");
                }
            }
            () = shutdown.notified() => info!("Cast listener stopping"),
        }
    }))
}

/// Register whatever an adapter says it needs discoverable. The app supplies only
/// [`MDNS_HOST`] — the box's one name. Everything else, instance name included, comes
/// from the adapter, because the instance-naming convention is per-protocol.
fn advertise_adapter(adapter: &dyn SourceAdapter, mdns: &mut MdnsResponder) {
    use substrate_mdns::MdnsService;
    for ad in adapter.advertisements() {
        match ad {
            Advertisement::MdnsService {
                ty,
                instance,
                port,
                txt,
            } => {
                let svc = txt.into_iter().fold(
                    MdnsService::new(ty, instance, MDNS_HOST, port),
                    |svc, (key, value)| svc.with_txt(key, value),
                );
                if let Err(e) = mdns.advertise(&svc) {
                    warn!(error = %e, protocol = %adapter.kind(), "failed to advertise");
                }
            }
            other => warn!(
                ?other,
                protocol = %adapter.kind(),
                "adapter asked for an advertisement the app doesn't serve yet"
            ),
        }
    }
}

/// Stand up the AirPlay/RAOP RTSP listeners, advertise what they ask for, and run them
/// until `shutdown`. Returns the actor's join handle so shutdown can wait on it.
fn spawn_airplay(
    config: &Config,
    mdns: &mut MdnsResponder,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let receiver = AirPlayReceiver::new(proto_airplay::AirPlayIdentity {
        name: config.advertised_name(ProtocolKind::AirPlay),
        device_id: derive_mac(&config.uuid),
        host: MDNS_HOST.to_string(),
    });

    advertise_adapter(&receiver, mdns);
    info!("enabled: AirPlay (RTSP control on 7000/7011)");
    // Say this once, plainly: the control plane answers, the media plane can't start.
    // A sender will find us, connect, and stall at pairing rather than mirror.
    warn!(
        "AirPlay control is live, but pairing and FairPlay-SAP are not implemented — \
           mirroring will not start (Q1)"
    );

    let sink = SessionSink::new(SourceId::new(ProtocolKind::AirPlay, "listener"), event_tx);
    let adapter = Arc::new(receiver);
    tokio::spawn(async move {
        tokio::select! {
            res = adapter.run(sink) => {
                if let Err(e) = res {
                    warn!(error = %e, "AirPlay adapter exited");
                }
            }
            () = shutdown.notified() => info!("AirPlay listeners stopping"),
        }
    })
}

fn spotify_device_id(config: &Config) -> String {
    config.uuid.replace('-', "")
}

/// The screenshot handle, or an uninhabited stand-in.
///
/// Without the `render` feature there is no compositor and so no handle to hold. Making
/// the type uninhabited rather than `cfg`-ing the route away means the endpoint still
/// exists and still answers — and the "no renderer" branch is the *only* representable
/// state, so the compiler agrees rather than the comment claiming it.
#[cfg(feature = "render")]
type Screenshot = pipeline::ScreenshotHandle;
#[cfg(not(feature = "render"))]
type Screenshot = std::convert::Infallible;

/// `GET /screenshot.png` — what the panel is showing, right now.
///
/// Exists so a surface can be reviewed without standing in front of the display, which is
/// otherwise the only way to see whether a layout change worked.
///
/// A build with no renderer still answers, saying it cannot draw rather than 404ing:
/// "no such endpoint" and "this binary has no compositor" are different problems and only
/// one of them is worth chasing.
#[cfg(feature = "render")]
async fn screenshot_route(
    axum::extract::State(handle): axum::extract::State<Option<Screenshot>>,
) -> axum::response::Response {
    use axum::response::IntoResponse as _;

    let Some(handle) = handle else {
        return no_renderer();
    };
    // On a blocking pool: the capture waits for the render thread to present, which is a
    // frame away at best and never at worst, and a runtime worker must not sit on that.
    let shot =
        tokio::task::spawn_blocking(move || handle.capture(std::time::Duration::from_secs(2)))
            .await;
    match shot {
        Ok(Ok(png)) => (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "image/png")],
            png,
        )
            .into_response(),
        Ok(Err(e)) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!("{e}\n"),
        )
            .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e}\n"),
        )
            .into_response(),
    }
}

/// The same endpoint in a build with no compositor to photograph.
#[cfg(not(feature = "render"))]
async fn screenshot_route(
    axum::extract::State(_): axum::extract::State<Option<Screenshot>>,
) -> axum::response::Response {
    no_renderer()
}

fn no_renderer() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "this build has no renderer; rebuild with the `render` feature\n",
    )
        .into_response()
}

/// Build the idle/attract image from the enabled protocols. Rendered at 1920×1080 and
/// scaled to fill the panel.
#[cfg(feature = "render")]
fn build_attract(config: &Config) -> Option<(u32, u32, Vec<u8>)> {
    use pipeline::attract::{render, AttractRow, AttractScene, WidgetSlot};

    let name = &config.friendly_name;
    // Each row names the entry that surface actually publishes, not the bare friendly
    // name: the whole job of this screen is to tell someone what to look for in their
    // picker, and since every surface now advertises `name#protocol` the bare name is
    // a string they will not find.
    let detail = |verb: &str, kind: ProtocolKind| {
        format!("{verb} \u{2192} {}", config.advertised_name(kind))
    };
    let mut rows = Vec::new();
    if config.enable.cast {
        rows.push(AttractRow::new(
            [0x42, 0x85, 0xf4, 0xff],
            "Chrome / Edge",
            detail("Cast", ProtocolKind::Cast),
        ));
    }
    if config.enable.airplay {
        rows.push(AttractRow::new(
            [0xff, 0xff, 0xff, 0xff],
            "iPhone / Mac",
            detail("AirPlay", ProtocolKind::AirPlay),
        ));
    }
    if config.enable.dlna {
        rows.push(AttractRow::new(
            [0x3d, 0xdc, 0x84, 0xff],
            "Android / VLC",
            detail("Cast or DLNA", ProtocolKind::Dlna),
        ));
    }
    if config.enable.spotify {
        rows.push(AttractRow::new(
            [0x1d, 0xb9, 0x54, 0xff],
            "Spotify",
            detail("Devices", ProtocolKind::Spotify),
        ));
    }
    if config.enable.bluetooth {
        rows.push(AttractRow::new(
            [0x00, 0x82, 0xfc, 0xff],
            "Any phone",
            detail("Bluetooth", ProtocolKind::Bluetooth),
        ));
    }
    if config.enable.dial {
        rows.push(AttractRow::new(
            [0xff, 0x00, 0x00, 0xff],
            "YouTube",
            detail("Cast button", ProtocolKind::YouTubeLounge),
        ));
    }

    // Reserve the widget card only if something will actually paint into it: with no CEF
    // build (or no URL configured) the text should use the full width rather than frame a
    // permanently empty panel.
    let widget = match (cfg!(feature = "cef"), &config.attract_widget_url) {
        (true, Some(_)) => WidgetSlot::RightCard,
        _ => WidgetSlot::None,
    };
    let scene = AttractScene {
        title: name.clone(),
        tagline: "Throw anything at the wall — no app to install.".to_string(),
        rows,
        footer: format!(
            "castaway  •  {}",
            config.http_base_url().replace("http://", "")
        ),
        widget,
    };
    // Native panel resolution (Dell C6522QT is 4K): a 1:1 background keeps the dither
    // pattern intact — GPU upscaling would smear it and re-introduce banding.
    let (w, h) = (3840, 2160);
    match render(&scene, w, h) {
        Ok(rgba) => Some((w, h, rgba)),
        Err(e) => {
            warn!(error = %e, "failed to render attract scene");
            None
        }
    }
}

/// Derive a stable MAC-style id from the UUID (AirPlay wants a `AA:BB:..` device id).
fn derive_mac(uuid: &str) -> String {
    let hex: String = uuid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(12)
        .collect();
    let padded = format!("{hex:0<12}");
    padded
        .as_bytes()
        .chunks(2)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join(":")
        .to_uppercase()
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::device_uuid;

    #[test]
    fn each_protocol_gets_its_own_device_uuid_and_keeps_it() {
        // Two UPnP root devices on one host must not share a UUID: both advertise
        // `upnp:rootdevice` and the bare `uuid:` target, so a shared one produces two
        // `200 OK`s with the same USN and different LOCATIONs — and a control point that
        // dedupes on USN (most do) picks one arbitrarily. When it picked DLNA's, the
        // response had no `Application-URL` and DIAL was invisible.
        let base = "0f8c1e2a-1111-4000-8000-00000000abcd";
        let dial = device_uuid(base, "dial");
        assert_ne!(dial, base, "must not collide with the DLNA root device");
        assert_ne!(dial, device_uuid(base, "cast"));
        // Derived, not random: a sender that remembers a device by UUID has to find the
        // same one after a restart.
        assert_eq!(dial, device_uuid(base, "dial"));
        assert!(uuid::Uuid::parse_str(&dial).is_ok(), "must be a real uuid");
    }

    #[test]
    fn a_malformed_configured_uuid_still_yields_stable_distinct_ones() {
        // The config is hand-edited. A typo should degrade to "still works, still
        // distinct" rather than to two devices sharing an identity again.
        let dial = device_uuid("not-a-uuid", "dial");
        let cast = device_uuid("not-a-uuid", "cast");
        assert_ne!(dial, cast);
        assert_eq!(dial, device_uuid("not-a-uuid", "dial"));
    }
}
