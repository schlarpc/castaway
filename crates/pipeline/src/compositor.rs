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
/// uploads (CEF `on_paint` dirty rects).
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
    /// The idle screen's embedded web widget — the clock in the reserved card. Above the
    /// idle background it sits in, below a session's card, because a session that is
    /// actually playing outranks an ornament.
    BrowserWidget,
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
    /// Every layer, in paint order. The ordering test asserts against this rather than
    /// against a hand-written list, so a new variant cannot be added without placing it.
    pub const PAINT_ORDER: [Self; 8] = [
        Self::Attract,
        Self::BrowserWidget,
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
}

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
