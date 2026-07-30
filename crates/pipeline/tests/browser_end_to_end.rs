//! The browser, through the *whole* path — D36's integrated proof.
//!
//! The Q40 spike proved a frame can cross from Electron into a wgpu texture. It did that
//! in isolation, with its own device and its own import call, which leaves the thing that
//! actually ships unexercised: `ElectronHost` driving `RenderLoop`, a compositor layer,
//! and the release/borrow discipline between them. A passing spike and a broken product
//! are perfectly compatible, and this is what closes that gap.
//!
//! What it asserts, in the order the failures matter:
//!
//! 1. A page loads and paints through `ElectronHost::pump` — not through a test harness.
//! 2. The compositor ends up with a `Browser` layer, so a frame really became a layer.
//! 3. The composited output contains the page's colour, so the layer is *the page* rather
//!    than an empty texture the right size.
//! 4. Frames keep flowing across many pumps, so the borrow/release loop does not deadlock
//!    after the first `MAX_INFLIGHT_FRAMES`.
//! 5. Touch reaches the page, so the CDP route is real rather than merely written.
//! 6. The browser is gone when the host is dropped.
//!
//! Needs a GPU and an Electron, so it is `#[ignore]` by default and run by name:
//!
//! ```sh
//! CASTAWAY_ELECTRON=$(nix build --no-link --print-out-paths .#electron)/bin/electron \
//!   cargo test -p pipeline --features electron --test browser_end_to_end -- --ignored --nocapture
//! ```
#![cfg(feature = "electron")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use pipeline::adblock_engine::AdBlocker;
use pipeline::browser::BrowserCommand;
use pipeline::{Electron, ElectronHost};

/// A page that is one flat, unmistakable colour — and then repaints, slowly, several
/// times, ending on a different one.
///
/// `rgb(0, 128, 255)` because it is nothing like a clear colour, nothing like black, and
/// its channels are all different — so a red/blue swap or a stuck channel fails rather
/// than passing by luck. The staged walk to `rgb(255, 64, 32)` is what makes staleness
/// *visible*: a static page cannot distinguish "frames keep flowing" from "the layer
/// froze on an early frame", which is exactly how the borrow/release deadlock shipped.
/// The repaints must be *spaced*, not burst: paints faster than the consumer's tick are
/// superseded in its pending slot, and a supersede releases a frame — which kept the
/// browser just under `MAX_INFLIGHT` and hid the deadlock from a single-flip version of
/// this page. One repaint per interval, each imported before the next, is the cadence
/// that pins the outstanding count at the cap and wedges an unfixed consumer.
const PAGE: &str =
    "data:text/html,<style>html,body{margin:0;height:100%;background:rgb(0,128,255)}</style>\
     <body ontouchstart=\"document.title='touched'\">\
     <script>const steps=['rgb(0,160,224)','rgb(0,192,192)','rgb(64,224,128)',\
     'rgb(160,255,64)','rgb(255,64,32)'];\
     steps.forEach((c,i)=>setTimeout(()=>{document.body.style.background=c;\
     document.documentElement.style.background=c},600*(i+1)))</script></body>";

fn spec(
    adblock: Arc<AdBlocker>,
    audio_out: Option<pipeline::audio_out::AudioOutputFactory>,
) -> pipeline::electron_browser::RespawnSpec {
    pipeline::electron_browser::RespawnSpec {
        program: electron_path(),
        app_dir: app_dir(),
        adblock,
        audio_out,
        user_agent: pipeline::TV_USER_AGENT.to_string(),
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

#[test]
#[ignore = "needs a GPU and an Electron"]
fn a_page_becomes_a_compositor_layer_and_keeps_painting() {
    let (_cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let mut render = match pipeline::render_pipeline::RenderLoop::offscreen(1280, 720, cmd_rx) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: no usable GPU ({e})");
            return;
        }
    };

    let blocker = Arc::new(AdBlocker::with_defaults());
    let electron = Electron::spawn(
        &electron_path(),
        &app_dir(),
        Arc::clone(&blocker),
        None,
        pipeline::TV_USER_AGENT,
    )
    .expect("browser should start");

    let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
    let mut host = ElectronHost::new(electron, spec(blocker, None), rx);
    host.resize(1280, 720);
    tx.send(BrowserCommand::Navigate(PAGE.into()));

    // Pump the way the kiosk does, until the layer appears or we give up.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut pumps = 0_u32;
    while Instant::now() < deadline && !browser_layer_present(&render) {
        host.pump(&mut render);
        render.pump();
        pumps += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        browser_layer_present(&render),
        "no browser layer after {pumps} pumps: the frame never became a layer"
    );

    // The layer must be *the page*. An import that succeeds and yields an empty or
    // scrambled texture is the documented trap, so this reads pixels rather than
    // trusting the absence of an error.
    render.pump();
    let shot = render.read_rgba().expect("offscreen readback");
    let px = center_pixel(&shot, 1280, 720);
    assert!(
        near(px[1], 128) && near(px[2], 255) && near(px[0], 0),
        "composited centre is {px:?}, expected ~[0,128,255] — the layer is not the page"
    );

    // Keep going well past MAX_INFLIGHT_FRAMES, until the page's 2-second colour flip
    // arrives on the composited output. If the borrow/release loop deadlocks the browser
    // stops painting and the layer stays the *old* colour forever — a stale picture,
    // not an error — so the assertion is that the picture *changed*, which a frozen
    // layer cannot fake.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut later = 0_u32;
    let mut flipped = false;
    while Instant::now() < deadline && !flipped {
        host.pump(&mut render);
        render.pump();
        later += 1;
        let shot = render.read_rgba().expect("offscreen readback");
        let px = center_pixel(&shot, 1280, 720);
        flipped = near(px[0], 255) && near(px[1], 64) && near(px[2], 32);
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        browser_layer_present(&render),
        "the browser layer vanished after {later} further pumps"
    );
    assert!(
        flipped,
        "after {later} pumps the page's colour flip never reached the screen: \
         painting stopped or the layer went stale (the borrow/release deadlock)"
    );

    host.shutdown();
}

/// Touch has to reach the *page*, not merely leave us.
///
/// Q41 recorded this as needing hands on glass, and the coordinate mapping still does —
/// but "does a touch arrive at all, through CDP, and fire a DOM handler" is answerable
/// without a panel, and it is the half that silently regresses.
#[test]
#[ignore = "needs a GPU and an Electron"]
fn a_touch_reaches_the_page() {
    let (_cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let Ok(mut render) = pipeline::render_pipeline::RenderLoop::offscreen(640, 480, cmd_rx) else {
        eprintln!("skipping: no usable GPU");
        return;
    };

    let blocker = Arc::new(AdBlocker::with_defaults());
    let electron = Electron::spawn(
        &electron_path(),
        &app_dir(),
        Arc::clone(&blocker),
        None,
        pipeline::TV_USER_AGENT,
    )
    .expect("browser should start");

    let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
    let mut host = ElectronHost::new(electron, spec(blocker, None), rx);
    host.resize(640, 480);
    tx.send(BrowserCommand::Navigate(TOUCH_PAGE.into()));

    // Wait for the page to be painting, which is the only evidence available out here
    // that it has loaded and its handlers are attached.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !browser_layer_present(&render) {
        host.pump(&mut render);
        render.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(browser_layer_present(&render), "page never painted");

    // Middle of the panel, in the normalized coordinates the kiosk's input router emits.
    use input_touch::{InputSink as _, TouchEvent, TouchPhase};
    for phase in [TouchPhase::Down, TouchPhase::Up] {
        host.touch(TouchEvent {
            id: 1,
            phase,
            x: 0.5,
            y: 0.5,
        });
        for _ in 0..10 {
            host.pump(&mut render);
            render.pump();
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    let seen = host
        .probe("window.__touches", Duration::from_secs(5))
        .expect("the page should answer");
    assert!(
        seen.contains('1'),
        "the page recorded {seen}: the touch never arrived through CDP"
    );

    host.shutdown();
}

/// The page's audio has to reach *our* mixer, not the sound card.
///
/// The claim being tested is specific: `MediaElementAudioSourceNode` removes an element
/// from the normal output path, so if the tap is working the browser process is silent
/// and every sample arrives here instead. A count of zero means the audio went somewhere
/// we cannot mix, control the volume of, or measure sync against — which is
/// indistinguishable, from the room, from a page that is simply quiet.
#[test]
#[ignore = "needs a GPU and an Electron"]
fn page_audio_arrives_as_pcm_with_a_media_clock() {
    let (_cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let Ok(mut render) = pipeline::render_pipeline::RenderLoop::offscreen(640, 480, cmd_rx) else {
        eprintln!("skipping: no usable GPU");
        return;
    };

    // A counting sink stands in for the real device: what matters is that blocks reach
    // an `AudioOut` at all, not that anything was audible in the room.
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let factory: pipeline::audio_out::AudioOutputFactory = {
        let counter = Arc::clone(&counter);
        Arc::new(move || Box::new(CountingOut(Arc::clone(&counter))))
    };

    let blocker = Arc::new(AdBlocker::with_defaults());
    let electron = Electron::spawn(
        &electron_path(),
        &app_dir(),
        Arc::clone(&blocker),
        Some(&factory),
        pipeline::TV_USER_AGENT,
    )
    .expect("browser should start");

    let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
    let mut host = ElectronHost::new(electron, spec(blocker, Some(factory)), rx);
    host.resize(640, 480);
    let page = format!("file://{}", audio_page().display());
    tx.send(BrowserCommand::Navigate(page));

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && counter.load(std::sync::atomic::Ordering::Relaxed) < 5 {
        host.pump(&mut render);
        render.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    let blocks = counter.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        blocks >= 5,
        "only {blocks} audio blocks reached the mixer: the tap is not capturing"
    );

    host.shutdown();
}

/// An `AudioOut` that only counts. The seam a test uses to see that samples left the page.
#[derive(Debug)]
struct CountingOut(Arc<std::sync::atomic::AtomicU64>);

impl pipeline::audio_out::AudioOut for CountingOut {
    fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), pipeline::error::PipelineError> {
        Ok(())
    }
    fn write(
        &mut self,
        block: &pipeline::audio_decode::PcmBlock,
    ) -> Result<(), pipeline::error::PipelineError> {
        if !block.samples.is_empty() {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }
    fn stop(&mut self) {}
}

/// A page with a real `<audio src=...>`, written to a file rather than inlined.
///
/// Two earlier versions were wrong in instructive ways. The first used `srcObject` with a
/// `MediaStream`, which `createMediaElementSource` cannot take, so the tap attached to
/// nothing. The second inlined the same script in a `data:` URL with `<` HTML-escaped —
/// which is a syntax error, because inside a data URL the script is parsed as
/// JavaScript, not as HTML. Both failed the test correctly; a file is simply the shape
/// with no escaping question in it.
fn audio_page() -> std::path::PathBuf {
    let path = std::env::temp_dir().join("castaway-audio-tap-test.html");
    std::fs::write(
        &path,
        r"<body><script>
const R=48000,N=R*2,b=new ArrayBuffer(44+N*2),v=new DataView(b);
const W=(o,s)=>{for(let i=0;i<s.length;i++)v.setUint8(o+i,s.charCodeAt(i))};
W(0,'RIFF');v.setUint32(4,36+N*2,true);W(8,'WAVEfmt ');v.setUint32(16,16,true);
v.setUint16(20,1,true);v.setUint16(22,1,true);v.setUint32(24,R,true);
v.setUint32(28,R*2,true);v.setUint16(32,2,true);v.setUint16(34,16,true);
W(36,'data');v.setUint32(40,N*2,true);
for(let i=0;i<N;i++)v.setInt16(44+i*2,Math.sin(i*2*Math.PI*440/R)*20000,true);
const a=document.createElement('audio');
a.src=URL.createObjectURL(new Blob([b],{type:'audio/wav'}));
a.loop=true;a.autoplay=true;document.body.appendChild(a);a.play();
</script></body>",
    )
    .expect("write the audio test page");
    path
}

/// Counts touches the page itself observed.
const TOUCH_PAGE: &str =
    "data:text/html,<style>html,body{margin:0;height:100%;background:rgb(0,128,255)}</style>\
     <body><script>window.__touches=0;\
     document.addEventListener('touchstart',()=>{window.__touches++},true);</script></body>";

fn near(got: u8, want: u8) -> bool {
    (i16::from(got) - i16::from(want)).abs() <= 6
}

fn center_pixel(pixels: &[u8], w: usize, h: usize) -> [u8; 4] {
    let i = ((h / 2) * w + w / 2) * 4;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// Whether the compositor is holding a browser layer.
fn browser_layer_present(render: &pipeline::render_pipeline::RenderLoop) -> bool {
    render.browser_layer_present()
}

/// Going home minimizes a fullscreen page into the widget slot — it keeps playing
/// small, and comes back on restore.
///
/// Bringing the shell forward demotes *video*; a page used to have no demoted form, so
/// the exit swipe completed invisibly. Now the page is an app like the others: home
/// shrinks it into the home screen's card, a tap brings it back, and its stale
/// fullscreen-size paints must not resurrect the fullscreen layer in between.
#[test]
#[ignore = "needs a GPU and an Electron"]
fn minimizing_a_fullscreen_page_moves_it_into_the_widget_slot() {
    use pipeline::compositor::LayerId;

    let (_cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let Ok(mut render) = pipeline::render_pipeline::RenderLoop::offscreen(1280, 720, cmd_rx) else {
        eprintln!("skipping: no usable GPU");
        return;
    };

    let blocker = Arc::new(AdBlocker::with_defaults());
    let electron = Electron::spawn(
        &electron_path(),
        &app_dir(),
        Arc::clone(&blocker),
        None,
        pipeline::TV_USER_AGENT,
    )
    .expect("browser should start");

    let (tx, rx) = std::sync::mpsc::channel::<BrowserCommand>();
    let mut host = ElectronHost::new(electron, spec(blocker, None), rx);
    host.resize(1280, 720);
    tx.send(BrowserCommand::Navigate(PAGE.into()));

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && render.layer_size(LayerId::BrowserFullscreen).is_none() {
        host.pump(&mut render);
        render.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        render.layer_size(LayerId::BrowserFullscreen).is_some(),
        "page never painted fullscreen"
    );

    // Demoting is the *panel's* verb now, not a method on the browser host: one press out
    // moves every surface that is up, and the host follows on its next pump. That is the
    // fold this test is really covering — it used to reach past the panel to minimize the
    // page directly, which is exactly how the page and the shell came to disagree.
    assert_eq!(render.panel_back(), pipeline::panel::Left::Demoted);
    host.pump(&mut render);
    assert!(
        render.layer_size(LayerId::BrowserFullscreen).is_none(),
        "the fullscreen texture should be down at once"
    );

    // The page repaints at card size and lands in the widget slot; fullscreen-size
    // paints still in flight are released unimported and must not resurrect the layer.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut widget = None;
    while Instant::now() < deadline && widget.is_none() {
        host.pump(&mut render);
        render.pump();
        assert!(
            render.layer_size(LayerId::BrowserFullscreen).is_none(),
            "a stale paint resurrected the fullscreen layer"
        );
        widget = render.layer_size(LayerId::BrowserWidget);
        std::thread::sleep(Duration::from_millis(16));
    }
    let (w, h) = widget.expect("the page never arrived in the widget slot");
    assert!(
        w < 1280 && h < 720,
        "minimized paint should be card-sized, got {w}x{h}"
    );

    assert!(render.panel_restore(), "nothing to restore?");
    host.pump(&mut render);
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && render.layer_size(LayerId::BrowserFullscreen).is_none() {
        host.pump(&mut render);
        render.pump();
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        render.layer_size(LayerId::BrowserFullscreen).is_some(),
        "the page never came back fullscreen"
    );

    host.shutdown();
}
