//! Whole frames, compared against images somebody looked at once and blessed.
//!
//! Every other pixel assertion in this crate samples: a handful of coordinates in
//! `wgpu_compositor::tests`, a centre pixel ±2 in `nv12::gpu`, an alpha probe in `shape`,
//! or a liveness check — `any(p[0] > 8)`, `len == w*h*4`, `png[1..4] == b"PNG"`. None of
//! them can see "the layout is subtly wrong", "the text did not render", "the theme is
//! inverted", or "the card is drawing the previous track" (#203). D38 defers a font change
//! to avoid invalidating the golden-image tests; until this file there were none.
//!
//! **What this covers.** Six scenes rasterised on the CPU — `attract`, `service`,
//! `picker`, `nowplaying_card`, `transport` — which is where the layout, the type and the
//! palette live, and which need no GPU at all, so they run in every check on every box.
//! Then two *composited* ones, where the panel is a wgpu stack of those same rasters and
//! the subject is the compositor's geometry: a session full-screen, and a session demoted
//! to its corner with the shell behind it. Those two skip where there is honestly no
//! adapter and fail where a build promised one, through `test_gpu` like every other pixel
//! test here.
//!
//! **The comparison is a tolerance, not a hash** — an exact compare goes red on a
//! rasteriser bump that moved four antialiased edges, and that is how golden-image suites
//! end up disabled. But it is *two* tolerances, because one is not enough and the first
//! version of this file proved it: whole-frame mean alone let a deliberate sixteen-level
//! recolouring of every subtitle through, since type is a percent or two of a screen. So a
//! mean over the frame **and** a count of pixels that moved a long way, and a scene has to
//! satisfy both. See [`MEAN_TOLERANCE`] and [`CHANGED_FRACTION`].
//!
//! **When one of these fails legitimately** — a deliberate design change — look at the
//! images the failure writes into `CARGO_TARGET_TMPDIR` (it prints the paths), decide the
//! new one is right, and re-bless with `CASTAWAY_BLESS_GOLDEN=1 cargo test -p pipeline
//! --test golden_scenes`. Blessing is a decision, which is why it is a separate act and
//! not a fallback the test does on its own when a file is missing.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use castaway_core::{
    Artwork, ControlCapabilities, ImageFormat, NowPlaying, PlaybackState, QueueItem, RepeatMode,
    SourceDescription,
};
use pipeline::attract::{to_png, AttractScene};
use pipeline::nowplaying_card::NowPlayingCard;

/// Mean absolute per-channel difference over the whole frame, in 0–255 units.
///
/// This is the coarse half, and on its own it is far too coarse: text is a percent or two
/// of a screen, so recolouring every subtitle by sixteen levels moves the mean by 0.05 and
/// sails under any threshold worth having. Measured, not assumed — the first version of
/// this file used the mean alone and two deliberate palette changes passed it.
const MEAN_TOLERANCE: f64 = 2.0;

/// How far one channel of one pixel may move before that pixel counts as changed.
///
/// Above the few levels a rasteriser can shift an antialiased edge by, well below a
/// colour a person would call different.
const CHANNEL_TOLERANCE: u8 = 6;

/// How much of the frame may be changed by that much before the scene is a different
/// scene.
///
/// Two hundredths of a percent — about 100 pixels of a 960×540 screen, which is a few
/// glyphs. A moved line of type, a restyled button or an inverted theme are all far above
/// it; a handful of edge pixels landing differently is below.
const CHANGED_FRACTION: f64 = 0.0002;

/// The environment variable that turns a comparison into a recording.
const BLESS: &str = "CASTAWAY_BLESS_GOLDEN";

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden")
}

/// Where a failure leaves the evidence. Cargo guarantees this for integration tests.
fn scratch() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// Encode a blessed image as small as PNG will go.
///
/// Not `attract::to_png`, which is the panel's own encoder and is tuned the other way —
/// it serves `/screenshot.png` on request, where spending a second on compression would
/// be the wrong trade. These are written once and then live in the repository forever, and
/// the difference is not marginal: the same 4K strip is 960 kB at the panel's setting and
/// 64 kB at this one, because the gradients are blue-noise dithered and cheap filtering
/// leaves that noise in the stream.
fn to_small_png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::ImageEncoder as _;
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Best, FilterType::Adaptive)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .unwrap();
    out
}

/// Compare one rendered scene against its blessed image.
///
/// # Panics
/// If the scene has moved past either tolerance, or has never been blessed.
fn assert_golden(name: &str, width: u32, height: u32, rgba: &[u8]) {
    assert_eq!(
        rgba.len(),
        (width as usize) * (height as usize) * 4,
        "{name}: a renderer returned a buffer that is not the surface it was asked for"
    );
    let path = golden_dir().join(format!("{name}.png"));

    if std::env::var_os(BLESS).is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, to_small_png(width, height, rgba)).unwrap();
        eprintln!("blessed {} ({width}x{height})", path.display());
        return;
    }

    let blessed = image::load_from_memory(&std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{name}: no blessed image at {} ({e}). If this scene is new, look at what it \
             renders and then record it with {BLESS}=1",
            path.display()
        )
    }))
    .unwrap()
    .to_rgba8();

    assert_eq!(
        (blessed.width(), blessed.height()),
        (width, height),
        "{name}: the blessed image is {}x{} and the scene now renders {width}x{height}",
        blessed.width(),
        blessed.height()
    );

    // Two readings of the same difference, because they miss opposite things. The mean
    // sees a whole surface drifting — a gradient, a theme, a channel swap — and cannot see
    // type. The count sees a few hundred pixels moving a long way, which is what a
    // relocated line, a missing string or a restyled control looks like, and cannot see a
    // whole frame moving one level.
    let mut total = 0f64;
    let mut changed = 0usize;
    for (before, after) in blessed.as_raw().chunks_exact(4).zip(rgba.chunks_exact(4)) {
        let mut worst = 0u8;
        for (&a, &b) in before.iter().zip(after) {
            let d = a.abs_diff(b);
            total += f64::from(d);
            worst = worst.max(d);
        }
        if worst > CHANNEL_TOLERANCE {
            changed += 1;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let mean = total / rgba.len() as f64;
    #[allow(clippy::cast_precision_loss)]
    let fraction = changed as f64 / (rgba.len() / 4) as f64;
    if mean <= MEAN_TOLERANCE && fraction <= CHANGED_FRACTION {
        return;
    }

    // Leave something to look at. A number is not enough to decide whether a difference
    // is a regression or a design change, and re-rendering it by hand means reconstructing
    // the scene from the test.
    let actual = scratch().join(format!("{name}.actual.png"));
    let diff = scratch().join(format!("{name}.diff.png"));
    std::fs::write(&actual, to_png(width, height, rgba).unwrap()).unwrap();
    // Amplified, because the interesting failures are small: an eight-times difference is
    // still visible when the real one is a few levels.
    let amplified: Vec<u8> = blessed
        .as_raw()
        .iter()
        .zip(rgba)
        .enumerate()
        .map(|(i, (&a, &b))| {
            if i % 4 == 3 {
                0xff
            } else {
                a.abs_diff(b).saturating_mul(8)
            }
        })
        .collect();
    std::fs::write(&diff, to_png(width, height, &amplified).unwrap()).unwrap();
    panic!(
        "{name}: mean absolute error {mean:.3}/255 (limit {MEAN_TOLERANCE}) and \
         {:.4}% of pixels changed by more than {CHANNEL_TOLERANCE}/255 (limit {:.4}%). \
         Rendered: {}. Difference (×8): {}. Blessed: {}",
        fraction * 100.0,
        CHANGED_FRACTION * 100.0,
        actual.display(),
        diff.display(),
        path.display()
    );
}

/// The panel is 3840×2160 and these scenes are blessed at a quarter of it.
///
/// Every layout in this crate is scale-driven from `DESIGN_HEIGHT`, so the size is a
/// choice about what a fixture costs rather than about coverage: a 4K screen is a nine
/// megabyte PNG, and a repository that is otherwise text should not gain nine of those
/// every time somebody legitimately changes a colour. The strip below is the exception,
/// and is blessed at the panel's own resolution, because scaling is the thing it is there
/// to check.
const W: u32 = 960;
const H: u32 = 540;

#[test]
fn the_home_screen() {
    // What the room looks at when nothing is casting, which is most of the time. Every
    // tile, its glyph, the seasonal gradient, the footer and the widget frame.
    let scene = AttractScene::demo();
    assert_golden(
        "home",
        W,
        H,
        &pipeline::attract::render(&scene, W, H).unwrap(),
    );
}

#[test]
fn a_service_screen_one_press_in() {
    let home = AttractScene::demo();
    let tile = home.tiles.iter().find(|t| t.id == "cast").cloned().unwrap();
    let detail = tile.detail.clone().unwrap();
    let screen = pipeline::service::ServiceScreen { tile, detail };
    assert_golden(
        "service-cast",
        W,
        H,
        &pipeline::service::render(&screen, W, H).unwrap(),
    );
}

#[test]
fn a_picker_two_presses_in() {
    // The deepest the shell goes, and the one screen whose content is a list rather than
    // a fixed layout — so it is where a row that renders off the bottom would hide.
    use pipeline::picker::{Picker, PickerItem};
    let hosts = Picker::loading("Moonlight", "Looking for hosts…")
        .with_subtitle("Moonlight hosts on this network")
        .with_items(
            vec![
                PickerItem::new("a", "somepc").with_detail("10.0.0.7  ·  paired"),
                PickerItem::new("b", "loungebox").with_detail("10.0.0.9  ·  not paired"),
                PickerItem::new("c", "basement-rig").with_detail("10.0.0.22  ·  paired"),
            ],
            "No hosts found on this network",
        );
    assert_golden(
        "picker-hosts",
        W,
        H,
        &pipeline::picker::render(&hosts, W, H).unwrap(),
    );
}

/// A cover, generated rather than checked in: deterministic, and small enough that the
/// fixture is the *card*, not somebody's album art.
fn cover(size: u32) -> Artwork {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x * 255 / size.max(1), y * 255 / size.max(1));
            rgba.extend_from_slice(&[
                u8::try_from(fx).unwrap(),
                u8::try_from(fy).unwrap(),
                0x80,
                0xff,
            ]);
        }
    }
    Artwork::new(ImageFormat::Png, to_png(size, size, &rgba).unwrap().into())
}

fn spotify_card() -> NowPlayingCard {
    NowPlayingCard {
        track: NowPlaying::new(PlaybackState::Playing)
            .with_title("Windowlicker")
            .with_artist("Aphex Twin")
            .with_album("Windowlicker"),
        source: SourceDescription::new()
            .with_display_name("schlarpc")
            .with_link("Spotify Connect · 44100 Hz · stereo"),
        up_next: Vec::new(),
        controls: ControlCapabilities::TRANSPORT
            | ControlCapabilities::SEEK
            | ControlCapabilities::SHUFFLE
            | ControlCapabilities::REPEAT,
    }
}

#[test]
fn a_now_playing_card_with_cover_art_and_a_queue() {
    // Mid-track, shuffling, repeating: the art panel filled, more queued than the card
    // draws, and the scrubber somewhere other than an end.
    let mut card = spotify_card();
    card.track = card
        .track
        .clone()
        .with_artwork(cover(300))
        .with_shuffle(true)
        .with_repeat(RepeatMode::Context);
    card.track.position = Some(std::time::Duration::from_secs(97));
    card.track.duration = Some(std::time::Duration::from_secs(366));
    card.up_next = vec![
        QueueItem::new("Come to Daddy").with_artist("Aphex Twin"),
        QueueItem::new("Roygbiv").with_artist("Boards of Canada"),
        QueueItem::new("Xtal").with_artist("Aphex Twin"),
        QueueItem::new("Alberto Balsalm").with_artist("Aphex Twin"),
        QueueItem::new("Rhubarb").with_artist("Aphex Twin"),
    ];
    assert_golden(
        "card-with-art",
        W,
        H,
        &pipeline::nowplaying_card::render(&card, W, H).unwrap(),
    );
}

#[test]
fn a_now_playing_card_before_the_cover_has_arrived() {
    // The state every Spotify session is in for its first second, and the state a
    // Bluetooth phone that sends no cover stays in forever (#50). The art panel draws
    // itself empty rather than leaving a hole, and that is what this pins.
    assert_golden(
        "card-no-art",
        W,
        H,
        &pipeline::nowplaying_card::render(&spotify_card(), W, H).unwrap(),
    );
}

#[test]
fn the_transport_strip_at_the_panels_own_resolution() {
    // 3840×2160, because the strip's geometry is derived from the surface and the panel is
    // the only surface that matters. Everything else here is blessed at half size; this is
    // the one where the scaling is the subject.
    let (w4k, h4k) = (3840, 2160);
    let mut card = spotify_card();
    card.track.position = Some(std::time::Duration::from_secs(97));
    card.track.duration = Some(std::time::Duration::from_secs(366));
    card.track = card.track.clone().with_shuffle(false);
    card.track.repeat = Some(RepeatMode::Off);

    let (_x, y, sw, sh) = pipeline::transport::placement(w4k, h4k);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (pw, ph) = (sw.round() as u32, sh.round() as u32);
    #[allow(clippy::cast_precision_loss)]
    let (top, bottom) = pipeline::nowplaying_card::background_span(y / h4k as f32, 1.0);
    let strip = pipeline::transport::render(&card.transport(), pw, ph, top, bottom).unwrap();
    assert_golden("transport-strip-4k", pw, ph, &strip);
}

// ---- the composited scenes -----------------------------------------------------------
//
// Everything above is a raster this crate produced on the CPU. These two are the whole
// panel: the same rasters uploaded as textures and stacked by the compositor, which is
// where the *geometry* lives — full-screen fit, the demoted corner, the floor behind it.
//
// They are goldenable across Vulkan implementations, which is not obvious and was
// measured rather than assumed: the same composited frame under this box's hardware
// adapter and under Mesa's lavapipe (what `nix flake check` supplies) differs by **at most
// one level on any channel**, mean 0.007/255, with not one pixel over
// [`CHANNEL_TOLERANCE`]. So a blessed image travels between a developer's GPU and CI.

/// A frame whose content says which way up it is and which channel is which.
///
/// Flat grey — what the other GPU tests use, because they are asking a different question
/// — would hide a vertical flip, a channel swap and a fit that stretched instead of
/// letterboxing, which are three of the four things a composited golden is here for.
fn test_pattern(width: u32, height: u32) -> castaway_core::DecodedFrame {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let (left, top) = (x < width / 2, y < height / 2);
            let ramp = u8::try_from(x * 200 / width.max(1)).unwrap_or(u8::MAX);
            let pixel: [u8; 4] = match (left, top) {
                (true, true) => [0xd0, 0x20, 0x20, 0xff],
                (false, true) => [0x20, 0xd0, 0x20, 0xff],
                (true, false) => [0x20, 0x20, 0xd0, 0xff],
                (false, false) => [ramp, ramp, 0x40, 0xff],
            };
            data.extend_from_slice(&pixel);
        }
    }
    castaway_core::DecodedFrame {
        width,
        height,
        pts: std::time::Duration::ZERO,
        image: castaway_core::FrameImage::Cpu {
            format: castaway_core::PixelFormat::Rgba8,
            data: bytes::Bytes::from(data),
        },
    }
}

/// Run every motion out, the way the kiosk does frame by frame.
///
/// Motion is `dt`-driven rather than clock-driven (`tick_motion`), which is what makes a
/// composited golden possible at all: the frame these tests read back is the settled one
/// every time, not wherever a spring happened to be when the test got there.
fn settle(render: &mut pipeline::render_pipeline::RenderLoop) {
    let mut guard = 0;
    while render.tick_motion(std::time::Duration::from_millis(16)) {
        guard += 1;
        assert!(guard < 600, "a motion never settled");
    }
    render.pump();
}

#[test]
fn a_session_playing_full_screen() {
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };
    tx.send(pipeline::render_pipeline::RenderCommand::Home(Box::new(
        AttractScene::demo(),
    )));
    // 16:9 into 16:9: the picture fills the panel and the shell is behind it, invisible.
    tx.send(pipeline::render_pipeline::RenderCommand::Video(
        test_pattern(1280, 720),
    ));
    render.pump();
    settle(&mut render);
    assert!(render.pip_rect().is_none(), "nothing else wants the panel");
    assert_golden("session-fullscreen", W, H, &render.read_rgba().unwrap());
}

#[test]
fn a_session_demoted_to_the_corner_while_someone_uses_the_shell() {
    // #27/#28: pressing Home during a film demotes the picture rather than stopping it.
    // The rules are unit-tested in `panel`, the seam is driven in `pip_and_idle`, and this
    // is the only thing that looks at the result — that the corner is where the model says
    // and the shell behind it is drawn, not merely reachable.
    let (tx, rx) = pipeline::render_channel(8);
    let Some(mut render) = pipeline::test_gpu::render_loop(W, H, rx) else {
        return;
    };
    tx.send(pipeline::render_pipeline::RenderCommand::Home(Box::new(
        AttractScene::demo(),
    )));
    tx.send(pipeline::render_pipeline::RenderCommand::Video(
        test_pattern(1280, 720),
    ));
    render.pump();
    render.set_shell_foreground(true);
    settle(&mut render);
    assert!(render.pip_rect().is_some(), "the video should be demoted");
    assert_golden(
        "session-demoted-over-home",
        W,
        H,
        &render.read_rgba().unwrap(),
    );
}
