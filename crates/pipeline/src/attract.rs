//! The idle "attract" / lobby scene: what the panel shows when nothing is casting. It's
//! the first thing anyone sees, so it names the receiver and shows what it can do —
//! and, deliberately, nothing else. Rendered on the CPU (the shared [`crate::text`] rasterizer over a
//! gradient) into an RGBA image the compositor shows as a background layer (video covers
//! it when a cast starts).
//!
//! Pure/deterministic, so it unit-tests without a GPU and can be dumped to PNG ([`to_png`]).
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

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

/// Whether the idle scene reserves room for the live web widget (the browser clock). Without
/// a browser there is nothing to put there, so the text uses the full width instead of
/// leaving a hole — hence an enum the renderer must match on rather than a maybe-empty
/// rect threaded through every call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WidgetSlot {
    /// No widget: the screen uses its full width.
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
    /// Dim footer: what this is, which build, and where to reach it.
    ///
    /// The only text on this screen that is not the receiver's name or a tile label. The
    /// idle screen used to carry a tagline and a line telling you to tap something; both
    /// were read once by whoever wrote them and by nobody since.
    pub footer: String,
    /// Room reserved for the live web widget (the browser clock layer).
    pub widget: WidgetSlot,
    /// The season colouring the background, if today is in one (#24). `None` most of the
    /// year; the app decides, so the renderer stays pure and testable on any date.
    pub season: Option<crate::theme::Season>,
    /// Whether to draw DMA-chan in the corner.
    pub mascot: bool,
    /// Every service, as a tappable tile (D38).
    ///
    /// Two kinds behind one shape, told apart by [`Tile::detail`]: a tile whose service
    /// you drive from your own phone opens a screen saying what to tap over there, and a
    /// tile the panel acts on itself becomes an event for `app`. Empty by default, which
    /// is exactly what a receiver with nothing enabled should show.
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
    /// A payload to render as a QR code beside the instructions, when the
    /// protocol has one a phone scans instead of typing — FCast's `fcast://r/…`
    /// connection URL, and (the reuse this was factored for) a Matter `MT:…`
    /// commissioning code. `None` leaves the screen text-only.
    pub qr_payload: Option<String>,
}

/// The glyph a tile draws. Distance fields rather than an icon font, for the same reason
/// the transport strip's are (see [`crate::shape`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TileGlyph {
    /// Google Cast.
    Cast,
    /// AirPlay.
    AirPlay,
    /// DLNA.
    Dlna,
    /// Spotify.
    Spotify,
    /// YouTube.
    YouTube,
    /// Bluetooth.
    Bluetooth,
    /// Moonlight.
    Moonlight,
    /// Matter Cast.
    MatterCast,
    /// A folder — a local media library.
    Folder,
    /// Miracast.
    Miracast,
    /// A cog — settings.
    Gear,
    /// FCast.
    FCast,
}

impl TileGlyph {
    /// The vendored artwork for this mark.
    ///
    /// One file per variant, matched here and nowhere else, so adding a variant is a
    /// compile error until it has a mark to draw.
    const fn svg(self) -> &'static str {
        match self {
            Self::Cast => include_str!("../assets/glyphs/cast.svg"),
            Self::AirPlay => include_str!("../assets/glyphs/airplay.svg"),
            Self::Dlna => include_str!("../assets/glyphs/dlna.svg"),
            Self::Spotify => include_str!("../assets/glyphs/spotify.svg"),
            Self::YouTube => include_str!("../assets/glyphs/youtube.svg"),
            Self::Bluetooth => include_str!("../assets/glyphs/bluetooth.svg"),
            Self::Moonlight => include_str!("../assets/glyphs/moonlight.svg"),
            Self::MatterCast => include_str!("../assets/glyphs/matter.svg"),
            Self::Folder => include_str!("../assets/glyphs/folder.svg"),
            Self::Miracast => include_str!("../assets/glyphs/miracast.svg"),
            Self::Gear => include_str!("../assets/glyphs/gear.svg"),
            Self::FCast => include_str!("../assets/glyphs/fcast.svg"),
        }
    }
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
            footer: "castaway  •  0a1b2c3  •  10.0.0.5:8080".into(),
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
                        qr_payload: None,
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
                        qr_payload: None,
                    }),
                },
                Tile {
                    id: "dlna".into(),
                    label: "DLNA".into(),
                    glyph: TileGlyph::Dlna,
                    accent: [0x56, 0xba, 0x5b, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "Send a video from Android or VLC.".into(),
                        steps: vec![
                            "VLC → the cast button".into(),
                            "Or any app with \"Play on\" / \"Cast to device\"".into(),
                        ],
                        advertised: Some(advertised(ProtocolKind::Dlna)),
                        qr_payload: None,
                    }),
                },
                Tile {
                    id: "spotify".into(),
                    label: "Spotify".into(),
                    glyph: TileGlyph::Spotify,
                    accent: [0x1d, 0xb9, 0x54, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "Play to the room, and keep the phone as the remote.".into(),
                        steps: vec!["Play something".into(), "Tap Devices, bottom-left".into()],
                        advertised: Some(advertised(ProtocolKind::Spotify)),
                        qr_payload: None,
                    }),
                },
                Tile {
                    id: "youtube".into(),
                    label: "YouTube".into(),
                    glyph: TileGlyph::YouTube,
                    accent: [0xff, 0x00, 0x00, 0xff],
                    detail: Some(ServiceDetail {
                        headline: "The cast button in the YouTube app.".into(),
                        steps: vec!["Tap it, and pick this screen".into()],
                        advertised: Some(advertised(ProtocolKind::YouTubeLounge)),
                        qr_payload: None,
                    }),
                },
                Tile {
                    id: "gamestream".into(),
                    label: "Moonlight".into(),
                    glyph: TileGlyph::Moonlight,
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
    // No fonts means the renderer fails on the very same call and nothing is drawn, so
    // nothing should answer to a touch either. Returning an empty layout is what keeps
    // the two halves of D33 agreeing even in the failure case.
    let Ok(f) = text::fonts() else {
        return Vec::new();
    };
    layout(scene, width, height, &f).tiles
}

/// A line of text, placed: where its pen starts, its baseline, and its size.
#[derive(Debug, Clone, Copy)]
struct Line {
    x: f32,
    baseline: f32,
    px: f32,
}

/// Everything on the Home screen, placed, in one pass.
///
/// It exists because the screen used to be laid out twice — the renderer positioned the
/// text from one set of constants and [`tile_layout`] positioned the tiles from another —
/// and the two only agreed for one particular title. Every block now comes out of here,
/// so they share an edge and a rhythm by construction rather than by coincidence.
struct Layout {
    s: f32,
    card: Option<InsetRect>,
    title: Line,
    tiles: Vec<(String, crate::shape::Rect)>,
    footer: Line,
}

/// The gap between the left column and the widget card, in design pixels.
const GUTTER: f32 = 56.0;

fn layout(scene: &AttractScene, width: u32, height: u32, f: &text::Fonts) -> Layout {
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);
    let s = height.max(1) as f32 / DESIGN_HEIGHT;
    let margin = 90.0 * s;
    let card = scene.widget.rect(width, height);

    // One left edge, shared by the title and the tiles.
    //
    // The title used to be *centred in the column* while everything below it was
    // left-aligned at the margin. They lined up only because the demo title happened to
    // be nearly as wide as the column allowed: any other receiver name and the header
    // slid off the grid the rest of the screen is built on.
    let left = margin;
    let right = card.map_or(w - margin, |c| c.x as f32 - GUTTER * s);
    let avail = (right - left).max(1.0);

    let title_px = fit_px(&f.bold, &scene.title, 76.0 * s, avail);
    let title = Line {
        x: left,
        baseline: 118.0f32.mul_add(s, text::ascent(&f.bold, title_px)),
        px: title_px,
    };

    let foot_px = 24.0 * s;
    let footer = Line {
        // Right-aligned to the panel margin, which is also the card's right edge: the
        // card is centred and the whole bottom-right quarter of the screen was empty,
        // while the bottom-left already carried the tiles. This is the right column's
        // second element, and it shares that column's edge.
        x: (w - margin - text::measure(&f.regular, &scene.footer, foot_px)).max(left),
        baseline: 50.0f32.mul_add(-s, h),
        px: foot_px,
    };

    // The box the tiles have to fit inside: from under the title down to clear air above
    // the footer.
    let grid_top = 58.0f32.mul_add(s, title.baseline);
    let grid_bottom = footer.baseline - text::ascent(&f.regular, foot_px) - 36.0 * s;

    Layout {
        s,
        card,
        title,
        tiles: solve_grid(
            &scene.tiles,
            left,
            grid_top,
            avail,
            (grid_bottom - grid_top).max(1.0),
            s,
        ),
        footer,
    }
}

/// Fit every tile inside a fixed box.
///
/// The grid used to be a constant tile size at a constant top, growing downward as tiles
/// were added: the seventh one started below the footer text and ran off the bottom of
/// the panel — half drawn, and the half below the edge unreachable. Here the *box* is
/// fixed and the tiles are solved to fit it, so no number of them can overflow and
/// nothing ever needs to scroll.
///
/// Columns are chosen to make the tiles as large as the box allows. Ties go to the
/// arrangement with the fewest empty cells, so four tiles are a 2×2 block rather than a
/// row of three with an orphan underneath.
fn solve_grid(
    tiles: &[Tile],
    left: f32,
    top: f32,
    avail_w: f32,
    avail_h: f32,
    s: f32,
) -> Vec<(String, crate::shape::Rect)> {
    if tiles.is_empty() {
        return Vec::new();
    }
    let n = tiles.len();
    let gap = 26.0 * s;
    // Capped so a lone tile is a tile and not a billboard.
    let max_side = 220.0 * s;

    let side_for = |cols: usize| {
        let rows = n.div_ceil(cols);
        ((avail_w - gap * (cols - 1) as f32) / cols as f32)
            .min((avail_h - gap * (rows - 1) as f32) / rows as f32)
            .min(max_side)
    };
    let empty_for = |cols: usize| cols * n.div_ceil(cols) - n;

    let mut cols = 1;
    for candidate in 2..=n {
        let (a, b) = (side_for(candidate), side_for(cols));
        // A hair of tolerance so two arrangements separated by a rounding error are
        // decided by which wraps more evenly, not by float noise.
        if a > b + 0.5 || (a > b - 0.5 && empty_for(candidate) < empty_for(cols)) {
            cols = candidate;
        }
    }
    let side = side_for(cols).max(1.0);

    // Centred in the box vertically. With the tagline and the heading gone the box is
    // taller than the grid usually needs, and a block pinned to its top left a hole
    // between the tiles and the footer while the right-hand column sat centred.
    let rows = n.div_ceil(cols) as f32;
    let top = (avail_h - side.mul_add(rows, gap * (rows - 1.0))).max(0.0) / 2.0 + top;

    tiles
        .iter()
        .enumerate()
        .map(|(i, tile)| {
            let (row, col) = (i / cols, i % cols);
            (
                tile.id.clone(),
                crate::shape::Rect {
                    x: (side + gap).mul_add(col as f32, left),
                    y: (side + gap).mul_add(row as f32, top),
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
fn fit_px(font: &text::Face, text: &str, px: f32, avail: f32) -> f32 {
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

    let l = layout(scene, width, height, &f);
    let s = l.s;

    // The widget card, framed *around* the reserved rect: the browser layer covers that
    // rect exactly, so a frame drawn inside it would vanish on the first paint. The inner
    // fill is what an un-painted card shows — an empty panel rather than a hole.
    if let Some(card) = l.card {
        let (cx, cy) = (card.x as f32, card.y as f32);
        // Her lower torso goes down first, so the card frame drawn next covers it where
        // they overlap: she is *behind* the panel she leans on, edge and all. Her
        // foreground half is not here at all — it is the MascotOverlay layer, composited
        // above the live page (see `render_mascot_overlay`).
        if scene.mascot {
            draw_mascot_inner(&mut buf, width, height, s, l.card);
        }
        let (cw, ch) = (card.width as f32, card.height as f32);
        // The largest object on the screen had the thinnest, flattest edge on it, so the
        // tiles outranked the thing that shows a live dashboard. A heavier frame with a
        // lit inner lip reads as a recessed window rather than a grey band.
        let edge = (4.0 * s).max(1.0);
        let lip = (1.5 * s).max(1.0);
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
        text::fill_rect(
            &mut buf,
            width,
            height,
            cx - lip,
            cy - lip,
            lip.mul_add(2.0, cw),
            lip.mul_add(2.0, ch),
            theme::tinted(pal.card_edge, theme::TEXT, 0.22),
        );
        text::fill_rect(&mut buf, width, height, cx, cy, cw, ch, pal.card_bg);
    }

    for (line, font, colour, body) in [
        (l.title, &f.bold, pal.title, scene.title.as_str()),
        (l.footer, &f.regular, pal.footer, scene.footer.as_str()),
    ] {
        text::draw_text(
            &mut buf,
            width,
            height,
            line.x,
            line.baseline,
            body,
            line.px,
            colour,
            font,
        );
    }

    // With no card there is no panel to lean on and no page to layer against: she
    // stands whole in the corner, drawn straight into the scene.
    if scene.mascot && l.card.is_none() {
        draw_mascot(&mut buf, width, height, s, None);
    }

    // Tiles, from the same layout the hit test reads, so the two cannot disagree about
    // where a tile is (D33).
    for (tile, (_, rect)) in scene.tiles.iter().zip(l.tiles) {
        draw_tile(&mut buf, width, height, tile, rect, &f, &pal);
    }

    Ok(buf)
}

// Two layers, as the site stacks them: `inner` is her lower torso alone and `outer` is
// head, arms and sash. The split is load-bearing (and why the art ships as two files):
// the torso goes *behind* the widget card — frame and all — while the arms leaning on
// its top edge land *in front of* the live page inside it, which only a compositor
// layer above the page can do.
const MASCOT_OUTER: &[u8] = include_bytes!("../assets/brand/mascot-outer.png");
const MASCOT_INNER: &[u8] = include_bytes!("../assets/brand/mascot-inner.png");

/// She hangs over the panel's top edge with her elbows dangling across it; this is how
/// much of her height crosses the line.
const OVERHANG: f32 = 0.10;

/// Where the mascot sits: pixel origin and target size, shared by the scene's inner
/// half and the overlay's outer half so the two cannot land misaligned.
fn mascot_placement(
    width: u32,
    height: u32,
    s: f32,
    card: Option<InsetRect>,
    (iw, ih): (u32, u32),
) -> Option<(f32, f32, u32, u32)> {
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
        return None;
    }
    let target_w = (target_h as f32 * iw as f32 / ih as f32) as u32;
    let (ox, oy) = match card {
        Some(card) => (
            // Anchored to the card's right edge rather than to a fraction of its width,
            // so a different card aspect moves her with the edge she is leaning on
            // instead of sliding her sideways for no reason.
            (card.x + card.width) as f32 - target_w as f32 - 44.0 * s,
            card.y as f32 - target_h as f32 * (1.0 - OVERHANG),
        ),
        // No panel to lean on: stand her in the corner instead.
        None => (
            width as f32 - target_w as f32 - 70.0 * s,
            height as f32 - target_h as f32 - 60.0 * s,
        ),
    };
    Some((ox, oy, target_w, target_h))
}

/// Draw one mascot layer into `buf` at the placement, scaled nearest-neighbour — like
/// the album-art path: the source is far larger than the target, so the artefacts a
/// filter would fix are not visible.
fn blit_mascot_layer(
    buf: &mut [u8],
    width: u32,
    height: u32,
    layer: &image::RgbaImage,
    (ox, oy, target_w, target_h): (f32, f32, u32, u32),
) {
    let (iw, ih) = (layer.width().max(1), layer.height().max(1));
    for py in 0..target_h {
        for px in 0..target_w {
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

/// Both mascot layers straight into the scene — the card-less arrangement, where there
/// is nothing for her to be behind or in front of. Inner first, then outer over it: her
/// arms and hands have to occlude the body, not be painted under it.
fn draw_mascot(buf: &mut [u8], width: u32, height: u32, s: f32, card: Option<InsetRect>) {
    let (Ok(outer), Ok(inner)) = (
        image::load_from_memory(MASCOT_OUTER),
        image::load_from_memory(MASCOT_INNER),
    ) else {
        return;
    };
    let outer = outer.to_rgba8();
    let inner = inner.to_rgba8();
    let Some(placement) = mascot_placement(width, height, s, card, (outer.width(), outer.height()))
    else {
        return;
    };
    blit_mascot_layer(buf, width, height, &inner, placement);
    blit_mascot_layer(buf, width, height, &outer, placement);
}

/// The torso alone, into the scene, to be covered by the card frame drawn after it.
fn draw_mascot_inner(buf: &mut [u8], width: u32, height: u32, s: f32, card: Option<InsetRect>) {
    let (Ok(outer), Ok(inner)) = (
        image::load_from_memory(MASCOT_OUTER),
        image::load_from_memory(MASCOT_INNER),
    ) else {
        return;
    };
    let outer = outer.to_rgba8();
    let inner = inner.to_rgba8();
    // Placement from the *outer* image's dimensions for both halves: the two files are
    // the same canvas, and sizing each from itself would let them drift.
    let Some(placement) = mascot_placement(width, height, s, card, (outer.width(), outer.height()))
    else {
        return;
    };
    blit_mascot_layer(buf, width, height, &inner, placement);
}

/// The mascot's foreground half, rasterised alone on transparency for the
/// `MascotOverlay` compositor layer, with the rect it belongs at.
///
/// `None` when the scene has no mascot, no widget card to lean on (the corner
/// arrangement draws her whole into the scene instead), or no room for her.
#[must_use]
pub fn render_mascot_overlay(
    scene: &AttractScene,
    width: u32,
    height: u32,
) -> Option<(Vec<u8>, InsetRect)> {
    if !scene.mascot {
        return None;
    }
    let card = scene.widget.rect(width, height)?;
    let s = height.max(1) as f32 / DESIGN_HEIGHT;
    let outer = image::load_from_memory(MASCOT_OUTER).ok()?.to_rgba8();
    let (ox, oy, target_w, target_h) = mascot_placement(
        width,
        height,
        s,
        Some(card),
        (outer.width(), outer.height()),
    )?;
    if target_w == 0 {
        return None;
    }
    let mut buf = vec![0u8; (target_w * target_h * 4) as usize];
    // Into a buffer exactly her size, at the origin; the layer's transform places it.
    blit_mascot_layer(
        &mut buf,
        target_w,
        target_h,
        &outer,
        (0.0, 0.0, target_w, target_h),
    );
    Some((
        buf,
        InsetRect {
            x: ox.max(0.0) as u32,
            y: oy.max(0.0) as u32,
            width: target_w,
            height: target_h,
        },
    ))
}

/// Draw one tile: a rounded plate, its glyph, and a label under it.
///
/// Every dimension is a fraction of the tile rather than of the panel, because the grid
/// A tile's corner radius, as a fraction of its height.
///
/// Named because it is now used twice: the tile draws itself with it, and a screen opened out
/// of that tile *starts* with it and flattens to the panel's own square corner as it grows —
/// which is what makes the growth read as the tile expanding rather than as a screen scaling up
/// (see `crate::motion`).
pub const TILE_RADIUS: f32 = 0.147;

/// now sizes tiles to fit its box: a corner radius or a label in absolute pixels would
/// look right at six tiles and wrong at ten.
fn draw_tile(
    buf: &mut [u8],
    width: u32,
    height: u32,
    tile: &Tile,
    rect: crate::shape::Rect,
    f: &text::Fonts,
    pal: &Palette,
) {
    use crate::shape;

    let accent = theme::regulated(tile.accent);
    let plate = theme::tinted(pal.tile_bg, accent, 0.12);
    let radius = rect.h * TILE_RADIUS;
    // A plate tinted toward the accent, with the accent itself as the edge. The accent is
    // not the whole fill: six saturated squares on a dark wall is a toy, and the label has
    // to stay readable across a room. But an untinted plate left a three-pixel border
    // doing all the work of telling six services apart from across that room.
    shape::rounded_rect(buf, width, height, rect, radius, plate);
    shape::rounded_outline(
        buf,
        width,
        height,
        rect,
        radius,
        (rect.h * 0.015).max(1.0),
        accent,
    );

    let (cx, cy) = rect.center();
    // Glyph sits above centre; the label takes the bottom third.
    let gy = cy - rect.h * 0.10;
    draw_tile_glyph(
        buf,
        (width, height),
        tile.glyph,
        (cx, gy),
        rect.h * 0.26,
        accent,
    );

    let px = fit_px(&f.regular, &tile.label, rect.h * 0.147, rect.w * 0.92);
    let lw = text::measure(&f.regular, &tile.label, px);
    text::draw_text(
        buf,
        width,
        height,
        cx - lw / 2.0,
        rect.h.mul_add(0.88, rect.y),
        &tile.label,
        px,
        pal.label,
        &f.regular,
    );
}

/// Draw a tile's mark, tinted, into a `2g`-square box centred on `at`.
///
/// The marks are vendored SVGs rasterised here (see `assets/glyphs/README.md`). They used
/// to be hand-rolled distance fields, and every one that looked wrong was a geometry bug
/// in this file rather than in any artwork: the Bluetooth rune's diagonals met on the
/// spine instead of crossing it, the gear's teeth were sized with a radius where the
/// primitive wanted a diameter, and DLNA and Spotify were approximations of marks that
/// exist. Real artwork also means the brand marks are the brands' own.
///
/// Only the alpha survives rasterisation: every mark is monochrome and the tile picks the
/// colour, so the SVG is a stencil rather than a picture. Rasterised per draw, which is
/// per navigation and resize rather than per frame — the same bargain the mascot's PNG
/// decode already makes.
///
/// `pub(crate)` because the service screen draws the same mark large — the screen a tile
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
    let side = (g * 2.0).round().max(1.0) as u32;
    let Some(mask) = glyph_mask(glyph, side) else {
        return;
    };
    let (ox, oy) = (cx - g, cy - g);
    for y in 0..side {
        for x in 0..side {
            let a = mask[(y * side + x) as usize];
            if a == 0 {
                continue;
            }
            text::blend_over(
                buf,
                width,
                height,
                (ox + x as f32) as i32,
                (oy + y as f32) as i32,
                color,
                f32::from(a) / 255.0,
            );
        }
    }
}

/// Rasterise a mark into a `side`×`side` coverage mask.
///
/// `None` if the artwork will not parse or the size is degenerate, which the caller takes
/// as "draw nothing": a tile with no glyph is legible, and a panic here would take down a
/// screen over a decoration.
fn glyph_mask(glyph: TileGlyph, side: u32) -> Option<Vec<u8>> {
    use resvg::tiny_skia;
    let tree = resvg::usvg::Tree::from_str(glyph.svg(), &resvg::usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(side, side)?;
    let size = tree.size();
    // Fit rather than stretch: the marks are square-ish on a 24-unit grid, but nothing
    // guarantees the next one will be, and a stretched logo is worse than a small one.
    let scale = (side as f32 / size.width()).min(side as f32 / size.height());
    let transform = tiny_skia::Transform::from_translate(
        (side as f32 - size.width() * scale) / 2.0,
        (side as f32 - size.height() * scale) / 2.0,
    )
    .pre_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Some(pixmap.data().chunks_exact(4).map(|px| px[3]).collect())
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

    /// Every variant's vendored SVG parses and rasterises to actual ink. The compile
    /// check on `TileGlyph::svg` only proves a file exists — a mark drawn with SVG
    /// features `resvg` doesn't render (the FCast original's `foreignObject` layers,
    /// say) would pass it and draw an empty tile.
    #[test]
    fn every_glyph_rasterises_to_ink() {
        let all = [
            TileGlyph::Cast,
            TileGlyph::AirPlay,
            TileGlyph::Dlna,
            TileGlyph::Spotify,
            TileGlyph::YouTube,
            TileGlyph::Bluetooth,
            TileGlyph::Moonlight,
            TileGlyph::MatterCast,
            TileGlyph::Folder,
            TileGlyph::Miracast,
            TileGlyph::Gear,
            TileGlyph::FCast,
        ];
        // The match makes forgetting a new variant here a compile error.
        let noted = |g: TileGlyph| match g {
            TileGlyph::Cast
            | TileGlyph::AirPlay
            | TileGlyph::Dlna
            | TileGlyph::Spotify
            | TileGlyph::YouTube
            | TileGlyph::Bluetooth
            | TileGlyph::Moonlight
            | TileGlyph::MatterCast
            | TileGlyph::Folder
            | TileGlyph::Miracast
            | TileGlyph::Gear
            | TileGlyph::FCast => (),
        };
        for glyph in all {
            noted(glyph);
            let mask = glyph_mask(glyph, 64).unwrap();
            let ink = mask.iter().filter(|&&a| a > 0).count();
            assert!(ink > 64, "{glyph:?} rasterised to (almost) nothing");
        }
    }

    /// A scene with `n` tiles, for the layout tests.
    fn scene_with(n: usize) -> AttractScene {
        let mut scene = AttractScene::demo();
        scene.tiles = (0..n)
            .map(|i| Tile {
                id: format!("t{i}"),
                label: "Service".into(),
                glyph: TileGlyph::Cast,
                accent: theme::BLUE,
                detail: None,
            })
            .collect();
        scene
    }

    #[test]
    fn every_tile_fits_the_panel_at_any_count() {
        // The regression this exists for: the grid was a constant tile size at a constant
        // top, so it grew downward as tiles were added. At seven the third row started
        // below the footer and ran off the bottom edge — drawn half on the panel, with
        // the rest of it nowhere. Nothing here may scroll, so the grid has to fit.
        for (w, h) in [(1920, 1080), (3840, 2160), (1280, 720)] {
            for n in 1..=16 {
                let scene = scene_with(n);
                let tiles = tile_layout(&scene, w, h);
                assert_eq!(tiles.len(), n, "{n} tiles at {w}x{h}");
                let card = scene.widget.rect(w, h).expect("the demo reserves a card");
                for (id, r) in &tiles {
                    assert!(
                        r.x >= 0.0 && r.y >= 0.0 && r.w > 0.0 && r.h > 0.0,
                        "{id} of {n} at {w}x{h} is {r:?}"
                    );
                    assert!(
                        r.x + r.w <= w as f32 && r.y + r.h <= h as f32,
                        "{id} of {n} at {w}x{h} runs off the panel: {r:?}"
                    );
                    assert!(
                        r.x + r.w <= card.x as f32,
                        "{id} of {n} at {w}x{h} runs under the widget card: {r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn tiles_never_overlap_each_other() {
        // Two tiles sharing pixels would make the hit test's first match arbitrary.
        for n in 1..=16 {
            let tiles = tile_layout(&scene_with(n), 1920, 1080);
            for (i, (_, a)) in tiles.iter().enumerate() {
                for (_, b) in &tiles[i + 1..] {
                    let apart = a.x + a.w <= b.x + 0.01
                        || b.x + b.w <= a.x + 0.01
                        || a.y + a.h <= b.y + 0.01
                        || b.y + b.h <= a.y + 0.01;
                    assert!(apart, "{n} tiles: {a:?} overlaps {b:?}");
                }
            }
        }
    }

    #[test]
    fn a_bigger_grid_never_makes_a_tile_bigger() {
        // The property that makes "no overflow" hold without a clamp: adding tiles can
        // only shrink them. A count that made them grow would mean the solver found room
        // that was not there.
        let side = |n| tile_layout(&scene_with(n), 1920, 1080)[0].1.w;
        for n in 1..16 {
            assert!(
                side(n + 1) <= side(n) + 0.01,
                "{n} → {} tiles grew from {} to {}",
                n + 1,
                side(n),
                side(n + 1)
            );
        }
    }

    #[test]
    fn the_header_and_the_tiles_share_one_left_edge() {
        // What "slapdash" actually was: the title was centred in the column while
        // everything under it was left-aligned, and the two agreed only because the demo
        // title happened to be as wide as the column. Any other receiver name and the
        // header slid off the grid.
        let f = text::fonts().unwrap();
        for title in [
            "dma.space/screen",
            "castaway",
            "a",
            "the hackerspace wall panel",
        ] {
            let mut scene = AttractScene::demo();
            scene.title = title.into();
            let l = layout(&scene, 1920, 1080, &f);
            let left = l.tiles[0].1.x;
            assert!(
                (l.title.x - left).abs() < 0.01,
                "title of {title:?} is off-grid"
            );
        }
    }

    #[test]
    fn the_tiles_stay_centred_between_the_title_and_the_footer() {
        // The vertical half of the alignment bug: the grid used to sit at an absolute
        // height while the text above it moved, so the room over the tiles opened and
        // closed with the length of the receiver's name — tens of pixels, and in the
        // worst case a tagline almost touching the heading under it.
        //
        // The grid is centred in whatever room is left now, so what has to hold is that
        // the *balance* is title-independent: the two clearances differ by the same fixed
        // amount whatever is written above them. (The block itself still shifts a little
        // between titles, because a title too wide for the column is shrunk to fit and a
        // shorter one is not — but it shifts as a centred block, not as a gap opening.)
        let f = text::fonts().unwrap();
        let balance = |title: &str| {
            let mut scene = AttractScene::demo();
            scene.title = title.into();
            let l = layout(&scene, 1920, 1080, &f);
            let top = l.tiles.iter().map(|(_, r)| r.y).fold(f32::MAX, f32::min);
            let bottom = l.tiles.iter().map(|(_, r)| r.y + r.h).fold(0.0, f32::max);
            let above = top - l.title.baseline;
            let below = (l.footer.baseline - text::ascent(&f.regular, l.footer.px)) - bottom;
            assert!(
                above > 0.0 && below > 0.0,
                "{title:?} collides: {above} {below}"
            );
            above - below
        };
        let reference = balance("dma.space/screen");
        for title in [
            "castaway",
            "a",
            "the hackerspace wall panel in the front room",
        ] {
            assert!(
                (balance(title) - reference).abs() < 1.0,
                "{title:?} threw the grid off centre"
            );
        }
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
