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

use castaway_core::{ControlCapabilities, ControlTxn, CoreError, PlaybackEnd, RemoteControl};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::state::{Renderer, TransportState};

/// A [`RemoteControl`] over a live DLNA session.
///
/// Holds the whole service state rather than the two pieces it strictly needs, because a
/// press on the glass has three consequences and not two: the transport state moves, the
/// transaction goes to the pipeline, and every GENA subscriber has to be told — and a
/// remote that could do the first two but not the third would leave a subscribed control
/// point convinced the session was still playing.
pub struct DlnaRemote {
    state: Arc<crate::service::DlnaState>,
}

impl DlnaRemote {
    /// Wrap the service's shared state.
    #[must_use]
    pub(crate) const fn new(state: Arc<crate::service::DlnaState>) -> Self {
        Self { state }
    }

    /// The renderer this remote drives.
    fn renderer(&self) -> &Mutex<Renderer> {
        &self.state.renderer
    }

    /// What a DLNA session lets the panel do.
    ///
    /// Derived from what the *pipeline* will actually honour for a URL session, not from
    /// what the AVTransport service template lists. `RenderPipeline::control` freezes the
    /// media clock for play/pause, moves the demuxer for a seek, and applies volume and
    /// mute to the output gain — so those are the buttons, and no others appear.
    ///
    /// Deliberately absent: `NEXT`/`PREVIOUS`, because a renderer has no playlist to move
    /// through. `SetNextAVTransportURI` is a queue of exactly one and is not that.
    #[must_use]
    pub const fn capabilities() -> ControlCapabilities {
        ControlCapabilities::PLAY
            .or(ControlCapabilities::PAUSE)
            .or(ControlCapabilities::STOP)
            .or(ControlCapabilities::SEEK)
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
            let mut renderer = self.renderer().lock().await;
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
                // A seek does not change the transport state — playing stays playing and
                // paused stays paused, which is §2.4.11 and is also what a scrubbed,
                // paused session has to do to be usable. The *position* moves, and that
                // is answered from the pipeline's clock rather than stored here.
                ControlTxn::Seek(_) => {}
                other => {
                    // Unreachable through `issue`, which checks the capability set first.
                    // Refusing rather than succeeding silently keeps a direct caller
                    // honest, and keeps a verb added upstream from becoming a no-op.
                    return Err(CoreError::UnsupportedControl(format!("{other:?}")));
                }
            }
        }

        debug!(?txn, "dlna: transport from the panel");
        // Subscribers hear about a press on the glass exactly as they hear about one on
        // the phone. Leaving this out is the same bug as never eventing at all, restricted
        // to the half of the transport a person is standing in front of.
        self.publish().await;
        self.state
            .sink
            .emit(castaway_core::SessionEvent::Control(txn))
            .await
            .map_err(|e| CoreError::Adapter(format!("dlna control: {e}")))
    }

    async fn media_ended(&self, end: PlaybackEnd) -> Result<(), CoreError> {
        // DLNA is one of the two protocols where this is not a no-op, because here the
        // receiver is the player: the control point pushed a URL and has no way at all to
        // learn what became of it except by asking us.
        //
        // The transport state is the whole answer. It moves to STOPPED, which is what a
        // queue watches for before sending the next track, and the status carries whether
        // this was an ending or a failure — §2.2.2's `ERROR_OCCURRED` is for exactly the
        // URL that could not be fetched, which until now read as PLAYING / OK forever.
        info!(%end, "dlna: the pipeline finished with the item");
        self.renderer().lock().await.media_ended(&end);
        // The event this exists for. A subscriber that is *not* polling — which is the
        // whole point of having subscribed — has no other way to learn that the item is
        // over, so without this the eventing path reproduces exactly the gap the polling
        // path had.
        self.publish().await;
        Ok(())
    }
}

impl DlnaRemote {
    /// Tell every subscriber what changed, if anything did.
    async fn publish(&self) {
        crate::service::publish_if_changed(&self.state, crate::gena::EventedService::AvTransport)
            .await;
        crate::service::publish_if_changed(
            &self.state,
            crate::gena::EventedService::RenderingControl,
        )
        .await;
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
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "test"), tx);
        let service = crate::DlnaService::new("Test TV", "abcd-1234", sink);
        let state = service.state();
        let renderer = Arc::clone(&state.renderer);
        (DlnaRemote::new(state), renderer, rx)
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
            ControlTxn::Seek(Duration::from_secs(1)),
            ControlTxn::Volume(0.5),
            ControlTxn::Mute(true),
        ] {
            assert!(caps.supports(&txn), "{txn:?} should be offered");
        }
        // A renderer handed one URL has no playlist for next/previous to move through.
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
            remote.issue(ControlTxn::Next).await,
            Err(CoreError::UnsupportedControl(_))
        ));
        assert!(rx.try_recv().is_err(), "nothing reached the pipeline");
    }

    /// A seek moves the position and nothing else. A paused session that resumed itself
    /// because somebody dragged the scrubber would be a panel that starts playing when
    /// you were only looking for a spot.
    #[tokio::test]
    async fn a_seek_from_the_panel_does_not_disturb_the_transport_state() {
        let (remote, renderer, mut rx) = remote();
        renderer.lock().await.state = TransportState::PausedPlayback;

        remote
            .issue(ControlTxn::Seek(Duration::from_secs(90)))
            .await
            .unwrap();
        assert_eq!(renderer.lock().await.state, TransportState::PausedPlayback);

        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.event,
            SessionEvent::Control(ControlTxn::Seek(d)) if d == Duration::from_secs(90)
        ));
    }
}
