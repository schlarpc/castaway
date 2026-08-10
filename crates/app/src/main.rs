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
mod instance;
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
// `/stream/*`: the composited output, live, in a browser (#101). Always compiled — a build
// with no encoder still answers, saying why.
mod remote_http;
mod stream_http;

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
use tracing::{debug, error, info, warn};

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
    // The machine's UTC offset, read here because this is the last moment it can be: a
    // holiday is a local-date concept (#263) — Halloween starts when it gets dark *here*,
    // not at an hour picked by the box's longitude — and on unix the offset can only be
    // read soundly while the process is single-threaded, which stops being true the
    // moment the runtime is built. A box that will not say (no tz database, exotic
    // container) decorates on UTC dates, which is what it did before it was asked.
    #[cfg(feature = "render")]
    let utc_offset_secs: i32 =
        time::UtcOffset::current_local_offset().map_or(0, time::UtcOffset::whole_seconds);
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
    // Before the runtime, the kiosk window and the browser child — all three of which used
    // to come up on a doubled start, leaving a second full-screen kiosk that discovered
    // nothing and answered nothing (#100). The port bind that would have said so happens
    // far too late and inside a task that only warns.
    //
    // Keyed on the state directory, because that is what actually collides: the link-key
    // file, the pairing store and the browser profile are all under it, and an operator
    // who deliberately points a second castaway at a second state directory is running a
    // genuinely separate receiver.
    //
    // Held for the life of the process. Dropping it releases the lock.
    let _instance = match instance::acquire(&config.state_dir()) {
        Ok(lock) => {
            debug!(path = %lock.path().display(), "instance lock");
            lock
        }
        Err(e @ instance::InstanceError::AlreadyRunning { .. }) => {
            // Both, deliberately. stderr is for whoever typed the command; the log is for
            // the panel, where there is no terminal and the service manager swallows it.
            error!("{e}");
            // No prefix: the error already names the program, and "castaway: castaway is
            // already running" is what adding one produces.
            eprintln!("{e}");
            std::process::exit(1);
        }
        // A lock file that cannot be opened at all is an operator problem, not a second
        // launch, and must not be reported as one.
        Err(e) => return Err(anyhow::Error::new(e).context("taking the instance lock")),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    let (event_tx, event_rx) = mpsc::channel::<SourceMessage>(64);
    let shutdown = Arc::new(Notify::new());
    // A fullscreen kiosk has no window chrome, so ctrl-c must also stop the winit loop;
    // the flag is checked on every wake, and setting it comes with a wake (#59).
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
        // The kiosk loop sleeps between frames (#59); everything outside the render
        // channel that queues work for it wakes it through clones of this. The kiosk
        // arms it once the event loop exists.
        let wake = render_pipeline.waker();
        // Where a remote peer's contacts land on their way to the main thread (#18). Made
        // here, beside the waker, because it is the same kind of thing: a producer off
        // this thread queues work and wakes the loop, and the loop drains when it runs.
        let remote_input = Arc::new(input_touch::RemoteInputQueue::new(wake.clone()));
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
        // The panel's one audio output (#111). It owns the device; every source — every
        // protocol session, and the browser's captured page audio — takes a `MixInput`
        // from it and the mixer sums them. It is also what makes the selection above
        // reach a *live* session: the mixer reopens under everything at once, rather than
        // each source keeping whatever device it happened to open with.
        #[cfg(feature = "audio")]
        let mixer = Arc::new(pipeline::mixer::AudioMixer::with_gain(
            pipeline::audio_out::output_factory(audio_selector.clone()),
            render_pipeline.gain(),
        ));
        // A copy of the mix on disk, if the config asked for one. A tap rather than an
        // output, so the speakers keep working while it records (#186).
        #[cfg(feature = "audio")]
        record_the_mix(&config, &mixer);
        // The output stream's audio (#101), as a tap on the mixer: the samples the
        // speakers were given, not a reconstruction of them. A session cannot be added
        // later that reaches the speakers and misses the stream, because the mixer is the
        // only way to reach the speakers.
        #[cfg(all(feature = "audio", feature = "stream"))]
        let stream_audio = Arc::new(pipeline::stream::StreamAudio::new());
        #[cfg(all(feature = "audio", feature = "stream"))]
        mixer.add_tap(stream_audio.tap());
        #[cfg(feature = "audio")]
        let render_pipeline = render_pipeline.with_mixer(Arc::clone(&mixer));
        // The music visualiser (#15): a second tap on the same mixer the output stream
        // taps, so the bars are drawn from the samples the speakers were given rather than
        // from one source's idea of itself. Attached at both ends — the mixer feeds it,
        // the render loop reads it — because those are opposite ends of the audio path and
        // neither should have to know about the other.
        #[cfg(all(feature = "audio", feature = "render"))]
        let visualizer = {
            let analyzer = Arc::new(pipeline::visualizer::Analyzer::new());
            mixer.add_tap(Arc::clone(&analyzer) as Arc<dyn pipeline::mixer::MixTap>);
            Some(analyzer)
        };
        // A second handle on the render channel, for the shell: it pushes screens in
        // answer to panel presses, and the pipeline itself is about to be moved into the
        // session manager.
        let render_tx = render_pipeline.commands();
        // Panel presses the shell could not answer locally. Small: these are taps by a
        // person, not a stream.
        let (shell_event_tx, shell_event_rx) = mpsc::channel::<pipeline::shell::ShellEvent>(8);
        let shot_handle = render_pipeline.screenshot_handle();
        // Taken here for the same reason as the screenshot handle, and just as inert: no
        // encoder is opened and no frame is read back until `/stream/live.m3u8` is asked
        // for (#101).
        #[cfg(all(feature = "audio", feature = "stream"))]
        let stream_sound = Some(Arc::clone(&stream_audio));
        // A build with no audio path streams pictures only, which is a whole stream.
        #[cfg(all(not(feature = "audio"), feature = "stream"))]
        let stream_sound = None;
        #[cfg(feature = "stream")]
        let stream_handle =
            render_pipeline.stream_handle(pipeline::stream::StreamConfig::default(), stream_sound);
        // Taken here for the same reason as the screenshot handle: after the pipeline is
        // moved into the session manager, nothing out here holds it, and the DLNA service
        // that has to answer "how far through is this" is built inside `serve`.
        let playback: Arc<dyn castaway_core::PlaybackReport> =
            Arc::new(render_pipeline.playback_handle());
        // The panel starts where the operator said. Set once, here, because `Gain`
        // outlives every session — a level is the panel's, not a source's (#86). The
        // browser no longer needs a handle of its own: it writes into the mixer, and the
        // mixer applies this to the sum (#111).
        //
        // Both numbers, because they are the two that are 30 dB apart in the middle and
        // look alike written down (#85). A log with only the position cannot tell you the
        // panel came up inaudible, which is exactly what it did not tell anyone (#178).
        #[cfg(feature = "audio")]
        {
            let start = castaway_core::Volume::from_position(config.initial_volume);
            render_pipeline.gain().set(start);
            info!(
                position = config.initial_volume,
                amplitude = start.amplitude(),
                "output volume starts where the config says"
            );
        }

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
            let release_wake = wake.clone();
            render_pipeline.set_screen_release(Arc::new(move || {
                let _ = release_tx.send(pipeline::BrowserCommand::Hide);
                release_wake.wake();
            }));
        }

        // A hosted Cast application taking the panel (#16). The same channel DIAL's
        // launch uses, reached through the session manager rather than around it — which
        // is what makes a hosted page preempt whatever was playing instead of covering it.
        #[cfg(feature = "electron")]
        {
            let host_tx = nav_tx.clone();
            let host_wake = wake.clone();
            render_pipeline.set_page_host(Arc::new(move |page: castaway_core::HostedPage| {
                let _ = host_tx.send(pipeline::BrowserCommand::Navigate(page.url));
                host_wake.wake();
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
        // Taken before `run` consumes the manager: it is the only route from a session
        // that can be driven off the glass — a Miracast source with UIBC — to the loop
        // that owns the glass (#125).
        let touch_surface = manager.touch_handle();
        runtime.spawn(manager.run(event_rx));

        // Cloned before the DIAL closure takes ownership: Matter's launches go to the
        // same browser, and so does the Cast platform shim.
        #[cfg(feature = "electron")]
        let nav_tx_matter = nav_tx.clone();
        #[cfg(feature = "electron")]
        let nav_tx_cast = nav_tx.clone();
        #[cfg(feature = "electron")]
        let on_dial = {
            let dial_wake = wake.clone();
            move |event: proto_dial::DialEvent| {
                match event {
                    proto_dial::DialEvent::Launched(params) => {
                        let url = params.leanback_url();
                        info!(%url, "DIAL launch: navigating kiosk browser");
                        let _ = nav_tx.send(pipeline::BrowserCommand::Navigate(url));
                    }
                    proto_dial::DialEvent::Stopped => {
                        info!("DIAL stop: hiding kiosk browser");
                        let _ = nav_tx.send(pipeline::BrowserCommand::Hide);
                    }
                }
                dial_wake.wake();
            }
        };
        #[cfg(feature = "electron")]
        let on_dial = Some(on_dial);

        // Matter's browser launches ride the same navigation channel DIAL's do — one
        // browser, one place that drives it — but arrive as a stream rather than a
        // callback, because they cross the thread `rs-matter` runs on.
        #[cfg(feature = "electron")]
        let browser_launch_tx = {
            let (tx, mut rx) = mpsc::unbounded_channel::<proto_matter::BrowserLaunch>();
            let nav = nav_tx_matter;
            let matter_wake = wake.clone();
            runtime.spawn(async move {
                while let Some(launch) = rx.recv().await {
                    info!(url = %launch.url, "Matter launch: navigating kiosk browser");
                    let _ = nav.send(pipeline::BrowserCommand::Navigate(launch.url));
                    matter_wake.wake();
                }
            });
            Some(tx)
        };
        #[cfg(not(feature = "electron"))]
        let browser_launch_tx: Option<mpsc::UnboundedSender<proto_matter::BrowserLaunch>> = None;
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
        // The remote-control transport (#18). Built here because this is where all three
        // of its halves exist at once: the encoder's live fan-out, the input queue the
        // kiosk drains, and the handle that starts the tap.
        // The UDP range ICE binds, allocated once for the whole process. Both WebRTC
        // users draw from it — the remote-control page's peers (#18) and FCast's
        // mirroring receiver (#248) — and they must share the *allocator*, not just the
        // numbers: two pools over one range hand out the same port and the second bind
        // fails at the moment a real peer connects.
        #[cfg(feature = "remote")]
        let ice_ports = std::sync::Arc::new(pipeline::ice_ports::PortPool::new(
            config.remote.ice_ports.first,
            config.remote.ice_ports.last,
        ));
        // The serving interface *and* loopback, for both. A peer pairs one of our
        // candidates with one of its own, and a browser never offers a loopback
        // candidate — it gathers its real interfaces and stops. So a browser open on the
        // panel itself would have nothing to pair against if we offered only 127.0.0.1,
        // and nothing to pair against if we offered only the LAN address while it sat on
        // loopback.
        #[cfg(feature = "remote")]
        let ice_bind_ips = {
            let lan = std::net::IpAddr::V4(config.resolved_interface());
            let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
            if lan == loopback {
                vec![lan]
            } else {
                vec![lan, loopback]
            }
        };
        #[cfg(feature = "remote")]
        let remote_service = if config.remote.enable {
            let starter = stream_handle.clone();
            pipeline::remote::RemoteService::new(
                pipeline::remote::RemoteConfig {
                    ice_ports: std::sync::Arc::clone(&ice_ports),
                    bind_ips: ice_bind_ips.clone(),
                    accept_input: config.remote.input,
                },
                std::sync::Arc::clone(stream_handle.stream().feed()),
                std::sync::Arc::clone(&remote_input),
                std::sync::Arc::new(move || starter.ensure_running()),
            )
            .inspect_err(|e| warn!(error = %e, "remote control is unavailable"))
            .ok()
        } else {
            info!("remote control is disabled by config");
            None
        };
        // The clock, read here at the boundary; `seasonal_accent` itself is pure (#263).
        // Resolved before `ShellChannels` is handed over, because `serve` rebuilds Home
        // when FCast learns its pairing URL (#248), and that rebuild must not drop the
        // season the first paint had.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let season = seasonal_accent(config.theme, now_secs, utc_offset_secs);
        // Where a v4 FCast sender's screen arrives (#248). Independent of
        // `[remote] enable`, which governs whether the panel's own picture goes *out*:
        // this is a casting protocol's media plane, gated by `enable.fcast` like the rest
        // of FCast, and its absence is what makes the advertised capability honest.
        #[cfg(feature = "remote")]
        let mirror_backend: Option<std::sync::Arc<dyn castaway_core::MirrorBackend>> = if config
            .enable
            .fcast
        {
            pipeline::mirror_in::MirrorReceiver::new(pipeline::mirror_in::MirrorReceiverConfig {
                ice_ports: std::sync::Arc::clone(&ice_ports),
                bind_ips: ice_bind_ips,
            })
            .inspect_err(|e| warn!(error = %e, "FCast mirroring is unavailable"))
            .ok()
            .map(|receiver| receiver as std::sync::Arc<dyn castaway_core::MirrorBackend>)
        } else {
            None
        };
        let handles = PipelineHandles {
            screenshot: Some(shot_handle),
            #[cfg(feature = "stream")]
            stream: Some(stream_handle),
            #[cfg(not(feature = "stream"))]
            stream: None,
            #[cfg(feature = "remote")]
            remote: remote_service,
            #[cfg(not(feature = "remote"))]
            remote: None,
            playback: Some(playback),
            #[cfg(feature = "remote")]
            mirror: mirror_backend,
            #[cfg(not(feature = "remote"))]
            mirror: None,
            #[cfg(feature = "electron")]
            cast_platform_shim: Some({
                let tx = nav_tx_cast;
                let shim_wake = wake.clone();
                Arc::new(move |port| {
                    let _ = tx.send(pipeline::BrowserCommand::CastPlatform(port));
                    shim_wake.wake();
                })
            }),
            shell: Some(ShellChannels {
                events: shell_event_rx,
                render: render_tx.clone(),
                settings: settings_catalog,
                season,
            }),
        };
        runtime.spawn(async move {
            if let Err(e) = serve(
                serve_cfg,
                serve_tx,
                serve_shutdown,
                serve_osd,
                Launchers {
                    dial: on_dial,
                    browser: browser_launch_tx,
                },
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
            // path and installs the engine behind the shared cell when it lands. The
            // browser is handed the *cell* and reads it at every query, so an install —
            // the startup fetch and every daily one after it — is live at once rather
            // than at the next process start (#239).
            let list_cache = pipeline::filterlists::CachePaths::default();
            let adblock = pipeline::adblock_engine::SharedBlocker::new(
                pipeline::filterlists::load_cached_only(&list_cache)
                    .unwrap_or_else(pipeline::adblock_engine::AdBlocker::with_defaults),
            );
            // Lists change on the order of days and this box stays up for weeks, so
            // re-fetch daily rather than running whatever it booted with forever.
            pipeline::filterlists::spawn_daily_refresh(list_cache, adblock.clone());

            let program = config.browser_program();
            let app_dir = config.browser_app_dir();
            let electron = pipeline::Electron::spawn(
                &program,
                &app_dir,
                adblock.clone(),
                // The browser's audio is captured out of the page and mixed here rather
                // than played by the browser process, so it joins the mix as one more
                // source. It used to take a device of its own, which is how page audio
                // came to bypass the panel's volume entirely (#86).
                Some(&mixer),
                pipeline::TV_USER_AGENT,
                wake.clone(),
            )
            .map_err(|e| anyhow::anyhow!("browser: {e}"))?;

            let host = pipeline::ElectronHost::new(
                electron,
                pipeline::electron_browser::RespawnSpec {
                    program,
                    app_dir,
                    adblock,
                    mixer: Some(StdArc::clone(&mixer)),
                    user_agent: pipeline::TV_USER_AGENT.to_string(),
                    waker: wake.clone(),
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
        spawn_ctrl_c(&runtime, &shutdown, &kiosk_exit, wake.clone(), remote);

        info!("kiosk: opening fullscreen output (close the window or ctrl-c to stop)");
        // Built before the runtime has loaded the FCast identity, so it has no QR yet;
        // `serve` sends a fuller Home the moment it does (#248).
        let attract = build_attract(&config, season, &HomeState::default());
        // `auto` follows the calendar, so the calendar has to be watched; a forced or
        // plain palette never changes, and gets no task to not-change it with.
        if config.theme == pipeline::theme::ThemeChoice::Auto {
            if let Some(scene) = attract.clone() {
                runtime.spawn(seasonal_rollover(
                    config.theme,
                    utc_offset_secs,
                    scene,
                    render_tx.clone(),
                ));
            }
        }
        // No size here on purpose: the controller rasterizes each banner for whatever the
        // surface measures at the time, so it follows the panel and any resize.
        let osd_controller = OsdController::new(osd_rx);
        // The winit event loop MUST own the main thread. It no longer doubles as a
        // message pump — the browser has its own — but it still drives the per-frame
        // import and stops the subprocess when the loop exits.
        let wiring = pipeline::kiosk::KioskWiring {
            attract,
            osd: Some(osd_controller),
            exit: Some(kiosk_exit),
            controls,
            shell_sink,
            remote_input: Some(remote_input),
            touch_surface: Some(touch_surface),
            #[cfg(feature = "audio")]
            visualizer,
        };
        #[cfg(feature = "electron")]
        pipeline::kiosk::run_with_browser(rx, wiring, browser_host)
            .map_err(|e| anyhow::anyhow!("kiosk: {e}"))?;
        #[cfg(not(feature = "electron"))]
        pipeline::kiosk::run(rx, wiring).map_err(|e| anyhow::anyhow!("kiosk: {e}"))?;
        shutdown.notify_waiters();
        // Dropping the runtime waits for every blocking task that has already started —
        // and the SponsorBlock Lounge stream is a blocking read that can sit inside its
        // 90-second timeout with nothing to interrupt it. The window is gone; nobody is
        // served by waiting. One second lets short blocking work (a screenshot encode, a
        // DNS lookup) finish, then the process leaves and the OS reclaims the rest.
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    #[cfg(not(feature = "render"))]
    {
        use pipeline::NullPipeline;
        let display: Box<dyn DisplayControl> = Box::new(NullDisplay);
        // The headless build makes sound too — an A2DP sink is audio with a card for a
        // screen — so it gets the same audio path the kiosk does: one mixer, opening the
        // device the config names, with the recording tap on it if one was asked for.
        // Before this it built its mixer internally against `OutputSelector::default()`,
        // which meant `[audio.output]` parsed and did nothing here.
        #[cfg(feature = "audio")]
        let media_pipeline = {
            let mixer = Arc::new(pipeline::mixer::AudioMixer::new(
                pipeline::audio_out::output_factory(pipeline::audio_select::OutputSelector::new(
                    config
                        .audio
                        .output
                        .choice_for(pipeline::audio_select::active_backend())
                        .selection(),
                )),
            ));
            record_the_mix(&config, &mixer);
            NullPipeline::new().with_mixer(mixer)
        };
        #[cfg(not(feature = "audio"))]
        let media_pipeline = NullPipeline::new();
        let manager = SessionManager::new(media_pipeline, Some(display), SessionConfig::default())
            .with_osd(osd.clone());
        let remote = manager.remote_handle();
        runtime.spawn(manager.run(event_rx));
        // Headless: no renderer, so drain the OSD channel to the log.
        std::thread::spawn(move || drain_osd_to_log(&osd_rx));
        // No kiosk loop to wake in this build; the waker stays unarmed and inert.
        spawn_ctrl_c(
            &runtime,
            &shutdown,
            &kiosk_exit,
            castaway_core::Waker::new(),
            remote,
        );
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
            // Headless: nothing to open a page with, so a Matter content app pointed at
            // the browser is dropped rather than accepted and abandoned.
            Launchers {
                dial: on_dial,
                browser: None,
            },
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

/// Attach the recording tap to `mixer`, if `[audio] record` names a path.
///
/// Nothing is returned: the mixer holds the tap for as long as it holds the device, which
/// is the whole run. A failure is a warning rather than a fatal error — the recording is a
/// diagnostic, and a panel that refused to boot because a debug path was unwritable would
/// be the worse outcome by a distance. The warning names the path, so the reason is one
/// line away from the absent file.
#[cfg(feature = "audio")]
fn record_the_mix(config: &Config, mixer: &Arc<pipeline::mixer::AudioMixer>) {
    let Some(path) = &config.audio.record else {
        return;
    };
    match pipeline::audio_record::MixRecorder::create(path) {
        Ok(recorder) => mixer.add_tap(recorder),
        Err(e) => warn!(error = %e, path = %path.display(), "not recording the mix"),
    }
}

/// ctrl-c triggers the same shutdown as a kiosk window close: stop the services and
/// tell the winit loop to exit.
fn spawn_ctrl_c(
    runtime: &tokio::runtime::Runtime,
    shutdown: &Arc<Notify>,
    kiosk_exit: &Arc<std::sync::atomic::AtomicBool>,
    kiosk_wake: castaway_core::Waker,
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
        // The kiosk sleeps between frames (#59) and checks the flag when awake; a
        // ctrl-c on an idle panel has to wake it to be noticed.
        kiosk_wake.wake();
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
    /// …and what it is showing *continuously*, for `/stream/*` (#101). Costs nothing
    /// until something fetches the playlist, which is what starts the encoder.
    stream: Option<stream_http::Stream>,
    /// …and what makes that duplicate *touchable*, for `/remote/*` (#18). `None` when
    /// `remote.enable` is off, or when no async runtime could be found to drive it.
    remote: Option<remote_http::Remote>,
    /// Where the media-URL session has got to, for the protocols in which the receiver is
    /// the player and has to report its own position. Absent in a build with no decoder,
    /// which then honestly answers "no such information" rather than inventing a zero.
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
    /// Where a v4 FCast sender's screen arrives (#248). `None` in a build with no media
    /// plane, and the protocol then advertises `mirroring: false` — which is the point of
    /// carrying it as an `Option` rather than as a config flag.
    mirror: Option<Arc<dyn castaway_core::MirrorBackend>>,
    /// Panel presses the shell could not answer itself (D38), and the channel back to
    /// the render loop for the screens they produce.
    ///
    /// Both halves travel together because they are one conversation: a press arrives,
    /// something is looked up, a screen goes back. Absent in a build with no renderer,
    /// where there is no panel to press.
    #[cfg(feature = "render")]
    shell: Option<ShellChannels>,
    /// How to tell the browser where the Cast receiver platform is (#16).
    ///
    /// The port is only known once the platform has bound, and the browser has to be
    /// told before anything navigates to a receiver page — the SDK's own fallback is a
    /// hardcoded 8008, and a page pointed at the wrong port never dials at all. `None`
    /// withdraws the shim, so a page that is not a Cast application cannot find one.
    /// Absent in a build with no browser, where there is nothing to arm.
    #[cfg(feature = "electron")]
    cast_platform_shim: Option<Arc<dyn Fn(Option<u16>) + Send + Sync>>,
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
    /// The seasonal accent resolved on the main thread at startup (#263), carried so a
    /// Home rebuilt here keeps the palette the first paint had rather than reverting it
    /// to plain. The clock is read once, at that boundary; this is the answer.
    season: Option<pipeline::theme::Season>,
}

/// The two ways something that is not a media URL gets opened, and the one place that
/// knows whether either exists.
///
/// A struct rather than two parameters because they are one fact: this build has a
/// browser, or it does not. Both are `None` together in a headless build, and a
/// `spawn_*` that finds its own field empty declines the capability up front rather than
/// accepting a launch it would have to abandon.
struct Launchers<D> {
    /// DIAL launch/stop, as a callback — it arrives on the runtime that owns the browser.
    dial: Option<D>,
    /// Matter content-app launches, as a stream — they cross the thread `rs-matter` runs
    /// on, so they cannot be a borrowed callback.
    browser: Option<mpsc::UnboundedSender<proto_matter::BrowserLaunch>>,
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
    launchers: Launchers<impl Fn(proto_dial::DialEvent) + Send + 'static>,
    handles: PipelineHandles,
    // Signalled when the kiosk browser has given up on the launched page.
    mut abandoned: mpsc::UnboundedReceiver<()>,
) -> anyhow::Result<()> {
    let Launchers {
        dial: on_dial,
        browser: browser_launches,
    } = launchers;
    let PipelineHandles {
        screenshot,
        stream,
        remote,
        playback,
        mirror,
        #[cfg(feature = "render")]
        shell,
        #[cfg(feature = "electron")]
        cast_platform_shim,
    } = handles;
    let (iface, iface_source) = config.resolved_interface_with_source();
    info!(
        name = %config.friendly_name,
        interface = %iface,
        source = ?iface_source,
        http_port = config.http_port,
        "castaway services starting"
    );
    // The fallback is right — a box with no route should still boot and render — but
    // it must not be *quiet*: every mDNS record, SSDP LOCATION and DIAL URL below
    // will now name an address no other machine can dial, so from the LAN the
    // receiver simply does not exist. Say so, once, where an operator reading the
    // log after "nothing can see it" will find the fix.
    if iface_source == config::InterfaceSource::LoopbackFallback {
        error!(
            "no LAN IPv4 address could be auto-detected; advertising on 127.0.0.1, so \
             no device on the network can discover or reach this receiver. Set \
             `interface = \"<this box's LAN IPv4>\"` in castaway.toml to fix this."
        );
    }

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
                // At most one resolver at a time, which is `pump_dial`'s whole job — see
                // there for what a second one costs.
                screen::pump_dial(
                    &mut dial_rx,
                    |params| {
                        let code = params.pairing_code.clone()?;
                        Some(tokio::spawn(screen::publish_screen_id(
                            code,
                            screen.clone(),
                        )))
                    },
                    on_dial,
                )
                .await;
            });
        }
    }

    let mut adapter_handles = Vec::new();
    // The receiver platform a hosted Cast application's page talks to (#16). Loopback
    // only, and started before the adapter that needs it because the port has to be known
    // in order to be handed to the browser — the SDK's own fallback is a hardcoded 8008,
    // and a page pointed at the wrong port fails silently.
    //
    // A failure here costs app hosting and nothing else: media-URL casting and mirroring
    // do not use it, so the receiver comes up without it rather than not at all.
    #[cfg_attr(not(feature = "electron"), allow(unused_mut, unused_assignments))]
    let mut cast_platform: Option<proto_cast::PlatformHost> = None;
    #[cfg(feature = "electron")]
    if config.enable.cast {
        match proto_cast::PlatformServer::new(proto_cast::DeviceCapabilities::default())
            .bind()
            .await
        {
            Ok((host, task)) => {
                let port = host.port();
                // The browser is told the port the moment it exists, so the shim is in
                // place before anything navigates to a receiver page.
                if let Some(arm) = &cast_platform_shim {
                    arm(Some(port));
                }
                adapter_handles.push(tokio::spawn(task));
                cast_platform = Some(host);
            }
            Err(e) => {
                warn!(error = %e, "no Cast receiver platform; hosted applications are unavailable");
            }
        }
    }

    // Cast is the first protocol whose adapter owns a real listener, so it advertises
    // itself: what goes in the TXT record comes from the same object that answers the
    // port, and the two can't drift.
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
        // Logged and skipped rather than fatal, for the same reason as GameStream,
        // Miracast and Bluetooth below — and this one is not hypothetical.
        //
        // Cast is the only adapter whose *startup* can fail for a reason that has
        // nothing to do with hardware: the replayed device identities stop covering the
        // calendar on 2027-03-21 (AirServer) and 2027-12-06 (CKS), so a panel with no
        // uplink past the later of those resolves no credential at all. Until this was a
        // `?`, that took the whole process with it — DLNA, AirPlay, Bluetooth, Spotify
        // and the screen — on every boot.
        //
        // A panel that happened to already be running degraded gracefully instead
        // (`ReplayProvider::current_at` keeps serving the stale credential and says so),
        // which made the failure mode "runs untended for years, then dies at the first
        // power cut". That is precisely backwards for something screwed to a wall.
        match spawn_cast(
            &config,
            media_ports,
            &mut mdns,
            event_tx.clone(),
            shutdown.clone(),
            playback.clone(),
            cast_platform.clone(),
        )
        .await
        {
            Ok(handle) => adapter_handles.push(handle),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "Cast unavailable; continuing without it");
            }
        }
    }
    if config.enable.airplay {
        adapter_handles.push(spawn_airplay(
            &config,
            media_ports,
            &mut mdns,
            event_tx.clone(),
            shutdown.clone(),
            playback.clone(),
        ));
    }
    // Only the panel reads this back, to draw the pairing QR on the FCast tile.
    #[cfg_attr(not(feature = "render"), allow(unused_mut, unused_assignments))]
    let mut fcast_connect_url: Option<String> = None;
    if config.enable.fcast {
        let wiring = spawn_fcast(
            &config,
            iface,
            &mut mdns,
            event_tx.clone(),
            shutdown.clone(),
            playback.clone(),
            mirror.clone(),
        );
        adapter_handles.push(wiring.task);
        fcast_connect_url = wiring.connect_url;
        // The two media shapes FCast has that are not URLs — content a sender pushed, and
        // `fcomp://` it serves itself — become ordinary URLs on the shared host (#249).
        http = http.merge(wiring.router);
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
    if config.enable.matter {
        // The other inverted protocol, and inverted a different way from GameStream: the
        // panel is discovered normally, but it is the *commissioner* — a phone joins a
        // fabric this box administers before it can say anything. An unwritable state
        // directory is logged and skipped like the rest, because a receiver that can
        // still do AirPlay should not refuse to start over it.
        match spawn_matter(
            &config,
            &mut mdns,
            event_tx.clone(),
            shutdown.clone(),
            osd.clone(),
            browser_launches,
            playback.clone(),
        ) {
            Ok(handle) => adapter_handles.push(handle),
            Err(e) => {
                warn!(error = %format!("{e:#}"), "Matter Casting unavailable; continuing without it");
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
        season,
    }) = shell
    {
        // Home was built on the main thread before any of this existed, so anything the
        // running receiver had to *learn* — the FCast pairing QR, whose payload pins a key
        // loaded from disk a moment ago — is not on it yet. Rebuild and send it now.
        //
        // Sent rather than mutated: `RenderCommand::Home` carries the model and the render
        // thread draws it at the true surface size (D38), and the stack's `update_home`
        // refreshes Home without yanking anyone out of a screen they are reading.
        if fcast_connect_url.is_some() {
            let home = HomeState {
                fcast_connect_url: fcast_connect_url.clone(),
            };
            if let Some(scene) = build_attract(&config, season, &home) {
                info!("FCast: the pairing QR is on the service tile");
                render.send(pipeline::RenderCommand::Home(Box::new(scene)));
            }
        }
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
    let http = http
        .route(
            "/screenshot.png",
            axum::routing::get(screenshot_route).with_state(screenshot),
        )
        .merge(stream_http::routes(stream))
        .merge(remote_http::routes(remote));
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
        (config.enable.matter, "Matter Casting"),
        (config.enable.fcast, "FCast"),
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
         a{{color:#8cf}}h1{{font-size:1.4rem}}h2{{font-size:1.1rem;margin-top:2rem}}\
         li{{margin:.2rem 0}}\
         video{{width:100%;background:#000;border-radius:.4rem;display:block}}\
         #stream-note{{color:#999;font-size:.85rem;min-height:1.2em}}</style>\
         </head><body>\
         <h1>{name}</h1>\
         <p>castaway {version} is up. This box accepts:</p>\
         <ul>{services}</ul>\
         {player}\
         <p><a href=\"/screenshot.png\">A still of what the panel is showing</a>, or \
         <a href=\"{playlist}\">the same output as HLS</a> for a player that wants a URL.</p>\
         </body></html>",
        version = env!("CARGO_PKG_VERSION"),
        // The live duplicate (#101). Present in every build: where there is no encoder the
        // player says so, which is the same answer `/screenshot.png` gives and for the
        // same reason.
        // The panel, live and drivable (#18) — stopped until somebody presses it, so a
        // landing page left open in a tab costs nothing. This replaced the HLS `<video>`
        // that used to sit here: two players for the same output, one of them seconds
        // behind and untouchable, was one too many. The HLS routes stay for players that
        // want a URL rather than a page.
        player = pipeline::remote::PLAYER,
        playlist = stream_http::PLAYLIST_PATH,
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
    app_hosting: Option<proto_cast::PlatformHost>,
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
                 cast.cks (#40/#51)"
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
    .context("building the CASTv2 receiver")?
    // The same UUID the device id above is made from, dashes intact: `eureka_info`
    // reports it as `ssdp_udn` (#226).
    .with_udn(config.uuid.clone());
    // Cast is the other protocol in which the receiver is the player, so a sender's
    // scrubber can only be answered from our own clock — the same seam DLNA reads.
    let receiver = match playback {
        Some(report) => receiver.with_playback(report),
        None => receiver,
    };
    // Hosting somebody else's receiver page (#16). Both halves or neither: the registry
    // says which page an app id names, the platform is what that page talks to.
    let hosting = app_hosting.is_some();
    let receiver = match app_hosting {
        Some(platform) => {
            receiver.with_app_hosting(Arc::new(cast_registry::Registry::new()), platform)
        }
        None => receiver,
    };

    advertise_adapter(&receiver, mdns);
    if hosting {
        info!("enabled: Google Cast (CASTv2 media-URL LOAD, mirroring, and hosted applications)");
    } else {
        info!("enabled: Google Cast (CASTv2 media-URL LOAD)");
    }

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
                subtypes,
            } => {
                let svc = txt.into_iter().fold(
                    MdnsService::new(ty, instance, MDNS_HOST, port),
                    |svc, (key, value)| svc.with_txt(key, value),
                );
                let svc = subtypes
                    .into_iter()
                    .fold(svc, substrate_mdns::MdnsService::with_subtype);
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

    // The PIN is the adapter's to choose and announce, not the config file's (#78). It
    // is also the adapter's job to notice the host is already paired, so this stays
    // harmless when the setting is left in place after provisioning.
    if let Some(host) = &gs.pair_host {
        command_tx
            .try_send(proto_gamestream::GameStreamCommand::PairAndAnnounce { host: host.clone() })
            .context("queueing the configured GameStream pairing")?;
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
        // The same capability table both ways in: a source that reaches us over
        // infrastructure must negotiate the same formats and the same RTP port as one that
        // formed a group, or the panel would behave differently depending on how it was
        // found.
        let mice_caps = caps.clone();
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
        let mut adapter =
            MiracastAdapter::new(backend, config.advertised_name(ProtocolKind::Miracast));
        if config.miracast.infrastructure {
            adapter = adapter.with_mice(proto_miracast::MiceService {
                // The receiver's own UUID: a container id is meant to identify *this
                // panel* across restarts, which is exactly what that already is.
                container_id: config.uuid.clone(),
                capability: proto_miracast::Capability::insecure(),
                caps: mice_caps,
            });
        }
        let adapter = Arc::new(adapter);
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
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
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
    // The AirPlay *video* path polls `/playback-info` for the position it draws in its
    // own scrubber, so it needs the same handle DLNA's `GetPositionInfo` reads (#80). A
    // build with no media pipeline has nothing to report and says so by not offering one.
    let receiver = match playback {
        Some(report) => receiver.with_playback(report),
        None => receiver,
    };

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

/// The FCast listener, and the two things about it the rest of the process needs back.
struct FCastWiring {
    task: tokio::task::JoinHandle<()>,
    /// The `fcast://r/…` connection URL to draw as a QR on the FCast tile, when v4 is
    /// announced (#248). `None` otherwise, and the tile is unchanged.
    connect_url: Option<String>,
    /// Pushed content and the `fcomp://` proxy, for the shared HTTP host (#249).
    router: Router,
}

/// Stand up the FCast listener and advertise `_fcast._tcp` (#241).
///
/// `advertised` is the address the receiver is reachable on — the same one every mDNS
/// record names — because the QR has to point somewhere a phone can dial, and the
/// listener itself binds the wildcard.
fn spawn_fcast(
    config: &Config,
    advertised: std::net::Ipv4Addr,
    mdns: &mut MdnsResponder,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
    mirror: Option<Arc<dyn castaway_core::MirrorBackend>>,
) -> FCastWiring {
    // The base a *sender* would use, not loopback: what this hands the decoder is an
    // ordinary URL, and nothing about it should assume the fetcher is in this process.
    let receiver = proto_fcast::FCastReceiver::new(config.advertised_name(ProtocolKind::FCast))
        .with_local_host(config.http_base_url());
    // FCast's `PlaybackUpdate`s carry the position the sender draws in its scrubber,
    // read from the same handle DLNA's `GetPositionInfo` reads. A build with no media
    // pipeline has nothing to report and says so by not offering one.
    let receiver = match playback {
        Some(report) => receiver.with_playback(report),
        None => receiver,
    };
    // The v4 TLS identity (#248): the key persists so the fingerprint — the trust
    // anchor senders pin, and what a printed QR encodes — survives restarts. An
    // unwritable state directory degrades to a fresh per-boot key (the reference
    // receiver's behaviour every boot) rather than losing the protocol.
    let receiver = match fcast_v4_identity() {
        Ok(identity) => receiver.with_v4(identity, config.fcast.announce_v4),
        Err(e) => {
            warn!(error = %format!("{e:#}"), "FCast v4 identity unavailable; running v1-v3 only");
            receiver
        }
    };
    // A v4 sender is told `mirroring: true` only when there is a plane to answer its
    // offer on, so the capability and the code that honours it are one fact (#248).
    let receiver = match mirror {
        Some(backend) => receiver.with_mirroring(backend),
        None => receiver,
    };

    advertise_adapter(&receiver, mdns);
    info!(
        announce_v4 = config.fcast.announce_v4,
        "enabled: FCast (JSON session on 46899; v4 TLS behind [fcast] announce_v4)"
    );

    let connect_url = receiver.connection_url(vec![advertised.to_string()]);
    let router = receiver.router();
    let sink = SessionSink::new(SourceId::new(ProtocolKind::FCast, "listener"), event_tx);
    let adapter = Arc::new(receiver);
    let task = tokio::spawn(async move {
        tokio::select! {
            res = adapter.run(sink) => {
                if let Err(e) = res {
                    warn!(error = %e, "FCast adapter exited");
                }
            }
            () = shutdown.notified() => info!("FCast listener stopping"),
        }
    });
    FCastWiring {
        task,
        connect_url,
        router,
    }
}

/// Load or create the persisted FCast v4 TLS key (#248).
fn fcast_v4_identity() -> anyhow::Result<proto_fcast::identity::V4Identity> {
    use anyhow::Context as _;
    let dir = castaway_paths::host().state().join("fcast");
    let key_path = dir.join("v4-key.der");
    if let Ok(key) = std::fs::read(&key_path) {
        return proto_fcast::identity::V4Identity::from_key(&key)
            .with_context(|| format!("rebuilding the v4 identity from {}", key_path.display()));
    }
    let (identity, key) =
        proto_fcast::identity::V4Identity::generate().context("generating a v4 identity")?;
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&key_path, &key))
        .with_context(|| format!("persisting the v4 key to {}", key_path.display()))?;
    Ok(identity)
}

/// Build the Matter Casting receiver and hand it its mDNS record.
fn spawn_matter(
    config: &Config,
    mdns: &mut MdnsResponder,
    event_tx: mpsc::Sender<SourceMessage>,
    shutdown: Arc<Notify>,
    osd: castaway_core::OsdSink,
    browser_launches: Option<mpsc::UnboundedSender<proto_matter::BrowserLaunch>>,
    playback: Option<Arc<dyn castaway_core::PlaybackReport>>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    use proto_matter::{ContentApp, LaunchTarget, MatterAdapter, MatterConfig};

    let apps = config
        .matter
        .apps
        .iter()
        .filter_map(|app| {
            let launch = match app.surface {
                crate::config::MatterSurface::MediaUrl => LaunchTarget::MediaUrl,
                crate::config::MatterSurface::Browser => {
                    // A browser app on a build with no browser is an app that would accept
                    // a cast and then have nowhere to put it. Dropped here rather than at
                    // launch time, so the phone is told "no such app" up front.
                    if browser_launches.is_none() {
                        warn!(
                            app = %app.name,
                            "Matter: skipping a browser content app in a build with no browser"
                        );
                        return None;
                    }
                    LaunchTarget::Browser {
                        search: app.search.clone(),
                    }
                }
            };

            Some(ContentApp {
                // Overwritten by the catalogue: an endpoint is a position in the node's
                // tree, not something config gets to pick.
                endpoint: 0,
                vendor_id: app.vendor_id,
                product_id: app.product_id,
                vendor_name: app.vendor_name.clone(),
                name: app.name.clone(),
                application_id: app.application_id.clone(),
                catalog_vendor_id: app.catalog_vendor_id,
                launch,
            })
        })
        .collect::<Vec<_>>();

    let adapter = MatterAdapter::new(MatterConfig {
        friendly_name: config.advertised_name(ProtocolKind::MatterCast),
        host: MDNS_HOST.to_string(),
        state_dir: config.matter.state_dir.clone(),
        vendor_id: config.matter.vendor_id,
        product_id: config.matter.product_id,
        catalogue: proto_matter::Catalogue::new(apps),
        bind: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
    })
    .with_osd(osd)
    // The operational `_matter._tcp` record depends on fabric state, so it cannot be in
    // the startup advertisement list below — the adapter publishes it through this late
    // handle on the same shared responder once the fabric has a member (#173).
    .with_operational_mdns(mdns.advertiser());

    let adapter = match browser_launches {
        Some(launches) => adapter.with_browser(launches),
        None => adapter,
    };

    // MediaPlayback's Duration, and the bound on Seek, come from the pipeline (#283). The
    // headless build has nothing that reports one, and the adapter then answers Null and
    // refuses absolute seeks — which is the truth there, not a shortcut.
    let adapter = match playback {
        Some(report) => adapter.with_playback(report),
        None => adapter,
    };

    advertise_adapter(&adapter, mdns);
    info!("enabled: Matter Casting (commissioner on 5550, node on 5540)");

    let sink = SessionSink::new(
        SourceId::new(ProtocolKind::MatterCast, "listener"),
        event_tx,
    );
    let adapter = Arc::new(adapter);
    Ok(tokio::spawn(async move {
        tokio::select! {
            res = adapter.run(sink) => {
                if let Err(e) = res {
                    warn!(error = %e, "Matter adapter exited");
                }
            }
            () = shutdown.notified() => info!("Matter listeners stopping"),
        }
    }))
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
/// What Home shows that the config alone cannot say — facts only the running receiver
/// knows.
///
/// Home is a function of the config *and* of what actually came up, and the two are
/// learned in different places: the config is read on the main thread before the kiosk
/// opens, while the FCast v4 identity is loaded on the runtime inside [`serve`], after
/// the window already exists. Keeping the runtime half in one small struct means Home is
/// still one pure function of its inputs, and rebuilding it is sending the same function
/// a fuller argument rather than reaching into a scene that has already been drawn.
///
/// `Default` is "nothing has come up yet", which is the honest state at first paint.
#[cfg(feature = "render")]
#[derive(Debug, Clone, Default)]
struct HomeState {
    /// The `fcast://r/…` connection URL, once FCast is up and announcing v4 (#248).
    /// `None` keeps the FCast tile exactly as it was — the QR is an addition to the
    /// service card, not a thing it is missing.
    fcast_connect_url: Option<String>,
}

#[cfg(feature = "render")]
/// The Home screen's model — what the panel shows when nothing is casting.
///
/// Returns the *scene*, not pixels: the render thread draws it at the true surface size,
/// so the panel is no longer handed a 3840x2160 bitmap to stretch (D38).
fn build_attract(
    config: &Config,
    season: Option<pipeline::theme::Season>,
    home: &HomeState,
) -> Option<pipeline::attract::AttractScene> {
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
                // Only FCast has one to scan today; the service screen draws whatever is
                // set and leaves the card alone when it is not.
                qr_payload: match kind {
                    ProtocolKind::FCast => home.fcast_connect_url.clone(),
                    _ => None,
                },
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
    if config.enable.fcast {
        // The one tile whose steps change with what came up. With v4 announced there is a
        // code on the card, and scanning it is a *better* route than the picker — it pins
        // the receiver's key, where a device found over mDNS is trusted on the network's
        // word — so the card says so instead of leaving a QR nobody was told to use.
        let mut steps = vec![
            "Play something in Grayjay".to_string(),
            "Tap cast, and pick this screen".to_string(),
        ];
        if home.fcast_connect_url.is_some() {
            steps.push("Or scan the code to connect directly".into());
        }
        tiles.push(service(
            "fcast",
            "FCast",
            TileGlyph::FCast,
            [0x26, 0xd1, 0xff, 0xff],
            "The cast button in Grayjay.",
            steps,
            ProtocolKind::FCast,
        ));
    }
    if config.enable.matter {
        // The one tile whose second step is on the *panel*: every other protocol here is
        // "pick this screen and you are done", and this one puts a number on the glass
        // that has to be carried back to the phone.
        tiles.push(service(
            "matter",
            "Matter",
            TileGlyph::MatterCast,
            [0xf6, 0x9b, 0x21, 0xff],
            "Cast from an app that speaks Matter.",
            vec![
                "Tap cast in the app".into(),
                "Type the code shown here".into(),
            ],
            ProtocolKind::MatterCast,
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
        // What today is, resolved by the caller at the boundary and passed in: once at
        // startup, and again by `seasonal_rollover` at each local midnight — the panel
        // is up for weeks, so the screen has to enter and leave June without a restart.
        season,
        mascot: true,
    };
    Some(scene)
}

/// What season `choice` puts on the screen at `now_unix_secs`, on the machine's own
/// calendar (#24, #263). Only where there is a screen to decorate.
///
/// The calendar rather than only a config flag: nobody is going to remember to turn
/// Pride on in June and off in July, and a decoration that needs an edit is a decoration
/// that never appears.
///
/// Pure on purpose — the clock is read at the call sites (startup, and the rollover
/// task's wake), never in here, so June is testable in March.
#[cfg(feature = "render")]
fn seasonal_accent(
    choice: pipeline::theme::ThemeChoice,
    now_unix_secs: u64,
    utc_offset_secs: i32,
) -> Option<pipeline::theme::Season> {
    // A forced choice needs no calendar, which also means a clock this box cannot read
    // does not stop someone asking for a palette outright.
    if let Some(forced) = choice.forced() {
        return Some(forced);
    }
    let (month, day) = local_civil_date(now_unix_secs, utc_offset_secs)?;
    choice.resolve(month, day)
}

/// The local `(month, day)` of a Unix instant, given the machine's UTC offset.
///
/// Civil date by Howard Hinnant's algorithm. A date crate for a page of arithmetic is
/// not worth the dependency — the panel's seasons are day-grained, and the one genuinely
/// hard part (what the offset *is*) is read from the OS once, at the top of `main`.
#[cfg(feature = "render")]
fn local_civil_date(unix_secs: u64, utc_offset_secs: i32) -> Option<(u32, u32)> {
    let local = i64::try_from(unix_secs)
        .ok()?
        .checked_add(i64::from(utc_offset_secs))?;
    let days = local.div_euclid(86_400);
    let z = days + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).ok()?;
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).ok()?;
    Some((month, day))
}

/// Seconds from `now` until just past the next *local* midnight — the moment the
/// seasonal palette can change (#263).
///
/// A minute past rather than on the stroke, so a clock that is seconds out does not wake
/// the task on the wrong side of the boundary and make it sleep a whole day with
/// yesterday's palette up.
#[cfg(feature = "render")]
fn secs_until_next_local_midnight(now_unix_secs: u64, utc_offset_secs: i32) -> u64 {
    let local = i64::try_from(now_unix_secs)
        .unwrap_or(0)
        .saturating_add(i64::from(utc_offset_secs));
    let into_day = local.rem_euclid(86_400);
    // rem_euclid is in 0..86_400, so this is in 60..=86_460 and the cast cannot lose.
    u64::try_from(86_400 - into_day + 60).unwrap_or(86_460)
}

/// Re-send the Home scene whenever the local date rolls the season over (#263).
///
/// The panel is up for weeks, so the screen has to enter and leave June without a
/// restart. Sleeps to just past each local midnight and re-resolves; only a change is
/// sent, so an ordinary Tuesday costs one comparison a day. Runs only for `auto` — a
/// forced palette has nothing to roll over to.
///
/// The startup UTC offset is carried as the fallback: a DST change mid-uptime moves
/// local midnight, and where the OS will answer a threaded process (Windows will, unix
/// soundly refuses) the fresh offset is used; elsewhere the palette flips within an hour
/// of the boundary instead, which for a day-grained decoration is close enough.
#[cfg(feature = "render")]
async fn seasonal_rollover(
    choice: pipeline::theme::ThemeChoice,
    startup_offset_secs: i32,
    mut scene: pipeline::attract::AttractScene,
    render: pipeline::RenderTx,
) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    };
    let offset = || {
        time::UtcOffset::current_local_offset()
            .map_or(startup_offset_secs, time::UtcOffset::whole_seconds)
    };
    let mut worn = scene.season;
    loop {
        let wait = secs_until_next_local_midnight(now_secs(), offset());
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let season = seasonal_accent(choice, now_secs(), offset());
        if season != worn {
            info!(
                from = worn.map_or("plain", pipeline::theme::Season::name),
                to = season.map_or("plain", pipeline::theme::Season::name),
                "theme: the season changed overnight"
            );
            worn = season;
            scene.season = season;
            render.send(pipeline::RenderCommand::Home(Box::new(scene.clone())));
        }
    }
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
    #[cfg(feature = "render")]
    use super::{build_attract, HomeState};

    /// The date-driven palette, tested at fixed instants (#263): the clock is read at
    /// the call sites, so June is provable in March and — the part the golden-scene
    /// suite leans on — an ordinary date is provably plain.
    #[cfg(feature = "render")]
    mod seasonal {
        use super::super::{local_civil_date, seasonal_accent, secs_until_next_local_midnight};
        use pipeline::theme::{Season, ThemeChoice};

        /// 2026-08-09 12:00:00 UTC — an ordinary day in an ordinary week.
        const ORDINARY: u64 = 1_786_276_800;
        /// 2026-06-15 12:00:00 UTC — the middle of Pride.
        const MID_JUNE: u64 = 1_781_524_800;
        /// 2026-05-31 23:30:00 UTC — before June in London, inside it in Berlin.
        const LATE_MAY_UTC: u64 = 1_780_270_200;
        /// 2026-06-01 02:00:00 UTC — inside June in London, still May in Seattle.
        const EARLY_JUNE_UTC: u64 = 1_780_279_200;
        /// 2026-12-25 12:00:00 UTC — Christmas Day.
        const CHRISTMAS: u64 = 1_798_200_000;

        #[test]
        fn an_ordinary_date_resolves_to_the_base_palette() {
            // The contract the golden scenes rest on: `auto` on a normal day is `None`,
            // which is the panel's own dark ramp, byte for byte. If this fails, the
            // composited goldens are about to change without anyone choosing to.
            assert_eq!(seasonal_accent(ThemeChoice::Auto, ORDINARY, 0), None);
            assert_eq!(seasonal_accent(ThemeChoice::Auto, ORDINARY, -25_200), None);
            assert_eq!(seasonal_accent(ThemeChoice::Auto, ORDINARY, 7_200), None);
        }

        #[test]
        fn the_calendar_seasons_arrive_on_schedule() {
            assert_eq!(
                seasonal_accent(ThemeChoice::Auto, MID_JUNE, 0),
                Some(Season::Pride)
            );
            assert_eq!(
                seasonal_accent(ThemeChoice::Auto, CHRISTMAS, 0),
                Some(Season::Christmas)
            );
        }

        #[test]
        fn a_holiday_lands_on_the_local_date_not_the_utc_one() {
            // 23:30 UTC on 31 May: Berlin (UTC+2) is already an hour into June.
            assert_eq!(seasonal_accent(ThemeChoice::Auto, LATE_MAY_UTC, 0), None);
            assert_eq!(
                seasonal_accent(ThemeChoice::Auto, LATE_MAY_UTC, 7_200),
                Some(Season::Pride)
            );
            // 02:00 UTC on 1 June: Seattle (UTC-7) still has hours of May left.
            assert_eq!(
                seasonal_accent(ThemeChoice::Auto, EARLY_JUNE_UTC, 0),
                Some(Season::Pride)
            );
            assert_eq!(
                seasonal_accent(ThemeChoice::Auto, EARLY_JUNE_UTC, -25_200),
                None
            );
        }

        #[test]
        fn a_forced_choice_needs_no_clock_and_plain_refuses_one() {
            // Halloween in June, because the config said so...
            assert_eq!(
                seasonal_accent(ThemeChoice::Halloween, MID_JUNE, 0),
                Some(Season::Halloween)
            );
            // ...and plain in June, for the photograph.
            assert_eq!(seasonal_accent(ThemeChoice::Plain, MID_JUNE, 0), None);
        }

        #[test]
        fn the_civil_date_arithmetic_agrees_with_the_calendar() {
            assert_eq!(local_civil_date(MID_JUNE, 0), Some((6, 15)));
            assert_eq!(local_civil_date(ORDINARY, 0), Some((8, 9)));
            // The offset crosses a month boundary in both directions.
            assert_eq!(local_civil_date(LATE_MAY_UTC, 7_200), Some((6, 1)));
            assert_eq!(local_civil_date(EARLY_JUNE_UTC, -25_200), Some((5, 31)));
        }

        #[test]
        fn the_rollover_sleeps_to_just_past_local_midnight() {
            // 2026-08-09 23:00:00 UTC. On UTC, midnight is an hour out; the task wakes
            // sixty seconds past it so a slightly-slow clock cannot strand it on the
            // wrong side of the boundary for a day.
            let eleven_pm_utc: u64 = 1_786_316_400;
            assert_eq!(secs_until_next_local_midnight(eleven_pm_utc, 0), 3_660);
            // In Seattle it is 16:00 — eight hours and a minute to local midnight.
            assert_eq!(
                secs_until_next_local_midnight(eleven_pm_utc, -25_200),
                28_860
            );
            // Exactly on midnight, the whole day (plus the margin) lies ahead.
            assert_eq!(secs_until_next_local_midnight(86_400, 0), 86_460);
        }
    }

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

    /// Spotify's device id is the panel's UUID with the dashes taken out, and it is the
    /// value the whole blob decryption is keyed to.
    ///
    /// `spotify_device_id` — one `replace` — had no test at all. `tests/pairing.rs`
    /// proves `getInfo`'s id and the blob's id agree *within* `proto-spotify`; nothing
    /// proved the id the session runner uses is the one this crate advertised, and the
    /// crate's own docs say a mismatch "fails login with nothing that looks like a
    /// cause" (#200).
    ///
    /// The other half of that identity is inside `proto-spotify`: `SpotifyService::new`
    /// stores this string once and `with_playback` hands the *same* field to
    /// `ConnectSettings`, so there is one value and not two. This end asserts what goes
    /// in, and that it is stable across restarts — a generated-at-startup id would pair
    /// once and then fail every reconnect.
    #[test]
    fn the_spotify_device_id_is_the_panels_uuid_and_survives_a_restart() {
        let config = crate::config::Config {
            uuid: "0f8c1e2a-1111-4000-8000-00000000abcd".to_owned(),
            ..Default::default()
        };
        let id = super::spotify_device_id(&config);

        assert_eq!(id, "0f8c1e2a11114000800000000000abcd");
        assert!(
            !id.contains('-'),
            "Spotify's device id is hex with no separators; a dash here is a login that \
             fails with nothing that looks like a cause"
        );
        assert_eq!(
            id.len(),
            32,
            "a UUID is 32 hex digits once the dashes are gone"
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

        // Derived from the config, which is file-backed — so the same box advertises the
        // same id after a reboot, and a phone that paired yesterday still recognises it.
        assert_eq!(id, super::spotify_device_id(&config));

        // And it is *this* panel's, not a constant: two boxes on one LAN that shared an id
        // would each decrypt the other's blob.
        let other = crate::config::Config {
            uuid: "0f8c1e2a-1111-4000-8000-00000000abce".to_owned(),
            ..Default::default()
        };
        assert_ne!(id, super::spotify_device_id(&other));
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

    /// The two UPnP root devices `serve` registers, built with the same expressions it
    /// uses — DLNA on `config.uuid`, DIAL on `device_uuid(&config.uuid, "dial")` — share
    /// no `USN` across their whole advertised surface (#202).
    ///
    /// `each_protocol_gets_its_own_device_uuid_and_keeps_it` pins the derivation;
    /// this pins the consequence at the SSDP layer, where the collision actually bit:
    /// every `SsdpDevice` expands to a `uuid:…::upnp:rootdevice` target and a bare
    /// `uuid:…` target, so two services wired to one UUID answer `ssdp:all` and
    /// `upnp:rootdevice` searches with identical USNs and different LOCATIONs, and a
    /// control point that dedupes on USN throws one description away. If someone
    /// "simplifies" the DIAL wiring back to `config.uuid`, this is the test that says
    /// what breaks.
    #[test]
    fn dial_and_dlna_root_devices_share_no_usn() {
        let config = crate::config::Config {
            uuid: "0f8c1e2a-1111-4000-8000-00000000abcd".to_owned(),
            ..Default::default()
        };
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(4);
        let sink = castaway_core::SessionSink::new(
            castaway_core::SourceId::new(castaway_core::ProtocolKind::Dlna, "http"),
            event_tx,
        );
        let dlna = proto_dlna::DlnaService::new(
            config.advertised_name(castaway_core::ProtocolKind::Dlna),
            &config.uuid,
            sink,
        );
        let (dial_tx, _dial_rx) = tokio::sync::mpsc::channel(4);
        let dial = proto_dial::DialService::new(
            config.advertised_name(castaway_core::ProtocolKind::YouTubeLounge),
            config.http_base_url(),
            device_uuid(&config.uuid, "dial"),
            dial_tx,
        );

        let dlna_usns: std::collections::HashSet<String> = dlna
            .ssdp_device()
            .targets()
            .into_iter()
            .map(|t| t.usn)
            .collect();
        let dial_usns: std::collections::HashSet<String> = dial
            .ssdp_device()
            .targets()
            .into_iter()
            .map(|t| t.usn)
            .collect();
        let shared: Vec<&String> = dlna_usns.intersection(&dial_usns).collect();
        assert!(
            shared.is_empty(),
            "DLNA and DIAL answer overlapping searches (upnp:rootdevice, ssdp:all), so \
             a shared USN makes one of them invisible to control points that dedupe on \
             it: {shared:?}"
        );
    }

    /// Home is a function of the config *and* of what came up (#248).
    ///
    /// The first paint has no QR, because the FCast identity is loaded on the runtime
    /// after the window already exists; the rebuild `serve` sends once it has one puts the
    /// payload on the FCast tile and nowhere else. This is the assertion the "live tile"
    /// follow-up is about — the screen has always drawn the field, and nothing set it.
    #[cfg(feature = "render")]
    #[test]
    fn the_fcast_tile_takes_its_qr_from_runtime_state() {
        let config = crate::config::Config::default();
        let payload = "fcast://r/eyJuYW1lIjoiUGFuZWwifQ==";

        let fresh = build_attract(&config, None, &HomeState::default()).expect("a Home scene");
        let fcast = |scene: &pipeline::attract::AttractScene| {
            scene
                .tiles
                .iter()
                .find(|t| t.id == "fcast")
                .and_then(|t| t.detail.clone())
                .expect("an FCast service card")
        };
        assert_eq!(fcast(&fresh).qr_payload, None, "nothing has come up yet");

        let up = build_attract(
            &config,
            None,
            &HomeState {
                fcast_connect_url: Some(payload.to_string()),
            },
        )
        .expect("a Home scene");
        assert_eq!(fcast(&up).qr_payload.as_deref(), Some(payload));
        assert!(
            fcast(&up).steps.iter().any(|s| s.contains("scan")),
            "a code nobody was told to use is furniture: {:?}",
            fcast(&up).steps
        );

        // And only that tile: every other service card is untouched by FCast's state.
        for tile in &up.tiles {
            if tile.id != "fcast" {
                assert_eq!(
                    tile.detail.as_ref().and_then(|d| d.qr_payload.clone()),
                    None,
                    "{} grew a QR",
                    tile.id
                );
            }
        }
    }
}
