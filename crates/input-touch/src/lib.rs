//! # input-touch
//!
//! The C6522QT is an interactive panel: touch arrives over USB HID. This crate models
//! touch as an *input* vector (not just display control), so the 65" surface drives the
//! compositor / browser UI (architecture §8). A [`TouchSource`] emits normalized
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

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    /// The primary button.
    Left,
    /// The middle button / wheel click.
    Middle,
    /// The secondary button.
    Right,
}

/// A mouse/pointer update. Coordinates are normalized to the surface like
/// [`TouchEvent`]; wheel deltas are in pixels (positive `dy` scrolls content up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerEvent {
    /// The pointer moved.
    Move {
        /// X position, 0.0 (left) .. 1.0 (right).
        x: f32,
        /// Y position, 0.0 (top) .. 1.0 (bottom).
        y: f32,
    },
    /// A button was pressed or released at a position.
    Button {
        /// X position, 0.0 .. 1.0.
        x: f32,
        /// Y position, 0.0 .. 1.0.
        y: f32,
        /// Which button.
        button: PointerButton,
        /// `true` on press, `false` on release.
        down: bool,
    },
    /// A scroll at a position, deltas in pixels.
    Wheel {
        /// X position, 0.0 .. 1.0.
        x: f32,
        /// Y position, 0.0 .. 1.0.
        y: f32,
        /// Horizontal scroll delta in pixels.
        dx: f32,
        /// Vertical scroll delta in pixels (positive scrolls content up).
        dy: f32,
    },
}

/// A consumer of routed panel/window input: the surface that currently "owns" the
/// screen. The kiosk (or any input router) decodes raw events — winit window events
/// today, HID/evdev on the physical panel — into normalized [`TouchEvent`]s /
/// [`PointerEvent`]s and delivers them to exactly one focused sink. The browser
/// layer is the first implementor; protocol adapters or native UI layers that want
/// direct interaction implement this same trait and take focus.
pub trait InputSink {
    /// A touch contact update.
    fn touch(&mut self, event: TouchEvent);
    /// A mouse/pointer update.
    fn pointer(&mut self, event: PointerEvent);
    /// Abandon every contact this sink currently believes is down.
    ///
    /// Needed because a sink can lose the panel mid-gesture — the shell navigates away
    /// while fingers are still on the glass — and a contact that never ends leaves the
    /// far side believing a finger is down for the rest of the session. The default is a
    /// no-op for sinks that keep no such state.
    fn cancel_all(&mut self) {}
}

/// A source of touch events. The backend spawns a reader and forwards events on the
/// channel; the compositor/browser layer consumes them.
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
