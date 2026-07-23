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

/// Identifies a compositor layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayerId {
    /// The main cast video/mirroring surface.
    Video,
    /// The CEF browser surface (PiP / YouTube TV surface).
    Browser,
    /// The OSD text/overlay layer.
    Osd,
}

/// A composited layer: a texture placed with a transform, z-order, and opacity.
#[derive(Debug, Clone)]
pub struct Layer {
    /// Which layer this is.
    pub id: LayerId,
    /// Draw order — higher is on top.
    pub z: i32,
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
        // A real backend sorts by z and draws; here we just account.
        self.layers.sort_by_key(|l| l.z);
    }
}

impl NullCompositor {
    /// Number of active layers (test/inspection helper).
    #[must_use]
    pub fn layer_count(&self) -> usize {
        self.layers.len()
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
            z: 0,
            opacity: 1.0,
            transform: Transform::default(),
        });
        c.upsert_layer(Layer {
            id: LayerId::Video,
            z: 1,
            opacity: 1.0,
            transform: Transform::default(),
        });
        assert_eq!(c.layer_count(), 1);
        c.upsert_layer(Layer {
            id: LayerId::Osd,
            z: 10,
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
}
