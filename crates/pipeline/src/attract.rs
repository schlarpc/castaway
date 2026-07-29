//! The idle "attract" / lobby scene: what the panel shows when nothing is casting. It's
//! the first thing anyone sees, so it names the receiver and tells them how to throw
//! media at it. Rendered on the CPU (the shared [`crate::text`] rasterizer over a
//! gradient) into an RGBA image the compositor shows as a background layer (video covers
//! it when a cast starts).
//!
//! Pure/deterministic, so it unit-tests without a GPU and can be dumped to PNG ([`to_png`]).
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use ab_glyph::FontRef;

use crate::compositor::Transform;
use crate::error::PipelineError;
use crate::text::{self, Rgba};
use crate::theme;

/// The design resolution the layout is authored against; every dimension scales from it,
/// so the scene looks the same at 720p and 4K but is rasterized natively.
const DESIGN_HEIGHT: f32 = 720.0;

/// A pixel rectangle on the attract surface — where a *live* layer goes. Whole device
/// pixels, because the compositor maps that layer's texels 1:1 onto the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsetRect {
    /// Left edge in device pixels.
    pub x: u32,
    /// Top edge in device pixels.
    pub y: u32,
    /// Width in device pixels.
    pub width: u32,
    /// Height in device pixels.
    pub height: u32,
}

impl InsetRect {
    /// The compositor placement for this rect on a `surface_width`×`surface_height`
    /// surface: normalized scale + offset, one texel per device pixel.
    #[must_use]
    pub fn transform(self, surface_width: u32, surface_height: u32) -> Transform {
        let (sw, sh) = (surface_width.max(1) as f32, surface_height.max(1) as f32);
        Transform {
            scale_x: self.width as f32 / sw,
            scale_y: self.height as f32 / sh,
            offset_x: self.x as f32 / sw,
            offset_y: self.y as f32 / sh,
        }
    }
}

/// Whether the idle scene reserves room for the live web widget (the CEF clock). Without
/// a browser there is nothing to put there, so the text uses the full width instead of
/// leaving a hole — hence an enum the renderer must match on rather than a maybe-empty
/// rect threaded through every call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WidgetSlot {
    /// No widget: title and tagline are centered across the whole surface.
    #[default]
    None,
    /// Reserve a card in the top-right corner; the browser layer paints into it and the
    /// text block moves left to make room.
    RightCard,
}

impl WidgetSlot {
    /// The reserved rect for a `width`×`height` surface, or `None` when nothing is
    /// reserved. The single source of truth for the widget's geometry: the renderer draws
    /// the frame around it and the browser host sizes its viewport from it, so the two
    /// cannot drift.
    #[must_use]
    pub fn rect(self, width: u32, height: u32) -> Option<InsetRect> {
        match self {
            Self::None => None,
            Self::RightCard => {
                let (w, h) = (width.max(1), height.max(1));
                let margin = (90.0 * (h as f32 / DESIGN_HEIGHT)) as u32;
                // 16:9 and sized off the width, so a page written for a landscape
                // viewport isn't letterboxed inside a stripe. Clamped so a small or
                // portrait surface yields a valid (if cramped) rect rather than a
                // zero-sized texture.
                //
                // Sized as a *minimised app* rather than an ornament: this is where the
                // dashboard lives when nothing is streaming, so it has to be readable
                // from across the room, not merely present.
                let card_w = (w * 42 / 100).clamp(1, w);
                let card_h = (card_w * 9 / 16).clamp(1, h);
                // Vertically centred. It is the minimised app, not a corner ornament,
                // and centring also gives the mascot leaning on its top edge room to be
                // a readable size — hers is derived from the gap above it.
                let top = h.saturating_sub(card_h) / 2;
                Some(InsetRect {
                    x: w.saturating_sub(margin.saturating_add(card_w)),
                    y: top,
                    width: card_w,
                    height: card_h,
                })
            }
        }
    }
}

/// The full idle scene.
#[derive(Debug, Clone, PartialEq)]
pub struct AttractScene {
    /// Big title — the receiver's friendly name.
    pub title: String,
    /// One-line tagline under the title.
    pub tagline: String,
    /// Dim footer (network info).
    pub footer: String,
    /// Room reserved for the live web widget (the CEF clock layer).
    pub widget: WidgetSlot,
    /// A seasonal stripe under the title, if today is one (#24). `None` most of the
    /// year; the app decides, so the renderer stays pure and testable on any date.
    pub season: Option<crate::theme::Season>,
    /// Whether to draw DMA-chan in the corner.
    pub mascot: bool,
    /// Things the panel can start *itself*, as tappable tiles (D38).
    ///
    /// The rows above and these are two different kinds of thing, and the screen is
    /// clearer for keeping them apart: a row is an instruction aimed at your phone
    /// ("Cast → this name"), and the tap happens over there. A tile is something the
    /// panel goes and does, and the tap happens here. Empty by default, which is exactly
    /// what a receiver with no such sources should show.
    pub tiles: Vec<Tile>,
}

/// A tappable tile on the Home screen — one per thing the panel can be, or do.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    /// Opaque identity, echoed back when the tile is pressed. The shell does not know
    /// what a tile *means* — `app` does — so this is whatever string it finds useful.
    pub id: String,
    /// Short label under the glyph.
    pub label: String,
    /// Which glyph to draw.
    pub glyph: TileGlyph,
    /// Accent colour for the glyph and the tile's edge.
    pub accent: Rgba,
    /// What to show when it is pressed, for a service the panel simply *receives*.
    ///
    /// `Some` means the press is answered locally: the shell pushes a screen with this
    /// service's instructions and no round trip. That is most of them — Cast, AirPlay,
    /// DLNA, Spotify — where there is nothing to do but tell someone what to tap on
    /// their own device.
    ///
    /// `None` means the panel has to go and do something, so the press becomes an event
    /// `app` handles: GameStream opens a host picker, media opens a library.
    pub detail: Option<ServiceDetail>,
}

/// What a service's own screen says.
///
/// The instructions used to be rows on the idle screen, one per enabled protocol, which
/// made the first thing anyone saw a wall of text about six things they were not doing.
/// Now the idle screen shows what the panel *is*, and the details live one tap in.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceDetail {
    /// One line saying what this is: "Cast a tab, or your whole screen."
    pub headline: String,
    /// What to do, in order. Short enough to read from across a room.
    pub steps: Vec<String>,
    /// The exact name to look for in a picker, shown prominently — it is the one string
    /// that has to be got right, and the one nobody can guess.
    pub advertised: Option<String>,
}

/// The glyph a tile draws. Distance fields rather than an icon font, for the same reason
/// the transport strip's are (see [`crate::shape`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TileGlyph {
    /// A screen with signal arcs — Google Cast.
    Cast,
    /// A screen with an upward triangle — AirPlay.
    AirPlay,
    /// A plain screen — DLNA, and anything else that just wants a display.
    Screen,
    /// A waveform — audio: Spotify, Bluetooth, the visualizer.
    Waveform,
    /// A rounded frame with a play triangle — YouTube.
    Video,
    /// The Bluetooth rune.
    Bluetooth,
    /// A gamepad — GameStream / Moonlight.
    Gamepad,
    /// A folder — local media.
    Folder,
    /// A camera — the intercom view.
    Camera,
    /// A gear — settings.
    Gear,
}

impl AttractScene {
    /// A representative scene for previews/tests.
    #[must_use]
    pub fn demo() -> Self {
        use castaway_core::ProtocolKind;

        let name = "dma.space/screen";
        // Each tile names the *advertised* instance, `<name>#<protocol>`, because that is
        // the string the picker shows — one box appears in four pickers at once and a
        // bare name makes them indistinguishable. Built from `ProtocolKind::slug()`
        // rather than written out, so this preview cannot drift from what `app` really
        // advertises: it did exactly that once, and the screenshot told people to look
        // for a name no picker was showing.
        let advertised = |kind: ProtocolKind| format!("{name}#{}", kind.slug());
        Self {
            title: name.into(),
            tagline: "Throw anything at the wall — no app to install.".into(),
            footer: "castaway  •  DLNA / mDNS on 10.0.0.5:8080".into(),
            widget: WidgetSlot::RightCard,
            season: crate::theme::season(6, 15),
            mascot: true,
            tiles: vec![
                Tile {
                    id: "cast".into(),
                    label: "Google Cast".into(),
                    glyph: TileGlyph::Cast,
                    accent: [0x42, 0x85, 0xf4, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "Cast a tab, or your whole screen.".into(),
                        steps: vec![
                            "Open Chrome or Edge".into(),
                            "Menu → Cast, or the cast button in a video".into(),
                        ],
                        advertised: Some(advertised(ProtocolKind::Cast)),
                    }),
                },
                Tile {
                    id: "airplay".into(),
                    label: "AirPlay".into(),
                    glyph: TileGlyph::AirPlay,
                    accent: theme::TEXT,
                    detail: Some(ServiceDetail {
                        headline: "Mirror an iPhone, iPad or Mac.".into(),
                        steps: vec![
                            "Control Centre → Screen Mirroring".into(),
                            "Or the AirPlay button in a video or track".into(),
                        ],
                        advertised: Some(advertised(ProtocolKind::AirPlay)),
                    }),
                },
                Tile {
                    id: "dlna".into(),
                    label: "DLNA".into(),
                    glyph: TileGlyph::Screen,
                    accent: [0x56, 0xba, 0x5b, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "Send a video from Android or VLC.".into(),
                        steps: vec![
                            "VLC → the cast button".into(),
                            "Or any app with \"Play on\" / \"Cast to device\"".into(),
                        ],
                        advertised: Some(advertised(ProtocolKind::Dlna)),
                    }),
                },
                Tile {
                    id: "spotify".into(),
                    label: "Spotify".into(),
                    glyph: TileGlyph::Waveform,
                    accent: [0x1d, 0xb9, 0x54, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "Play to the room, and keep the phone as the remote.".into(),
                        steps: vec!["Play something".into(), "Tap Devices, bottom-left".into()],
                        advertised: Some(advertised(ProtocolKind::Spotify)),
                    }),
                },
                Tile {
                    id: "youtube".into(),
                    label: "YouTube".into(),
                    glyph: TileGlyph::Video,
                    accent: [0xff, 0x00, 0x00, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "The cast button in the YouTube app.".into(),
                        steps: vec!["Tap it, and pick this screen".into()],
                        advertised: Some(advertised(ProtocolKind::YouTubeLounge)),
                    }),
                },
                Tile {
                    id: "gamestream".into(),
                    label: "Moonlight".into(),
                    glyph: TileGlyph::Gamepad,
                    accent: theme::BLUE,
                    // No detail: this one the panel goes and does, so the press becomes
                    // an event and `app` opens a host picker.
                    detail: None,
                },
            ],
        }
    }
}

/// Where the Home screen's tiles land, in device pixels.
///
/// One layout serves drawing and hit-testing (D33): a tile cannot be drawn where it
/// cannot be pressed, or pressed where nothing is drawn. Returned as `(id, rect)` pairs
/// in the order they are drawn.
#[must_use]
pub fn tile_layout(
    scene: &AttractScene,
    width: u32,
    height: u32,
) -> Vec<(String, crate::shape::Rect)> {
    if scene.tiles.is_empty() {
        return Vec::new();
    }
    let s = height as f32 / DESIGN_HEIGHT;
    let margin = 90.0 * s;
    let gap = 26.0 * s;

    // The tiles *are* the screen now — the instructions that used to fill the left column
    // moved behind them. So they get the whole width left of the widget card, laid out as
    // a grid that wraps, rather than a row tucked into a corner.
    let card = scene.widget.rect(width, height);
    let avail = card.map_or(width as f32 - margin * 2.0, |c| {
        c.x as f32 - margin - 40.0 * s
    });
    let top = 300.0 * s;

    // Sized for a finger on a 65-inch panel: the tile *is* the touch target.
    let side = 150.0 * s;
    let per_row = (((avail + gap) / (side + gap)).floor() as usize).max(1);
    scene
        .tiles
        .iter()
        .enumerate()
        .map(|(i, tile)| {
            let (row, col) = (i / per_row, i % per_row);
            (
                tile.id.clone(),
                crate::shape::Rect {
                    x: margin + (side + gap) * col as f32,
                    y: top + (side + gap) * row as f32,
                    w: side,
                    h: side,
                },
            )
        })
        .collect()
}

/// Which tile a panel-normalized point is on, if any.
#[must_use]
pub fn tile_hit(scene: &AttractScene, width: u32, height: u32, x: f32, y: f32) -> Option<String> {
    let (px, py) = (x * width as f32, y * height as f32);
    tile_layout(scene, width, height)
        .into_iter()
        .find(|(_, rect)| rect.contains(px, py))
        .map(|(id, _)| id)
}

struct Palette {
    bg_top: Rgba,
    bg_bottom: Rgba,
    title: Rgba,
    tagline: Rgba,
    label: Rgba,
    footer: Rgba,
    card_edge: Rgba,
    card_bg: Rgba,
    tile_bg: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            bg_top: theme::BG_TOP,
            bg_bottom: theme::BG_BOTTOM,
            title: theme::TEXT,
            tagline: theme::ACCENT,
            label: theme::TEXT_BODY,
            footer: theme::TEXT_FAINT,
            card_edge: theme::EDGE,
            card_bg: theme::WELL,
            tile_bg: theme::PLATE,
        }
    }
}

/// Shrink `px` until `text` fits in `avail`. The rasterizer clips at the surface edge,
/// not at the widget card, so a long friendly name would otherwise run straight under it.
fn fit_px(font: &FontRef, text: &str, px: f32, avail: f32) -> f32 {
    let w = text::measure(font, text, px);
    if w <= avail || w <= 0.0 || avail <= 0.0 {
        px
    } else {
        px * avail / w
    }
}

/// Render the scene to an RGBA8 image of `width`×`height`.
///
/// # Errors
/// [`PipelineError`] if the embedded fonts fail to load (never, in practice).
pub fn render(scene: &AttractScene, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let f = text::fonts()?;
    let pal = Palette::default();

    let mut buf = vec![0u8; (width * height * 4) as usize];
    // The season colours the room, not a band across it. A stripe was the first attempt
    // and it read as a decoration stuck on top; the background is the thing that makes a
    // panel across a room feel different without asking anyone to look at it.
    match scene.season {
        Some(season) => text::fill_gradient_stops(&mut buf, width, height, &season.gradient()),
        None => text::fill_gradient(&mut buf, width, height, pal.bg_top, pal.bg_bottom),
    }

    let w = width as f32;
    // Scale everything relative to a 720p design so it looks right at any resolution.
    let s = height as f32 / DESIGN_HEIGHT;
    let margin = 90.0 * s;

    // The widget card, framed *around* the reserved rect: the browser layer covers that
    // rect exactly, so a frame drawn inside it would vanish on the first paint. The inner
    // fill is what an un-painted card shows — an empty panel rather than a hole.
    let slot = scene.widget.rect(width, height);
    if let Some(card) = slot {
        let edge = (3.0 * s).max(1.0);
        let (cx, cy) = (card.x as f32, card.y as f32);
        let (cw, ch) = (card.width as f32, card.height as f32);
        text::fill_rect(
            &mut buf,
            width,
            height,
            cx - edge,
            cy - edge,
            cw + edge * 2.0,
            ch + edge * 2.0,
            pal.card_edge,
        );
        text::fill_rect(&mut buf, width, height, cx, cy, cw, ch, pal.card_bg);
    }

    // The text column: the whole surface, or everything left of the widget card. The rows
    // below stay left-aligned either way — the card sits above them.
    let column = slot.map_or(w, |card| card.x as f32 - 40.0 * s);
    let avail = (column - margin * 2.0).max(1.0);

    // Title (bold, centered in the column).
    let title_px = fit_px(&f.bold, &scene.title, 76.0 * s, avail);
    let title_w = text::measure(&f.bold, &scene.title, title_px);
    let mut y = 120.0 * s + text::ascent(&f.bold, title_px);
    text::draw_text(
        &mut buf,
        width,
        height,
        (column - title_w) / 2.0,
        y,
        &scene.title,
        title_px,
        pal.title,
        &f.bold,
    );

    // Tagline (centered in the column).
    let tag_px = fit_px(&f.regular, &scene.tagline, 30.0 * s, avail);
    let tag_w = text::measure(&f.regular, &scene.tagline, tag_px);
    y += 46.0 * s + text::ascent(&f.regular, tag_px);
    text::draw_text(
        &mut buf,
        width,
        height,
        (column - tag_w) / 2.0,
        y,
        &scene.tagline,
        tag_px,
        pal.tagline,
        &f.regular,
    );

    // DMA-chan, bottom-right, behind nothing and in the way of nothing: the corner the
    // tiles and the widget card both leave empty.
    if scene.mascot {
        draw_mascot(&mut buf, width, height, s, slot);
    }

    // Tiles: what the panel can start itself. Drawn from the same layout the hit test
    // uses, so the two cannot disagree about where a tile is (D33).
    let tiles = tile_layout(scene, width, height);
    if let Some((_, first)) = tiles.first() {
        // A heading, because the two halves of this screen mean opposite things and
        // nothing else says so: the rows are what to do on your phone, these are what
        // this panel will go and do.
        let head_px = 22.0 * s;
        text::draw_text(
            &mut buf,
            width,
            height,
            first.x,
            first.y - 18.0 * s,
            "TAP FOR HOW, OR JUST CAST",
            head_px,
            pal.footer,
            &f.bold,
        );
    }
    for (tile, (_, rect)) in scene.tiles.iter().zip(tiles) {
        draw_tile(&mut buf, width, height, tile, rect, s, &f, &pal);
    }

    // Footer.
    let foot_px = 24.0 * s;
    let foot_baseline = height as f32 - 50.0 * s;
    text::draw_text(
        &mut buf,
        width,
        height,
        margin,
        foot_baseline,
        &scene.footer,
        foot_px,
        pal.footer,
        &f.regular,
    );

    Ok(buf)
}

/// The mascot, decoded from the vendored PNG and drawn at a height proportional to the
/// panel. Decoding per render is fine: Home is drawn on navigation and resize, not per
/// frame.
fn draw_mascot(buf: &mut [u8], width: u32, height: u32, s: f32, card: Option<InsetRect>) {
    // Two layers, as the site stacks them: the outer is her silhouette and the inner is
    // the body inside it. Drawing only one of them left her without a lower torso, which
    // read as a cropping bug rather than a missing layer.
    const MASCOT_OUTER: &[u8] = include_bytes!("../assets/brand/mascot-outer.png");
    const MASCOT_INNER: &[u8] = include_bytes!("../assets/brand/mascot-inner.png");
    let (Ok(outer), Ok(inner)) = (
        image::load_from_memory(MASCOT_OUTER),
        image::load_from_memory(MASCOT_INNER),
    ) else {
        return;
    };
    let outer = outer.to_rgba8();
    let inner = inner.to_rgba8();
    let img = &outer;
    // She hangs over the panel's top edge with her elbows dangling across it. Only the
    // very bottom of her crosses that line — everything below it lands inside the card's
    // rect, which the browser layer covers exactly, so the overhang is hidden behind the
    // panel she is leaning on without anything having to clip it.
    const OVERHANG: f32 = 0.10;
    // Sized from the room above the panel rather than from a number, so the overhang is
    // 10% at any resolution and she never has to be clamped against the top of the
    // screen — a clamp would silently push her further over the edge, which is exactly
    // what a fixed size did.
    let target_h = match card {
        Some(card) => {
            let room = (card.y as f32 - 12.0 * s).max(0.0);
            (room / (1.0 - OVERHANG)) as u32
        }
        None => (190.0 * s) as u32,
    };
    if target_h == 0 {
        return;
    }
    let (iw, ih) = (img.width().max(1), img.height().max(1));
    let target_w = (target_h as f32 * iw as f32 / ih as f32) as u32;
    let (ox, oy) = match card {
        Some(card) => (
            card.x as f32 + card.width as f32 * 0.64,
            card.y as f32 - target_h as f32 * (1.0 - OVERHANG),
        ),
        // No panel to lean on: stand her in the corner instead.
        None => (
            width as f32 - target_w as f32 - 70.0 * s,
            height as f32 - target_h as f32 - 60.0 * s,
        ),
    };
    // Inner (the lower torso) first, then outer over it: her arms and hands are on the
    // outer layer and have to occlude the body, not be painted under it.
    for layer in [&inner, &outer] {
        for py in 0..target_h {
            for px in 0..target_w {
                // Nearest-neighbour, like the album-art path: the source is far larger
                // than the target, so the artefacts a filter would fix are not visible.
                let sx = px * iw / target_w.max(1);
                let sy = py * ih / target_h.max(1);
                let p = layer.get_pixel(sx.min(iw - 1), sy.min(ih - 1)).0;
                if p[3] == 0 {
                    continue;
                }
                text::blend_over(
                    buf,
                    width,
                    height,
                    (ox + px as f32) as i32,
                    (oy + py as f32) as i32,
                    [p[0], p[1], p[2], 0xff],
                    f32::from(p[3]) / 255.0,
                );
            }
        }
    }
}

/// Draw one tile: a rounded plate, its glyph, and a label under it.
#[allow(clippy::too_many_arguments)]
fn draw_tile(
    buf: &mut [u8],
    width: u32,
    height: u32,
    tile: &Tile,
    rect: crate::shape::Rect,
    s: f32,
    f: &text::Fonts,
    pal: &Palette,
) {
    use crate::shape;

    let radius = 22.0 * s;
    // A plate a shade lighter than the background, with the accent as a thin edge. The
    // accent is not the fill: five saturated squares on a dark wall is a toy, and the
    // label has to stay readable across a room.
    shape::rounded_rect(buf, width, height, rect, radius, pal.tile_bg);
    shape::rounded_outline(
        buf,
        width,
        height,
        rect,
        radius,
        (2.0 * s).max(1.0),
        tile.accent,
    );

    let (cx, cy) = rect.center();
    // Glyph sits above centre; the label takes the bottom third.
    let gy = cy - rect.h * 0.10;
    let g = rect.h * 0.26;
    draw_tile_glyph(buf, (width, height), tile.glyph, (cx, gy), g, tile.accent);

    let label_px = 22.0 * s;
    let avail = rect.w - 12.0 * s;
    let px = fit_px(&f.regular, &tile.label, label_px, avail);
    let lw = text::measure(&f.regular, &tile.label, px);
    text::draw_text(
        buf,
        width,
        height,
        cx - lw / 2.0,
        rect.y + rect.h - 20.0 * s,
        &tile.label,
        px,
        pal.label,
        &f.regular,
    );
}

/// The tile glyphs, as distance fields. `g` is the glyph's half-extent.
///
/// `pub(crate)` because the service screen draws the same glyph large — the screen a tile
/// opens should look like the tile that opened it.
pub(crate) fn draw_tile_glyph(
    buf: &mut [u8],
    surface: (u32, u32),
    glyph: TileGlyph,
    at: (f32, f32),
    g: f32,
    color: Rgba,
) {
    let (width, height) = surface;
    let (cx, cy) = at;
    use crate::shape::{self, Rect};

    // A "screen" is the base of three of these; drawn once and reused so Cast, AirPlay
    // and DLNA read as a family rather than three unrelated pictures.
    let screen = |buf: &mut [u8], inset: f32| {
        let body = Rect {
            x: cx - g * (1.0 - inset),
            y: cy - g * (0.72 - inset * 0.5),
            w: g * 2.0 * (1.0 - inset),
            h: g * 1.44 * (1.0 - inset * 0.7),
        };
        shape::rounded_outline(buf, width, height, body, g * 0.16, g * 0.16, color);
    };
    let hole = theme::BG_TOP;

    match glyph {
        TileGlyph::Cast => {
            screen(buf, 0.0);
            // Three arcs from the bottom-left corner, the near-universal cast mark.
            let (ox, oy) = (cx - g * 0.66, cy + g * 0.44);
            for i in 1..=3 {
                let r = g * 0.26 * i as f32;
                shape::fill_sdf(
                    buf,
                    width,
                    height,
                    Rect::around(ox, oy, r + g * 0.2),
                    color,
                    |px, py| {
                        // Quarter arc: distance to the circle, clipped to up-and-right.
                        if px < ox - 0.5 || py > oy + 0.5 {
                            return 1e3;
                        }
                        shape::sd_circle(px, py, ox, oy, r).abs() - g * 0.07
                    },
                );
            }
        }
        TileGlyph::AirPlay => {
            screen(buf, 0.0);
            // The triangle sits below the screen, overlapping its lower edge.
            shape::fill_sdf(
                buf,
                width,
                height,
                Rect::around(cx, cy + g * 0.7, g),
                color,
                |px, py| {
                    shape::sd_triangle(
                        px,
                        py,
                        [
                            (cx, cy + g * 0.36),
                            (cx + g * 0.46, cy + g * 0.98),
                            (cx - g * 0.46, cy + g * 0.98),
                        ],
                    )
                },
            );
        }
        TileGlyph::Screen => {
            screen(buf, 0.0);
            // A stand, so it is a display rather than an empty rectangle.
            shape::rounded_rect(
                buf,
                width,
                height,
                Rect {
                    x: cx - g * 0.3,
                    y: cy + g * 0.72,
                    w: g * 0.6,
                    h: g * 0.2,
                },
                g * 0.08,
                color,
            );
        }
        TileGlyph::Video => {
            screen(buf, 0.0);
            shape::fill_sdf(
                buf,
                width,
                height,
                Rect::around(cx, cy, g),
                color,
                |px, py| {
                    shape::sd_triangle(
                        px,
                        py,
                        [
                            (cx - g * 0.24, cy - g * 0.34),
                            (cx + g * 0.42, cy),
                            (cx - g * 0.24, cy + g * 0.34),
                        ],
                    )
                },
            );
        }
        TileGlyph::Bluetooth => {
            // The rune: a vertical spine with two crossed pairs of diagonals.
            let (tx, ty) = (cx + g * 0.42, cy - g * 0.5);
            let (bx2, by2) = (cx + g * 0.42, cy + g * 0.5);
            let thick = g * 0.13;
            let seg = |buf: &mut [u8], ax: f32, ay: f32, bx: f32, by: f32| {
                shape::fill_sdf(
                    buf,
                    width,
                    height,
                    Rect::around((ax + bx) / 2.0, (ay + by) / 2.0, g * 1.2),
                    color,
                    |px, py| shape::sd_segment(px, py, ax, ay, bx, by) - thick / 2.0,
                );
            };
            seg(buf, cx, cy - g, cx, cy + g);
            seg(buf, cx, cy - g, tx, ty);
            seg(buf, tx, ty, cx - g * 0.42, cy);
            seg(buf, cx, cy + g, bx2, by2);
            seg(buf, bx2, by2, cx - g * 0.42, cy);
        }
        TileGlyph::Gamepad => {
            let body = Rect {
                x: cx - g,
                y: cy - g * 0.55,
                w: g * 2.0,
                h: g * 1.1,
            };
            shape::rounded_rect(buf, width, height, body, g * 0.45, color);
            let arm = g * 0.34;
            let thick = g * 0.12;
            shape::rounded_rect(
                buf,
                width,
                height,
                Rect {
                    x: cx - g * 0.62 - arm / 2.0,
                    y: cy - thick / 2.0,
                    w: arm,
                    h: thick,
                },
                thick / 2.0,
                hole,
            );
            shape::rounded_rect(
                buf,
                width,
                height,
                Rect {
                    x: cx - g * 0.62 - thick / 2.0,
                    y: cy - arm / 2.0,
                    w: thick,
                    h: arm,
                },
                thick / 2.0,
                hole,
            );
            shape::disc(
                buf,
                width,
                height,
                cx + g * 0.48,
                cy - g * 0.14,
                g * 0.22,
                hole,
            );
            shape::disc(
                buf,
                width,
                height,
                cx + g * 0.74,
                cy + g * 0.14,
                g * 0.22,
                hole,
            );
        }
        TileGlyph::Folder => {
            let tab = Rect {
                x: cx - g,
                y: cy - g * 0.72,
                w: g * 0.9,
                h: g * 0.3,
            };
            shape::rounded_rect(buf, width, height, tab, g * 0.12, color);
            let body = Rect {
                x: cx - g,
                y: cy - g * 0.48,
                w: g * 2.0,
                h: g * 1.2,
            };
            shape::rounded_rect(buf, width, height, body, g * 0.18, color);
        }
        TileGlyph::Camera => {
            let body = Rect {
                x: cx - g,
                y: cy - g * 0.5,
                w: g * 1.75,
                h: g * 1.1,
            };
            shape::rounded_rect(buf, width, height, body, g * 0.22, color);
            shape::fill_sdf(
                buf,
                width,
                height,
                Rect::around(cx + g * 0.6, cy, g),
                color,
                |px, py| {
                    shape::sd_triangle(
                        px,
                        py,
                        [
                            (cx + g * 0.78, cy - g * 0.42),
                            (cx + g * 1.18, cy),
                            (cx + g * 0.78, cy + g * 0.42),
                        ],
                    )
                },
            );
            shape::disc(buf, width, height, cx - g * 0.15, cy, g * 0.5, hole);
        }
        TileGlyph::Waveform => {
            let heights = [0.35_f32, 0.75, 1.0, 0.6, 0.45];
            let bar = g * 0.24;
            let step = g * 0.46;
            for (i, hfrac) in heights.iter().enumerate() {
                let x = cx - step * 2.0 + step * i as f32;
                let bh = g * hfrac;
                shape::rounded_rect(
                    buf,
                    width,
                    height,
                    Rect {
                        x: x - bar / 2.0,
                        y: cy - bh,
                        w: bar,
                        h: bh * 2.0,
                    },
                    bar / 2.0,
                    color,
                );
            }
        }
        TileGlyph::Gear => {
            shape::disc(buf, width, height, cx, cy, g * 1.6, color);
            for i in 0..6 {
                let a = std::f32::consts::TAU * i as f32 / 6.0;
                let (sx, sy) = (cx + a.cos() * g * 1.05, cy + a.sin() * g * 1.05);
                shape::disc(buf, width, height, sx, sy, g * 0.5, color);
            }
            shape::disc(buf, width, height, cx, cy, g * 0.7, hole);
        }
    }
}

/// Encode an RGBA image as PNG bytes (for previews / captures).
///
/// # Errors
/// [`PipelineError`] on encode failure.
pub fn to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| PipelineError::InvalidFrame(png_err(&e)))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| PipelineError::InvalidFrame(png_err(&e)))?;
    }
    Ok(out)
}

fn png_err(_e: &png::EncodingError) -> &'static str {
    "png encode failed"
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn renders_non_blank_scene_with_text() {
        let scene = AttractScene::demo();
        let (w, h) = (1280, 720);
        let img = render(&scene, w, h).unwrap();
        assert_eq!(img.len(), (w * h * 4) as usize);
        let bright = img
            .chunks_exact(4)
            .any(|p| u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]) > 600);
        assert!(
            bright,
            "expected bright text pixels over the dark background"
        );
        let png = to_png(w, h, &img).unwrap();
        assert!(png.starts_with(b"\x89PNG"), "valid PNG signature");
    }

    #[test]
    fn scales_to_4k_without_panicking() {
        let img = render(&AttractScene::demo(), 3840, 2160).unwrap();
        assert_eq!(img.len(), 3840 * 2160 * 4);
    }

    #[test]
    fn widget_card_is_a_16_9_rect_on_the_right_and_vertically_centred() {
        let (w, h) = (3840, 2160);
        let card = WidgetSlot::RightCard.rect(w, h).unwrap();
        let aspect = f64::from(card.width) / f64::from(card.height);
        assert!((aspect - 16.0 / 9.0).abs() < 0.02, "aspect {aspect}");
        // Inside the surface, in the top-right quadrant.
        assert!(card.x + card.width < w && card.y + card.height < h);
        assert!(card.x > w / 2, "the card belongs on the right");
        // Centred within a row of slack, rather than pinned to the top: the mascot's
        // size comes from the gap above it, so a card that crept upward would shrink her.
        let centre_error = (card.y as i64 - ((h - card.height) / 2) as i64).abs();
        assert!(centre_error < 4, "the card should be vertically centred");
        assert_eq!(WidgetSlot::None.rect(w, h), None);
    }

    /// The card's transform is what the compositor places the browser layer with, so its
    /// texels must land on whole device pixels — same reason as the OSD banner.
    #[test]
    fn widget_transform_maps_texels_onto_whole_device_pixels() {
        for (w, h) in [(1280, 720), (1920, 1080), (3840, 2160)] {
            let card = WidgetSlot::RightCard.rect(w, h).unwrap();
            let t = card.transform(w, h);
            // Sub-pixel tolerance, not f32::EPSILON: normalizing and re-multiplying by a
            // 4K dimension loses far more than one ulp, and what matters is that the quad
            // lands on the pixel grid.
            assert!((t.scale_x * w as f32 - card.width as f32).abs() < 0.01);
            assert!((t.scale_y * h as f32 - card.height as f32).abs() < 0.01);
            assert!((t.offset_x * w as f32 - card.x as f32).abs() < 0.01);
        }
    }

    #[test]
    fn degenerate_surface_still_yields_a_usable_card() {
        let card = WidgetSlot::RightCard.rect(0, 0).unwrap();
        assert!(card.width >= 1 && card.height >= 1);
        let t = card.transform(0, 0);
        assert!(t.scale_x.is_finite() && t.offset_y.is_finite());
    }

    /// The reserved card is empty background, and the text must not be drawn through it —
    /// so the pixels inside it stay the card fill even with a very long title.
    #[test]
    fn text_does_not_bleed_into_the_reserved_card() {
        let (w, h) = (1280, 720);
        let scene = AttractScene {
            title: "a-very-long-receiver-name.example.invalid".into(),
            widget: WidgetSlot::RightCard,
            // Without the mascot, who now leans on the card's top edge and *deliberately*
            // overlaps it — the browser layer covers that rect exactly, so her legs are
            // hidden behind the panel she is sitting on. This test is about text, which
            // has nowhere to hide.
            mascot: false,
            ..AttractScene::demo()
        };
        let img = render(&scene, w, h).unwrap();
        let card = WidgetSlot::RightCard.rect(w, h).unwrap();
        for y in card.y..card.y + card.height {
            for x in card.x..card.x + card.width {
                let p = ((y * w + x) * 4) as usize;
                let bright = u16::from(img[p]) + u16::from(img[p + 1]) + u16::from(img[p + 2]);
                assert!(bright < 60, "text bled into the card at {x},{y}");
            }
        }
    }
}
