//! The reverse channel: the panel driving Spotify.
//!
//! A finger on "pause" on the C6522QT has to reach the playback that the phone started.
//! For Bluetooth that means AVRCP going back to the phone; for Spotify the receiver *is*
//! the player, so the same [`RemoteControl`] handle instead drives librespot's Connect
//! state — which then tells every other device on the account what happened.
//!
//! Note what is deliberately absent. Queue manipulation is not exposed as a
//! [`ControlTxn::SetQueue`]: that carries [`MediaUri`]s, and a `spotify:track:…` URI is
//! not something the pipeline can open. Widening [`MediaUri`] to accept it would let a
//! `SessionEvent::Play` for a Spotify URI typecheck and then fail at the decoder. The
//! queueing that actually matters — the phone's queue reaching this device — happens
//! inside the Connect state machine and needs no transaction from us (#49 covers
//! surfacing that queue on screen).
//!
//! [`MediaUri`]: castaway_core::MediaUri

use std::fmt;
use std::sync::Arc;

use castaway_core::{ControlCapabilities, ControlTxn, CoreError, RemoteControl, RepeatMode};
use librespot_connect::Spirc;
use tracing::warn;

/// A [`RemoteControl`] backed by a live Connect session.
pub struct SpotifyRemote {
    spirc: Arc<Spirc>,
}

impl SpotifyRemote {
    /// Wrap a live `Spirc` handle.
    #[must_use]
    pub const fn new(spirc: Arc<Spirc>) -> Self {
        Self { spirc }
    }

    /// What a Connect session lets us drive.
    ///
    /// `MUTE` is absent because `Spirc` has no mute: the only way to fake it is setting
    /// the volume to zero, which throws away the level we would need to restore, and a
    /// mute that cannot be lifted leaves a silent panel — the failure mode #55 already
    /// caught once by a different route.
    ///
    /// `SET_QUEUE` is absent for the typing reason in the module docs.
    #[must_use]
    pub const fn capabilities() -> ControlCapabilities {
        ControlCapabilities::PLAY
            .or(ControlCapabilities::PAUSE)
            .or(ControlCapabilities::STOP)
            .or(ControlCapabilities::SEEK)
            .or(ControlCapabilities::VOLUME)
            .or(ControlCapabilities::NEXT)
            .or(ControlCapabilities::PREVIOUS)
            .or(ControlCapabilities::SHUFFLE)
            .or(ControlCapabilities::REPEAT)
    }
}

/// Spotify keeps repeat as two independent flags — repeat-the-context and
/// repeat-the-track — where [`RepeatMode`] is one three-valued answer. Setting *both*
/// on every change is what keeps them in step: sending only the flag that turned on
/// would leave the other one set from a previous mode, so asking for repeat-one after
/// repeat-all would get a device that does both and agrees with neither button.
/// One of the two flag writes a repeat change is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatCall {
    /// `repeat_context`.
    Context(bool),
    /// `repeat_track`.
    Track(bool),
}

/// Both flag writes, in the order that never publishes a both-set state.
///
/// The flag being *cleared* goes first. A fixed order cannot do it: context-first is
/// right for Track→Off and Context→Track, and wrong for Track→Context, where
/// `repeat(true)` lands while the track flag is still set — and that intermediate is not
/// internal. librespot's two handlers each touch only their own flag and `notify()` after
/// every command, so (true, true) reaches the Spotify cluster and the phone's own UI
/// between the two sends: [`repeat_flags`] describes it as "a device that does both and
/// agrees with neither button", which is exactly what gets drawn.
const fn repeat_calls(mode: RepeatMode) -> [RepeatCall; 2] {
    let (context, track) = repeat_flags(mode);
    if context {
        [RepeatCall::Track(track), RepeatCall::Context(context)]
    } else {
        [RepeatCall::Context(context), RepeatCall::Track(track)]
    }
}

const fn repeat_flags(mode: RepeatMode) -> (bool, bool) {
    match mode {
        RepeatMode::Context => (true, false),
        RepeatMode::Track => (false, true),
        // Includes the non-exhaustive catch-all: a mode we do not understand turns
        // repeat off rather than leaving whatever was set before.
        _ => (false, false),
    }
}

/// One call on librespot's `Spirc`, chosen before any of it is made.
///
/// The dispatch used to be a `match` that called straight through, so the decisions in it
/// — chiefly that "stop" hangs up rather than pausing — were only observable by holding a
/// live `Spirc`, which no test can build (#199). Choosing the call first makes the choice
/// a value, and the applying half below has nothing left to decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpircCall {
    Play,
    Pause,
    /// Give up being the active device, pausing on the way out.
    ///
    /// This is `Stop`, and it is deliberately not `pause()`: "stop" on the panel should
    /// hand the account back to whatever the user picks next, not leave castaway holding
    /// a paused session nobody in the room can see. The `bool` is librespot's
    /// `pause` argument — leaving it false would hand the account back with the audio
    /// still running here.
    Disconnect {
        pause: bool,
    },
    SetPosition(u32),
    SetVolume(u16),
    Next,
    Previous,
    Shuffle(bool),
}

/// What one transaction becomes on the wire, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallPlan {
    /// One call, which is every verb but repeat.
    One(SpircCall),
    /// Repeat is two flag writes; see [`repeat_calls`] for why both, and why in that order.
    Repeat([RepeatCall; 2]),
}

/// Decide what a transaction does, without doing any of it.
///
/// # Errors
/// [`CoreError::UnsupportedControl`] for a verb Connect has no honest implementation of.
fn plan(txn: &ControlTxn) -> Result<CallPlan, CoreError> {
    let one = |call| Ok(CallPlan::One(call));
    match txn {
        ControlTxn::Play => one(SpircCall::Play),
        ControlTxn::Pause => one(SpircCall::Pause),
        ControlTxn::Stop => one(SpircCall::Disconnect { pause: true }),
        ControlTxn::Seek(position) => one(SpircCall::SetPosition(
            u32::try_from(position.as_millis()).unwrap_or(u32::MAX),
        )),
        // Spotify's 16-bit volume is a slider position like everyone else's — it is what
        // the phone's own control shows — so it takes the position back out rather than
        // the amplitude (#85). librespot owns the taper on its side.
        ControlTxn::Volume(level) => one(SpircCall::SetVolume(volume_to_spotify(level.position()))),
        ControlTxn::Next => one(SpircCall::Next),
        ControlTxn::Previous => one(SpircCall::Previous),
        ControlTxn::Shuffle(on) => one(SpircCall::Shuffle(*on)),
        ControlTxn::Repeat(mode) => Ok(CallPlan::Repeat(repeat_calls(*mode))),
        // Unreachable via `issue`, which checks capabilities first — but this is a trait
        // method anyone can call directly, and a silent success here would look like a
        // queue that was accepted and then ignored.
        //
        // The wildcard is forced: `ControlTxn` is `#[non_exhaustive]`, so a downstream
        // crate cannot match it closed. Refusing by default is the safe direction — a verb
        // added later arrives here as "unsupported" rather than as a silent no-op.
        ControlTxn::Mute(_) | ControlTxn::SetQueue { .. } | _ => {
            Err(CoreError::UnsupportedControl(format!("{txn:?}")))
        }
    }
}

impl fmt::Debug for SpotifyRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Spirc` is an opaque command sender with nothing worth printing.
        f.debug_struct("SpotifyRemote").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl RemoteControl for SpotifyRemote {
    fn capabilities(&self) -> ControlCapabilities {
        Self::capabilities()
    }

    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
        let spirc = &self.spirc;
        let apply = |call| match call {
            SpircCall::Play => spirc.play(),
            SpircCall::Pause => spirc.pause(),
            SpircCall::Disconnect { pause } => spirc.disconnect(pause),
            SpircCall::SetPosition(ms) => spirc.set_position_ms(ms),
            SpircCall::SetVolume(raw) => spirc.set_volume(raw),
            SpircCall::Next => spirc.next(),
            SpircCall::Previous => spirc.prev(),
            SpircCall::Shuffle(on) => spirc.shuffle(on),
        };
        let result = match plan(&txn)? {
            CallPlan::One(call) => apply(call),
            CallPlan::Repeat(calls) => {
                let mut outcome = Ok(());
                for call in calls {
                    if outcome.is_err() {
                        break;
                    }
                    outcome = match call {
                        RepeatCall::Context(on) => spirc.repeat(on),
                        RepeatCall::Track(on) => spirc.repeat_track(on),
                    };
                }
                outcome
            }
        };

        result.map_err(|e| {
            warn!(error = %e, ?txn, "spotify control failed");
            CoreError::Adapter(format!("spotify control {txn:?} failed: {e}"))
        })
    }
}

/// Read Spotify's 16-bit volume back as a slider position.
///
/// The inverse of [`volume_to_spotify`], and it exists because the phone is authoritative:
/// a finger on the phone's slider reaches us as a `VolumeChanged` player event, and the
/// panel has to follow it rather than keep playing at whatever the last local level was
/// (#199). Same reading in both directions — Spotify's number is a slider position, not an
/// amplitude, so [`castaway_core::Volume::from_position`] owns the taper (#85).
pub(crate) fn volume_from_spotify(raw: u16) -> castaway_core::Volume {
    castaway_core::Volume::from_position(f32::from(raw) / f32::from(u16::MAX))
}

/// Map a `0.0..=1.0` level onto Spotify's 16-bit volume scale.
///
/// Clamped rather than wrapped: a caller that computed 1.2 from a slider should get full
/// volume, not silence from a truncating cast.
fn volume_to_spotify(level: f32) -> u16 {
    let clamped = level.clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (f32::from(u16::MAX) * clamped).round() as u16;
    scaled
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_advertised_set_matches_what_spirc_can_actually_do() {
        let caps = SpotifyRemote::capabilities();
        for txn in [
            ControlTxn::Play,
            ControlTxn::Pause,
            ControlTxn::Stop,
            ControlTxn::Seek(std::time::Duration::from_secs(1)),
            ControlTxn::Volume(castaway_core::Volume::from_position(0.5)),
            ControlTxn::Next,
            ControlTxn::Previous,
            ControlTxn::Shuffle(true),
            ControlTxn::Repeat(RepeatMode::Context),
        ] {
            assert!(caps.supports(&txn), "{txn:?} should be advertised");
        }
    }

    #[test]
    fn verbs_with_no_honest_implementation_are_not_advertised() {
        // A button the panel renders and Spotify then ignores is worse than no button.
        let caps = SpotifyRemote::capabilities();
        assert!(!caps.supports(&ControlTxn::Mute(true)));
        assert!(!caps.supports(&ControlTxn::SetQueue {
            items: Vec::new(),
            start_index: 0,
        }));
    }

    /// The two Spotify flags are set on every repeat change, never just the one that
    /// turned on — otherwise switching from repeat-all to repeat-one leaves both set and
    /// the device agrees with neither button on the panel.
    #[test]
    fn every_repeat_mode_states_both_flags() {
        assert_eq!(repeat_flags(RepeatMode::Off), (false, false));
        assert_eq!(repeat_flags(RepeatMode::Context), (true, false));
        assert_eq!(repeat_flags(RepeatMode::Track), (false, true));
    }

    /// Every transition between every pair of modes, with the flags applied one at a
    /// time the way librespot applies them.
    #[test]
    fn no_repeat_transition_passes_through_a_state_with_both_flags_set() {
        let modes = [RepeatMode::Off, RepeatMode::Context, RepeatMode::Track];
        for from in modes {
            for to in modes {
                let (mut context, mut track) = repeat_flags(from);
                for call in repeat_calls(to) {
                    match call {
                        RepeatCall::Context(on) => context = on,
                        RepeatCall::Track(on) => track = on,
                    }
                    assert!(
                        !(context && track),
                        "{from:?} -> {to:?} published both flags after {call:?}"
                    );
                }
                assert_eq!((context, track), repeat_flags(to), "{from:?} -> {to:?}");
            }
        }
    }

    /// What each verb actually does to the Connect session.
    ///
    /// Every line here was previously observable only by holding a live `Spirc`, which
    /// means it was observable only by a person with a phone (#199).
    #[test]
    fn every_advertised_verb_maps_to_the_call_it_says_it_does() {
        use std::time::Duration;

        let one = |txn: &ControlTxn| match plan(txn) {
            Ok(CallPlan::One(call)) => call,
            other => panic!("{txn:?} should be one call, got {other:?}"),
        };
        assert_eq!(one(&ControlTxn::Play), SpircCall::Play);
        assert_eq!(one(&ControlTxn::Pause), SpircCall::Pause);
        assert_eq!(one(&ControlTxn::Next), SpircCall::Next);
        assert_eq!(one(&ControlTxn::Previous), SpircCall::Previous);
        assert_eq!(one(&ControlTxn::Shuffle(true)), SpircCall::Shuffle(true));
        assert_eq!(
            one(&ControlTxn::Seek(Duration::from_millis(4200))),
            SpircCall::SetPosition(4200)
        );
    }

    /// Stop hangs up; it does not pause.
    ///
    /// The surprising one, and deliberate: leaving castaway holding a paused session
    /// means the account stays on a device nobody in the room can see, and the next
    /// person's phone finds it occupied. `pause: true` matters as much as the disconnect
    /// — hanging up while still feeding the mixer would give the room audio from a device
    /// that has just told the cloud it is gone.
    #[test]
    fn stop_gives_the_account_back_rather_than_pausing() {
        assert_eq!(
            plan(&ControlTxn::Stop).unwrap(),
            CallPlan::One(SpircCall::Disconnect { pause: true })
        );
    }

    /// A seek past what Spotify's 32-bit millisecond field can hold saturates.
    ///
    /// Not reachable from the strip, which scrubs within a duration — but `ControlTxn`
    /// carries a `Duration`, and a wrapping cast would turn "seek to the end of a very
    /// long podcast" into "seek to somewhere near the beginning".
    #[test]
    fn a_seek_beyond_the_wire_format_saturates_rather_than_wrapping() {
        let huge = std::time::Duration::from_millis(u64::from(u32::MAX) + 5000);
        assert_eq!(
            plan(&ControlTxn::Seek(huge)).unwrap(),
            CallPlan::One(SpircCall::SetPosition(u32::MAX))
        );
    }

    #[test]
    fn a_verb_with_no_honest_implementation_is_refused_rather_than_swallowed() {
        // Reachable: `issue` checks capabilities, but `issue_unchecked` is a trait method
        // anyone can call. A silent `Ok` here is a queue that was accepted and ignored.
        for txn in [
            ControlTxn::Mute(true),
            ControlTxn::SetQueue {
                items: Vec::new(),
                start_index: 0,
            },
        ] {
            assert!(
                matches!(plan(&txn), Err(CoreError::UnsupportedControl(_))),
                "{txn:?} should be refused"
            );
        }
    }

    #[test]
    fn a_repeat_change_plans_both_flag_writes() {
        assert_eq!(
            plan(&ControlTxn::Repeat(RepeatMode::Track)).unwrap(),
            CallPlan::Repeat([RepeatCall::Context(false), RepeatCall::Track(true)])
        );
    }

    /// The phone's slider and the panel's are the same scale, read the same way.
    ///
    /// Both directions in one test because the failure that matters is asymmetry: a
    /// finger on the phone reaching us through a different curve than the one we send back
    /// makes the two sliders disagree, and each correction moves the other.
    #[test]
    fn a_volume_survives_the_round_trip_through_spotifys_scale() {
        for position in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let sent = volume_to_spotify(position);
            let back = volume_from_spotify(sent);
            assert!(
                (back.position() - position).abs() < 0.001,
                "{position} came back as {}",
                back.position()
            );
        }
        // The ends are exact, because the ends are what a listener notices: a "silent"
        // that is not silent, or a "full" that is a decibel down.
        assert_eq!(volume_from_spotify(0), castaway_core::Volume::SILENT);
        assert_eq!(volume_from_spotify(u16::MAX), castaway_core::Volume::FULL);
    }

    #[test]
    fn volume_covers_the_whole_scale_and_clamps_outside_it() {
        assert_eq!(volume_to_spotify(0.0), 0);
        assert_eq!(volume_to_spotify(1.0), u16::MAX);
        // A slider that overshoots must saturate, not wrap around to silence.
        assert_eq!(volume_to_spotify(1.5), u16::MAX);
        assert_eq!(volume_to_spotify(-0.2), 0);
        assert_eq!(volume_to_spotify(0.5), 32_768);
    }
}
