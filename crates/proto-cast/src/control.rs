//! The reverse channel: telling a Cast sender what became of what it sent.
//!
//! Cast is the second protocol where the receiver *is* the player, and it had the same gap
//! DLNA did: a sender hands over a URL and has no way to learn what happened to it except
//! by being told. Nothing told it. The media status stayed `PLAYING` for the life of the
//! connection, so a queue never advanced past its first item, and a URL the box could not
//! fetch was indistinguishable — from the phone — from a cast that was working perfectly.
//!
//! The shape is deliberately the same as `proto-dlna`'s. A press on the panel moves this
//! session's own state *and* is broadcast to every sender, because a receiver that moved
//! for one view and not the other leaves the sender's pause button toggling playback back
//! on — which is the failure [`ControlTxn`] being absolute rather than a toggle exists to
//! prevent.
//!
//! What is different is where the state lives. DLNA's renderer is shared behind a mutex
//! because HTTP handlers arrive on any task; a `CastSession` belongs to one connection and
//! is owned by the actor pumping it, so this sends *requests* to that actor rather than
//! reaching into the session. The session stays single-owner, and the actor stays the only
//! thing that touches it.

use std::fmt;

use castaway_core::{ControlCapabilities, ControlTxn, CoreError, PlaybackEnd, RemoteControl};
use tokio::sync::mpsc;
use tracing::debug;

/// Something for the connection's actor to fold into its session and write back out.
#[derive(Debug)]
pub(crate) enum FromReceiver {
    /// The pipeline finished with the item, or failed to play it.
    Ended(PlaybackEnd),
    /// A transport verb from the panel.
    Control(ControlTxn),
}

/// A [`RemoteControl`] over a live Cast session.
pub struct CastRemote {
    to_actor: mpsc::Sender<FromReceiver>,
    sink: castaway_core::SessionSink,
}

impl CastRemote {
    /// Wrap the connection's request channel and the session's event sink.
    #[must_use]
    pub(crate) const fn new(
        to_actor: mpsc::Sender<FromReceiver>,
        sink: castaway_core::SessionSink,
    ) -> Self {
        Self { to_actor, sink }
    }

    /// What a Cast session lets the panel do.
    ///
    /// The same set as DLNA's, and for the same reason: it is derived from what
    /// `RenderPipeline::control` will honour for a URL session, not from what Cast's
    /// `supportedMediaCommands` bitmask happens to say. `NEXT`/`PREVIOUS` are absent
    /// because the queue lives in the sender, which is the only thing that can move it.
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

impl fmt::Debug for CastRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CastRemote").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl RemoteControl for CastRemote {
    fn capabilities(&self) -> ControlCapabilities {
        Self::capabilities()
    }

    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
        debug!(?txn, "cast: transport from the panel");
        // The sender's view first, so it cannot be left behind. Best-effort: a connection
        // that has gone is not a reason to refuse the panel, because the *pipeline* half
        // below is what the person pressing the button is actually asking for.
        if self
            .to_actor
            .send(FromReceiver::Control(txn.clone()))
            .await
            .is_err()
        {
            debug!(
                ?txn,
                "cast: the sender connection has gone; telling only the pipeline"
            );
        }
        self.sink
            .emit(castaway_core::SessionEvent::Control(txn))
            .await
            .map_err(|e| CoreError::Adapter(format!("cast control: {e}")))
    }

    async fn media_ended(&self, end: PlaybackEnd) -> Result<(), CoreError> {
        // The report this type exists for. It becomes a broadcast `MEDIA_STATUS` with
        // `playerState: IDLE` and an `idleReason` — `FINISHED` for an item that played
        // through, `ERROR` for one that could not be fetched — which is what a sender
        // watches for before it sends the next thing.
        self.to_actor
            .send(FromReceiver::Ended(end))
            .await
            .map_err(|e| CoreError::Adapter(format!("cast end report: {e}")))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::time::Duration;

    use castaway_core::{ProtocolKind, SessionEvent, SessionSink, SourceId};

    use super::*;

    fn remote() -> (
        CastRemote,
        mpsc::Receiver<FromReceiver>,
        mpsc::Receiver<castaway_core::SourceMessage>,
    ) {
        let (to_actor, actor_rx) = mpsc::channel(8);
        let (tx, rx) = mpsc::channel(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Cast, "test"), tx);
        (CastRemote::new(to_actor, sink), actor_rx, rx)
    }

    /// The capability set must not promise more than `RenderPipeline::control` honours for
    /// a URL session, or the panel draws a button that does nothing.
    #[test]
    fn only_what_the_pipeline_can_actually_do_is_advertised() {
        let caps = CastRemote::capabilities();
        for txn in [
            ControlTxn::Play,
            ControlTxn::Pause,
            ControlTxn::Stop,
            ControlTxn::Seek(Duration::from_secs(1)),
            ControlTxn::Volume(castaway_core::Volume::from_position(0.5)),
            ControlTxn::Mute(true),
        ] {
            assert!(caps.supports(&txn), "{txn:?} should be offered");
        }
        // The queue lives in the sender, and only the sender can move it.
        assert!(!caps.supports(&ControlTxn::Next));
        assert!(!caps.supports(&ControlTxn::Previous));
    }

    /// A press on the glass has to reach both views of the session: the pipeline that is
    /// playing, and the sender whose UI is showing the transport.
    #[tokio::test]
    async fn a_press_on_the_panel_reaches_the_pipeline_and_the_sender() {
        let (remote, mut actor_rx, mut rx) = remote();
        remote.issue(ControlTxn::Pause).await.unwrap();

        assert!(matches!(
            actor_rx.try_recv(),
            Ok(FromReceiver::Control(ControlTxn::Pause))
        ));
        assert!(matches!(
            rx.recv().await.unwrap().event,
            SessionEvent::Control(ControlTxn::Pause)
        ));
    }

    /// A sender that has already gone must not stop the panel from pausing what is on the
    /// screen — the person pressing it is asking about the room, not about the phone.
    #[tokio::test]
    async fn a_gone_sender_does_not_refuse_the_panel() {
        let (remote, actor_rx, mut rx) = remote();
        drop(actor_rx);
        remote.issue(ControlTxn::Pause).await.unwrap();
        assert!(matches!(
            rx.recv().await.unwrap().event,
            SessionEvent::Control(ControlTxn::Pause)
        ));
    }

    #[tokio::test]
    async fn the_end_of_the_item_is_handed_to_the_connection() {
        let (remote, mut actor_rx, _rx) = remote();
        remote
            .media_ended(PlaybackEnd::Failed(
                castaway_core::PlaybackFailure::unobtainable("connection refused"),
            ))
            .await
            .unwrap();
        match actor_rx.try_recv() {
            Ok(FromReceiver::Ended(end)) => assert!(end.is_failure()),
            other => panic!("expected an end report, got {other:?}"),
        }
    }
}
