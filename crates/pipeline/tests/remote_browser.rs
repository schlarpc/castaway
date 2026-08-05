//! The remote player, driven by a real browser (#18).
//!
//! `remote_negotiation.rs` proves an offer is answered. It cannot prove the answer is
//! *usable*, because the thing on the other end is a hand-written SDP string that agrees
//! with whatever we send it. Two bugs got through it for exactly that reason:
//!
//! - the pump stamped every packet with the payload type we *register* rather than the one
//!   the peer negotiated, which a real browser rejects and a fixture cannot notice, because
//!   the fixture offered the same number;
//! - gathering completion was signalled to nobody, so every connection sat out a
//!   three-second timeout — visible only as wall-clock time nothing asserted on.
//!
//! So this runs the real player, in Chromium (Electron is Chromium), against the real
//! transport, and asks the browser itself whether it decoded anything. Then it puts a
//! finger on the picture and checks it comes back out of the input queue on the panel's
//! side — the whole loop, with no human in it.
//!
//! Needs a GPU and an Electron, so it is `#[ignore]` by default and run by name:
//!
//! ```sh
//! CASTAWAY_ELECTRON=$(nix build --no-link --print-out-paths .#electron)/bin/electron \
//!   cargo test -p pipeline --features remote,electron --test remote_browser -- --ignored --nocapture
//! ```

#![cfg(all(feature = "remote", feature = "electron"))]
#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use input_touch::{InputOrigin, RemoteEvent, RemoteInputQueue};
use pipeline::adblock_engine::AdBlocker;
use pipeline::electron_browser::{Electron, ElectronHost};
use pipeline::nv12::Nv12Planes;
use pipeline::remote::{RemoteConfig, RemoteService};
use pipeline::stream::feed::LiveFeed;
use pipeline::stream::H264Encoder;
use pipeline::BrowserCommand;

/// Small enough that the software encoder keeps up on any box, large enough that the
/// browser has something to scale.
const SIZE: (u32, u32) = (320, 180);

/// Clear of both `[media_ports]` and the ranges `remote_negotiation.rs` uses.
const ICE_PORTS: (u16, u16) = (45600, 45607);

/// Every address a candidate may be offered on, the way the app picks them.
///
/// Loopback alone is not enough and this test is what proved it: Chromium gathers its real
/// interfaces and *never* offers a loopback candidate, so a panel bound only to 127.0.0.1
/// has nothing for a browser on the same machine to pair with — ICE says "no candidate
/// pairs" and the connection sits in `Connecting` forever.
fn bind_ips() -> Vec<std::net::IpAddr> {
    let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    // The route trick `config::detect_ipv4` uses: connect a UDP socket at the default
    // route and ask which address the kernel chose. No packet is sent.
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only, unconnected, and never bound to a served port"
    )]
    let detected = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
        .ok()
        .and_then(|socket| {
            socket
                .connect((std::net::Ipv4Addr::new(8, 8, 8, 8), 80))
                .ok()?;
            socket.local_addr().ok()
        })
        .map(|addr| addr.ip())
        .filter(|ip| !ip.is_loopback());
    match detected {
        Some(lan) => vec![lan, loopback],
        None => vec![loopback],
    }
}

fn electron_path() -> std::path::PathBuf {
    std::env::var_os("CASTAWAY_ELECTRON")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "electron".into())
}

fn app_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../browser-host")
        .canonicalize()
        .unwrap()
}

/// The real player, in a document with nothing else in it.
///
/// The landing page's prose is not what is under test, and a full-viewport stage means a
/// touch at the centre of the panel is a touch at the centre of the picture.
fn document() -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"></head>\
         <body style=\"margin:0;background:#000\">\
         <style>#remote h2,#hint{{display:none}}#stage{{aspect-ratio:auto;height:100vh}}</style>\
         {player}</body></html>",
        player = pipeline::remote::PLAYER
    )
}

/// Serve the document and answer offers, on an ephemeral port.
///
/// Hand-rolled rather than axum: this crate does not depend on it, and the whole surface
/// is two routes with no negotiation of their own.
///
/// The runtime handle is the caller's, and multi-threaded, which is not incidental. A peer
/// connection spawns a driver task and *that* is what advances ICE; answering inside
/// `block_on` on a throwaway current-thread runtime spawns the driver onto a runtime that
/// stops running the moment the answer is returned. The symptom is a connection that
/// negotiates perfectly, reports `Checking`, logs "no candidate pairs", and then does
/// nothing forever — with valid candidates on both sides. The app is fine because axum
/// runs on the multi-threaded runtime that outlives every request.
fn serve(
    service: Arc<RemoteService>,
    runtime: tokio::runtime::Handle,
    stop: Arc<AtomicBool>,
) -> u16 {
    use std::io::{BufRead, BufReader, Read, Write};

    // Not a registered bind site: a test's loopback socket never faces the LAN, which is
    // exactly the carve-out `clippy.toml` names.
    #[expect(
        clippy::disallowed_methods,
        reason = "test-only loopback listener; see clippy.toml"
    )]
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let body = document();

    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let Ok((mut socket, _)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            };
            socket.set_nonblocking(false).unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            let mut length = 0usize;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let response = if request.starts_with("POST /remote/whep") {
                let mut offer = vec![0u8; length];
                reader.read_exact(&mut offer).ok();
                let offer = String::from_utf8_lossy(&offer).into_owned();
                for line in offer
                    .lines()
                    .filter(|l| l.starts_with("a=candidate") || l.starts_with("m="))
                {
                    eprintln!("OFFER {line}");
                }
                match runtime.block_on(service.answer(&offer)) {
                    Ok(answer) => http(201, "application/sdp", &answer),
                    Err(e) => http(503, "text/plain", &format!("{e}")),
                }
            } else {
                http(200, "text/html; charset=utf-8", &body)
            };
            let _ = socket.write_all(response.as_bytes());
            let _ = socket.flush();
        }
    });
    port
}

fn http(status: u16, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Encode a moving picture into the feed until told to stop.
///
/// A real encoder on real pixels, because the point is that a browser decodes what comes
/// out. The picture moves so a stuck first frame is distinguishable from a live one.
fn encode_into(feed: Arc<LiveFeed>, stop: Arc<AtomicBool>) -> Option<std::thread::JoinHandle<()>> {
    let (width, height) = SIZE;
    let rate = pipeline::stream::cadence::FrameRate::DEFAULT;
    let mut encoder = match H264Encoder::open(width, height, rate, 1_000_000, 30) {
        Ok(encoder) => encoder,
        Err(e) => {
            eprintln!("skipping: no H.264 encoder ({e})");
            return None;
        }
    };
    Some(std::thread::spawn(move || {
        let stride = width.div_ceil(256) * 256;
        let uv_offset = (stride * height) as usize;
        let mut luma = 16u8;
        while !stop.load(Ordering::Acquire) {
            luma = luma.wrapping_add(3).max(16);
            let mut data = vec![128u8; uv_offset + (stride * height / 2) as usize];
            data[..uv_offset].fill(luma);
            let planes = Nv12Planes {
                width,
                height,
                data,
                y_stride: stride,
                uv_offset,
                uv_stride: stride,
            };
            if let Ok(samples) = encoder.encode(&planes) {
                for sample in samples {
                    feed.publish(&sample, encoder.config());
                }
            }
            std::thread::sleep(Duration::from_millis(33));
        }
    }))
}

/// Pump the browser the way the kiosk does, polling `expression` until `ready` accepts it.
fn until(
    host: &mut ElectronHost,
    render: &mut pipeline::render_pipeline::RenderLoop,
    expression: &str,
    ready: impl Fn(&str) -> bool,
    within: Duration,
) -> Option<String> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        host.pump(render);
        render.pump();
        if let Ok(value) = host.probe(expression, Duration::from_millis(500)) {
            if ready(&value) {
                return Some(value);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Every remote contact the panel has been handed, in order.
fn drained(input: &RemoteInputQueue) -> Vec<RemoteEvent> {
    input.drain()
}

#[test]
#[ignore = "needs a GPU and an Electron"]
fn a_real_browser_plays_the_panel_and_its_touches_come_back() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pipeline=debug,webrtc=warn,rtc=warn".into()),
        )
        .try_init();
    // Multi-threaded and held for the whole test: it is what drives every peer
    // connection's background driver. See `serve`.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();
    let _guard = runtime.enter();

    let (_cmd_tx, cmd_rx) = pipeline::render_channel(8);
    let (width, height) = (1280, 720);
    let Some(mut render) = pipeline::test_gpu::render_loop(width, height, cmd_rx) else {
        return;
    };

    let feed = Arc::new(LiveFeed::new());
    let input = Arc::new(RemoteInputQueue::new(castaway_core::Waker::new()));
    let service = RemoteService::new(
        RemoteConfig {
            ice_ports: ICE_PORTS,
            bind_ips: bind_ips(),
            accept_input: true,
        },
        Arc::clone(&feed),
        Arc::clone(&input),
        Arc::new(|| {}),
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let Some(encoder) = encode_into(Arc::clone(&feed), Arc::clone(&stop)) else {
        return;
    };
    let port = serve(
        Arc::clone(&service),
        runtime.handle().clone(),
        Arc::clone(&stop),
    );

    let blocker = Arc::new(AdBlocker::with_defaults());
    let electron = Electron::spawn(
        &electron_path(),
        &app_dir(),
        Arc::clone(&blocker),
        None,
        pipeline::TV_USER_AGENT,
        castaway_core::Waker::new(),
    )
    .expect("browser should start");
    let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
    let spec = pipeline::electron_browser::RespawnSpec {
        program: electron_path(),
        app_dir: app_dir(),
        adblock: blocker,
        // Renamed to `mixer` when #111 made the panel have one output rather than a
        // device per session; this test was not built with `--features electron` and so
        // did not fail when it stopped compiling.
        mixer: None,
        user_agent: pipeline::TV_USER_AGENT.to_string(),
        waker: castaway_core::Waker::new(),
    };
    let mut host = ElectronHost::new(electron, spec, rx);
    host.resize(width, height);
    tx.send(BrowserCommand::Navigate(format!(
        "http://127.0.0.1:{port}/"
    )))
    .unwrap();

    // 1. It loads stopped. This is not a warm-up: "nothing is encoded until you press it"
    //    is the property that lets the player sit on the landing page at all, and a page
    //    that negotiated on load would break it silently.
    let loaded = until(
        &mut host,
        &mut render,
        "document.getElementById('stage').dataset.state",
        |v| v.contains("idle"),
        Duration::from_secs(30),
    );
    assert!(loaded.is_some(), "the player never loaded");
    assert_eq!(
        service.peer_count(),
        0,
        "a page that merely loaded must not have connected anything"
    );

    // 2. Press play. Clicked through the DOM rather than by aiming a synthetic pointer at
    //    the button: where the button *is* is not what this test is about, and the touch
    //    routing gets exercised for real in step 4.
    let _ = host.probe(
        "document.getElementById('play').click(), 'pressed'",
        Duration::from_secs(5),
    );

    // 3. The browser decoded what we sent it. `getVideoPlaybackQuality` is the decisive
    //    signal — `videoWidth` can be filled in from the track's parameters before a
    //    single frame is decoded, and `state === 'live'` only says a track arrived. This
    //    counts pictures that actually came out of the decoder, which is what both of the
    //    bugs this test exists for would have left at zero.
    let frames = until(
        &mut host,
        &mut render,
        "document.getElementById('panel').getVideoPlaybackQuality().totalVideoFrames",
        |v| {
            v.trim()
                .trim_matches('"')
                .parse::<u32>()
                .is_ok_and(|n| n > 0)
        },
        Duration::from_secs(30),
    );
    let frames = frames.unwrap_or_else(|| {
        panic!(
            "the browser decoded nothing: connected={}",
            service.peer_count()
        )
    });
    println!("browser decoded {} frames", frames.trim());

    assert_eq!(service.peer_count(), 1, "the peer should be connected");
    assert!(
        feed.watched(),
        "a connected peer should be holding the encoder alive"
    );

    // 4. …and now the other direction. A real pointer on the picture, routed through the
    //    browser exactly as the panel's own glass routes one, and it has to come back out
    //    of the queue on this side as a contact belonging to that peer.
    let _ = drained(&input);
    use input_touch::{ContactId, InputSink as _, TouchEvent, TouchPhase};
    for phase in [TouchPhase::Down, TouchPhase::Up] {
        host.touch(TouchEvent::new(ContactId::panel(1), phase, 0.5, 0.5));
        for _ in 0..20 {
            host.pump(&mut render);
            render.pump();
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while Instant::now() < deadline && seen.len() < 2 {
        host.pump(&mut render);
        render.pump();
        seen.extend(drained(&input));
        std::thread::sleep(Duration::from_millis(50));
    }

    let contacts: Vec<_> = seen
        .iter()
        .filter_map(|event| match event {
            RemoteEvent::Input(input_touch::Input::Touch(t)) => Some(*t),
            _ => None,
        })
        .collect();
    assert!(
        contacts.len() >= 2,
        "a press and a release should have come back; got {seen:?}"
    );
    assert!(
        matches!(contacts[0].id.origin(), InputOrigin::Remote(_)),
        "the contact should belong to the peer that sent it, not the panel"
    );
    assert_eq!(contacts[0].phase, TouchPhase::Down);
    assert!(
        contacts.iter().any(|t| t.phase == TouchPhase::Up),
        "the release must arrive, or the panel holds a finger forever"
    );
    // The centre of the picture is the centre of the panel. Loose, because the video is
    // letterboxed into a stage of a different aspect and the page maps through that.
    assert!(
        (contacts[0].x - 0.5).abs() < 0.1 && (contacts[0].y - 0.5).abs() < 0.1,
        "a touch at the centre arrived at {:?}",
        (contacts[0].x, contacts[0].y)
    );

    stop.store(true, Ordering::Release);
    let _ = encoder.join();
    host.shutdown();
}
