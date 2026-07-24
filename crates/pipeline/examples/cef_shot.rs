//! Render a URL offscreen with CEF and save a PNG — proves the CEF pipeline works.
//!
//! `cargo run -p pipeline --example cef_shot --features cef -- out.png [URL] [WxH] [secs]`

use std::time::{Duration, Instant};

use pipeline::cef_browser::{Cef, CefFrameSink};

fn main() -> std::process::ExitCode {
    // MUST be first: a subprocess invocation re-execs this binary and returns None.
    let mut cef = match Cef::bootstrap() {
        Ok(Some(cef)) => cef,
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cef bootstrap failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Show adblock (and other) logs. `castaway::adblock` logs at INFO on each block.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| "cef_shot.png".to_string());
    let url = args
        .next()
        .unwrap_or_else(|| "https://example.com".to_string());
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((1280u32, 720u32));
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    // Optional 5th arg: a filter-list file (e.g. EasyList) to use instead of the
    // compact built-in list.
    if let Some(list_path) = args.next() {
        match std::fs::read_to_string(&list_path) {
            Ok(text) => {
                cef.set_adblock(pipeline::cef_adblock::AdBlocker::from_list_text(&text));
                eprintln!("loaded filter list from {list_path}");
            }
            Err(e) => eprintln!("could not read {list_path}: {e}"),
        }
    }

    if let Err(e) = cef.initialize() {
        eprintln!("cef initialize failed: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let sink = CefFrameSink::default();
    let _browser = match cef.create_offscreen(&url, w, h, sink.clone()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("create browser failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Pump the message loop while the page loads; grab the freshest frame.
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut paints = 0u32;
    while Instant::now() < deadline {
        cef.pump();
        if sink.latest().is_some() {
            paints += 1;
        }
        std::thread::sleep(Duration::from_millis(16));
    }

    let (seen, blocked) = cef.adblock_stats();
    println!("adblock: {blocked} blocked / {seen} requests inspected");
    println!("--- top hosts requested (host: seen/blocked) ---");
    for (host, s, b) in cef.adblock_hosts().into_iter().take(30) {
        println!("  {host}: {s}/{b}");
    }

    let Some(frame) = sink.latest() else {
        eprintln!("no frame painted after {secs}s (paints seen: {paints})");
        cef.shutdown();
        return std::process::ExitCode::FAILURE;
    };

    // CEF paints BGRA; swizzle to RGBA for PNG.
    let mut rgba = frame.bgra.clone();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    match pipeline::attract::to_png(frame.width, frame.height, &rgba) {
        Ok(png) => {
            if let Err(e) = std::fs::write(&out, png) {
                eprintln!("write png: {e}");
            } else {
                println!("wrote {out} ({}x{}) after {paints} paints", frame.width, frame.height);
            }
        }
        Err(e) => eprintln!("png encode: {e}"),
    }

    cef.shutdown();
    std::process::ExitCode::SUCCESS
}
