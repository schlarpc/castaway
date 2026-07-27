//! The reverse channel: the panel driving a DLNA session.
//!
//! DLNA is the third shape this trait has taken, and the interesting one. For Bluetooth
//! the receiver drives the *phone* over AVRCP; for Spotify it drives librespot's Connect
//! state, which tells the account. For DLNA there is nobody to drive — a control point
//! pushes a URL and the receiver *is* the player — so a finger on the panel acts on our
//! own pipeline, and the transport state is updated underneath so a control point that
//! polls `GetTransportInfo` sees what the room sees.
//!
//! That second half is what makes this more than a local shortcut. Without it, pausing on
//! the panel and then looking at the phone that started the cast shows a control point
//! still convinced it is playing, and pressing its pause button would toggle back to
//! playing — the two views of one session disagreeing, which is the failure
//! [`castaway_core::ControlTxn`] being absolute rather than a toggle exists to prevent.

use std::fmt;
use std::sync::Arc;

use castaway_core::{ControlCapabilities, ControlTxn, CoreError, RemoteControl};
use tokio::sync::Mutex;
use tracing::debug;

use crate::state::{Renderer, TransportState};

/// A [`RemoteControl`] over a live DLNA session.
pub struct DlnaRemote {
    renderer: Arc<Mutex<Renderer>>,
    /// Where the transaction goes once the transport state has been updated: into the
    /// session manager, which routes it to the pipeline that is actually playing.
    sink: castaway_core::SessionSink,
}

impl DlnaRemote {
    /// Wrap the renderer state and the session's event sink.
    #[must_use]
    pub const fn new(renderer: Arc<Mutex<Renderer>>, sink: castaway_core::SessionSink) -> Self {
        Self { renderer, sink }
    }

    /// What a DLNA session lets the panel do.
    ///
    /// Derived from what the *pipeline* will actually honour for a URL session, not from
    /// what the AVTransport service template lists. `RenderPipeline::control` freezes the
    /// media clock for play/pause and applies volume and mute to the output gain, and
    /// refuses everything else — so those are the buttons, and no others appear.
    ///
    /// Deliberately absent:
    /// - `SEEK`, because `decode_av` cannot move the demuxer yet (GAPS G61). The scrubber
    ///   still draws, because knowing how far through you are is worth having; it just
    ///   takes no touches.
    /// - `NEXT`/`PREVIOUS`, because a renderer has no playlist to move through.
    ///   `SetNextAVTransportURI` is a queue of exactly one and is not that.
    #[must_use]
    pub const fn capabilities() -> ControlCapabilities {
        ControlCapabilities::PLAY
            .or(ControlCapabilities::PAUSE)
            .or(ControlCapabilities::STOP)
            .or(ControlCapabilities::VOLUME)
            .or(ControlCapabilities::MUTE)
    }
}

impl fmt::Debug for DlnaRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DlnaRemote").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl RemoteControl for DlnaRemote {
    fn capabilities(&self) -> ControlCapabilities {
        Self::capabilities()
    }

    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
        // The transport state first, so a control point polling `GetTransportInfo` and the
        // panel never disagree about what is happening. UPnP's volume scale is 0–100 and
        // ours is 0.0–1.0; the renderer holds the UPnP one because that is what it has to
        // answer `GetVolume` with.
        {
            let mut renderer = self.renderer.lock().await;
            match &txn {
                ControlTxn::Play => renderer.state = TransportState::Playing,
                ControlTxn::Pause => renderer.state = TransportState::PausedPlayback,
                ControlTxn::Stop => renderer.state = TransportState::Stopped,
                ControlTxn::Volume(level) => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        renderer.volume = (level.clamp(0.0, 1.0) * 100.0).round() as u8;
                    }
                }
                ControlTxn::Mute(muted) => renderer.muted = *muted,
                other => {
                    // Unreachable through `issue`, which checks the capability set first.
                    // Refusing rather than succeeding silently keeps a direct caller
                    // honest, and keeps a verb added upstream from becoming a no-op.
                    return Err(CoreError::UnsupportedControl(format!("{other:?}")));
                }
            }
        }

        debug!(?txn, "dlna: transport from the panel");
        self.sink
            .emit(castaway_core::SessionEvent::Control(txn))
            .await
            .map_err(|e| CoreError::Adapter(format!("dlna control: {e}")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use castaway_core::{ProtocolKind, SessionEvent, SessionSink, SourceId, SourceMessage};
    use tokio::sync::mpsc;

    use super::*;

    fn remote() -> (
        DlnaRemote,
        Arc<Mutex<Renderer>>,
        mpsc::Receiver<SourceMessage>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        let renderer = Arc::new(Mutex::new(Renderer::default()));
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "test"), tx);
        (DlnaRemote::new(Arc::clone(&renderer), sink), renderer, rx)
    }

    /// The capability set must not promise more than `RenderPipeline::control` honours
    /// for a URL session, or the panel draws a button that does nothing.
    #[test]
    fn only_what_the_pipeline_can_actually_do_is_advertised() {
        let caps = DlnaRemote::capabilities();
        for txn in [
            ControlTxn::Play,
            ControlTxn::Pause,
            ControlTxn::Stop,
            ControlTxn::Volume(0.5),
            ControlTxn::Mute(true),
        ] {
            assert!(caps.supports(&txn), "{txn:?} should be offered");
        }
        // Seek needs the demuxer to move, which it cannot yet (G61); a renderer has no
        // playlist for next/previous to move through.
        assert!(!caps.supports(&ControlTxn::Seek(Duration::from_secs(1))));
        assert!(!caps.supports(&ControlTxn::Next));
        assert!(!caps.supports(&ControlTxn::Previous));
    }

    /// The point of updating the state as well as forwarding: a control point that polls
    /// `GetTransportInfo` must see the pause that happened on the glass, or the two views
    /// of one session disagree and its pause button toggles back to playing.
    #[tokio::test]
    async fn a_pause_on_the_panel_is_visible_to_the_control_point() {
        let (remote, renderer, mut rx) = remote();
        renderer.lock().await.state = TransportState::Playing;

        remote.issue(ControlTxn::Pause).await.unwrap();
        assert_eq!(
            renderer.lock().await.state,
            TransportState::PausedPlayback,
            "the control point would still be told PLAYING"
        );
        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.event,
            SessionEvent::Control(ControlTxn::Pause)
        ));

        remote.issue(ControlTxn::Play).await.unwrap();
        assert_eq!(renderer.lock().await.state, TransportState::Playing);
    }

    /// UPnP's `GetVolume` answers on a 0–100 scale, so the renderer has to hold it that
    /// way — the panel's slider is a fraction.
    #[tokio::test]
    async fn volume_is_stored_on_the_scale_upnp_answers_with() {
        let (remote, renderer, _rx) = remote();
        remote.issue(ControlTxn::Volume(0.25)).await.unwrap();
        assert_eq!(renderer.lock().await.volume, 25);
        // A slider that overshoots saturates rather than wrapping to silence.
        remote.issue(ControlTxn::Volume(1.5)).await.unwrap();
        assert_eq!(renderer.lock().await.volume, 100);
    }

    #[tokio::test]
    async fn a_verb_outside_the_set_is_refused_rather_than_dropped() {
        let (remote, _renderer, mut rx) = remote();
        assert!(matches!(
            remote.issue(ControlTxn::Seek(Duration::from_secs(5))).await,
            Err(CoreError::UnsupportedControl(_))
        ));
        assert!(rx.try_recv().is_err(), "nothing reached the pipeline");
    }
}
