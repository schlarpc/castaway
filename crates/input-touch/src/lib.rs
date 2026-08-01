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

/// One remote peer, for as long as it is connected.
///
/// Allocated by whoever accepts the connection and never reused within a process run, so
/// a peer that drops and reconnects is a *different* origin — which is what keeps a
/// reconnect from inheriting the contacts the previous connection left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemoteId(u64);

impl RemoteId {
    /// Wrap a peer counter. The caller owns the counter; this is a newtype, not a
    /// registry.
    #[must_use]
    pub const fn new(n: u64) -> Self {
        Self(n)
    }

    /// The underlying counter, for logging.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Where a contact came from.
///
/// Contacts from different origins share one router and one set of maps, so the origin
/// has to be part of a contact's *identity* rather than tracked beside it. Two phones
/// both numbering their first finger `0` are two contacts, not one, and a peer that drops
/// mid-drag must cancel its own contacts without disturbing anyone else's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InputOrigin {
    /// The physical panel: winit touch, or an HID/evdev backend.
    Panel,
    /// The primary mouse button on the panel's own machine, standing in for a finger.
    PanelPointer,
    /// One remote peer driving the panel over the network.
    Remote(RemoteId),
}

/// A contact's identity: which device it belongs to, and which of that device's contacts
/// it is.
///
/// Replaces the bare `u32` the router used to key its maps on, where the mouse's
/// stand-in contact reserved `u32::MAX` and a comment claimed real ids could not collide
/// with it. With remotes in the picture that claim stops being true — and it was never
/// something the compiler checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContactId {
    origin: InputOrigin,
    raw: u32,
}

impl ContactId {
    /// A contact from the physical panel, with the backend's own tracking id.
    #[must_use]
    pub const fn panel(raw: u32) -> Self {
        Self {
            origin: InputOrigin::Panel,
            raw,
        }
    }

    /// The single contact the primary mouse button stands in for.
    pub const POINTER: Self = Self {
        origin: InputOrigin::PanelPointer,
        raw: 0,
    };

    /// A contact from a remote peer.
    ///
    /// `raw` is whatever that peer calls the contact; it is only ever compared against
    /// other contacts from the *same* peer, so a hostile or careless one can collide with
    /// nothing but itself.
    #[must_use]
    pub const fn remote(peer: RemoteId, raw: u32) -> Self {
        Self {
            origin: InputOrigin::Remote(peer),
            raw,
        }
    }

    /// Which device this contact belongs to.
    #[must_use]
    pub const fn origin(self) -> InputOrigin {
        self.origin
    }

    /// Whether this contact belongs to `origin`.
    #[must_use]
    pub fn is_from(self, origin: InputOrigin) -> bool {
        self.origin == origin
    }
}

/// A single touch contact update. Coordinates are normalized to the panel surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchEvent {
    /// Contact identity, stable for the life of one finger's contact.
    pub id: ContactId,
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
    pub fn new(id: ContactId, phase: TouchPhase, x: f32, y: f32) -> Self {
        Self {
            id,
            phase,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        }
    }

    /// Whether this contact ends here — the two phases after which its id is dead.
    #[must_use]
    pub fn ends_contact(&self) -> bool {
        matches!(self.phase, TouchPhase::Up | TouchPhase::Cancel)
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

/// One routed input event, in the router's own vocabulary.
///
/// The seam between decoding and routing. Everything upstream — winit window events, an
/// HID backend, a remote peer's wire messages — turns into one of these, and everything
/// downstream takes them without knowing which. It is what lets the routing be tested
/// without a window and driven from a socket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// A contact: a finger, or a button standing in for one.
    Touch(TouchEvent),
    /// A pointer update that carries no per-origin state — a hover, or a wheel.
    ///
    /// Deliberately *not* origin-tagged, because there is only one cursor. A remote peer
    /// emits [`PointerEvent::Wheel`], which is positional and stateless, and its clicks
    /// and drags arrive as [`Input::Touch`] instead; hover from a remote is dropped
    /// rather than fighting the panel's own cursor for the same hover state.
    Pointer(PointerEvent),
}

/// A consumer of routed panel/window input: the surface that currently "owns" the
/// screen. The kiosk (or any input router) decodes raw events — winit window events
/// today, HID/evdev on the physical panel, a remote peer over the network — into
/// normalized [`TouchEvent`]s / [`PointerEvent`]s and delivers them to exactly one
/// focused sink. The browser layer is the first implementor; protocol adapters or native
/// UI layers that want direct interaction implement this same trait and take focus.
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
    /// Abandon every contact belonging to one origin, leaving the rest alone.
    ///
    /// What a dropped remote peer gets. [`Self::cancel_all`] is the wrong tool there: two
    /// phones can be driving at once, and cancelling everything because one of them lost
    /// Wi-Fi yanks the other person's drag out from under them.
    ///
    /// The default is a no-op, for sinks that keep no contact state.
    fn cancel_origin(&mut self, origin: InputOrigin) {
        let _ = origin;
    }
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
        let e = TouchEvent::new(ContactId::panel(1), TouchPhase::Down, 1.5, -0.2);
        assert_eq!(e.x, 1.0);
        assert_eq!(e.y, 0.0);
    }

    #[test]
    fn the_same_raw_id_from_different_origins_is_different_contacts() {
        // The whole reason ContactId exists. Two phones both numbering their first
        // finger 0, and the panel's own contact 0, are three contacts — and the router
        // keys its maps on this, so if they compared equal their drags would merge.
        let a = ContactId::remote(RemoteId::new(1), 0);
        let b = ContactId::remote(RemoteId::new(2), 0);
        let panel = ContactId::panel(0);
        assert_ne!(a, b);
        assert_ne!(a, panel);
        assert_ne!(b, panel);
    }

    #[test]
    fn the_mouse_stand_in_collides_with_nothing() {
        // It used to be `u32::MAX` plus a comment asserting real ids stayed low. Now it
        // is a different origin, which is a fact rather than a hope — including against
        // a panel contact whose raw id really is u32::MAX.
        assert_ne!(ContactId::POINTER, ContactId::panel(u32::MAX));
        assert_ne!(ContactId::POINTER, ContactId::panel(0));
        assert_ne!(
            ContactId::POINTER,
            ContactId::remote(RemoteId::new(0), u32::MAX)
        );
        assert_eq!(ContactId::POINTER.origin(), InputOrigin::PanelPointer);
    }

    #[test]
    fn a_reconnecting_peer_is_a_new_origin() {
        // RemoteId is never reused, so contacts the dropped connection left behind can
        // never be inherited by the reconnect.
        assert_ne!(
            ContactId::remote(RemoteId::new(7), 0),
            ContactId::remote(RemoteId::new(8), 0)
        );
    }

    #[test]
    fn is_from_matches_only_its_own_origin() {
        let peer = RemoteId::new(3);
        let c = ContactId::remote(peer, 5);
        assert!(c.is_from(InputOrigin::Remote(peer)));
        assert!(!c.is_from(InputOrigin::Remote(RemoteId::new(4))));
        assert!(!c.is_from(InputOrigin::Panel));
    }

    #[test]
    fn null_touch_yields_channel_once() {
        let mut t = NullTouch::new();
        assert!(t.events().is_some());
        assert!(t.events().is_none());
    }
}
