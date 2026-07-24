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

mod config;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use castaway_core::{
    osd_channel, DisplayControl, ProtocolKind, SessionConfig, SessionManager, SessionSink,
    SourceId, SourceMessage,
};
use control_display::NullDisplay;
use proto_dial::DialService;
use proto_dlna::DlnaService;
use proto_spotify::SpotifyService;
use substrate_mdns::MdnsResponder;
use substrate_ssdp::{Responder, ResponderConfig, SsdpDevice};
use tokio::sync::{mpsc, Notify};
use tracing::{info, warn};

use crate::config::Config;

fn main() -> anyhow::Result<()> {
    // CEF is multi-process: subprocesses re-exec this same binary, so bootstrap must be
    // the very first thing in main — before tracing, config, or the tokio runtime.
    // `None` means this invocation *was* a subprocess and has already run to completion.
    #[cfg(feature = "cef")]
    let cef = match pipeline::Cef::bootstrap().map_err(|e| anyhow::anyhow!("cef bootstrap: {e}"))? {
        Some(cef) => cef,
        None => return Ok(()),
    };

    init_tracing();
    let config = Config::load("castaway.toml").context("loading config")?;
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

    #[cfg(feature = "render")]
    {
        use pipeline::{OsdController, RenderPipeline};
        let (render_pipeline, rx) = RenderPipeline::new(3);
        let display: Box<dyn DisplayControl> = Box::new(NullDisplay);
        let manager = SessionManager::new(render_pipeline, Some(display), SessionConfig::default())
            .with_osd(osd.clone());
        runtime.spawn(manager.run(event_rx));

        // DIAL launch → navigate the main-thread CEF browser to YouTube leanback with
        // the sender's pairing params, so the phone binds to this screen; DIAL stop →
        // hide it. Without CEF the events are only logged.
        #[cfg(feature = "cef")]
        let (nav_tx, nav_rx) = std::sync::mpsc::channel::<pipeline::BrowserCommand>();
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
        #[cfg(not(feature = "cef"))]
        let on_dial = log_dial_event;

        let serve_cfg = config.clone();
        let serve_tx = event_tx.clone();
        let serve_shutdown = shutdown.clone();
        let serve_osd = osd.clone();
        runtime.spawn(async move {
            if let Err(e) = serve(serve_cfg, serve_tx, serve_shutdown, serve_osd, on_dial).await {
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
            cef.set_adblock(pipeline::easylist::load_or_fetch(
                pipeline::easylist::EASYLIST_URL,
                &pipeline::easylist::default_cache_path(),
            ));
            cef.initialize()
                .map_err(|e| anyhow::anyhow!("cef initialize: {e}"))?;
            pipeline::BrowserHost::new(cef, nav_rx)
        };

        // Registered after `cef.initialize()` on purpose: Chromium installs its own
        // SIGINT handler during init, which would silently replace an earlier one.
        spawn_ctrl_c(&runtime, &shutdown, &kiosk_exit);

        info!("kiosk: opening fullscreen output (close the window or ctrl-c to stop)");
        let attract = build_attract(&config);
        let osd_controller = OsdController::new(osd_rx, 1280, 720);
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
        runtime.spawn(manager.run(event_rx));
        // Headless: no renderer, so drain the OSD channel to the log.
        std::thread::spawn(move || drain_osd_to_log(&osd_rx));
        spawn_ctrl_c(&runtime, &shutdown, &kiosk_exit);
        runtime.block_on(serve(config, event_tx, shutdown, osd, log_dial_event))?;
    }

    Ok(())
}

/// ctrl-c triggers the same shutdown as a kiosk window close: stop the services and
/// tell the winit loop to exit.
fn spawn_ctrl_c(
    runtime: &tokio::runtime::Runtime,
    shutdown: &Arc<Notify>,
    kiosk_exit: &Arc<std::sync::atomic::AtomicBool>,
) {
    let shutdown = shutdown.clone();
    let kiosk_exit = kiosk_exit.clone();
    runtime.spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("ctrl-c: shutting down");
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

/// What to do with a DIAL launch/stop when there's no kiosk browser: log it.
#[cfg(not(feature = "cef"))]
fn log_dial_event(event: proto_dial::DialEvent) {
    match event {
        proto_dial::DialEvent::Launched(params) => {
            warn!(pairing = ?params.pairing_code, "YouTube launched; no kiosk browser (build with `cef`)");
        }
        proto_dial::DialEvent::Stopped => info!("YouTube stopped"),
    }
}

/// Stand up the shared HTTP host, SSDP responder, and mDNS responder for the enabled
/// protocols, and run until `shutdown` is signalled. `osd` is cloned to each adapter so
/// they can surface their own status on the overlay; DIAL launch/stop events are handed
/// to `on_dial` (the kiosk browser navigation hook, or a logger).
async fn serve(
    config: Config,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
    osd: castaway_core::OsdSink,
    on_dial: impl Fn(proto_dial::DialEvent) + Send + 'static,
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
        let dlna =
            DlnaService::new(&config.friendly_name, &config.uuid, sink).with_osd(osd.clone());
        http = http.merge(dlna.router());
        ssdp_devices.push((dlna.ssdp_device(), dlna.description_path().to_string()));
        info!("enabled: DLNA MediaRenderer");
    }

    if config.enable.spotify {
        let sink = SessionSink::new(
            SourceId::new(ProtocolKind::Spotify, "http"),
            event_tx.clone(),
        );
        let spotify = SpotifyService::new(&config.friendly_name, spotify_device_id(&config), sink)
            .with_osd(osd.clone());
        http = http.merge(spotify.router());
        mdns.advertise(&spotify.mdns_service(config.http_port, "castaway"))
            .context("advertising Spotify")?;
        info!("enabled: Spotify Connect (onboarding/pairing)");
    }

    let (dial_tx, mut dial_rx) = mpsc::channel(8);
    if config.enable.dial {
        let dial = DialService::new(
            &config.friendly_name,
            config.http_base_url(),
            dial_tx.clone(),
        )
        .with_osd(osd.clone());
        http = http.merge(dial.router());
        ssdp_devices.push((
            dial.ssdp_device(&config.uuid),
            dial.description_path().to_string(),
        ));
        info!("enabled: DIAL → YouTube leanback (launch/stop)");
        tokio::spawn(async move {
            while let Some(event) = dial_rx.recv().await {
                on_dial(event);
            }
        });
    }

    advertise_socket_protocols(&config, &mut mdns);

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
    })
    .await;
    drop(mdns);
    Ok(())
}

/// mDNS-advertise Cast/AirPlay if enabled. Their TCP actors (Cast TLS, AirPlay RTSP)
/// aren't wired yet, so this is off by default.
fn advertise_socket_protocols(config: &Config, mdns: &mut MdnsResponder) {
    use substrate_mdns::MdnsService;
    if config.enable.cast {
        let svc = MdnsService::new(
            proto_cast::CAST_SERVICE_TYPE,
            &config.friendly_name,
            "castaway",
            proto_cast::CAST_PORT,
        )
        .with_txt("id", config.uuid.replace('-', ""))
        .with_txt("md", "castaway")
        .with_txt("fn", &config.friendly_name)
        .with_txt("ca", "5")
        .with_txt("ve", "05");
        if let Err(e) = mdns.advertise(&svc) {
            warn!(error = %e, "failed to advertise Cast");
        }
        warn!("Cast advertised, but the CASTv2 TLS actor is not wired yet (pure core only)");
    }
    if config.enable.airplay {
        let ident = proto_airplay::AirPlayIdentity {
            name: config.friendly_name.clone(),
            device_id: derive_mac(&config.uuid),
            host: "castaway".to_string(),
        };
        for svc in [ident.airplay_service(), ident.raop_service()] {
            if let Err(e) = mdns.advertise(&svc) {
                warn!(error = %e, "failed to advertise AirPlay/RAOP");
            }
        }
        warn!("AirPlay advertised, but the RTSP actor + FairPlay are not wired yet (Q1)");
    }
}

fn spotify_device_id(config: &Config) -> String {
    config.uuid.replace('-', "")
}

/// Build the idle/attract image from the enabled protocols. Rendered at 1920×1080 and
/// scaled to fill the panel.
#[cfg(feature = "render")]
fn build_attract(config: &Config) -> Option<(u32, u32, Vec<u8>)> {
    use pipeline::attract::{render, AttractRow, AttractScene};

    let name = &config.friendly_name;
    let detail = |verb: &str| format!("{verb} \u{2192} {name}");
    let mut rows = Vec::new();
    if config.enable.cast {
        rows.push(AttractRow::new(
            [0x42, 0x85, 0xf4, 0xff],
            "Chrome / Edge",
            detail("Cast"),
        ));
    }
    if config.enable.airplay {
        rows.push(AttractRow::new(
            [0xff, 0xff, 0xff, 0xff],
            "iPhone / Mac",
            detail("AirPlay"),
        ));
    }
    if config.enable.dlna {
        rows.push(AttractRow::new(
            [0x3d, 0xdc, 0x84, 0xff],
            "Android / VLC",
            detail("Cast or DLNA"),
        ));
    }
    if config.enable.spotify {
        rows.push(AttractRow::new(
            [0x1d, 0xb9, 0x54, 0xff],
            "Spotify",
            detail("Devices"),
        ));
    }
    if config.enable.dial {
        rows.push(AttractRow::new(
            [0xff, 0x00, 0x00, 0xff],
            "YouTube",
            "Cast button".to_string(),
        ));
    }

    let scene = AttractScene {
        title: name.clone(),
        tagline: "Throw anything at the wall — no app to install.".to_string(),
        rows,
        footer: format!(
            "castaway  •  {}",
            config.http_base_url().replace("http://", "")
        ),
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
