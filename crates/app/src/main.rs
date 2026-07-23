//! The castaway binary. Composes the enabled protocol adapters into one session
//! manager driving one pipeline, behind one shared HTTP host, one SSDP responder, and
//! one mDNS responder — the "advertise once, not five racing daemons" goal. This is the
//! only crate that uses `anyhow` (ground rule 7).

mod config;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use castaway_core::{
    DisplayControl, ProtocolKind, SessionConfig, SessionManager, SessionSink, SourceId,
    SourceMessage,
};
use control_display::NullDisplay;
use pipeline::NullPipeline;
use proto_dial::DialService;
use proto_dlna::DlnaService;
use proto_spotify::SpotifyService;
use substrate_mdns::MdnsResponder;
use substrate_ssdp::{Responder, ResponderConfig, SsdpDevice};
use tokio::sync::{mpsc, Notify};
use tracing::{info, warn};

use crate::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let config = Config::load("castaway.toml").context("loading config")?;
    let iface = config.resolved_interface();
    info!(
        name = %config.friendly_name,
        interface = %iface,
        http_port = config.http_port,
        "castaway starting"
    );

    // --- session manager over the null pipeline (real pipeline lands behind features) ---
    let (event_tx, event_rx) = mpsc::channel::<SourceMessage>(64);
    let display: Box<dyn DisplayControl> = Box::new(NullDisplay);
    let manager = SessionManager::new(NullPipeline::new(), Some(display), SessionConfig::default());
    let manager_handle = tokio::spawn(manager.run(event_rx));

    // --- shared HTTP host: merge each enabled protocol's router ---
    let mut http = Router::new();
    let mut ssdp_devices: Vec<(SsdpDevice, String)> = Vec::new();
    let mut mdns = MdnsResponder::new().context("creating mDNS responder")?;

    if config.enable.dlna {
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "http"), event_tx.clone());
        let dlna = DlnaService::new(&config.friendly_name, &config.uuid, sink);
        http = http.merge(dlna.router());
        ssdp_devices.push((dlna.ssdp_device(), dlna.description_path().to_string()));
        info!("enabled: DLNA MediaRenderer");
    }

    if config.enable.spotify {
        let sink = SessionSink::new(
            SourceId::new(ProtocolKind::Spotify, "http"),
            event_tx.clone(),
        );
        let spotify = SpotifyService::new(&config.friendly_name, spotify_device_id(&config), sink);
        http = http.merge(spotify.router());
        mdns.advertise(&spotify.mdns_service(config.http_port, "castaway"))
            .context("advertising Spotify")?;
        info!("enabled: Spotify Connect (onboarding/pairing)");
    }

    let (launch_tx, mut launch_rx) = mpsc::channel(8);
    if config.enable.dial {
        let dial = DialService::new(config.http_base_url(), launch_tx.clone());
        http = http.merge(dial.router());
        ssdp_devices.push((
            dial.ssdp_device(&config.uuid),
            dial.description_path().to_string(),
        ));
        info!("enabled: DIAL → YouTube Lounge (launch)");
        // Until the Lounge bind-channel client lands, log launches.
        tokio::spawn(async move {
            while let Some(params) = launch_rx.recv().await {
                warn!(pairing = ?params.pairing_code, "YouTube launched; Lounge client is a follow-up");
            }
        });
    }

    // mDNS-only advertisements for socket protocols whose TCP actors aren't wired yet.
    advertise_socket_protocols(&config, &mut mdns);

    // --- shutdown signalling ---
    let shutdown = Arc::new(Notify::new());

    // --- SSDP responder over all registered devices ---
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

    // --- HTTP host ---
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

    info!("castaway running — press Ctrl-C to stop");
    tokio::signal::ctrl_c().await.ok();
    info!("shutting down (sending SSDP byebye, unregistering mDNS)");
    shutdown.notify_waiters();

    // mDNS unregisters on drop; give async servers a moment to finish.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = ssdp_handle.await;
        let _ = http_handle.await;
    })
    .await;
    drop(mdns);
    drop(event_tx);
    let _ = manager_handle.await;
    Ok(())
}

/// mDNS-advertise Cast/AirPlay if enabled. Their TCP actors (Cast TLS, AirPlay RTSP)
/// aren't wired yet, so this is off by default — advertising without a listener would
/// only frustrate senders. When enabled we still warn.
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
