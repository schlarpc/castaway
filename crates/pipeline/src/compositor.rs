//! The compositor surface. A real compositor (wgpu, behind the `wgpu` feature) owns the
//! GPU device/queue/surface and a stack of textured-quad [`Layer`]s drawn back-to-front
//! each present. PiP is "just a layer with a scale+translate transform" — the whole
//! point of one compositor over five apps fighting the framebuffer (architecture §4).
//!
//! This module defines the backend-agnostic trait + a [`NullCompositor`] that logs. The
//! wgpu impl slots in behind the feature without changing this surface.

use tracing::debug;

/// A 2D affine transform (row-major 3x3, but we only expose scale+translate — enough for
/// PiP and overlays; a full `Mat3` lands with the wgpu backend).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Horizontal scale (1.0 = full width).
    pub scale_x: f32,
    /// Vertical scale.
    pub scale_y: f32,
    /// Horizontal offset in normalized surface coords (0.0..=1.0).
    pub offset_x: f32,
    /// Vertical offset.
    pub offset_y: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            scale_x: 1.0,
            scale_y: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

impl Transform {
    /// Whether this placement covers a normalized surface point.
    ///
    /// Used to answer "is that layer actually visible here", which is not the same
    /// question as "does that layer exist" — a letterboxed video leaves the bars
    /// uncovered, and something underneath is genuinely on screen there.
    #[must_use]
    pub fn covers(self, x: f32, y: f32) -> bool {
        x >= self.offset_x
            && y >= self.offset_y
            && x <= self.offset_x + self.scale_x
            && y <= self.offset_y + self.scale_y
    }

    /// A picture-in-picture transform: quarter-size in the given corner (0=TL,1=TR,2=BL,3=BR).
    #[must_use]
    pub fn pip(corner: u8) -> Self {
        let (ox, oy) = match corner {
            1 => (0.66, 0.02),
            2 => (0.02, 0.66),
            3 => (0.66, 0.66),
            _ => (0.02, 0.02),
        };
        Self {
            scale_x: 0.32,
            scale_y: 0.32,
            offset_x: ox,
            offset_y: oy,
        }
    }
}

/// An axis-aligned pixel region of a layer's texture — the granularity of partial
/// uploads (browser paint dirty rects).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    /// Left edge in pixels.
    pub x: u32,
    /// Top edge in pixels.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl DirtyRect {
    /// The full-frame rect for a `width`×`height` surface.
    #[must_use]
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Clamp to a `width`×`height` surface; `None` if nothing remains.
    #[must_use]
    pub fn clamped(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let w = self.width.min(width - x);
        let h = self.height.min(height - y);
        (w > 0 && h > 0).then_some(Self {
            x,
            y,
            width: w,
            height: h,
        })
    }

    /// Area in pixels.
    #[must_use]
    pub fn area(self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// Identifies a compositor layer, **and its depth**.
///
/// Declaration order is paint order, back to front: the compositor draws layers sorted by
/// this enum and nothing else. That is the whole ordering model, and it is deliberate.
///
/// It replaced a `z: i32` that callers passed in per upload. Two layers ended up at the
/// same depth (the idle screen's web widget and the now-playing card, both at `-5`) and
/// the tie fell to `HashMap` iteration order — nondeterministic, and harmless only
/// because those two never appear together in practice. Deriving order from identity
/// makes that unrepresentable: there is no depth to pass, so there is no depth to
/// collide, and the paint order of the whole system is this list (ground rule 1).
///
/// The cost is that a layer wanting two different depths needs two variants — which is
/// why the browser has two. That is honest: they are different surfaces with different
/// sizes and different meanings, and they were already the pair that collided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerId {
    /// The idle/attract background (shown when nothing is casting; video covers it).
    Attract,
    /// The screen being navigated *away* from, fading out over its replacement (D38).
    /// Directly above the shell's own surface and below everything else, so a transition
    /// never covers a cast or the controls for one.
    ShellPrev,
    /// The idle screen's embedded web widget — the clock in the reserved card. Above the
    /// idle background it sits in, below a session's card, because a session that is
    /// actually playing outranks an ornament.
    BrowserWidget,
    /// The mascot's foreground half — head, arms, sash — leaning over the widget card's
    /// top edge. A layer of its own because the split is the point: her torso is drawn
    /// *into* the idle scene behind the card frame, and her arms have to land in front
    /// of the live page the card holds, which no amount of drawing into the scene below
    /// that page can do. Never occludes: it is line art over transparency, and a layer
    /// this test cannot see through would swallow touches meant for the card.
    MascotOverlay,
    /// The now-playing card for an audio-only session, which has no pixels of its own.
    /// Above the attract scene, below video — a sender with pixels outranks a card about
    /// a sender without them.
    NowPlaying,
    /// The touchable transport strip under the now-playing card. Above the card and
    /// below video, for the same reason the card is: a sender with pixels of its own
    /// outranks controls for a sender without them.
    Transport,
    /// The main cast video/mirroring surface.
    Video,
    /// A cast surface filling the panel (YouTube leanback, Cast app receivers). Above
    /// video: it *is* the picture when it is up.
    BrowserFullscreen,
    /// The OSD text/overlay layer.
    Osd,
    /// The shell's navigation affordance — the home pill. Above everything, including a
    /// fullscreen cast surface, because the way out of a screen must never be behind the
    /// thing it is a way out of (D38).
    ShellOverlay,
}

impl LayerId {
    /// Whether this layer covering a point means a touch there stops being the business
    /// of whatever is underneath.
    ///
    /// True for every layer that is a surface of its own. False for [`Self::ShellPrev`],
    /// which is a copy of the shell taken mid-navigation: it is *drawn* above the shell,
    /// but it **is** the shell, and counting it as an occluder made every screen deaf for
    /// the length of its own transition — including the back control on the screen that
    /// had just been opened.
    #[must_use]
    pub const fn occludes(self) -> bool {
        !matches!(self, Self::ShellPrev | Self::MascotOverlay)
    }

    /// Every layer, in paint order. The ordering test asserts against this rather than
    /// against a hand-written list, so a new variant cannot be added without placing it.
    pub const PAINT_ORDER: [Self; 10] = [
        Self::Attract,
        Self::ShellPrev,
        Self::BrowserWidget,
        Self::MascotOverlay,
        Self::NowPlaying,
        Self::Transport,
        Self::Video,
        Self::BrowserFullscreen,
        Self::Osd,
        Self::ShellOverlay,
    ];

    /// Whether this layer is one of the browser's two surfaces.
    #[must_use]
    pub const fn is_browser(self) -> bool {
        matches!(self, Self::BrowserWidget | Self::BrowserFullscreen)
    }

    /// Layers whose mere presence hides this one entirely, wherever they are.
    ///
    /// Depth ([`Self::PAINT_ORDER`]) settles who wins where two layers *overlap*; this
    /// answers the different question of an ornament that must leave the stage when
    /// something outranking it is on at all. Only the idle screen's web widget behaves
    /// that way today: a now-playing card, the transport strip, or video each mean a
    /// session is playing, and a clock that yields only where the card happens to
    /// overlap it is the clock outranking the session everywhere else.
    ///
    /// A total function over the enum, like `PAINT_ORDER`, so a new layer cannot be
    /// added without saying whether the widget yields to it. Enforced by the compositor
    /// at present time — pure presence, no transition to get wrong. (The widget's other
    /// reason to leave, the shell being off its Home screen, is not a layer and is fed
    /// to the compositor by the render loop as an explicit suppression.)
    #[must_use]
    pub const fn yields_to(self) -> &'static [Self] {
        match self {
            // The clock yields the moment a session has anything on the panel: an ornament
            // must not outrank the thing the panel is actually doing.
            Self::BrowserWidget => &[Self::NowPlaying, Self::Transport, Self::Video],
            // The mascot leans on the clock and leaves with it — but *not* by yielding.
            // A hard hide is a pop, and she pops back the instant a departing card's layer
            // goes, which after `motion` is the moment that card has already faded to
            // nothing. Her visibility is driven as an opacity instead
            // (`RenderLoop::apply_floor`), from how much of a session surface is on the
            // panel, so she fades out as one arrives and back as it leaves.
            Self::MascotOverlay => &[],
            Self::Attract
            | Self::ShellPrev
            | Self::NowPlaying
            | Self::Transport
            | Self::Video
            | Self::BrowserFullscreen
            | Self::Osd
            | Self::ShellOverlay => &[],
        }
    }
}

/// A composited layer: a texture placed with a transform and opacity. Depth comes from
/// [`LayerId`] and is not a property of the placement.
#[derive(Debug, Clone)]
pub struct Layer {
    /// Which layer this is — and, by its ordering, how deep.
    pub id: LayerId,
    /// 0.0 (transparent) .. 1.0 (opaque).
    pub opacity: f32,
    /// Placement transform.
    pub transform: Transform,
}

/// The compositor backend. Adapters never call this directly — the session/pipeline
/// does, on the render thread (architecture §6).
pub trait Compositor: Send {
    /// Insert or update a layer.
    fn upsert_layer(&mut self, layer: Layer);
    /// Remove a layer if present.
    fn remove_layer(&mut self, id: LayerId);
    /// Composite all layers and present one frame.
    fn present(&mut self);
    /// Whether an opaque layer drawn *above* `id` covers the normalized surface point —
    /// i.e. whether `id` is hidden there.
    ///
    /// Exists because "is this control visible" is a question the input router has to be
    /// able to ask. A control that is covered must not keep answering to touches: the
    /// transport strip sits below video, so a video session that also published metadata
    /// left an invisible strip swallowing the bottom of the glass.
    ///
    /// Only near-opaque layers count as covering; a translucent one still shows what is
    /// under it, and a partly-placed one (a letterboxed video, a PiP) covers only where
    /// it actually is.
    /// A layer that is not [`LayerId::occludes`] never counts, however opaque it is.
    fn covered_above(&self, id: LayerId, x: f32, y: f32) -> bool;
}

/// Opacity at or above which a layer is treated as hiding what is under it.
pub(crate) const OPAQUE_ENOUGH: f32 = 0.99;

/// A no-op compositor that logs operations — the default when the `wgpu` feature is off.
#[derive(Default)]
pub struct NullCompositor {
    layers: Vec<Layer>,
}

impl Compositor for NullCompositor {
    fn upsert_layer(&mut self, layer: Layer) {
        if let Some(existing) = self.layers.iter_mut().find(|l| l.id == layer.id) {
            *existing = layer;
        } else {
            self.layers.push(layer);
        }
        debug!(layers = self.layers.len(), "null compositor: upsert");
    }

    fn remove_layer(&mut self, id: LayerId) {
        self.layers.retain(|l| l.id != id);
    }

    fn covered_above(&self, id: LayerId, x: f32, y: f32) -> bool {
        self.layers.iter().any(|l| {
            l.id > id && l.id.occludes() && l.opacity >= OPAQUE_ENOUGH && l.transform.covers(x, y)
        })
    }

    fn present(&mut self) {
        // A real backend sorts and draws; here we just account. Same key as the wgpu
        // backend so the null one cannot disagree about order.
        self.layers.sort_by_key(|l| l.id);
    }
}

impl NullCompositor {
    /// Number of active layers (test/inspection helper).
    #[must_use]
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// The layers in the order the last [`Compositor::present`] would have drawn them.
    ///
    /// Exists so the ordering guarantee is testable at all: it is otherwise invisible
    /// until someone looks at a 65-inch screen and sees the wrong thing on top.
    #[must_use]
    pub fn order(&self) -> Vec<LayerId> {
        self.layers.iter().map(|l| l.id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_same_layer_id() {
        let mut c = NullCompositor::default();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            opacity: 1.0,
            transform: Transform::default(),
        });
        c.upsert_layer(Layer {
            id: LayerId::Video,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert_eq!(c.layer_count(), 1);
        c.upsert_layer(Layer {
            id: LayerId::Osd,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert_eq!(c.layer_count(), 2);
        c.remove_layer(LayerId::Video);
        assert_eq!(c.layer_count(), 1);
    }

    #[test]
    fn pip_transform_is_scaled_down() {
        let t = Transform::pip(3);
        assert!(t.scale_x < 0.5 && t.offset_x > 0.5);
    }

    #[test]
    fn paint_order_is_total_and_matches_the_declared_order() {
        // The whole ordering model in one assertion. Before D38 depth was an `i32` each
        // caller passed in, two layers shared `-5`, and the tie fell to `HashMap`
        // iteration order — so what was on top depended on nothing you could read.
        let mut sorted = LayerId::PAINT_ORDER;
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            LayerId::PAINT_ORDER,
            "PAINT_ORDER must be in ascending paint order — declaration order *is* depth"
        );
        // And no two layers can compare equal, which is what makes it a total order.
        for (i, a) in LayerId::PAINT_ORDER.iter().enumerate() {
            for b in &LayerId::PAINT_ORDER[i + 1..] {
                assert_ne!(a, b, "duplicate layer in PAINT_ORDER");
            }
        }
    }

    #[test]
    fn the_navigation_affordance_is_above_every_cast_surface() {
        // D38's rule: the way out of a screen must never be behind the thing it is a way
        // out of. A fullscreen cast surface is the one that would otherwise swallow it.
        assert!(LayerId::ShellOverlay > LayerId::BrowserFullscreen);
        assert!(LayerId::ShellOverlay > LayerId::Video);
        assert!(LayerId::ShellOverlay > LayerId::Osd);
    }

    #[test]
    fn a_playing_session_outranks_the_idle_widget() {
        // The pair that used to collide at z = -5. A now-playing card is a live session;
        // the widget is an ornament on the idle screen.
        assert!(LayerId::NowPlaying > LayerId::BrowserWidget);
        assert!(LayerId::BrowserWidget > LayerId::Attract);
        // ...and video still covers both, per the doc comments on those variants.
        assert!(LayerId::Video > LayerId::Transport);
        assert!(LayerId::Transport > LayerId::NowPlaying);
    }

    #[test]
    fn the_idle_widget_yields_to_every_session_surface_entirely() {
        // Depth alone was not enough: a bluetooth session's card sits *above* the
        // widget but only covers it where they overlap, and a clock floating beside a
        // playing session is the ornament outranking the session everywhere the card
        // is not. `yields_to` is the whole-surface answer, and this pins its contents.
        assert_eq!(
            LayerId::BrowserWidget.yields_to(),
            &[LayerId::NowPlaying, LayerId::Transport, LayerId::Video]
        );
        // Yielding only makes sense to a layer that would also win where they overlap;
        // anything else would draw one way and occlude another.
        for id in LayerId::PAINT_ORDER {
            for &above in id.yields_to() {
                assert!(
                    above > id,
                    "{id:?} yields to {above:?}, which paints below it"
                );
            }
        }
        // The mascot leaves with the clock too, but by fading rather than yielding — see
        // `LayerId::yields_to`. A hard hide pops, and it pops back at the moment a departing
        // card has already faded to nothing, so her visibility is an opacity the render loop
        // drives from how much of a session is on the panel.
        assert!(LayerId::MascotOverlay.yields_to().is_empty());
        // The clock is the only layer that yields today; a new entry here should come with
        // the same kind of reasoning, not by accident.
        for id in LayerId::PAINT_ORDER {
            if id != LayerId::BrowserWidget {
                assert!(id.yields_to().is_empty(), "{id:?} unexpectedly yields");
            }
        }
    }

    #[test]
    fn coverage_is_geometric_not_merely_presence() {
        // Why `covered_above` asks where a layer *is* rather than whether it exists: a
        // partly-placed layer — a PiP, or a letterboxed video — leaves what is under it
        // genuinely on screen, and a control there must keep working.
        let mut c = NullCompositor::default();
        c.upsert_layer(Layer {
            id: LayerId::Transport,
            opacity: 1.0,
            transform: Transform::default(),
        });
        c.upsert_layer(Layer {
            id: LayerId::Video,
            opacity: 1.0,
            // Top half only.
            transform: Transform {
                scale_x: 1.0,
                scale_y: 0.5,
                offset_x: 0.0,
                offset_y: 0.0,
            },
        });
        assert!(
            c.covered_above(LayerId::Transport, 0.5, 0.25),
            "under the video"
        );
        assert!(
            !c.covered_above(LayerId::Transport, 0.5, 0.9),
            "below its bottom edge"
        );
    }

    #[test]
    fn a_screen_being_navigated_away_from_does_not_deafen_the_one_replacing_it() {
        // The regression: `ShellPrev` is opaque and full-screen for the first frames of
        // every navigation, so treating it as an occluder meant a press anywhere on the
        // shell was swallowed until the animation finished — the back control on a screen
        // did not answer for the length of the transition that had just opened it.
        let mut c = NullCompositor::default();
        c.upsert_layer(Layer {
            id: LayerId::ShellPrev,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert!(!c.covered_above(LayerId::Attract, 0.5, 0.5));
        // A real occluder in the same place still counts, so this is not a hole.
        c.upsert_layer(Layer {
            id: LayerId::Video,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert!(c.covered_above(LayerId::Attract, 0.5, 0.5));
    }

    #[test]
    fn a_translucent_layer_does_not_hide_what_is_under_it() {
        let mut c = NullCompositor::default();
        c.upsert_layer(Layer {
            id: LayerId::Video,
            opacity: 0.5,
            transform: Transform::default(),
        });
        assert!(!c.covered_above(LayerId::Transport, 0.5, 0.5));
    }

    #[test]
    fn a_layer_below_never_covers_one_above() {
        let mut c = NullCompositor::default();
        c.upsert_layer(Layer {
            id: LayerId::Attract,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert!(!c.covered_above(LayerId::ShellOverlay, 0.5, 0.5));
        // ...which is the property that keeps the navigation affordance reachable.
        c.upsert_layer(Layer {
            id: LayerId::BrowserFullscreen,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert!(!c.covered_above(LayerId::ShellOverlay, 0.5, 0.5));
    }

    #[test]
    fn null_compositor_presents_in_paint_order_regardless_of_insertion_order() {
        let mut c = NullCompositor::default();
        // Inserted front-to-back, deliberately the wrong way round.
        for id in LayerId::PAINT_ORDER.iter().rev() {
            c.upsert_layer(Layer {
                id: *id,
                opacity: 1.0,
                transform: Transform::default(),
            });
        }
        c.present();
        assert_eq!(c.order(), LayerId::PAINT_ORDER.to_vec());
    }
}
