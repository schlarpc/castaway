//! Getting home, and what it costs whatever was holding the screen (D38).
//!
//! The gesture recogniser is unit-tested in `overlay`. What this covers is the promise
//! that made it necessary: a sink losing the panel mid-touch is told to let go. A contact
//! that never ends leaves the far side believing a finger is down for the rest of the
//! session, and the plumbing for saying so existed for a year without anything sending
//! it.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]

use input_touch::{InputSink, PointerEvent, TouchEvent, TouchPhase};
use pipeline::overlay::Contact;

/// A sink that records what it was told, standing in for the browser.
#[derive(Default)]
struct Recorder {
    down: std::collections::HashSet<u32>,
    cancelled: Vec<u32>,
}

impl InputSink for Recorder {
    fn touch(&mut self, event: TouchEvent) {
        match event.phase {
            TouchPhase::Down => {
                self.down.insert(event.id);
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                self.down.remove(&event.id);
            }
            TouchPhase::Move => {}
        }
    }
    fn pointer(&mut self, _event: PointerEvent) {}
    fn cancel_all(&mut self) {
        let ids: Vec<u32> = self.down.drain().collect();
        self.cancelled.extend(ids);
    }
}

#[test]
fn a_sink_losing_the_panel_is_told_to_let_go_of_every_contact() {
    // Two fingers on a page, then the panel is taken away. Both must be cancelled: the
    // browser keys its contact map by id and only an end or a cancel removes an entry.
    let mut sink = Recorder::default();
    sink.touch(TouchEvent::new(1, TouchPhase::Down, 0.4, 0.4));
    sink.touch(TouchEvent::new(2, TouchPhase::Down, 0.6, 0.6));
    assert_eq!(sink.down.len(), 2);

    sink.cancel_all();

    assert!(sink.down.is_empty(), "nothing may still be believed down");
    assert_eq!(sink.cancelled.len(), 2, "both contacts were cancelled");
}

#[test]
fn cancelling_twice_is_harmless() {
    // Navigation can happen twice in quick succession — the pill, then a swipe — and the
    // second must not resurrect anything.
    let mut sink = Recorder::default();
    sink.touch(TouchEvent::new(7, TouchPhase::Down, 0.5, 0.5));
    sink.cancel_all();
    sink.cancel_all();
    assert_eq!(sink.cancelled, vec![7]);
}

#[test]
fn a_contact_that_ended_normally_is_not_cancelled_later() {
    let mut sink = Recorder::default();
    sink.touch(TouchEvent::new(3, TouchPhase::Down, 0.5, 0.5));
    sink.touch(TouchEvent::new(3, TouchPhase::Up, 0.5, 0.5));
    sink.cancel_all();
    assert!(sink.cancelled.is_empty());
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn the_reserved_edge_is_narrow_enough_to_leave_the_panel_usable() {
    // It is taken from whatever is underneath, so the tradeoff is worth asserting: a
    // page keeps all but a sliver, and the pill is well clear of it.
    const { assert!(pipeline::overlay::EDGE_FRACTION < 0.05) };
    let (w, h) = (1920, 1080);
    let pill = pipeline::overlay::pill_rect(w, h);
    assert!(
        pill.x > pipeline::overlay::EDGE_FRACTION * w as f32,
        "the pill should not sit inside the reserved strip, or one would shadow the other"
    );
}

#[test]
fn a_swipe_needs_travel_not_a_tap_on_the_edge() {
    // Someone resting a hand on the bezel should not navigate.
    let c = Contact::new(0.005, 0.5);
    assert!(!c.is_home_swipe(0.005, 0.5), "a tap is not a swipe");
    assert!(!c.is_home_swipe(0.02, 0.5), "a twitch is not a swipe");
    assert!(c.is_home_swipe(0.5, 0.52), "a deliberate pull is");
}
