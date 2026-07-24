//! Preview the whole idle screen with the live web widget in it: the attract scene, plus
//! a real CEF paint composited into the reserved card at exactly the geometry the kiosk
//! compositor would place it. Verifies the widget wiring without a display or a panel.
//!
//! `cargo run -p pipeline --example idle_preview --features cef -- out.png [WxH] [URL] [secs]`
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::time::{Duration, Instant};

use pipeline::attract::{AttractScene, WidgetSlot};
use pipeline::cef_browser::{BrowserRole, Cef, CefFrameSink};

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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .unwrap_or_else(|| "idle_preview.png".to_string());
    let (w, h) = args
        .next()
        .and_then(|s| {
            let (a, b) = s.split_once('x')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((3840u32, 2160u32));
    let url = args
        .next()
        .unwrap_or_else(|| "https://digitalclock.live/".to_string());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);

    // The background, with the card reserved — same call the kiosk makes.
    let scene = AttractScene {
        widget: WidgetSlot::RightCard,
        ..AttractScene::demo()
    };
    let mut frame = match pipeline::attract::render(&scene, w, h) {
        Ok(rgba) => rgba,
        Err(e) => {
            eprintln!("attract render failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The viewport the browser gets in this role — the card, in device pixels.
    let view = BrowserRole::AttractWidget.view((w, h));
    let card = view.rect;
    println!(
        "widget card: {}x{} at {},{} (z={})",
        card.width, card.height, card.x, card.y, view.z
    );

    if let Err(e) = cef.initialize() {
        eprintln!("cef initialize failed: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let sink = CefFrameSink::default();
    let _browser = match cef.create_offscreen(&url, card.width, card.height, sink.clone()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("create browser failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        cef.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    let Some(paint) = sink.latest() else {
        eprintln!("no frame painted after {secs}s");
        cef.shutdown();
        return std::process::ExitCode::FAILURE;
    };

    // Blit the paint into the card: the compositor's texel-per-pixel mapping, on the CPU.
    // BGRA in, RGBA out; the page is opaque, so this is a copy, not a blend.
    for row in 0..paint.height.min(h.saturating_sub(card.y)) {
        let cols = paint.width.min(w.saturating_sub(card.x)) as usize;
        let src = (row * paint.width) as usize * 4;
        let dst = ((card.y + row) * w + card.x) as usize * 4;
        for (out, px) in frame[dst..dst + cols * 4]
            .chunks_exact_mut(4)
            .zip(paint.bgra[src..src + cols * 4].chunks_exact(4))
        {
            out.copy_from_slice(&[px[2], px[1], px[0], 0xff]);
        }
    }

    match pipeline::attract::to_png(w, h, &frame) {
        Ok(png) => match std::fs::write(&out, png) {
            Ok(()) => println!("wrote {out} ({w}x{h}) with {url} in the widget card"),
            Err(e) => eprintln!("write png: {e}"),
        },
        Err(e) => eprintln!("png encode: {e}"),
    }

    cef.shutdown();
    std::process::ExitCode::SUCCESS
}
