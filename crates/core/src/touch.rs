//! Handing the glass to a session that can use it.
//!
//! Most sessions are things to look at. A few are things to *touch*: Miracast negotiates
//! UIBC, so a Windows laptop mirroring to the panel expects the panel's touchscreen to
//! drive it back. That needs a route from the input router — which owns the physical
//! panel and decodes its events — to whichever adapter currently holds the screen, and
//! the adapter is in a `proto-*` crate that must not know what a window is.
//!
//! So it works the way [`crate::control::RemoteControl`] already does for transport
//! buttons: the adapter publishes a handle, the session manager holds it for as long as
//! that source is the active one, and the surface layer asks the manager rather than the
//! adapter. Nothing in a protocol crate learns where touches come from, and nothing in
//! the render layer learns what UIBC is.
//!
//! Coordinates are normalized to the *panel*, `0.0..=1.0`, which is the one space both
//! ends already speak: it is what `input-touch` reports, and it is what a protocol needs
//! in order to undo its own letterboxing. Pixels would force the panel's resolution into
//! a protocol crate and be wrong the moment the panel changed.

use std::fmt;
use std::sync::Arc;

/// Where a contact is in its lifetime.
///
/// Lives here rather than in `input-touch` because both ends of the route need it and the
/// dependencies point this way; `input_touch::TouchPhase` is a re-export of this.
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

impl TouchPhase {
    /// Whether a contact's id is dead after this update.
    #[must_use]
    pub const fn ends_contact(self) -> bool {
        matches!(self, Self::Up | Self::Cancel)
    }
}

/// One contact update, in panel-normalized coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceTouch {
    /// Contact identity, stable for the life of one finger's contact.
    ///
    /// Opaque and only compared for equality — a protocol that needs a narrower id space
    /// (UIBC's pointer id is one byte) maps it, and the mapping is that protocol's
    /// business.
    pub contact: u64,
    /// Phase of this update.
    pub phase: TouchPhase,
    /// X position, 0.0 (left) .. 1.0 (right) of the panel.
    pub x: f32,
    /// Y position, 0.0 (top) .. 1.0 (bottom) of the panel.
    pub y: f32,
}

/// A session that can be driven from the panel's glass.
///
/// Published by an adapter as [`crate::SessionEvent::TouchSurface`] and dropped when the
/// session ends or is preempted, so the router never holds a route into a session that is
/// no longer on screen.
///
/// `&self` rather than `&mut self`: the router calls it from the thread that owns the
/// panel while the adapter's own actor is running elsewhere, which is the same reason
/// [`crate::control::RemoteControl`] takes `&self`.
pub trait TouchSurface: Send + Sync {
    /// A contact update, in panel-normalized coordinates.
    ///
    /// Called on the input thread and expected not to block: an implementation that talks
    /// to a socket queues, it does not wait. Points outside anything meaningful — a
    /// letterbox bar, say — are the implementation's to discard.
    fn touch(&self, touch: SurfaceTouch);

    /// Abandon every contact this surface believes is down.
    ///
    /// What losing the glass mid-gesture looks like: the shell navigates away while
    /// fingers are still on it, and a contact that never ends leaves the far side
    /// believing a finger is down for the rest of the session.
    fn cancel_all(&self);

    /// The panel is this many device pixels, as of now.
    ///
    /// Called when the surface is installed and whenever the panel changes size. A
    /// protocol that has to undo the compositor's letterboxing needs the panel's aspect
    /// to do it, and the router is the only party that knows it — the adapter knows the
    /// stream's resolution and nothing about the glass, which is the whole reason this
    /// direction exists. Defaulted for surfaces that map coordinates without it.
    fn panel_resized(&self, width: u32, height: u32) {
        let _ = (width, height);
    }
}

impl fmt::Debug for dyn TouchSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TouchSurface")
    }
}

/// The touch surface the active session published, if it published one.
///
/// A handle rather than the `Arc` itself so the router can hold one slot for the life of
/// the process and have it emptied under it — sessions come and go, the panel does not.
#[derive(Clone, Default)]
pub struct TouchHandle {
    inner: Arc<std::sync::Mutex<Option<Arc<dyn TouchSurface>>>>,
}

impl TouchHandle {
    /// An empty handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a surface, or clear it with `None`.
    ///
    /// Cancels the outgoing surface's contacts: whoever was holding the glass is not
    /// getting an `Up` for whatever is on it, and a session left believing a finger is
    /// down never recovers.
    pub fn set(&self, surface: Option<Arc<dyn TouchSurface>>) {
        let Ok(mut slot) = self.inner.lock() else {
            return;
        };
        if let Some(previous) = slot.take() {
            previous.cancel_all();
        }
        *slot = surface;
    }

    /// The current surface, if any.
    #[must_use]
    pub fn get(&self) -> Option<Arc<dyn TouchSurface>> {
        self.inner.lock().ok()?.clone()
    }
}

impl fmt::Debug for TouchHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TouchHandle")
            .field("held", &self.get().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Counting {
        touches: AtomicUsize,
        cancels: AtomicUsize,
    }

    impl TouchSurface for Counting {
        fn touch(&self, _touch: SurfaceTouch) {
            self.touches.fetch_add(1, Ordering::Relaxed);
        }
        fn cancel_all(&self) {
            self.cancels.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn replacing_a_surface_cancels_the_one_that_had_the_glass() {
        // The session losing the panel is not going to be sent an `Up` for whatever is
        // still on it, and one that thinks a finger is down never gets it back.
        let first = Arc::new(Counting::default());
        let second = Arc::new(Counting::default());
        let handle = TouchHandle::new();

        handle.set(Some(Arc::clone(&first) as Arc<dyn TouchSurface>));
        handle.set(Some(Arc::clone(&second) as Arc<dyn TouchSurface>));
        assert_eq!(first.cancels.load(Ordering::Relaxed), 1);
        assert_eq!(second.cancels.load(Ordering::Relaxed), 0);

        handle.set(None);
        assert_eq!(second.cancels.load(Ordering::Relaxed), 1);
        assert!(handle.get().is_none());
    }

    #[test]
    fn a_surface_only_hears_while_it_holds_the_slot() {
        let surface = Arc::new(Counting::default());
        let handle = TouchHandle::new();
        let poke = |h: &TouchHandle| {
            if let Some(s) = h.get() {
                s.touch(SurfaceTouch {
                    contact: 1,
                    phase: TouchPhase::Down,
                    x: 0.5,
                    y: 0.5,
                });
            }
        };

        poke(&handle);
        assert_eq!(
            surface.touches.load(Ordering::Relaxed),
            0,
            "nothing held it"
        );

        handle.set(Some(Arc::clone(&surface) as Arc<dyn TouchSurface>));
        poke(&handle);
        assert_eq!(surface.touches.load(Ordering::Relaxed), 1);

        handle.set(None);
        poke(&handle);
        assert_eq!(surface.touches.load(Ordering::Relaxed), 1, "and stops");
    }

    #[test]
    fn the_two_ending_phases_are_the_ones_that_retire_a_contact() {
        assert!(TouchPhase::Up.ends_contact());
        assert!(TouchPhase::Cancel.ends_contact());
        assert!(!TouchPhase::Down.ends_contact());
        assert!(!TouchPhase::Move.ends_contact());
    }
}
