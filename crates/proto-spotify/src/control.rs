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
//! inside the Connect state machine and needs no transaction from us (OPEN-QUESTIONS
//! Q38 covers surfacing that queue on screen).
//!
//! [`MediaUri`]: castaway_core::MediaUri

use std::fmt;
use std::sync::Arc;

use castaway_core::{ControlCapabilities, ControlTxn, CoreError, RemoteControl};
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
    /// mute that cannot be lifted leaves a silent panel — the failure mode Q31 already
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
        let result = match &txn {
            ControlTxn::Play => spirc.play(),
            ControlTxn::Pause => spirc.pause(),
            // Give up being the active device rather than merely pausing: "stop" on the
            // panel should hand the account back to whatever the user picks next, not
            // leave castaway holding a paused session nobody can see.
            ControlTxn::Stop => spirc.disconnect(true),
            ControlTxn::Seek(position) => {
                spirc.set_position_ms(u32::try_from(position.as_millis()).unwrap_or(u32::MAX))
            }
            ControlTxn::Volume(level) => spirc.set_volume(volume_to_spotify(*level)),
            ControlTxn::Next => spirc.next(),
            ControlTxn::Previous => spirc.prev(),
            // Unreachable via `issue`, which checks capabilities first — but this is a
            // trait method anyone can call directly, and a silent success here would look
            // like a queue that was accepted and then ignored.
            //
            // The wildcard is forced: `ControlTxn` is `#[non_exhaustive]`, so a downstream
            // crate cannot match it closed. Refusing by default is the safe direction — a
            // verb added later arrives here as "unsupported" rather than as a silent no-op.
            ControlTxn::Mute(_) | ControlTxn::SetQueue { .. } | _ => {
                return Err(CoreError::UnsupportedControl(format!("{txn:?}")));
            }
        };

        result.map_err(|e| {
            warn!(error = %e, ?txn, "spotify control failed");
            CoreError::Adapter(format!("spotify control {txn:?} failed: {e}"))
        })
    }
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
            ControlTxn::Volume(0.5),
            ControlTxn::Next,
            ControlTxn::Previous,
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
