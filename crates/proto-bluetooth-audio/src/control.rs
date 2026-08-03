//! The [`RemoteControl`] implementation: a finger on the panel reaching the phone.
//!
//! This is the far end of the interface `core` grew for Bluetooth. The session manager
//! holds an `Arc<dyn RemoteControl>`; calling `issue()` on it puts AVRCP passthrough
//! frames on the AVCTP channel, and the phone pauses.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

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
    /// Held atomically because it is learned in two stages: AVCTP connects first and the
    /// surface has to exist by then (a phone can be controlled the moment it is up), but
    /// the peer's `SupportedFeatures` arrives later over SDP. Narrowing after publication
    /// is the only order the protocols allow.
    capabilities: Arc<AtomicU16>,
    /// What the player accepts for shuffle and repeat, learned in a third stage — the
    /// 0x11/0x12 listings, which arrive later still and only once AVRCP is up.
    ///
    /// Not folded into `capabilities`: the bits say *whether* a setting can be written,
    /// this says *with which value*, and a player offering only group-repeat needs both
    /// answers to get a working button (#76).
    settings: Arc<RwLock<avrcp::PlayerSettings>>,
    frames: mpsc::Sender<AvcFrame>,
}

impl AvrcpControl {
    /// Build a control handle over a channel to the AVCTP actor.
    ///
    /// `capabilities` should come from [`avrcp::capabilities_for_passthrough`], narrowed
    /// by whatever the peer's supported-features bitmask actually claimed.
    #[must_use]
    pub fn new(capabilities: ControlCapabilities, frames: mpsc::Sender<AvcFrame>) -> Self {
        Self {
            capabilities: Arc::new(AtomicU16::new(capabilities.bits())),
            settings: Arc::new(RwLock::new(avrcp::PlayerSettings::default())),
            frames,
        }
    }

    /// A handle offering everything passthrough can express.
    #[must_use]
    pub fn passthrough(frames: mpsc::Sender<AvcFrame>) -> Self {
        Self::new(avrcp::capabilities_for_passthrough(), frames)
    }

    /// Narrow (or widen) what the panel may offer, once the peer's SDP record says.
    ///
    /// The alternative was to publish nothing until SDP completes, which costs the panel
    /// every control over a phone whose record we never manage to read — and an
    /// unreadable record is far commoner than a phone that genuinely controls nothing.
    pub fn set_capabilities(&self, capabilities: ControlCapabilities) {
        self.capabilities
            .store(capabilities.bits(), Ordering::Relaxed);
    }

    /// Record what the player's settings listings said, once they have arrived.
    ///
    /// The caller is expected to follow this with [`Self::set_capabilities`]: this decides
    /// what a shuffle press *sends*, and that decides whether the button is offered.
    pub fn set_player_settings(&self, settings: avrcp::PlayerSettings) {
        // A poisoned lock means some other task panicked mid-update; the settings it left
        // behind are still a valid snapshot, and refusing to drive the phone over it would
        // be a worse answer than carrying on.
        *self
            .settings
            .write()
            .unwrap_or_else(PoisonError::into_inner) = settings;
    }

    /// Queue one frame, mapping a dead channel onto a typed failure.
    async fn send(&self, frame: AvcFrame) -> Result<(), CoreError> {
        self.frames
            .send(frame)
            .await
            .map_err(|_| CoreError::Adapter("avrcp control channel closed".into()))
    }
}

#[async_trait::async_trait]
impl RemoteControl for AvrcpControl {
    fn capabilities(&self) -> ControlCapabilities {
        ControlCapabilities::from_bits(self.capabilities.load(Ordering::Relaxed))
    }

    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
        // Two mechanisms, and which one a verb belongs to is not a detail. Transport is a
        // passthrough keypress; shuffle and repeat are *player application settings*,
        // written with a vendor-dependent command. There is no passthrough key that means
        // "shuffle", so this is a fork rather than a fallback.
        if matches!(txn, ControlTxn::Shuffle(_) | ControlTxn::Repeat(_)) {
            let value = {
                let settings = self.settings.read().unwrap_or_else(PoisonError::into_inner);
                settings.value_for(&txn)
            };
            // `None` means the player never listed this setting, or listed no value that
            // expresses the mode asked for. Refusing is the honest answer: a write the
            // peer rejects leaves the panel showing a state the phone is not in.
            let Some(value) = value else {
                return Err(CoreError::UnsupportedControl(format!("{txn:?}")));
            };
            return self.send(avrcp::set_setting_value(&[value])).await;
        }

        let Some(operation) = avrcp::operation_for(&txn) else {
            // Unreachable through `issue()`, which checks capabilities first — but a
            // direct caller must not silently get a nearest-equivalent keypress.
            return Err(CoreError::UnsupportedControl(format!("{txn:?}")));
        };

        // Press *and* release, always, and both before returning. Sending only the press
        // leaves the phone believing the key is held down; many then auto-repeat, so one
        // tap on "next" walks the entire album.
        for frame in avrcp::passthrough(operation) {
            self.send(frame).await?;
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
