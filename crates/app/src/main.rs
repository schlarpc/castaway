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
mod logging;
mod screen;
// The network-surface registry (#22/#30): every socket, as data, generating the doc,
// the firewall JSON, and the --network-surface query.
mod surface;
// What the panel can change about itself, and how it persists. The types are always
// compiled — the store's tests are the config-file contract, and they must run in the
// build CI tests — but only the render build has screens to press them from, so the
// headless build is excused its "never used" (the same standing theme/browser have).
#[cfg_attr(not(feature = "render"), allow(unused))]
mod settings;
// The panel's own navigation: what happens when someone presses something the shell
// could not answer itself. Only meaningful where there is a panel.
#[cfg(feature = "render")]
mod shell_nav;
// Reading the screen is pure and always compiled; the actor that drives it needs a page
// to drive, so it exists only in the browser build (D27).
mod sponsorblock;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use cast_replay::{ReplayConfig, ReplayProvider};
use castaway_core::{
    osd_channel, Advertisement, DisplayControl, ProtocolKind, SessionConfig, SessionManager,
    SessionSink, SourceAdapter, SourceId, SourceMessage,
};
use control_display::NullDisplay;
use crypto_cast_auth::CastDeviceSigner;
use proto_airplay::AirPlayReceiver;
use proto_cast::{CastIdentity, CastReceiver, TlsIdentity};
use proto_dial::DialService;
use proto_dlna::DlnaService;
use proto_miracast::MiracastAdapter;
use proto_spotify::SpotifyService;
use substrate_mdns::MdnsResponder;
use substrate_ssdp::{Responder, ResponderConfig, SsdpDevice};
use tokio::sync::{mpsc, Notify};
use tracing::{debug, info, warn};

use crate::config::Config;

/// The mDNS host label every advertisement resolves to (`castaway.local.`). One name for
/// the box, however many services it publishes.
const MDNS_HOST: &str = "castaway";

/// The command line. Small on purpose: the config file is the interface, and the flags
/// only answer "which file?" and the read-only surface query.
#[derive(Debug, clap::Parser)]
#[command(name = "castaway", version, about = "Universal cast receiver")]
struct Cli {
    /// Path to the config file. Without it (or $CASTAWAY_CONFIG), `castaway.toml` in
    /// the working directory is used if present, else the platform config directory
    /// ($XDG_CONFIG_HOME/castaway/castaway.toml, or %LOCALAPPDATA%\castaway\config).
    #[arg(long, value_name = "PATH", env = config::CONFIG_ENV)]
    config: Option<std::path::PathBuf>,
    /// Print every socket this config binds, and exit.
    #[arg(long, value_name = "FORMAT", num_args = 0..=1, default_missing_value = "table")]
    network_surface: Option<surface::Format>,
}

fn main() -> anyhow::Result<()> {
    let cli = <Cli as clap::Parser>::parse();
    // Resolved once, and handed to both the loader and the settings store: the file the
    // screen saves settings into is by construction the file the next boot reads.
    let location = config::ConfigLocation::from_cli(cli.config);
    // A query, not a run: print what this config binds and exit before any socket is
    // bound or log file created.
    if let Some(format) = cli.network_surface {
        let config = Config::load_at(&location).context("loading config")?;
        print!("{}", surface::render(format, &config));
        return Ok(());
    }
    // Config first, because the file sink is configurable and a subscriber can only be
    // installed once. Nothing here logs — a config that fails to load returns an error
    // main prints itself, which is the same thing a `tracing` line would have said.
    //
    // (Ordinary otherwise. Under CEF this had to come before a bootstrap call that
    // re-execed this same binary as Chromium's subprocess, and getting the order wrong
    // silently un-instrumented the renderer. The browser is its own process now, and it
    // reports through the protocol, so there is nothing to sequence against — D36.)
    let config = Config::load_at(&location).context("loading config")?;
    logging::init(&config.log, castaway_paths::host());
    info!(
        path = %location.path().display(),
        origin = ?location.origin(),
        "config"
    );
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
    // has given up tells DIAL over a channel rather than holding it. Only the browser build
    // has a browser to give up.
    #[cfg_attr(not(feature = "electron"), allow(unused_variables))]
    let (abandoned_tx, abandoned_rx) = mpsc::unbounded_channel::<()>();

    #[cfg(feature = "render")]
    {
        use pipeline::{OsdController, RenderPipeline};
        let (render_pipeline, rx) = RenderPipeline::new(3);
        // Which device sound leaves through. Config decides where it starts, the
        // settings screen moves it live, and every session factory reads it at
        // stream-open — so a pick reaches the *next* session of every source with no
        // restart, while sessions already playing keep the device they opened.
        let audio_selector = pipeline::audio_select::OutputSelector::new(
            config
                .audio
                .output
                .choice_for(pipeline::audio_select::active_backend())
                .selection(),
        );
        #[cfg(feature = "audio")]
        let audio_factory: pipeline::audio_out::AudioOutputFactory =
            pipeline::audio_out::output_factory(audio_selector.clone());
        #[cfg(feature = "audio")]
        let render_pipeline = render_pipeline.with_audio_output(Arc::clone(&audio_factory));
        // A second handle on the render channel, for the shell: it pushes screens in
        // answer to panel presses, and the pipeline itself is about to be moved into the
        // session manager.
        let render_tx = render_pipeline.commands();
        // Panel presses the shell could not answer locally. Small: these are taps by a
        // person, not a stream.
        let (shell_event_tx, shell_event_rx) = mpsc::channel::<pipeline::shell::ShellEvent>(8);
        let shot_handle = render_pipeline.screenshot_handle();
        // Taken here for the same reason as the screenshot handle: after the pipeline is
        // moved into the session manager, nothing out here holds it, and the DLNA service
        // that has to answer "how far through is this" is built inside `serve`.
        let playback: Arc<dyn castaway_core::PlaybackReport> =
            Arc::new(render_pipeline.playback_handle());

        // DIAL launch → navigate the main-thread browser to YouTube leanback with
        // the sender's pairing params, so the phone binds to this screen; DIAL stop →
        // hide it. Without the browser there is no launch target, and DIAL goes unadvertised.
        #[cfg(feature = "electron")]
        let (nav_tx, nav_rx) = std::sync::mpsc::channel::<pipeline::BrowserCommand>();

        // Whoever casts next gets the panel. Nothing but DIAL `DELETE` used to dismiss
        // the leanback page, and nothing sends `DELETE` (D28) — so the first YouTube cast
        // owned the screen for the rest of the process, with later DLNA/Cast video
        // decoding underneath it and Spotify playing under YouTube's own audio.
        #[cfg(feature = "electron")]
        {
            let release_tx = nav_tx.clone();
            render_pipeline.set_screen_release(Arc::new(move || {
                let _ = release_tx.send(pipeline::BrowserCommand::Hide);
            }));
        }

        // The decode thread noticing that a URL finished, or could not be fetched at all,
        // is the only party that knows — and until this channel existed it logged and
        // exited, leaving a DLNA control point reading PLAYING / OK for the rest of the
        // process and a queued playlist stuck on its first track.
        let (ends_tx, ends_rx) = castaway_core::playback::end_channel();
        render_pipeline.set_playback_ends(ends_tx);

        let display: Box<dyn DisplayControl> = Box::new(NullDisplay);
        let manager = SessionManager::new(render_pipeline, Some(display), SessionConfig::default())
            .with_osd(osd.clone())
            .with_playback_ends(ends_rx);
        let remote = manager.remote_handle();
        runtime.spawn(manager.run(event_rx));

        #[cfg(feature = "electron")]
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
        #[cfg(feature = "electron")]
        let on_dial = Some(on_dial);
        // Rendering but browser-less: there is a screen, and still nothing to put YouTube
        // on it with.
        #[cfg(not(feature = "electron"))]
        let on_dial: Option<NoLauncher> = None;

        let serve_cfg = config.clone();
        let serve_tx = event_tx.clone();
        let serve_shutdown = shutdown.clone();
        let serve_osd = osd.clone();
        // What the Settings tile opens. The store points at the same file the config
        // came from, so what the screen saves is what the next boot reads.
        let settings_catalog =
            settings::Catalog::new(vec![Arc::new(settings::OutputDeviceSetting::new(
                audio_selector.clone(),
                settings::ConfigStore::at(&location),
            ))]);
        let handles = PipelineHandles {
            screenshot: Some(shot_handle),
            playback: Some(playback),
            shell: Some(ShellChannels {
                events: shell_event_rx,
                render: render_tx.clone(),
                settings: settings_catalog,
            }),
        };
        runtime.spawn(async move {
            if let Err(e) = serve(
                serve_cfg,
                serve_tx,
                serve_shutdown,
                serve_osd,
                on_dial,
                handles,
                abandoned_rx,
            )
            .await
            {
                warn!(error = %e, "service layer exited with error");
            }
        });

        // The browser is a subprocess now, spawned here on the main thread because the
        // kiosk loop that pumps it lives here too. Everything policy-shaped stays on this
        // side: the TV user agent (leanback keys off it), the block lists, and the
        // scriptlet bundle. The browser is handed those and asks us about the rest.
        #[cfg(feature = "electron")]
        let browser_host = {
            use std::sync::Arc as StdArc;

            // Whatever is already on disk, or the built-in list — instantly. Fetching on
            // the startup path used to block for up to ~110 s while `serve()` was already
            // telling senders the app was `running`; the refresh thread does it off that
            // path and swaps the engine in behind the shared cell when it lands.
            let list_cache = pipeline::filterlists::CachePaths::default();
            let blocker = StdArc::new(
                pipeline::filterlists::load_cached_only(&list_cache)
                    .unwrap_or_else(pipeline::adblock_engine::AdBlocker::with_defaults),
            );
            let shared: pipeline::adblock_engine::SharedBlocker =
                StdArc::new(std::sync::RwLock::new(StdArc::clone(&blocker)));
            // Lists change on the order of days and this box stays up for weeks, so
            // re-fetch daily rather than running whatever it booted with forever.
            pipeline::filterlists::spawn_daily_refresh(list_cache.clone(), StdArc::clone(&shared));

            let program = config.browser_program();
            let app_dir = config.browser_app_dir();
            let electron = pipeline::Electron::spawn(
                &program,
                &app_dir,
                StdArc::clone(&blocker),
                // The browser's audio is captured out of the page and mixed here rather
                // than played by the browser process, so it takes an output of its own —
                // one per session, like every other source.
                Some(&audio_factory),
                pipeline::TV_USER_AGENT,
            )
            .map_err(|e| anyhow::anyhow!("browser: {e}"))?;

            let host = pipeline::ElectronHost::new(
                electron,
                pipeline::electron_browser::RespawnSpec {
                    program,
                    app_dir,
                    adblock: blocker,
                    audio_out: Some(StdArc::clone(&audio_factory)),
                    user_agent: pipeline::TV_USER_AGENT.to_string(),
                },
                nav_rx,
            );
            // If the page dies and will not come back, stop telling senders it is there.
            // DIAL answering `running` with a published screen id invites a phone to
            // attach to nothing, which is the half of a browser crash a sender can see.
            let host = {
                let tx = abandoned_tx.clone();
                host.on_recovery_failed(Arc::new(move || {
                    let _ = tx.send(());
                }))
            };
            match &config.attract_widget_url {
                Some(url) => host.with_attract_widget(url),
                None => host,
            }
        };

        // A finger on the panel's transport strip has to reach whoever is actually
        // playing — the phone over AVRCP, or Spotify's cloud. The kiosk owns the main
        // thread and the remote is an async handle on the runtime, so the two are joined
        // by a callback that hops threads.
        //
        // `issue` rather than `issue_unchecked`: the strip is built from the same
        // capability set, so a refusal here means the two have drifted, and that is worth
        // a log line rather than a command the peer silently drops.
        let controls: Option<pipeline::kiosk::ControlSink> = {
            let remote = remote.clone();
            let handle = runtime.handle().clone();
            Some(Arc::new(move |txn: castaway_core::ControlTxn| {
                let Some(peer) = remote.get() else {
                    debug!(?txn, "transport: nothing is playing, so nothing to control");
                    return;
                };
                handle.spawn(async move {
                    if let Err(e) = peer.issue(txn.clone()).await {
                        warn!(error = %e, ?txn, "transport: the source refused the control");
                    }
                });
            }))
        };

        // Presses the panel cannot answer itself. Most of the shell is local — a
        // service tile opens that service's screen with no round trip — so this only
        // sees the ones that mean "go and do something": Moonlight's tile, and the rows
        // of the pickers it opens.
        let shell_sink: Option<pipeline::kiosk::ShellSink> = {
            let handle = runtime.handle().clone();
            let shell_tx = shell_event_tx.clone();
            Some(Arc::new(move |event: pipeline::shell::ShellEvent| {
                let tx = shell_tx.clone();
                handle.spawn(async move {
                    if tx.send(event).await.is_err() {
                        debug!("shell: nothing is listening for panel presses");
                    }
                });
            }))
        };

        // Chromium's SIGINT handler used to be installed into *this* process during
        // `cef_initialize`, silently replacing ours, so this had to come after it. The
        // browser owns its own signals now.
        spawn_ctrl_c(&runtime, &shutdown, &kiosk_exit, remote);

        info!("kiosk: opening fullscreen output (close the window or ctrl-c to stop)");
        let attract = build_attract(&config);
        // No size here on purpose: the controller rasterizes each banner for whatever the
        // surface measures at the time, so it follows the panel and any resize.
        let osd_controller = OsdController::new(osd_rx);
        // The winit event loop MUST own the main thread. It no longer doubles as a
        // message pump — the browser has its own — but it still drives the per-frame
        // import and stops the subprocess when the loop exits.
        #[cfg(feature = "electron")]
        pipeline::kiosk::run_with_browser(
            rx,
            attract,
            Some(osd_controller),
            Some(kiosk_exit),
            controls,
            shell_sink,
            browser_host,
        )
        .map_err(|e| anyhow::anyhow!("kiosk: {e}"))?;
        #[cfg(not(feature = "electron"))]
        pipeline::kiosk::run(
            rx,
            attract,
            Some(osd_controller),
            Some(kiosk_exit),
            controls,
            shell_sink,
        )
        .map_err(|e| anyhow::anyhow!("kiosk: {e}"))?;
        shutdown.notify_waiters();
        // Dropping the runtime waits for every blocking task that has already started —
        // and the SponsorBlock Lounge stream is a blocking read that can sit inside its
        // 90-second timeout with nothing to interrupt it. The window is gone; nobody is
        // served by waiting. One second lets short blocking work (a screenshot encode, a
        // DNS lookup) finish, then the process leaves and the OS reclaims the rest.
        runtime.shutdown_timeout(Duration::from_secs(1));
        return Ok(());
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
        // No renderer in this build: no screenshot to take, and nothing with a position
        // to report, so the DLNA service answers `GetPositionInfo` with the spec's
        // sentinel — which is the truth here, not a shortcut.
        runtime.block_on(serve(
            config,
            event_tx,
            shutdown,
            osd,
            on_dial,
            PipelineHandles::default(),
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
#[cfg(not(feature = "electron"))]
type NoLauncher = fn(proto_dial::DialEvent);

/// What the service layer can ask of the pipeline, if there is one.
///
/// Bundled rather than passed as loose arguments because both halves have the same
/// lifecycle and the same reason for existing: they are taken *before* the pipeline is
/// moved into the session manager, because after that nothing out here holds it. A build
/// with no renderer supplies [`Default::default`] and every field is honestly absent.
/// Not `Clone`: the shell's event receiver is single-consumer, which is the honest
/// shape — two things reading panel presses would each get half of them.
#[derive(Default)]
struct PipelineHandles {
    /// What the panel is showing, for `GET /screenshot.png`.
    screenshot: Option<Screenshot>,
    /// Where the media-URL session has got to, for the protocols in which the receiver is
    /// the player and has to report its own position. Absent in a build with no decoder,
    /// which then honestly answers "no such information" rather than inventing a zero.
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
    /// Panel presses the shell could not answer itself (D38), and the channel back to
    /// the render loop for the screens they produce.
    ///
    /// Both halves travel together because they are one conversation: a press arrives,
    /// something is looked up, a screen goes back. Absent in a build with no renderer,
    /// where there is no panel to press.
    #[cfg(feature = "render")]
    shell: Option<ShellChannels>,
}

/// The shell's two ends, as seen from the service side.
#[cfg(feature = "render")]
struct ShellChannels {
    /// Presses from the panel.
    events: mpsc::Receiver<pipeline::shell::ShellEvent>,
    /// Screens to show in answer. Bounded and drop-on-full like every other render
    /// command — a shell update that cannot get through is one frame of staleness, not a
    /// reason to block the runtime.
    render: pipeline::RenderTx,
    /// What the Settings tile opens: this build's settings, ready to list and apply.
    settings: settings::Catalog,
}

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
    handles: PipelineHandles,
    // Signalled when the kiosk browser has given up on the launched page.
    mut abandoned: mpsc::UnboundedReceiver<()>,
) -> anyhow::Result<()> {
    let PipelineHandles {
        screenshot,
        playback,
        #[cfg(feature = "render")]
        shell,
    } = handles;
    let iface = config.resolved_interface();
    info!(
        name = %config.friendly_name,
        interface = %iface,
        http_port = config.http_port,
        "castaway services starting"
    );

    // Validated once, up front, and failed loudly: a broken [media_ports] range means
    // the operator asked to control where media sockets land, and booting on the
    // ephemeral fallback would silently undo that (and reopen the firewall gap the
    // range exists to close, docs/network-surface.md).
    let media_ports = config
        .media_ports
        .policy()
        .context("parsing [media_ports] in castaway.toml")?;

    let mut http = Router::new();
    let mut ssdp_devices: Vec<(SsdpDevice, String)> = Vec::new();
    let mut mdns = MdnsResponder::new().context("creating mDNS responder")?;
    // Advertise on the serving LAN only. Left to auto-detection the daemon spoke on
    // every interface, so each record also carried the Tailscale address — pickers
    // browsing both interfaces listed the receiver twice, and clients that connected
    // over the tunnel reached services that answer for the LAN.
    mdns.restrict_to(std::net::IpAddr::V4(iface))
        .context("restricting mDNS to the serving interface")?;

    if config.enable.dlna {
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "http"), event_tx.clone());
        let dlna = DlnaService::new(
            config.advertised_name(ProtocolKind::Dlna),
            &config.uuid,
            sink,
        )
        .with_osd(osd.clone());
        // DLNA is the protocol where the receiver *is* the player, so the scrubber a
        // control point draws can only be answered from our own clock.
        let dlna = match playback.clone() {
            Some(report) => dlna.with_playback(report),
            None => dlna,
        };
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
                local_file_directories: config.spotify.local_file_directories.clone(),
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
    // The panel's close badge on a demoted page. A channel rather than a callback
    // because the two ends live in different arms of this function: the badge is
    // pressed in the shell (below), and the thing it stops is the DIAL launch (here).
    // With no DIAL to stop, the receiver is simply dropped and a press goes nowhere —
    // which cannot happen on a panel, since without a browser there is no page to
    // have demoted in the first place.
    // (Unused in a build with no panel to press it on; honest rather than dead.)
    #[cfg_attr(not(feature = "render"), allow(unused_variables))]
    let (close_page_tx, mut close_page_rx) = mpsc::unbounded_channel::<()>();
    match (config.enable.dial, on_dial) {
        (true, None) => warn!(
            "DIAL disabled: this build has no kiosk browser to launch YouTube in \
             (build with `--features electron`)"
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
            // page to attach to — this arm is the browser build by construction (D27).
            #[cfg(feature = "electron")]
            if config.sponsorblock.enabled {
                // The Lounge watcher doubles as the idle watch: a screen sitting at
                // "Ready to cast" with no video for a few minutes is nobody watching
                // anything, and the panel takes itself back. Wired as a DIAL stop so
                // the whole exit path is the one a phone's stop already takes — the
                // page hides, the app state reads stopped, the screen slot clears.
                let idle_dial = dial.clone();
                let idle_tx = dial_tx.clone();
                let on_idle: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                    let dial = idle_dial.clone();
                    let tx = idle_tx.clone();
                    tokio::spawn(async move {
                        dial.abandoned().await;
                        let _ = tx.send(proto_dial::DialEvent::Stopped).await;
                    });
                });
                tokio::spawn(sponsorblock::run(
                    config.sponsorblock.clone(),
                    screen.clone(),
                    osd.clone(),
                    Some(on_idle),
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
            // The close badge takes the same exit a phone's stop button does: the page
            // hides, the app state reads stopped, the screen slot clears — and the
            // widget goes back to being the clock.
            {
                let dial = dial.clone();
                let tx = dial_tx.clone();
                tokio::spawn(async move {
                    while close_page_rx.recv().await.is_some() {
                        dial.abandoned().await;
                        let _ = tx.send(proto_dial::DialEvent::Stopped).await;
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
    // Kept so the panel's shell can ask what has been discovered and ask for a launch.
    // Unread in a build with no panel, which is honest rather than dead: the adapter
    // still runs, there is just nothing to press.
    #[cfg_attr(
        not(feature = "render"),
        allow(unused_assignments, unused_variables, unused_mut)
    )]
    let mut gamestream: Option<(
        Arc<proto_gamestream::GameStreamAdapter>,
        mpsc::Sender<proto_gamestream::GameStreamCommand>,
    )> = None;
    if config.enable.cast {
        adapter_handles.push(
            spawn_cast(
                &config,
                media_ports,
                &mut mdns,
                event_tx.clone(),
                shutdown.clone(),
                playback.clone(),
            )
            .await?,
        );
    }
    if config.enable.airplay {
        adapter_handles.push(spawn_airplay(
            &config,
            media_ports,
            &mut mdns,
            event_tx.clone(),
            shutdown.clone(),
        ));
    }
    if config.enable.gamestream {
        // The inverted protocol (D37): nothing to advertise, because the panel is the
        // client. Logged and skipped rather than fatal for the same reason as Miracast
        // and Bluetooth — an unwritable state directory should not stop a receiver that
        // can still do everything else.
        match spawn_gamestream(&config, event_tx.clone(), shutdown.clone()) {
            Ok(wiring) => {
                adapter_handles.push(wiring.task);
                // Only the panel reads these back. A build with no renderer still runs
                // the adapter — there is simply nothing to press.
                #[cfg(feature = "render")]
                {
                    gamestream = Some((wiring.adapter, wiring.commands));
                }
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "GameStream unavailable; continuing without it");
            }
        }
    }
    if config.enable.miracast {
        // Miracast has no IP discovery to register: it advertises itself in an 802.11
        // beacon, which neither the mDNS nor the SSDP responder can carry (architecture
        // §1e). A radio that cannot be a group owner is logged and skipped rather than
        // fatal, for the same reason as Bluetooth below — a receiver that can still do
        // AirPlay should not refuse to start because of a Wi-Fi driver.
        match spawn_miracast(&config, event_tx.clone(), shutdown.clone()) {
            Ok(handle) => adapter_handles.push(handle),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "Miracast unavailable; continuing without it");
            }
        }
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

    // The panel's shell: presses it could not answer itself, and the screens that answer
    // them. Only started where there is a panel — a headless build has nothing to press.
    #[cfg(feature = "render")]
    if let Some(ShellChannels {
        events,
        render,
        settings,
    }) = shell
    {
        let (adapter, commands) = match &gamestream {
            Some((a, c)) => (Some(Arc::clone(a)), c.clone()),
            // A closed sender, so a press on a tile for an adapter that never started
            // fails fast and says so on the panel rather than hanging.
            None => {
                let (tx, _rx) = mpsc::channel(1);
                (None, tx)
            }
        };
        adapter_handles.push(tokio::spawn(shell_nav::run(
            events,
            render,
            adapter,
            commands,
            settings,
            osd.clone(),
            close_page_tx.clone(),
        )));
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
    #[expect(
        clippy::disallowed_methods,
        reason = "registered: the http/tcp http_port entry in surface.rs"
    )]
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
    // The root answered 404, and the root is exactly the URL a person types after
    // reading the advertised host:port off the panel — so "working as intended" read
    // as "the receiver is down". A landing page is the cheapest health signal there
    // is; a real web control UI can replace this handler without moving the URL.
    let landing = landing_page(&config);
    let http = http.route(
        "/",
        axum::routing::get(move || {
            let page = landing.clone();
            async move { axum::response::Html(page) }
        }),
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

/// The page at `/`: who this receiver is and which surfaces it is offering.
///
/// Rendered once at startup from the loaded config — it states what this boot
/// *advertises*, which is a config fact, not live session state. Anything dynamic
/// belongs to the future control UI, not here.
fn landing_page(config: &Config) -> String {
    // The friendly name is operator input headed into markup; everything else
    // interpolated below is our own constants.
    let name = config
        .friendly_name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut services = String::new();
    for (on, label) in [
        (config.enable.cast, "Google Cast"),
        (config.enable.airplay, "AirPlay"),
        (config.enable.dlna, "DLNA MediaRenderer"),
        (config.enable.dial, "YouTube (DIAL)"),
        (config.enable.spotify, "Spotify Connect"),
        (config.enable.bluetooth, "Bluetooth audio"),
        (config.enable.miracast, "Miracast"),
        (config.enable.gamestream, "GameStream (Moonlight client)"),
    ] {
        if on {
            services.push_str(&format!("<li>{label}</li>\n"));
        }
    }
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{name} · castaway</title>\
         <style>body{{font:16px/1.5 system-ui,sans-serif;max-width:38rem;\
         margin:3rem auto;padding:0 1rem;background:#111;color:#eee}}\
         a{{color:#8cf}}h1{{font-size:1.4rem}}li{{margin:.2rem 0}}</style>\
         </head><body>\
         <h1>{name}</h1>\
         <p>castaway {version} is up. This box accepts:</p>\
         <ul>{services}</ul>\
         <p><a href=\"/screenshot.png\">What the panel is showing right now</a></p>\
         </body></html>",
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// Stand up the CASTv2 TLS listener, advertise what it asks for, and run it until
/// `shutdown`. Returns the actor's join handle so shutdown can wait on it.
async fn spawn_cast(
    config: &Config,
    media_ports: castaway_core::MediaPorts,
    mdns: &mut MdnsResponder,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    // Three ways to be a Cast device, in descending order of how much they are ours.
    // A provisioned credential is the operator's own hardware identity and wins; a CKS
    // credential is real but shared and revocable; a generated key is correct and
    // rejected by every official sender.
    let (identity, cks_refresh) = match config
        .cast
        .credential
        .load()
        .context("loading the configured Cast device credential")?
    {
        Some(credential) => {
            info!("Cast device auth uses the provisioned device credential");
            let signer = CastDeviceSigner::from_pkcs8_pem(
                &credential.key_pem,
                credential.certificate_der,
                credential.intermediates_der,
            )
            .context("parsing the provisioned Cast device key")?;
            (
                CastIdentity::device_key(self_signed_tls()?, Arc::new(signer)),
                None,
            )
        }
        None if config.cast.replay.enabled => {
            let provider = Arc::new(
                ReplayProvider::resolve(ReplayConfig {
                    network: config.cast.replay.network,
                    identity_order: config.cast.replay.identity_order.clone(),
                    // `None` is what disables the endpoint: the provider treats a
                    // missing path as "this identity is table-only".
                    airserver_db_path: config
                        .cast
                        .replay
                        .airserver_live
                        .then(cast_replay::airserver_api::default_db_path),
                    ..ReplayConfig::default()
                })
                .await
                .context("resolving a CKS Cast credential")?,
            );
            let identity = CastIdentity::replay(Arc::clone(&provider));
            info!("Cast device auth uses {}", identity.describe());
            (identity, Some(provider))
        }
        None => {
            // RSA-2048 keygen takes seconds; it belongs on a blocking thread, not stalling
            // the runtime while the other adapters are trying to come up (ground rule 4).
            let dev = tokio::task::spawn_blocking(CastDeviceSigner::generate_dev)
                .await
                .context("joining Cast device-key generation")?
                .context("generating the Cast device key")?;
            warn!(
                "Cast device auth uses a self-generated dev credential; senders that verify \
                 the Google chain — which is every official one — will reject it. Set \
                 cast.credential in castaway.toml to provision a real one, or re-enable \
                 cast.cks (Q2/Q11)"
            );
            (
                CastIdentity::device_key(self_signed_tls()?, Arc::new(dev.signer)),
                None,
            )
        }
    };

    let receiver = CastReceiver::new(
        proto_cast::actor::default_listen_addr(),
        config.advertised_name(ProtocolKind::Cast).as_str(),
        config.uuid.replace('-', ""),
        identity,
        media_ports,
    )
    .context("building the CASTv2 receiver")?;
    // Cast is the other protocol in which the receiver is the player, so a sender's
    // scrubber can only be answered from our own clock — the same seam DLNA reads.
    let receiver = match playback {
        Some(report) => receiver.with_playback(report),
        None => receiver,
    };

    advertise_adapter(&receiver, mdns);
    info!("enabled: Google Cast (CASTv2 media-URL LOAD)");

    // The listener adapter's own tag; each accepted sender is retagged with its peer.
    let sink = SessionSink::new(SourceId::new(ProtocolKind::Cast, "listener"), event_tx);
    let adapter = Arc::new(receiver);
    Ok(tokio::spawn(async move {
        // A CKS credential expires every two days, so something has to replace it while
        // the panel runs. Without one there is nothing to refresh, and this arm simply
        // never resolves.
        let refresh = async move {
            match cks_refresh {
                Some(provider) => provider.run().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            res = adapter.run(sink) => {
                if let Err(e) = res {
                    warn!(error = %e, "Cast adapter exited");
                }
            }
            () = refresh => {}
            () = shutdown.notified() => info!("Cast listener stopping"),
        }
    }))
}

/// The self-signed TLS identity the non-CKS paths present.
///
/// A CKS credential brings its own certificate — the one its signature covers —
/// so this is only reached when the receiver signs for itself.
fn self_signed_tls() -> anyhow::Result<TlsIdentity> {
    TlsIdentity::self_signed(&["castaway.local".to_string()])
        .context("generating the Cast TLS identity")
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

/// Bring up the Wi-Fi Direct group and run the Miracast sink until `shutdown`.
///
/// Nothing is registered with the shared responders: the advertisement is an 802.11
/// beacon that wpa_supplicant transmits, not a service record. The failure this returns
/// early on is a capability set that cannot be built, which is a configuration error
/// rather than a radio one — the radio's own failures surface inside the backend, where
/// they can name the driver.
/// Start the GameStream client: browse for hosts, hold the pairing store, and act on
/// commands. Returns the actor's join handle so shutdown can wait on it.
///
/// Unlike every other adapter here this one advertises nothing and waits for no sender
/// — it dials out (D37). Its only source of intent today is this config, which can ask
/// it to pair with a host at startup and to begin streaming from one; the command
/// channel it is built with is the seam a panel-side chooser would drive instead.
/// Its two extra halves are read only by the panel's shell; a build with no renderer
/// still runs the adapter and simply has nothing to press.
#[cfg_attr(not(feature = "render"), allow(dead_code))]
struct GameStreamWiring {
    task: tokio::task::JoinHandle<()>,
    /// Held so the shell can ask it what it has discovered, and so its command channel
    /// stays open for presses that arrive long after startup.
    adapter: Arc<proto_gamestream::GameStreamAdapter>,
    commands: mpsc::Sender<proto_gamestream::GameStreamCommand>,
}

fn spawn_gamestream(
    config: &Config,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<GameStreamWiring> {
    let gs = &config.gamestream;
    let store = proto_gamestream::PairingStore::new(gs.state_dir.clone());
    let prefs = proto_gamestream::SessionPreferences {
        width: gs.width,
        height: gs.height,
        fps: gs.fps,
        bitrate_kbps: gs.bitrate_kbps,
        optimize_settings: gs.optimize_settings,
        play_audio_on_host: gs.play_audio_on_host,
        allow_hevc: gs.allow_hevc,
    };
    // Deep enough that a startup pair and a startup start both queue without the
    // adapter having to be running yet.
    let (command_tx, command_rx) = mpsc::channel(8);
    let adapter = Arc::new(
        proto_gamestream::GameStreamAdapter::new(store, prefs, command_rx)
            .context("loading the GameStream client identity")?,
    );

    // A half-configured pairing is an error rather than a silent no-op: someone who
    // set one of these two meant to pair, and starting without it looks identical to
    // success until the first session is attempted.
    match (&gs.pair_host, &gs.pair_pin) {
        (Some(host), Some(pin)) => {
            command_tx
                .try_send(proto_gamestream::GameStreamCommand::Pair {
                    host: host.clone(),
                    pin: pin.clone(),
                })
                .context("queueing the configured GameStream pairing")?;
        }
        (None, None) => {}
        (Some(_), None) => anyhow::bail!(
            "gamestream.pair_host is set but pair_pin is not; pairing needs the PIN that \
             will be typed into the host's own UI"
        ),
        (None, Some(_)) => anyhow::bail!(
            "gamestream.pair_pin is set but pair_host is not; there is no host to pair with"
        ),
    }

    if let Some(host) = &gs.autostart_host {
        command_tx
            .try_send(proto_gamestream::GameStreamCommand::Start {
                host: host.clone(),
                app: gs.autostart_app.clone(),
            })
            .context("queueing the configured GameStream session")?;
    }

    info!(
        state_dir = %gs.state_dir.display(),
        "enabled: GameStream client (browsing for Sunshine hosts)"
    );
    let sink = SessionSink::new(SourceId::new(ProtocolKind::GameStream, "client"), event_tx);
    let running = Arc::clone(&adapter);
    let task = tokio::spawn(async move {
        tokio::select! {
            res = Arc::clone(&running).run(sink) => {
                if let Err(e) = res {
                    warn!(error = %e, "GameStream adapter exited");
                }
            }
            () = shutdown.notified() => info!("GameStream client stopping"),
        }
    });
    // The command sender goes back to the caller rather than being held here: the panel
    // sends on it whenever someone presses a host, which is long after startup.
    Ok(GameStreamWiring {
        task,
        adapter,
        commands: command_tx,
    })
}

fn spawn_miracast(
    config: &Config,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    // D35 split the protocol from the radio precisely because they fail for unrelated
    // reasons: `proto-miracast` is portable and fixture-tested, while bringing up the
    // Wi-Fi Direct group is driver-specific and exists only for Linux
    // (`LinuxMiracastBackend` is `#[cfg(unix)]`). Refusing here rather than failing to
    // compile keeps the Windows artifact buildable, and the caller already treats this
    // as "log it and carry on" — a receiver that can still do AirPlay should not refuse
    // to start over a backend nobody has written yet.
    #[cfg(not(unix))]
    {
        let _ = (config, event_tx, shutdown);
        anyhow::bail!(
            "Miracast has no backend on this platform: only the Linux Wi-Fi Direct group \
             owner is implemented (see docs/miracast-protocol-notes.md §7 and D35)"
        )
    }
    #[cfg(unix)]
    {
        let caps = proto_miracast::SinkCapabilities::sink_default(config.miracast.rtp_port)
            .context("building the Miracast sink capabilities")?;
        let group = proto_miracast::GroupSubnet::parse(&config.miracast.group_cidr)
            .context("parsing [miracast] group_cidr")?;
        let backend = Arc::new(proto_miracast::LinuxMiracastBackend::new(
            proto_miracast::P2pConfig {
                control_dir: config.miracast.control_dir.clone().into(),
                interface: config.miracast.interface.clone(),
                device_name: config.advertised_name(ProtocolKind::Miracast),
                freq_mhz: config.miracast.freq_mhz,
                max_throughput_mbps: config.miracast.max_throughput_mbps,
                group,
            },
            caps,
        ));
        let adapter = Arc::new(MiracastAdapter::new(
            backend,
            config.advertised_name(ProtocolKind::Miracast),
        ));
        info!(
            interface = %config.miracast.interface,
            rtp_port = config.miracast.rtp_port,
            "enabled: Miracast (Wi-Fi Direct group owner)"
        );
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Miracast, "p2p"), event_tx);
        Ok(tokio::spawn(async move {
            tokio::select! {
                res = Arc::clone(&adapter).run(sink) => {
                    if let Err(e) = res {
                        warn!(error = %e, "Miracast adapter exited");
                    }
                }
                () = shutdown.notified() => info!("Miracast stopping"),
            }
        }))
    }
}

/// Stand up the AirPlay/RAOP RTSP listeners, advertise what they ask for, and run them
/// until `shutdown`. Returns the actor's join handle so shutdown can wait on it.
fn spawn_airplay(
    config: &Config,
    media_ports: castaway_core::MediaPorts,
    mdns: &mut MdnsResponder,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    let receiver = AirPlayReceiver::new(
        proto_airplay::AirPlayIdentity {
            name: config.advertised_name(ProtocolKind::AirPlay),
            device_id: derive_mac(&config.uuid),
            host: MDNS_HOST.to_string(),
            // `pi` is a stable per-protocol UUID, which is what every real receiver
            // advertises; reusing the device id here is the Roku/Samsung outlier behaviour.
            pairing_id: device_uuid(&config.uuid, "airplay"),
            offer_hevc: config.airplay.offer_hevc,
            mirror_height: config.airplay.mirror_height,
        },
        media_ports,
    );

    advertise_adapter(&receiver, mdns);
    // The warning that used to follow this line — "no media plane is implemented yet" —
    // outlived the media plane by several milestones and had people debugging the
    // advertisement for a limitation that no longer existed.
    info!("enabled: AirPlay (audio + mirroring, RTSP control on 7000)");

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
/// The Home screen's model — what the panel shows when nothing is casting.
///
/// Returns the *scene*, not pixels: the render thread draws it at the true surface size,
/// so the panel is no longer handed a 3840x2160 bitmap to stretch (D38).
fn build_attract(config: &Config) -> Option<pipeline::attract::AttractScene> {
    use pipeline::attract::{AttractScene, ServiceDetail, Tile, TileGlyph, WidgetSlot};

    let name = &config.friendly_name;
    // Every tile names the entry that surface actually publishes, not the bare friendly
    // name: one box appears in several pickers at once, and since each advertises
    // `name#protocol` the bare name is a string nobody will find.
    let advertised = |kind: ProtocolKind| config.advertised_name(kind).to_string();

    // A tile per enabled service. The instructions live *behind* the tile rather than on
    // the idle screen, because six protocols' worth of them was the first thing anyone
    // saw and none of it was about what they were doing.
    let service =
        |id: &str, label: &str, glyph, accent, headline: &str, steps: Vec<String>, kind| Tile {
            id: id.to_string(),
            label: label.to_string(),
            glyph,
            accent,
            detail: Some(ServiceDetail {
                headline: headline.to_string(),
                steps,
                advertised: Some(advertised(kind)),
            }),
        };

    let mut tiles = Vec::new();
    if config.enable.cast {
        tiles.push(service(
            "cast",
            "Google Cast",
            TileGlyph::Cast,
            [0x42, 0x85, 0xf4, 0xff],
            "Cast a tab, or your whole screen.",
            vec![
                "Open Chrome or Edge".into(),
                "Menu \u{2192} Cast, or the cast button in a video".into(),
            ],
            ProtocolKind::Cast,
        ));
    }
    if config.enable.airplay {
        tiles.push(service(
            "airplay",
            "AirPlay",
            TileGlyph::AirPlay,
            [0xff, 0xff, 0xff, 0xff],
            "Mirror an iPhone, iPad or Mac.",
            vec![
                "Control Centre \u{2192} Screen Mirroring".into(),
                "Or the AirPlay button in a video or track".into(),
            ],
            ProtocolKind::AirPlay,
        ));
    }
    if config.enable.dlna {
        tiles.push(service(
            "dlna",
            "DLNA",
            TileGlyph::Dlna,
            [0x3d, 0xdc, 0x84, 0xff],
            "Send a video from Android or VLC.",
            vec![
                "VLC \u{2192} the cast button".into(),
                "Or any app offering \"Play on\" or \"Cast to device\"".into(),
            ],
            ProtocolKind::Dlna,
        ));
    }
    if config.enable.spotify {
        tiles.push(service(
            "spotify",
            "Spotify",
            TileGlyph::Spotify,
            [0x1d, 0xb9, 0x54, 0xff],
            "Play to the room, and keep your phone as the remote.",
            vec!["Play something".into(), "Tap Devices, bottom-left".into()],
            ProtocolKind::Spotify,
        ));
    }
    if config.enable.dial {
        tiles.push(service(
            "youtube",
            "YouTube",
            TileGlyph::YouTube,
            [0xff, 0x00, 0x00, 0xff],
            "The cast button in the YouTube app.",
            vec!["Tap it, and pick this screen".into()],
            ProtocolKind::YouTubeLounge,
        ));
    }
    if config.enable.bluetooth {
        tiles.push(service(
            "bluetooth",
            "Bluetooth",
            TileGlyph::Bluetooth,
            [0x00, 0x82, 0xfc, 0xff],
            "Pair a phone and play straight to the room.",
            vec![
                "Settings \u{2192} Bluetooth".into(),
                "Pick this screen".into(),
            ],
            ProtocolKind::Bluetooth,
        ));
    }
    if config.enable.miracast {
        tiles.push(service(
            "miracast",
            "Miracast",
            TileGlyph::Miracast,
            [0x00, 0xa4, 0xef, 0xff],
            "Project a Windows desktop with no cable.",
            vec!["Press Win+K".into(), "Pick this screen".into()],
            ProtocolKind::Miracast,
        ));
    }
    if config.enable.gamestream {
        // The one tile with no instructions, because it is the one where the panel goes
        // and does something rather than waiting to be sent something. Pressing it opens
        // a host picker (D38), so there is nothing to tell anyone here.
        tiles.push(Tile {
            id: "gamestream".to_string(),
            label: "Moonlight".to_string(),
            glyph: TileGlyph::Moonlight,
            accent: [0x02, 0xab, 0xfc, 0xff],
            detail: None,
        });
    }

    // Settings, last and always: the receiver configures itself from its own glass, and
    // a build with nothing to configure says so on the settings screen rather than by
    // not having one. Like Moonlight's, a press is the app's to answer — it opens the
    // settings menu (shell_nav), not an instructions card.
    tiles.push(Tile {
        id: "settings".to_string(),
        label: "Settings".to_string(),
        glyph: TileGlyph::Gear,
        accent: [0x9a, 0xa3, 0xb2, 0xff],
        detail: None,
    });

    // Reserve the widget card only if something will actually paint into it: with no
    // browser build (or no URL configured) the text should use the full width rather than
    // frame a permanently empty panel.
    let widget = match (cfg!(feature = "electron"), &config.attract_widget_url) {
        (true, Some(_)) => WidgetSlot::RightCard,
        _ => WidgetSlot::None,
    };
    let scene = AttractScene {
        title: name.clone(),
        tiles,
        // What it is, which build, and where to reach it. Nothing else: the idle screen
        // used to carry a tagline and a line of instructions, and neither was information
        // anyone standing in front of the panel needed.
        footer: format!(
            "castaway  •  {}  •  {}",
            env!("CASTAWAY_GIT_REV"),
            config.http_base_url().replace("http://", "")
        ),
        widget,
        // What today is, asked once at build time rather than baked: the panel is up for
        // weeks, so the screen has to be able to change season without a restart — which
        // it does, because Home is rebuilt whenever the receiver's state changes.
        season: seasonal_accent(config.theme),
        mascot: true,
    };
    Some(scene)
}

/// What season it is, from the wall clock (#24). Only where there is a screen to
/// decorate.
///
/// Wall clock rather than a config flag: nobody is going to remember to turn Pride on in
/// June and off in July, and a decoration that needs an edit is a decoration that never
/// appears.
#[cfg(feature = "render")]
fn seasonal_accent(choice: pipeline::theme::ThemeChoice) -> Option<pipeline::theme::Season> {
    use std::time::{SystemTime, UNIX_EPOCH};
    // A forced choice needs no calendar, which also means a clock this box cannot read
    // does not stop someone asking for a palette outright.
    if let Some(forced) = choice.forced() {
        return Some(forced);
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    // Civil date from a Unix timestamp, Howard Hinnant's algorithm. A date crate for
    // three lines of arithmetic that never has to handle a timezone is not worth the
    // dependency — the panel's seasons are day-grained and it is on UTC.
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let _ = era;
    choice.resolve(month, day)
}

/// Derive a stable MAC-style id from the UUID (AirPlay wants a `AA:BB:..` device id).
///
/// The first octet is forced to a **locally-administered unicast** address. Taking UUID
/// hex verbatim leaves the low bit of octet 0 — the multicast bit — set about half the
/// time, and a multicast address is not a legal device identity. It does not have to
/// match a real interface (UxPlay generates a random one when it cannot find the real
/// MAC), but it does have to be a syntactically valid MAC, and it has to be stable:
/// a collision with another instance is reported by the responder as a name conflict.
fn derive_mac(uuid: &str) -> String {
    let hex: String = uuid
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(12)
        .collect();
    let padded = format!("{hex:0<12}");
    let mut octets: Vec<String> = padded
        .as_bytes()
        .chunks(2)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect();
    if let Some(first) = octets.first_mut() {
        // Clear the multicast bit, set the locally-administered bit.
        let byte = u8::from_str_radix(first, 16).unwrap_or(0) & 0xFE | 0x02;
        *first = format!("{byte:02x}");
    }
    octets.join(":").to_uppercase()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::device_uuid;

    /// The page at `/` names the box, says what is on, and omits what is off —
    /// with the operator's name HTML-escaped on its way into markup.
    #[test]
    fn the_landing_page_states_the_advertised_surface() {
        let mut config = crate::config::Config {
            friendly_name: "Lab <TV> & friends".to_owned(),
            ..Default::default()
        };
        config.enable.miracast = false;
        let page = super::landing_page(&config);
        assert!(page.contains("Lab &lt;TV&gt; &amp; friends"));
        assert!(!page.contains("Lab <TV>"), "raw markup must not survive");
        assert!(page.contains("Google Cast"));
        assert!(
            !page.contains("Miracast"),
            "a disabled surface is not listed"
        );
        assert!(page.contains("/screenshot.png"));
    }

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
