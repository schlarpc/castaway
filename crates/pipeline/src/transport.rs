//! The touchable transport strip along the bottom of the now-playing card.
//!
//! The C6522QT is a touch panel, and until now the only thing anyone could do with a
//! finger was drive the CEF browser. An audio session — Spotify or a Bluetooth phone —
//! put a card on the wall that you could look at and not touch, while the controls lived
//! on whichever phone had started it. This is the other half of [`RemoteControl`]: the
//! reverse channel existed, and nothing on the panel was wired to it.
//!
//! Three ideas hold this together:
//!
//! 1. **One layout, two consumers.** [`layout`] produces the rectangles; [`render`] draws
//!    into them and [`Layout::hit`] tests against them. A button therefore cannot be
//!    drawn somewhere it cannot be pressed, or pressed somewhere nothing is drawn, which
//!    is the classic way a touch UI goes subtly wrong and stays wrong.
//!
//! 2. **The source decides what exists.** Buttons are drawn from the active session's
//!    [`ControlCapabilities`], so the panel physically cannot offer a control the sender
//!    will refuse. Bluetooth advertises transport and no shuffle, so a phone gets four
//!    buttons; Spotify advertises shuffle and repeat too, so it gets six. Neither case is
//!    special-cased anywhere — it falls out of the capability set.
//!
//! 3. **Absolute intent, not toggles.** A press is turned into a [`ControlTxn`] against
//!    the state *currently on screen* ([`TransportModel::action`]): the play button sends
//!    `Pause` because the panel is showing "playing", not because it is tracking a toggle
//!    of its own. If the panel's view is stale the command is still unambiguous.
//!
//! Its own layer rather than part of the card texture, because the two repaint on
//! completely different schedules: the card changes per track, the scrubber changes every
//! second. At 4K the card is a 33 MB upload and this strip is under 4 MB, and the
//! difference is the whole reason the position tick was never republished before
//! (`proto-spotify::session` says so at the `PositionChanged` arm).
//!
//! [`RemoteControl`]: castaway_core::RemoteControl

use std::time::Duration;

use castaway_core::{ControlCapabilities, ControlTxn, NowPlaying, PlaybackState, RepeatMode};

#[cfg(feature = "render")]
pub use paint::render;

/// How much of the surface's width the strip spans, centred.
///
/// Not the full width: the strip sits under a card whose text column is already inset,
/// and a control bar running edge to edge on a 65-inch panel reads as a system UI rather
/// than as part of what is playing. It also keeps the texture small — see the module
/// docs on why this is its own layer.
pub const STRIP_WIDTH_FRACTION: f32 = 0.62;

/// How much of the surface's height the strip occupies, at the bottom.
pub const STRIP_HEIGHT_FRACTION: f32 = 0.20;

/// Where the strip sits on a `width` × `height` surface, in pixels: `(x, y, w, h)`.
///
/// One definition, used by the compositor placement, by the card's reserved space, and by
/// the touch router's conversion from panel coordinates into strip-local ones. Three
/// consumers of one number is exactly the situation that goes wrong when it is written
/// out three times.
#[must_use]
pub fn placement(width: u32, height: u32) -> (f32, f32, f32, f32) {
    let (w, h) = (width as f32, height as f32);
    let sw = w * STRIP_WIDTH_FRACTION;
    let sh = h * STRIP_HEIGHT_FRACTION;
    ((w - sw) / 2.0, h - sh, sw, sh)
}

/// Map a panel-normalized point (`0.0..=1.0`, as touch and pointer events arrive) into
/// strip-local pixels.
///
/// A free function rather than a method on the render loop so it can be tested without a
/// GPU. It is the seam between "where a finger is on a 65-inch panel" and "which button",
/// and it is the kind of arithmetic that is wrong by an offset for months because
/// everything still *looks* right — the buttons are drawn correctly, they just answer to
/// a different part of the glass.
#[must_use]
pub fn to_strip_local(x: f32, y: f32, width: u32, height: u32) -> (f32, f32) {
    let (ox, oy, _, _) = placement(width, height);
    (x * width as f32 - ox, y * height as f32 - oy)
}

/// A control the strip can offer.
///
/// Deliberately *not* one-to-one with [`ControlTxn`]: one button means different
/// transactions depending on what is on screen, and that mapping is
/// [`TransportModel::action`]'s job rather than something baked into the button's
/// identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportControl {
    /// Toggle shuffle.
    Shuffle,
    /// Previous track.
    Previous,
    /// Play or pause, depending on what is playing now.
    PlayPause,
    /// Next track.
    Next,
    /// Cycle the repeat mode.
    Repeat,
}

/// What a touch landed on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransportHit {
    /// A button.
    Press(TransportControl),
    /// The scrub track, at this fraction of the item's duration.
    Scrub(f32),
}

/// A rectangle in strip-local pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Whether `(px, py)` is inside.
    #[must_use]
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    /// The middle of the rectangle.
    #[must_use]
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

/// Everything the strip draws and decides from.
///
/// Built from the card's [`NowPlaying`] plus the active session's capabilities, because
/// neither alone is enough: the metadata says what to show and the capabilities say what
/// may be offered.
#[derive(Debug, Clone, PartialEq)]
pub struct TransportModel {
    /// What the sender is doing.
    pub state: PlaybackState,
    /// Shuffle state, when the source reports one.
    pub shuffle: Option<bool>,
    /// Repeat mode, when the source reports one.
    pub repeat: Option<RepeatMode>,
    /// Position within the item, when known.
    pub position: Option<Duration>,
    /// Total length, when known. Absent for a live stream, and the scrub track is
    /// suppressed rather than drawn against a guess.
    pub duration: Option<Duration>,
    /// What the active session will actually honour.
    pub capabilities: ControlCapabilities,
}

impl Default for TransportModel {
    fn default() -> Self {
        Self {
            state: PlaybackState::Stopped,
            shuffle: None,
            repeat: None,
            position: None,
            duration: None,
            capabilities: ControlCapabilities::NONE,
        }
    }
}

impl TransportModel {
    /// Build from a metadata snapshot and the session's capability set.
    #[must_use]
    pub fn from_now_playing(track: &NowPlaying, capabilities: ControlCapabilities) -> Self {
        Self {
            state: track.state,
            shuffle: track.shuffle,
            repeat: track.repeat,
            position: track.position,
            duration: track.duration,
            capabilities,
        }
    }

    /// Whether there is anything at all to draw.
    ///
    /// A session that advertises nothing gets no strip — an empty bar across the bottom
    /// of a two-metre screen is worse than the card alone, because it looks like controls
    /// that failed to load.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.controls().is_empty() && self.scrub_fraction().is_none()
    }

    /// Which buttons this session may offer, in display order.
    ///
    /// Shuffle and repeat need *both* the capability and a reported state: a source that
    /// can shuffle but never says whether it is shuffling would get a button whose glyph
    /// is a guess, and pressing it would toggle from an unknown starting point.
    #[must_use]
    pub fn controls(&self) -> Vec<TransportControl> {
        let mut out = Vec::with_capacity(5);
        if self.capabilities.contains(ControlCapabilities::SHUFFLE) && self.shuffle.is_some() {
            out.push(TransportControl::Shuffle);
        }
        if self.capabilities.contains(ControlCapabilities::PREVIOUS) {
            out.push(TransportControl::Previous);
        }
        // Either half is enough to draw the button: the glyph shows the action that is
        // available, and a source with only one of the two is a source that can still be
        // usefully pressed once.
        if self
            .capabilities
            .contains(ControlCapabilities::PLAY)
            .then_some(true)
            .or_else(|| {
                self.capabilities
                    .contains(ControlCapabilities::PAUSE)
                    .then_some(true)
            })
            .is_some()
        {
            out.push(TransportControl::PlayPause);
        }
        if self.capabilities.contains(ControlCapabilities::NEXT) {
            out.push(TransportControl::Next);
        }
        if self.capabilities.contains(ControlCapabilities::REPEAT) && self.repeat.is_some() {
            out.push(TransportControl::Repeat);
        }
        out
    }

    /// How far through the item we are, `0.0..=1.0`, or `None` when there is no duration
    /// to be a fraction of — a live radio stream has a position and no end.
    #[must_use]
    pub fn scrub_fraction(&self) -> Option<f32> {
        let duration = self.duration?;
        if duration.is_zero() {
            return None;
        }
        let position = self.position.unwrap_or_default().min(duration);
        #[allow(clippy::cast_possible_truncation)]
        Some((position.as_secs_f64() / duration.as_secs_f64()) as f32)
    }

    /// Whether dragging the scrub track does anything.
    ///
    /// A track is still *drawn* for a source that cannot seek — knowing how far through a
    /// song you are is worth having on its own — but it does not accept touches, so
    /// nothing appears to move and then snap back.
    #[must_use]
    pub fn is_seekable(&self) -> bool {
        self.capabilities.contains(ControlCapabilities::SEEK) && self.duration.is_some()
    }

    /// The transaction a touch means, given what is on screen right now.
    ///
    /// `None` when the press has no honest transaction behind it: the play button on a
    /// source that advertised only `Pause` and is already paused, for instance.
    #[must_use]
    pub fn action(&self, hit: TransportHit) -> Option<ControlTxn> {
        let txn = match hit {
            TransportHit::Press(TransportControl::PlayPause) => {
                if self.state.is_active() {
                    ControlTxn::Pause
                } else {
                    ControlTxn::Play
                }
            }
            TransportHit::Press(TransportControl::Previous) => ControlTxn::Previous,
            TransportHit::Press(TransportControl::Next) => ControlTxn::Next,
            // Absolute, computed from the state being displayed. A blind toggle applied
            // to a stale view turns shuffle on when the user meant off.
            TransportHit::Press(TransportControl::Shuffle) => ControlTxn::Shuffle(!self.shuffle?),
            TransportHit::Press(TransportControl::Repeat) => {
                ControlTxn::Repeat(self.repeat?.cycled())
            }
            TransportHit::Scrub(fraction) => {
                let duration = self.duration?;
                ControlTxn::Seek(duration.mul_f32(fraction.clamp(0.0, 1.0)))
            }
        };
        // The last word belongs to the capability set, not to the layout: this is a public
        // method and the two could drift.
        self.capabilities.supports(&txn).then_some(txn)
    }
}

/// The laid-out strip: where everything is, in strip-local pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// Strip width in pixels.
    pub width: f32,
    /// Strip height in pixels.
    pub height: f32,
    /// Buttons and their touch targets, in display order.
    pub buttons: Vec<(TransportControl, Rect)>,
    /// The scrub track's visual bar, when a duration is known.
    pub track: Option<Rect>,
    /// The scrub track's touch target — taller than the bar, because a 6-pixel-high
    /// target on a wall panel is a control nobody can hit.
    pub track_touch: Option<Rect>,
    /// Glyph size for the buttons.
    glyph: f32,
}

/// When in a touch's life it is being asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    /// A finger went down, or a button was pressed.
    Press,
    /// A finger lifted, or a button was released.
    Release,
}

impl Layout {
    /// What a touch at this phase should act on, if anything.
    ///
    /// Buttons fire on press and the scrub track on release, and the asymmetry is the
    /// point. A button wants to feel immediate — on a wall panel, a control that waits
    /// for the lift feels broken. A seek is the opposite: firing on press would make a
    /// finger placed anywhere near the bar jump the track before it had moved, so the
    /// release position is the one the user actually chose, and sliding before lifting
    /// adjusts it for free.
    #[must_use]
    pub fn hit_for(&self, x: f32, y: f32, phase: TouchPhase) -> Option<TransportHit> {
        match (self.hit(x, y), phase) {
            (Some(TransportHit::Press(c)), TouchPhase::Press) => Some(TransportHit::Press(c)),
            (Some(TransportHit::Scrub(f)), TouchPhase::Release) => Some(TransportHit::Scrub(f)),
            _ => None,
        }
    }

    /// What a touch at strip-local `(x, y)` landed on, ignoring phase.
    #[must_use]
    pub fn hit(&self, x: f32, y: f32) -> Option<TransportHit> {
        for (control, rect) in &self.buttons {
            if rect.contains(x, y) {
                return Some(TransportHit::Press(*control));
            }
        }
        if let Some(touch) = self.track_touch {
            if touch.contains(x, y) {
                let fraction = ((x - touch.x) / touch.w.max(1.0)).clamp(0.0, 1.0);
                return Some(TransportHit::Scrub(fraction));
            }
        }
        None
    }
}

/// Lay the strip out for a `width` × `height` surface.
///
/// Everything is a proportion of the strip height, so the same layout holds from a 720p
/// test surface to the panel's 2160p without a design-size constant to keep in sync.
#[must_use]
pub fn layout(model: &TransportModel, width: u32, height: u32) -> Layout {
    let w = width as f32;
    let h = height as f32;

    let controls = model.controls();
    let glyph = h * 0.26;
    // The touch target, not the glyph. Fitts' law on a wall: fingers are wide, the panel
    // is far away, and the cost of a generous target is whitespace nobody notices.
    let target = (glyph * 1.9).min(w / (controls.len().max(1) as f32));
    let row_y = h * 0.60;

    let total = target * controls.len() as f32;
    let mut x = (w - total) / 2.0;
    let mut buttons = Vec::with_capacity(controls.len());
    for control in controls {
        buttons.push((
            control,
            Rect {
                x,
                y: row_y - target / 2.0,
                w: target,
                h: target,
            },
        ));
        x += target;
    }

    // The scrub row sits above the buttons, inset from the panel edges so the bar reads
    // as part of the card rather than as a line across the whole wall.
    let (track, track_touch) = if model.scrub_fraction().is_some() {
        let inset = w * 0.12;
        let bar_h = (h * 0.035).max(2.0);
        let bar_y = h * 0.20;
        let bar = Rect {
            x: inset,
            y: bar_y,
            w: (w - inset * 2.0).max(1.0),
            h: bar_h,
        };
        let touch_h = (h * 0.28).max(bar_h);
        (
            Some(bar),
            Some(Rect {
                x: bar.x,
                y: bar_y + bar_h / 2.0 - touch_h / 2.0,
                w: bar.w,
                h: touch_h,
            }),
        )
    } else {
        (None, None)
    };

    Layout {
        width: w,
        height: h,
        buttons,
        track,
        track_touch,
        glyph,
    }
}

/// `m:ss`, or `h:mm:ss` past an hour.
///
/// Out here rather than beside the painting because it is the readout's *content*, and
/// content is the part worth asserting on without a rasterizer.
#[must_use]
pub fn format_time(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Painting the strip.
///
/// Gated on `render` because it needs the font/rasterizer machinery, while everything
/// above — the model, the layout and the hit test — is pure and stays available (and
/// tested) in a build with no renderer at all. That split is deliberate: the part that
/// can be wrong in a way nobody notices is the part that decides what a finger did, and
/// it should not need a GPU to test.
#[cfg(feature = "render")]
mod paint {
    use castaway_core::RepeatMode;

    use super::{format_time, layout, Rect, TransportControl, TransportModel};
    use crate::error::PipelineError;
    use crate::text::{self, Rgba};

    struct Palette {
        /// Drawn when the control is available and off/neutral.
        idle: Rgba,
        /// Drawn when a toggle is on — shuffle engaged, repeat engaged.
        active: Rgba,
        /// The play/pause button's disc.
        disc: Rgba,
        /// The glyph inside the disc.
        on_disc: Rgba,
        /// Unplayed part of the scrub track.
        track: Rgba,
        /// Played part.
        track_fill: Rgba,
        /// Time readout.
        time: Rgba,
    }

    impl Default for Palette {
        fn default() -> Self {
            Self {
                idle: [0xe8, 0xec, 0xf4, 0xff],
                // The card's artist colour, so "on" reads as the same accent the rest of the
                // card already uses rather than as a new idea.
                active: [0x4f, 0xd1, 0xc5, 0xff],
                disc: [0xff, 0xff, 0xff, 0xff],
                on_disc: [0x0a, 0x10, 0x1e, 0xff],
                track: [0x2a, 0x35, 0x52, 0xff],
                track_fill: [0x4f, 0xd1, 0xc5, 0xff],
                time: [0x9a, 0xa4, 0xb8, 0xff],
            }
        }
    }

    /// Draw the strip, returning RGBA8.
    ///
    /// `bg_top`/`bg_bottom` are the card's background colours *at this strip's position on
    /// the panel*, so the strip continues the card's gradient instead of sitting on it as a
    /// visible band. The caller owns that arithmetic because only it knows where the strip
    /// was placed.
    ///
    /// # Errors
    /// [`PipelineError`] if the bundled fonts cannot be parsed.
    pub fn render(
        model: &TransportModel,
        width: u32,
        height: u32,
        bg_top: Rgba,
        bg_bottom: Rgba,
    ) -> Result<Vec<u8>, PipelineError> {
        let fonts = text::fonts()?;
        let pal = Palette::default();
        let l = layout(model, width, height);

        let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
        text::fill_gradient(&mut buf, width, height, bg_top, bg_bottom);

        if let Some(track) = l.track {
            let fraction = model.scrub_fraction().unwrap_or(0.0);
            let radius = track.h / 2.0;
            rounded_bar(&mut buf, width, height, track, radius, pal.track);
            let played = Rect {
                w: (track.w * fraction).max(0.0),
                ..track
            };
            if played.w > 0.0 {
                rounded_bar(&mut buf, width, height, played, radius, pal.track_fill);
            }
            // The knob, only when it can be dragged. On a source that cannot seek the bar is
            // a progress indicator, and a knob would invite a gesture that does nothing.
            if model.is_seekable() {
                let (_, cy) = track.center();
                disc(
                    &mut buf,
                    width,
                    height,
                    track.x + track.w * fraction,
                    cy,
                    radius * 2.6,
                    pal.disc,
                );
            }

            // Elapsed and total, at the ends of the bar.
            let px = (height as f32 * 0.11).max(10.0);
            let elapsed = format_time(model.position.unwrap_or_default());
            let total = format_time(model.duration.unwrap_or_default());
            let baseline = track.y - track.h * 1.6;
            text::draw_text(
                &mut buf,
                width,
                height,
                track.x,
                baseline,
                &elapsed,
                px,
                pal.time,
                &fonts.regular,
            );
            let tw = text::measure(&fonts.regular, &total, px);
            text::draw_text(
                &mut buf,
                width,
                height,
                track.x + track.w - tw,
                baseline,
                &total,
                px,
                pal.time,
                &fonts.regular,
            );
        } else if let Some(position) = model.position {
            // No duration — a live stream. Elapsed time still answers "how long has this been
            // on", which is the question a shared screen actually gets asked.
            let px = (height as f32 * 0.11).max(10.0);
            let label = format_time(position);
            let tw = text::measure(&fonts.regular, &label, px);
            text::draw_text(
                &mut buf,
                width,
                height,
                (l.width - tw) / 2.0,
                l.height * 0.26,
                &label,
                px,
                pal.time,
                &fonts.regular,
            );
        }

        for (control, rect) in &l.buttons {
            let (cx, cy) = rect.center();
            match control {
                TransportControl::PlayPause => {
                    let r = l.glyph * 0.78;
                    disc(&mut buf, width, height, cx, cy, r * 2.0, pal.disc);
                    if model.state.is_active() {
                        pause_glyph(&mut buf, width, height, cx, cy, l.glyph * 0.72, pal.on_disc);
                    } else {
                        play_glyph(&mut buf, width, height, cx, cy, l.glyph * 0.78, pal.on_disc);
                    }
                }
                TransportControl::Previous => {
                    skip_glyph(&mut buf, width, height, cx, cy, l.glyph, pal.idle, false);
                }
                TransportControl::Next => {
                    skip_glyph(&mut buf, width, height, cx, cy, l.glyph, pal.idle, true);
                }
                TransportControl::Shuffle => {
                    let on = model.shuffle.unwrap_or(false);
                    let color = if on { pal.active } else { pal.idle };
                    shuffle_glyph(&mut buf, width, height, cx, cy, l.glyph, color);
                    if on {
                        active_dot(
                            &mut buf,
                            width,
                            height,
                            cx,
                            cy + l.glyph * 0.78,
                            l.glyph,
                            pal.active,
                        );
                    }
                }
                TransportControl::Repeat => {
                    let mode = model.repeat.unwrap_or_default();
                    let color = if mode.is_on() { pal.active } else { pal.idle };
                    repeat_glyph(&mut buf, width, height, cx, cy, l.glyph, color);
                    if matches!(mode, RepeatMode::Track) {
                        // Repeat-one is the mode people cannot tell from repeat-all at a
                        // glance, so it gets the numeral every music player puts there.
                        let px = l.glyph * 0.52;
                        let one = "1";
                        let tw = text::measure(&fonts.bold, one, px);
                        text::draw_text(
                            &mut buf,
                            width,
                            height,
                            cx - tw / 2.0,
                            cy + px * 0.36,
                            one,
                            px,
                            color,
                            &fonts.bold,
                        );
                    }
                    if mode.is_on() {
                        active_dot(
                            &mut buf,
                            width,
                            height,
                            cx,
                            cy + l.glyph * 0.78,
                            l.glyph,
                            pal.active,
                        );
                    }
                }
            }
        }

        Ok(buf)
    }

    // ---------------------------------------------------------------------------
    // Glyphs.
    //
    // Drawn from signed-distance functions rather than font characters, for two reasons: the
    // bundled text faces have no transport glyphs (and a fallback font on an appliance is a
    // dependency that may or may not be installed), and a distance field antialiases for
    // free at any size — these are drawn once per repaint at whatever the panel's scale is.
    // ---------------------------------------------------------------------------

    /// Rasterize a shape over its bounding box, `sd` returning signed distance in pixels
    /// (negative inside).
    fn fill_sdf<F: Fn(f32, f32) -> f32>(
        buf: &mut [u8],
        width: u32,
        height: u32,
        bounds: Rect,
        color: Rgba,
        sd: F,
    ) {
        #[allow(clippy::cast_possible_truncation)]
        let (x0, y0) = (bounds.x.floor() as i32, bounds.y.floor() as i32);
        #[allow(clippy::cast_possible_truncation)]
        let (x1, y1) = (
            (bounds.x + bounds.w).ceil() as i32,
            (bounds.y + bounds.h).ceil() as i32,
        );
        for py in y0..y1 {
            for px in x0..x1 {
                let d = sd(px as f32 + 0.5, py as f32 + 0.5);
                // Half-pixel band around the edge: coverage 1 well inside, 0 well outside.
                let coverage = (0.5 - d).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    text::blend_over(buf, width, height, px, py, color, coverage);
                }
            }
        }
    }

    fn sd_circle(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
    }

    /// Signed distance to a rounded box centred at `(cx, cy)` with half-extents `(hx, hy)`.
    fn sd_round_box(px: f32, py: f32, cx: f32, cy: f32, hx: f32, hy: f32, r: f32) -> f32 {
        let dx = (px - cx).abs() - (hx - r);
        let dy = (py - cy).abs() - (hy - r);
        let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
        outside + dx.max(dy).min(0.0) - r
    }

    /// Signed distance to a line segment.
    fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
        let (pax, pay) = (px - ax, py - ay);
        let (bax, bay) = (bx - ax, by - ay);
        let denom = bax.mul_add(bax, bay * bay).max(f32::EPSILON);
        let t = (pax.mul_add(bax, pay * bay) / denom).clamp(0.0, 1.0);
        (pax - bax * t).hypot(pay - bay * t)
    }

    /// Signed distance to a triangle (used for the play/skip arrowheads).
    fn sd_triangle(px: f32, py: f32, p: [(f32, f32); 3]) -> f32 {
        // Distance to the nearest edge, signed by a winding test. Cheaper and clearer than
        // the exact analytic form, and identical once quantized to a pixel.
        let d = sd_segment(px, py, p[0].0, p[0].1, p[1].0, p[1].1)
            .min(sd_segment(px, py, p[1].0, p[1].1, p[2].0, p[2].1))
            .min(sd_segment(px, py, p[2].0, p[2].1, p[0].0, p[0].1));
        let inside = {
            let sign = |a: (f32, f32), b: (f32, f32)| {
                (b.0 - a.0).mul_add(py - a.1, -((b.1 - a.1) * (px - a.0)))
            };
            let (s0, s1, s2) = (sign(p[0], p[1]), sign(p[1], p[2]), sign(p[2], p[0]));
            (s0 >= 0.0 && s1 >= 0.0 && s2 >= 0.0) || (s0 <= 0.0 && s1 <= 0.0 && s2 <= 0.0)
        };
        if inside {
            -d
        } else {
            d
        }
    }

    fn bounds_around(cx: f32, cy: f32, size: f32) -> Rect {
        Rect {
            x: cx - size,
            y: cy - size,
            w: size * 2.0,
            h: size * 2.0,
        }
    }

    fn disc(buf: &mut [u8], width: u32, height: u32, cx: f32, cy: f32, diameter: f32, color: Rgba) {
        let r = diameter / 2.0;
        fill_sdf(
            buf,
            width,
            height,
            bounds_around(cx, cy, r + 2.0),
            color,
            |px, py| sd_circle(px, py, cx, cy, r),
        );
    }

    /// A small dot under a toggle that is on. Colour alone is not enough on a panel seen from
    /// across a room and at an angle, and it is the one cue that survives a bad viewing angle.
    fn active_dot(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        glyph: f32,
        color: Rgba,
    ) {
        disc(buf, width, height, cx, cy, glyph * 0.16, color);
    }

    fn rounded_bar(buf: &mut [u8], width: u32, height: u32, rect: Rect, radius: f32, color: Rgba) {
        let (cx, cy) = rect.center();
        let (hx, hy) = (rect.w / 2.0, rect.h / 2.0);
        let r = radius.min(hx).min(hy);
        fill_sdf(
            buf,
            width,
            height,
            Rect {
                x: rect.x - 1.0,
                y: rect.y - 1.0,
                w: rect.w + 2.0,
                h: rect.h + 2.0,
            },
            color,
            |px, py| sd_round_box(px, py, cx, cy, hx, hy, r),
        );
    }

    fn play_glyph(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        size: f32,
        color: Rgba,
    ) {
        let h = size * 0.5;
        let w = size * 0.44;
        // Nudged right: a triangle centred on its bounding box looks left-of-centre inside a
        // disc, because its visual mass is toward the flat edge.
        let x = cx - w * 0.35 + size * 0.06;
        let pts = [(x, cy - h), (x, cy + h), (x + w * 2.0, cy)];
        fill_sdf(
            buf,
            width,
            height,
            bounds_around(cx, cy, size + 2.0),
            color,
            |px, py| sd_triangle(px, py, pts),
        );
    }

    fn pause_glyph(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        size: f32,
        color: Rgba,
    ) {
        let bar_w = size * 0.17;
        let bar_h = size * 0.5;
        let gap = size * 0.19;
        for sign in [-1.0f32, 1.0] {
            let bx = cx + sign * (gap + bar_w / 2.0);
            fill_sdf(
                buf,
                width,
                height,
                bounds_around(cx, cy, size + 2.0),
                color,
                |px, py| sd_round_box(px, py, bx, cy, bar_w / 2.0, bar_h, bar_w * 0.28),
            );
        }
    }

    /// Skip: a triangle against a bar. `forward` points right (next), otherwise left (prev).
    #[allow(clippy::too_many_arguments)]
    fn skip_glyph(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        size: f32,
        color: Rgba,
        forward: bool,
    ) {
        let dir = if forward { 1.0 } else { -1.0 };
        let h = size * 0.42;
        let w = size * 0.42;
        let bar_w = size * 0.11;
        let tip = cx + dir * w * 0.62;
        let back = cx - dir * w * 0.5;
        let pts = [(back, cy - h), (back, cy + h), (tip, cy)];
        let bar_x = cx + dir * (w * 0.62 + bar_w * 0.9);
        fill_sdf(
            buf,
            width,
            height,
            bounds_around(cx, cy, size + 2.0),
            color,
            |px, py| {
                sd_triangle(px, py, pts).min(sd_round_box(
                    px,
                    py,
                    bar_x,
                    cy,
                    bar_w / 2.0,
                    h,
                    bar_w * 0.3,
                ))
            },
        );
    }

    /// Shuffle: two crossing paths, each running horizontally before and after the
    /// crossing, with arrowheads on the right.
    ///
    /// The horizontal runs are what make this read as *shuffle* rather than as a cross:
    /// two bare diagonals are an X, and an X on a wall panel is "close" or "error" to
    /// everyone who walks past it.
    fn shuffle_glyph(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        size: f32,
        color: Rgba,
    ) {
        let s = size * 0.46;
        let t = size * 0.075;
        let (l, r) = (cx - s, cx + s * 0.62);
        let (top, bot) = (cy - s * 0.5, cy + s * 0.5);
        let stub = s * 0.34;
        let head = size * 0.15;
        fill_sdf(
            buf,
            width,
            height,
            bounds_around(cx, cy, size + 2.0),
            color,
            |px, py| {
                // Each path: a horizontal entry, the diagonal that crosses the other, and
                // a horizontal run into its arrowhead.
                let path = |ay: f32, by: f32| {
                    sd_segment(px, py, l, ay, l + stub, ay)
                        .min(sd_segment(px, py, l + stub, ay, r - stub, by))
                        .min(sd_segment(px, py, r - stub, by, r, by))
                        - t
                };
                let arrow = |y: f32| {
                    sd_triangle(px, py, [(r, y - head), (r, y + head), (r + head * 1.5, y)])
                };
                path(top, bot)
                    .min(path(bot, top))
                    .min(arrow(top))
                    .min(arrow(bot))
            },
        );
    }

    /// Repeat: a rounded loop, open at the top right, with an arrowhead closing it.
    fn repeat_glyph(
        buf: &mut [u8],
        width: u32,
        height: u32,
        cx: f32,
        cy: f32,
        size: f32,
        color: Rgba,
    ) {
        let s = size * 0.44;
        let t = size * 0.075;
        let head = size * 0.15;
        fill_sdf(
            buf,
            width,
            height,
            bounds_around(cx, cy, size + 2.0),
            color,
            |px, py| {
                // The ring, minus a notch at the top right where the arrowhead goes.
                let ring = (sd_round_box(px, py, cx, cy, s, s * 0.82, s * 0.45).abs()) - t;
                let notch_x = cx + s * 0.25;
                let notched = if px > notch_x && py < cy - s * 0.35 {
                    f32::MAX
                } else {
                    ring
                };
                let arrow = sd_triangle(
                    px,
                    py,
                    [
                        (notch_x, cy - s * 0.82 - head),
                        (notch_x, cy - s * 0.82 + head),
                        (notch_x + head * 1.6, cy - s * 0.82),
                    ],
                );
                notched.min(arrow)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn caps_all() -> ControlCapabilities {
        ControlCapabilities::PLAY
            | ControlCapabilities::PAUSE
            | ControlCapabilities::NEXT
            | ControlCapabilities::PREVIOUS
            | ControlCapabilities::SEEK
            | ControlCapabilities::SHUFFLE
            | ControlCapabilities::REPEAT
    }

    fn model() -> TransportModel {
        TransportModel {
            state: PlaybackState::Playing,
            shuffle: Some(false),
            repeat: Some(RepeatMode::Off),
            position: Some(Duration::from_secs(30)),
            duration: Some(Duration::from_secs(120)),
            capabilities: caps_all(),
        }
    }

    /// The load-bearing property: the panel cannot offer a control the sender will
    /// refuse, and it falls out of the capability set rather than out of a per-protocol
    /// branch. Bluetooth advertises transport and no shuffle; Spotify advertises both.
    #[test]
    fn only_capabilities_the_source_advertised_are_drawn() {
        let mut m = model();
        m.capabilities = ControlCapabilities::TRANSPORT;
        let controls = m.controls();
        assert_eq!(
            controls,
            vec![
                TransportControl::Previous,
                TransportControl::PlayPause,
                TransportControl::Next
            ],
            "a transport-only peer gets exactly the transport buttons"
        );
    }

    /// A capability with no reported state is not enough. A shuffle button whose glyph is
    /// a guess would toggle from an unknown starting point.
    #[test]
    fn shuffle_needs_both_the_capability_and_a_reported_state() {
        let mut m = model();
        m.shuffle = None;
        assert!(!m.controls().contains(&TransportControl::Shuffle));
        m.shuffle = Some(false);
        assert!(m.controls().contains(&TransportControl::Shuffle));
    }

    #[test]
    fn the_play_button_sends_the_opposite_of_what_is_happening() {
        let mut m = model();
        m.state = PlaybackState::Playing;
        assert_eq!(
            m.action(TransportHit::Press(TransportControl::PlayPause)),
            Some(ControlTxn::Pause)
        );
        m.state = PlaybackState::Paused;
        assert_eq!(
            m.action(TransportHit::Press(TransportControl::PlayPause)),
            Some(ControlTxn::Play)
        );
    }

    /// Absolute, not a toggle: the transaction states the wanted state so a stale view or
    /// a reordered command cannot land on the opposite of what was pressed.
    #[test]
    fn toggles_send_the_state_they_want_not_a_flip() {
        let mut m = model();
        m.shuffle = Some(false);
        assert_eq!(
            m.action(TransportHit::Press(TransportControl::Shuffle)),
            Some(ControlTxn::Shuffle(true))
        );
        m.shuffle = Some(true);
        assert_eq!(
            m.action(TransportHit::Press(TransportControl::Shuffle)),
            Some(ControlTxn::Shuffle(false))
        );
    }

    #[test]
    fn repeat_cycles_off_context_track_and_back() {
        let mut m = model();
        for (from, to) in [
            (RepeatMode::Off, RepeatMode::Context),
            (RepeatMode::Context, RepeatMode::Track),
            (RepeatMode::Track, RepeatMode::Off),
        ] {
            m.repeat = Some(from);
            assert_eq!(
                m.action(TransportHit::Press(TransportControl::Repeat)),
                Some(ControlTxn::Repeat(to)),
                "{from:?} should cycle to {to:?}"
            );
        }
    }

    /// A live stream has a position and no end. Drawing a bar against a guessed duration
    /// would put a scrubber on the wall that means nothing and moves wrongly.
    #[test]
    fn no_duration_means_no_scrub_track() {
        let mut m = model();
        m.duration = None;
        assert!(m.scrub_fraction().is_none());
        assert!(!m.is_seekable());
        let l = layout(&m, 1280, 200);
        assert!(l.track.is_none());
        assert!(l.track_touch.is_none());
    }

    #[test]
    fn a_source_that_cannot_seek_still_gets_a_progress_bar() {
        let mut m = model();
        m.capabilities = ControlCapabilities::TRANSPORT;
        assert!(
            m.scrub_fraction().is_some(),
            "progress is still worth showing"
        );
        assert!(!m.is_seekable(), "but it does not accept a drag");
        // And a touch on it produces no transaction, rather than one that gets refused.
        let l = layout(&m, 1280, 200);
        let touch = l.track_touch.unwrap();
        let hit = l
            .hit(touch.x + touch.w / 2.0, touch.y + touch.h / 2.0)
            .unwrap();
        assert!(m.action(hit).is_none());
    }

    #[test]
    fn scrubbing_maps_x_across_the_track_to_a_position() {
        let m = model();
        let l = layout(&m, 1000, 200);
        let touch = l.track_touch.unwrap();
        let hit = l
            .hit(touch.x + touch.w / 2.0, touch.y + touch.h / 2.0)
            .unwrap();
        match hit {
            TransportHit::Scrub(f) => assert!((f - 0.5).abs() < 0.01, "{f}"),
            other => panic!("expected a scrub, got {other:?}"),
        }
        // Halfway through a two-minute track is one minute.
        assert_eq!(
            m.action(hit),
            Some(ControlTxn::Seek(Duration::from_secs(60)))
        );
    }

    /// Every button drawn must be pressable at its centre, and every press must land on
    /// the button that was drawn there. This is the invariant the one-layout design
    /// exists to guarantee, so it is asserted rather than assumed.
    #[test]
    fn every_drawn_button_is_hittable_at_its_own_centre() {
        let m = model();
        let l = layout(&m, 1600, 240);
        assert_eq!(l.buttons.len(), 5);
        for (control, rect) in &l.buttons {
            let (cx, cy) = rect.center();
            assert_eq!(
                l.hit(cx, cy),
                Some(TransportHit::Press(*control)),
                "{control:?} is drawn at ({cx}, {cy}) but nothing is pressable there"
            );
        }
    }

    #[test]
    fn buttons_do_not_overlap_each_other() {
        let l = layout(&model(), 1600, 240);
        for (i, (_, a)) in l.buttons.iter().enumerate() {
            for (_, b) in l.buttons.iter().skip(i + 1) {
                let disjoint = a.x + a.w <= b.x || b.x + b.w <= a.x;
                assert!(disjoint, "{a:?} overlaps {b:?}");
            }
        }
    }

    /// Buttons on press, seeks on release. A seek that fired on press would jump the
    /// track the moment a finger landed near the bar, before it had moved anywhere.
    #[test]
    fn buttons_fire_on_press_and_seeks_on_release() {
        let m = model();
        let l = layout(&m, 1600, 240);
        let (bx, by) = l.buttons[0].1.center();
        assert!(l.hit_for(bx, by, TouchPhase::Press).is_some());
        assert!(l.hit_for(bx, by, TouchPhase::Release).is_none());

        let track = l.track_touch.unwrap();
        let (tx, ty) = track.center();
        assert!(l.hit_for(tx, ty, TouchPhase::Press).is_none());
        assert!(matches!(
            l.hit_for(tx, ty, TouchPhase::Release),
            Some(TransportHit::Scrub(_))
        ));
    }

    /// The panel is 3840×2160 and the strip is a rectangle somewhere near the bottom of
    /// it. A press in the middle of the play button, expressed the way a touch event
    /// actually arrives, has to come out as the play button.
    #[test]
    fn a_normalized_panel_touch_lands_on_the_button_under_it() {
        let (w, h) = (3840u32, 2160u32);
        let (ox, oy, sw, sh) = placement(w, h);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let l = layout(&model(), sw.round() as u32, sh.round() as u32);

        for (control, rect) in &l.buttons {
            let (lx, ly) = rect.center();
            // Back out to panel-normalized, the way an event would have arrived.
            let (nx, ny) = ((ox + lx) / w as f32, (oy + ly) / h as f32);
            let (bx, by) = to_strip_local(nx, ny, w, h);
            assert_eq!(
                l.hit(bx, by),
                Some(TransportHit::Press(*control)),
                "{control:?} at panel ({nx}, {ny}) mapped to ({bx}, {by}) and missed"
            );
        }
    }

    /// A touch on the card above the strip is not the strip's. Consuming it would eat
    /// input meant for whatever else is on screen.
    #[test]
    fn a_touch_above_the_strip_misses_it() {
        let (w, h) = (3840u32, 2160u32);
        let l = layout(&model(), 2380, 432);
        let (bx, by) = to_strip_local(0.5, 0.2, w, h);
        assert!(
            by < 0.0,
            "a touch at a fifth of the way down is above the strip"
        );
        assert!(l.hit(bx, by).is_none());
    }

    #[test]
    fn a_touch_on_nothing_is_nothing() {
        let l = layout(&model(), 1600, 240);
        assert!(l.hit(2.0, 2.0).is_none());
    }

    #[test]
    fn a_session_with_no_controls_and_no_duration_draws_nothing() {
        let m = TransportModel::default();
        assert!(m.is_empty());
    }

    #[test]
    fn time_is_formatted_the_way_a_music_player_writes_it() {
        assert_eq!(format_time(Duration::from_secs(0)), "0:00");
        assert_eq!(format_time(Duration::from_secs(65)), "1:05");
        assert_eq!(format_time(Duration::from_secs(600)), "10:00");
        assert_eq!(format_time(Duration::from_secs(3661)), "1:01:01");
    }

    #[cfg(feature = "render")]
    #[test]
    fn the_strip_rasterizes_at_panel_scale() {
        let m = model();
        let buf = super::render(&m, 640, 120, [0, 0, 0, 255], [0, 0, 0, 255]).unwrap();
        assert_eq!(buf.len(), 640 * 120 * 4);
        // Something was actually drawn: the glyphs are light on a black background, so a
        // buffer that is still black is a renderer that silently did nothing.
        assert!(
            buf.chunks_exact(4).any(|p| p[0] > 40),
            "the strip rendered nothing"
        );
    }
}
