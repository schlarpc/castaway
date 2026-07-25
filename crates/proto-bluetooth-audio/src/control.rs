//! The [`RemoteControl`] implementation: a finger on the panel reaching the phone.
//!
//! This is the far end of the interface `core` grew for Bluetooth. The session manager
//! holds an `Arc<dyn RemoteControl>`; calling `issue()` on it puts AVRCP passthrough
//! frames on the AVCTP channel, and the phone pauses.

use castaway_core::{ControlCapabilities, ControlTxn, CoreError, RemoteControl};
use tokio::sync::mpsc;

use crate::avctp::AvcFrame;
use crate::avrcp;

/// A handle that turns [`ControlTxn`]s into AVRCP frames on the AVCTP channel.
///
/// Holds a sender rather than the channel itself so it can be cloned into the session
/// manager and dropped independently of the actor that owns the socket (ground rule 3:
/// the protocol logic never touches I/O).
#[derive(Debug, Clone)]
pub struct AvrcpControl {
    capabilities: ControlCapabilities,
    frames: mpsc::Sender<AvcFrame>,
}

impl AvrcpControl {
    /// Build a control handle over a channel to the AVCTP actor.
    ///
    /// `capabilities` should come from [`avrcp::capabilities_for_passthrough`], narrowed
    /// by whatever the peer's supported-features bitmask actually claimed.
    #[must_use]
    pub const fn new(capabilities: ControlCapabilities, frames: mpsc::Sender<AvcFrame>) -> Self {
        Self {
            capabilities,
            frames,
        }
    }

    /// A handle offering everything passthrough can express.
    #[must_use]
    pub fn passthrough(frames: mpsc::Sender<AvcFrame>) -> Self {
        Self::new(avrcp::capabilities_for_passthrough(), frames)
    }
}

#[async_trait::async_trait]
impl RemoteControl for AvrcpControl {
    fn capabilities(&self) -> ControlCapabilities {
        self.capabilities
    }

    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
        let Some(operation) = avrcp::operation_for(&txn) else {
            // Unreachable through `issue()`, which checks capabilities first — but a
            // direct caller must not silently get a nearest-equivalent keypress.
            return Err(CoreError::UnsupportedControl(format!("{txn:?}")));
        };

        // Press *and* release, always, and both before returning. Sending only the press
        // leaves the phone believing the key is held down; many then auto-repeat, so one
        // tap on "next" walks the entire album.
        for frame in avrcp::passthrough(operation) {
            self.frames
                .send(frame)
                .await
                .map_err(|_| CoreError::Adapter("avrcp control channel closed".into()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use super::*;
    use crate::avctp::opcode;

    fn control() -> (AvrcpControl, mpsc::Receiver<AvcFrame>) {
        let (tx, rx) = mpsc::channel(8);
        (AvrcpControl::passthrough(tx), rx)
    }

    #[tokio::test]
    async fn a_pause_becomes_a_press_and_a_release_on_the_wire() {
        let (control, mut rx) = control();
        control.issue(ControlTxn::Pause).await.unwrap();

        let press = rx.recv().await.unwrap();
        let release = rx.recv().await.unwrap();
        assert_eq!(press.opcode, opcode::PASS_THROUGH);
        assert_eq!(press.operands[0], avrcp::operation::PAUSE);
        assert_eq!(release.operands[0], avrcp::operation::PAUSE | 0x80);
    }

    #[tokio::test]
    async fn an_unsupported_verb_never_reaches_the_channel() {
        // The capability check happens in the default `issue()` body, so this is the
        // core interface doing its job end to end: no frame, and a typed refusal.
        let (control, mut rx) = control();
        assert!(matches!(
            control
                .issue(ControlTxn::Seek(Duration::from_secs(30)))
                .await,
            Err(CoreError::UnsupportedControl(_))
        ));
        assert!(rx.try_recv().is_err(), "nothing should have been sent");
    }

    #[tokio::test]
    async fn a_dead_channel_is_an_adapter_error_not_a_silent_success() {
        // The phone walked out mid-tap. The panel needs to know the command went
        // nowhere, or it will show a paused state that isn't real.
        let (tx, rx) = mpsc::channel(1);
        let control = AvrcpControl::passthrough(tx);
        drop(rx);
        assert!(matches!(
            control.issue(ControlTxn::Play).await,
            Err(CoreError::Adapter(_))
        ));
    }

    #[tokio::test]
    async fn the_advertised_capabilities_match_what_can_actually_be_sent() {
        // Guards the invariant the touch UI is built on: every verb the capability set
        // offers must survive a real issue() call.
        let (control, mut rx) = control();
        for txn in [
            ControlTxn::Play,
            ControlTxn::Pause,
            ControlTxn::Stop,
            ControlTxn::Next,
            ControlTxn::Previous,
            ControlTxn::Mute(true),
        ] {
            assert!(
                control.capabilities().supports(&txn),
                "{txn:?} should be advertised"
            );
            control.issue(txn.clone()).await.unwrap();
            assert!(rx.recv().await.is_some(), "{txn:?} produced no press");
            assert!(rx.recv().await.is_some(), "{txn:?} produced no release");
        }
    }
}
