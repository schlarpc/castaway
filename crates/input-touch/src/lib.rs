//! # input-touch
//!
//! The C6522QT is an interactive panel: touch arrives over USB HID. This crate models
//! touch as an *input* vector (not just display control), so the 65" surface drives the
//! compositor / CEF UI (architecture §8). A [`TouchSource`] emits normalized
//! [`TouchEvent`]s; backends are evdev (Linux, `evdev` feature) and Raw Input / WM_POINTER
//! (Windows, `winuser` feature). The default [`NullTouch`] emits nothing.
//!
//! May use `unsafe` FFI for the HID backends, so no `forbid(unsafe_code)`; any `unsafe`
//! carries a `// SAFETY:` note (rule 8). Coordinates are normalized `0.0..=1.0` so the
//! consumer maps them to the render surface regardless of panel resolution.

use tokio::sync::mpsc;

/// The phase of a touch contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    /// A finger touched down.
    Down,
    /// A tracked finger moved.
    Move,
    /// A finger lifted.
    Up,
    /// The contact was cancelled (e.g. palm rejection).
    Cancel,
}

/// A single touch contact update. Coordinates are normalized to the panel surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchEvent {
    /// Contact/tracking id (stable for the life of one finger's contact).
    pub id: u32,
    /// Phase of this update.
    pub phase: TouchPhase,
    /// X position, 0.0 (left) .. 1.0 (right).
    pub x: f32,
    /// Y position, 0.0 (top) .. 1.0 (bottom).
    pub y: f32,
}

impl TouchEvent {
    /// Build a normalized event, clamping coordinates into range.
    #[must_use]
    pub fn new(id: u32, phase: TouchPhase, x: f32, y: f32) -> Self {
        Self {
            id,
            phase,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        }
    }
}

/// A source of touch events. The backend spawns a reader and forwards events on the
/// channel; the compositor/CEF layer consumes them.
pub trait TouchSource: Send {
    /// Take the receiving end of the touch event stream. Returns `None` if already taken.
    fn events(&mut self) -> Option<mpsc::Receiver<TouchEvent>>;
}

/// A touch source that never emits — the default when no HID backend is compiled in.
#[derive(Default)]
pub struct NullTouch {
    rx: Option<mpsc::Receiver<TouchEvent>>,
}

impl NullTouch {
    /// Create a null touch source (holds an empty, never-fed channel).
    #[must_use]
    pub fn new() -> Self {
        let (_tx, rx) = mpsc::channel(1);
        Self { rx: Some(rx) }
    }
}

impl TouchSource for NullTouch {
    fn events(&mut self) -> Option<mpsc::Receiver<TouchEvent>> {
        self.rx.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinates_are_clamped() {
        let e = TouchEvent::new(1, TouchPhase::Down, 1.5, -0.2);
        assert_eq!(e.x, 1.0);
        assert_eq!(e.y, 0.0);
    }

    #[test]
    fn null_touch_yields_channel_once() {
        let mut t = NullTouch::new();
        assert!(t.events().is_some());
        assert!(t.events().is_none());
    }
}
