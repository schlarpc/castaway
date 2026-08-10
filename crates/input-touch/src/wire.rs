//! What a remote peer is allowed to say, and how it becomes an [`Input`] (#18).
//!
//! The boundary between a stranger on the LAN and the panel's input router, and therefore
//! the place ground rule 1's "parse, don't validate" earns its keep: nothing downstream
//! takes a number from the wire, only a [`TouchEvent`] whose coordinates are already in
//! range and whose [`ContactId`] already carries the peer it came from. A peer cannot name
//! another peer's contact, because it does not get to name the origin at all — the
//! [`RemoteId`] is supplied by whoever accepted the connection.
//!
//! JSON, spelled out rather than abbreviated. Input is a few hundred bytes a second even
//! during a fast drag, so there is nothing to win by making the wire unreadable, and a
//! protocol you can watch go past in a browser console is one you can debug from a phone.

use serde::Deserialize;

use crate::{ContactId, Input, Key, PointerEvent, RemoteId, TouchEvent, TouchPhase};

/// The most text one message may carry, in bytes.
///
/// A bound on memory, not on typing: the client sends diffs of a phone-sized field, so a
/// real message is tens of bytes, and 4 KiB is a generous paste. Without a cap, a
/// hostile peer's data-channel messages (up to the channel's own limit, typically
/// 256 KiB) multiplied by the input queue's depth would be real memory.
pub const MAX_TEXT_BYTES: usize = 4096;

/// Why a message was rejected.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// Not JSON, or not a shape this understands.
    #[error("malformed remote input message: {0}")]
    Malformed(#[from] serde_json::Error),
    /// A coordinate that is not a number the panel can place — infinity, or NaN.
    ///
    /// Its own variant rather than a clamp, because `f32::clamp` propagates NaN rather
    /// than removing it: a NaN coordinate would sail through the range check and land in
    /// the router as a position no comparison is true about.
    #[error("remote input coordinate is not finite")]
    NotFinite,
    /// More text than one message is allowed to carry ([`MAX_TEXT_BYTES`]).
    ///
    /// Refused rather than truncated: cutting a UTF-8 string at a byte budget mid-glyph
    /// and inserting the front half is worse than inserting nothing, and no client this
    /// repo ships produces such a message.
    #[error("remote text message of {bytes} bytes exceeds {MAX_TEXT_BYTES}")]
    TextTooLong {
        /// How much the peer sent.
        bytes: usize,
    },
}

/// What a peer asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteCommand {
    /// Route this, exactly as if it had come off the glass.
    Input(Input),
    /// A special key, tapped (#260). See [`crate::Key`] for why keys and text are
    /// different messages.
    Key(Key),
    /// Composed text to insert at the panel's focus (#260). Never empty — an empty
    /// insertion means nothing, so it parses as [`Self::Unknown`] rather than queueing
    /// a no-op.
    Text(String),
    /// Go back to the home screen.
    ///
    /// An explicit affordance rather than a gesture because the panel's way home is a
    /// left-edge swipe, and on a phone that is the system back gesture on Android and
    /// swipe-to-go-back on iOS — the browser eats it before the page ever sees it. A
    /// remote that could only pass gestures through would have no way home at all.
    Home,
    /// A keepalive. Carries nothing; the fact of it is the message.
    Ping,
    /// Something this build does not know about.
    ///
    /// Not an error: a newer page served from a cache, or an older binary, should degrade
    /// to "that button does nothing" rather than to a dropped connection.
    Unknown,
}

/// One message off the wire.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Message {
    Touch {
        /// The peer's own contact number. Scoped to this peer — see [`ContactId::remote`].
        id: u32,
        phase: Phase,
        x: f32,
        y: f32,
        /// Accepted and ignored. Pointer Events offer contact geometry and pressure, and
        /// nothing in the panel reads either yet; taking them now means the day something
        /// does — palm rejection, a drawing surface — the client does not also need
        /// changing. See #18.
        #[serde(default)]
        #[allow(dead_code)]
        pressure: Option<f32>,
        #[serde(default)]
        #[allow(dead_code)]
        width: Option<f32>,
        #[serde(default)]
        #[allow(dead_code)]
        height: Option<f32>,
        /// Likewise: `mouse`, `touch` or `pen`. Recorded on the wire because it is free
        /// and unrecoverable later, not because anything routes on it — a peer's clicks
        /// and its fingers both arrive as contacts.
        #[serde(default)]
        #[allow(dead_code)]
        pointer: Option<String>,
    },
    Wheel {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
    },
    Key {
        key: KeyName,
    },
    Text {
        text: String,
    },
    Home,
    Ping,
    #[serde(other)]
    Unknown,
}

/// A key, as a peer spells it.
///
/// A separate enum from [`crate::Key`] so the wire can degrade: a newer page naming a
/// key this build has never heard of parses as [`KeyName::Unknown`] and becomes
/// [`RemoteCommand::Unknown`] — "that button does nothing" — instead of an error a
/// stricter deserializer would make of it.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum KeyName {
    Enter,
    Backspace,
    Delete,
    Tab,
    Up,
    Down,
    Left,
    Right,
    #[serde(other)]
    Unknown,
}

impl KeyName {
    const fn into_key(self) -> Option<Key> {
        match self {
            Self::Enter => Some(Key::Enter),
            Self::Backspace => Some(Key::Backspace),
            Self::Delete => Some(Key::Delete),
            Self::Tab => Some(Key::Tab),
            Self::Up => Some(Key::ArrowUp),
            Self::Down => Some(Key::ArrowDown),
            Self::Left => Some(Key::ArrowLeft),
            Self::Right => Some(Key::ArrowRight),
            Self::Unknown => None,
        }
    }
}

/// A contact phase, as a peer spells it.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Down,
    Move,
    Up,
    Cancel,
}

impl From<Phase> for TouchPhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Down => Self::Down,
            Phase::Move => Self::Move,
            Phase::Up => Self::Up,
            Phase::Cancel => Self::Cancel,
        }
    }
}

/// Every coordinate has to be a real position before anything downstream sees it.
fn finite(values: [f32; 2]) -> Result<(), WireError> {
    if values.iter().all(|v| v.is_finite()) {
        Ok(())
    } else {
        Err(WireError::NotFinite)
    }
}

/// Read one message from `peer`.
///
/// # Errors
/// [`WireError`] if the text is not a message this understands, or carries a coordinate
/// that is not a finite number.
pub fn parse(peer: RemoteId, text: &str) -> Result<RemoteCommand, WireError> {
    match serde_json::from_str::<Message>(text)? {
        Message::Touch {
            id, phase, x, y, ..
        } => {
            finite([x, y])?;
            // `TouchEvent::new` clamps, so a peer that reports a finger past the edge of
            // the video gets it pinned to the edge — the same thing the panel does with a
            // finger dragged off the glass, and deliberately not a cancellation: a drag
            // that wanders out of the frame and back is one gesture.
            Ok(RemoteCommand::Input(Input::Touch(TouchEvent::new(
                ContactId::remote(peer, id),
                phase.into(),
                x,
                y,
            ))))
        }
        Message::Wheel { x, y, dx, dy } => {
            finite([x, y])?;
            finite([dx, dy])?;
            Ok(RemoteCommand::Input(Input::Pointer(PointerEvent::Wheel {
                x: x.clamp(0.0, 1.0),
                y: y.clamp(0.0, 1.0),
                dx,
                dy,
            })))
        }
        Message::Key { key } => Ok(key
            .into_key()
            .map_or(RemoteCommand::Unknown, RemoteCommand::Key)),
        Message::Text { text } => {
            if text.len() > MAX_TEXT_BYTES {
                return Err(WireError::TextTooLong { bytes: text.len() });
            }
            if text.is_empty() {
                // Inserting nothing means nothing; degrading beats queueing a no-op.
                return Ok(RemoteCommand::Unknown);
            }
            Ok(RemoteCommand::Text(text))
        }
        Message::Home => Ok(RemoteCommand::Home),
        Message::Ping => Ok(RemoteCommand::Ping),
        Message::Unknown => Ok(RemoteCommand::Unknown),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::InputOrigin;

    const PEER: RemoteId = RemoteId::new(7);

    fn touch_of(command: RemoteCommand) -> TouchEvent {
        match command {
            RemoteCommand::Input(Input::Touch(t)) => t,
            other => panic!("expected a touch, got {other:?}"),
        }
    }

    #[test]
    fn a_press_becomes_a_contact_belonging_to_its_peer() {
        let c = parse(
            PEER,
            r#"{"type":"touch","id":3,"phase":"down","x":0.25,"y":0.75}"#,
        )
        .unwrap();
        let t = touch_of(c);
        assert_eq!(t.id, ContactId::remote(PEER, 3));
        assert_eq!(t.phase, TouchPhase::Down);
        assert!((t.x - 0.25).abs() < 1e-6);
        assert!((t.y - 0.75).abs() < 1e-6);
    }

    #[test]
    fn a_peer_cannot_name_another_peers_contact() {
        // The origin is not on the wire. Whatever id a peer sends, the contact it produces
        // belongs to the connection it arrived on — so one peer cannot end another's drag
        // by guessing a number.
        let a = touch_of(
            parse(
                RemoteId::new(1),
                r#"{"type":"touch","id":3,"phase":"up","x":0.5,"y":0.5}"#,
            )
            .unwrap(),
        );
        let b = touch_of(
            parse(
                RemoteId::new(2),
                r#"{"type":"touch","id":3,"phase":"up","x":0.5,"y":0.5}"#,
            )
            .unwrap(),
        );
        assert_ne!(a.id, b.id);
        assert!(a.id.is_from(InputOrigin::Remote(RemoteId::new(1))));
    }

    #[test]
    fn a_finger_past_the_edge_is_pinned_not_cancelled() {
        // A drag that leaves the video element and comes back is one gesture. The client
        // clamps too, so both ends agree on where the contact is.
        let t = touch_of(
            parse(
                PEER,
                r#"{"type":"touch","id":0,"phase":"move","x":-3.0,"y":9.0}"#,
            )
            .unwrap(),
        );
        assert!((t.x - 0.0).abs() < 1e-6);
        assert!((t.y - 1.0).abs() < 1e-6);
        assert_eq!(t.phase, TouchPhase::Move, "still a move, not a cancel");
    }

    #[test]
    fn a_coordinate_that_is_not_a_number_is_refused() {
        // Reachable, and the reason the check is not just a range test: JSON carries f64
        // and the wire type is f32, so any literal past ~3.4e38 deserialises to *infinity*
        // rather than failing. `f32::clamp` would then hand infinity straight through —
        // and would propagate a NaN the same way — leaving the router with a position no
        // comparison is true about.
        for body in [
            r#"{"type":"touch","id":0,"phase":"down","x":1e300,"y":0.5}"#,
            r#"{"type":"touch","id":0,"phase":"down","x":-3.5e38,"y":0.5}"#,
            r#"{"type":"touch","id":0,"phase":"down","x":0.5,"y":1e39}"#,
            r#"{"type":"wheel","x":0.5,"y":1e300,"dx":0,"dy":0}"#,
            r#"{"type":"wheel","x":0.5,"y":0.5,"dx":0,"dy":1e300}"#,
        ] {
            assert!(
                matches!(parse(PEER, body), Err(WireError::NotFinite)),
                "{body}"
            );
        }
        // Past f64's range too, which serde refuses before this ever sees it. Different
        // variant, same outcome — what matters is that neither reaches the router.
        assert!(parse(
            PEER,
            r#"{"type":"touch","id":0,"phase":"down","x":1e400,"y":0.5}"#
        )
        .is_err());
    }

    #[test]
    fn a_wheel_keeps_its_deltas_and_clamps_only_its_position() {
        // Scroll distance is the message; clamping a delta would silently shorten a fling.
        let RemoteCommand::Input(Input::Pointer(PointerEvent::Wheel { x, dy, .. })) = parse(
            PEER,
            r#"{"type":"wheel","x":5.0,"y":0.5,"dx":0,"dy":-240.5}"#,
        )
        .unwrap() else {
            panic!("expected a wheel");
        };
        assert!((x - 1.0).abs() < 1e-6);
        assert!((dy + 240.5).abs() < 1e-6);
    }

    #[test]
    fn every_phase_round_trips() {
        for (spelling, expected) in [
            ("down", TouchPhase::Down),
            ("move", TouchPhase::Move),
            ("up", TouchPhase::Up),
            ("cancel", TouchPhase::Cancel),
        ] {
            let body = format!(r#"{{"type":"touch","id":0,"phase":"{spelling}","x":0.5,"y":0.5}}"#);
            assert_eq!(touch_of(parse(PEER, &body).unwrap()).phase, expected);
        }
    }

    #[test]
    fn home_and_ping_carry_nothing() {
        assert_eq!(
            parse(PEER, r#"{"type":"home"}"#).unwrap(),
            RemoteCommand::Home
        );
        assert_eq!(
            parse(PEER, r#"{"type":"ping"}"#).unwrap(),
            RemoteCommand::Ping
        );
    }

    #[test]
    fn every_key_parses_to_the_key_it_names() {
        // The spellings are the wire contract with the remote page's tray buttons and
        // its keydown handler — see `remote/page.rs`.
        for (spelling, expected) in [
            ("enter", Key::Enter),
            ("backspace", Key::Backspace),
            ("delete", Key::Delete),
            ("tab", Key::Tab),
            ("up", Key::ArrowUp),
            ("down", Key::ArrowDown),
            ("left", Key::ArrowLeft),
            ("right", Key::ArrowRight),
        ] {
            let body = format!(r#"{{"type":"key","key":"{spelling}"}}"#);
            assert_eq!(
                parse(PEER, &body).unwrap(),
                RemoteCommand::Key(expected),
                "{spelling}"
            );
        }
    }

    #[test]
    fn a_key_this_build_does_not_know_degrades_rather_than_erroring() {
        // Same contract as an unknown message type: a newer page's new button must cost
        // that button, not the connection.
        assert_eq!(
            parse(PEER, r#"{"type":"key","key":"power"}"#).unwrap(),
            RemoteCommand::Unknown
        );
    }

    #[test]
    fn text_arrives_as_the_string_that_was_typed() {
        assert_eq!(
            parse(PEER, r#"{"type":"text","text":"héllo ✓"}"#).unwrap(),
            RemoteCommand::Text("héllo ✓".into())
        );
    }

    #[test]
    fn empty_text_is_a_no_op_not_a_queued_event() {
        assert_eq!(
            parse(PEER, r#"{"type":"text","text":""}"#).unwrap(),
            RemoteCommand::Unknown
        );
    }

    #[test]
    fn oversized_text_is_refused_whole() {
        // Truncating UTF-8 at a byte budget can split a glyph; refusal is the honest
        // failure, and no client this repo ships can produce the message.
        let body = format!(
            r#"{{"type":"text","text":"{}"}}"#,
            "x".repeat(MAX_TEXT_BYTES + 1)
        );
        assert!(matches!(
            parse(PEER, &body),
            Err(WireError::TextTooLong { bytes }) if bytes == MAX_TEXT_BYTES + 1
        ));
        // At the limit exactly, it goes through.
        let body = format!(
            r#"{{"type":"text","text":"{}"}}"#,
            "x".repeat(MAX_TEXT_BYTES)
        );
        assert!(matches!(parse(PEER, &body), Ok(RemoteCommand::Text(_))));
    }

    #[test]
    fn a_message_from_a_newer_client_is_ignored_not_fatal() {
        // A page served from a cache can outlive the binary that served it. Dropping the
        // connection over a message it does not know would turn a harmless mismatch into
        // an unusable remote.
        assert_eq!(
            parse(PEER, r#"{"type":"keyboard","key":"a"}"#).unwrap(),
            RemoteCommand::Unknown
        );
    }

    #[test]
    fn the_optional_fields_are_accepted_and_ignored() {
        // Room left on the wire for pressure and geometry (#18). A client that sends them
        // today must not be rejected by a panel that does not read them yet.
        let t = touch_of(
            parse(
                PEER,
                r#"{"type":"touch","id":1,"phase":"down","x":0.5,"y":0.5,
                    "pressure":0.7,"width":24.0,"height":26.0,"pointer":"touch"}"#,
            )
            .unwrap(),
        );
        assert_eq!(t.id, ContactId::remote(PEER, 1));
    }

    #[test]
    fn nonsense_is_an_error_rather_than_a_panic() {
        // Whatever a stranger on the LAN types into the data channel.
        for body in [
            "",
            "null",
            "[]",
            "{}",
            r#"{"type":"touch"}"#,
            r#"{"type":"touch","id":"three","phase":"down","x":0,"y":0}"#,
            r#"{"type":"touch","id":0,"phase":"sideways","x":0,"y":0}"#,
            "\u{0}\u{1}\u{2}",
        ] {
            assert!(parse(PEER, body).is_err(), "{body:?} should not parse");
        }
    }
}
