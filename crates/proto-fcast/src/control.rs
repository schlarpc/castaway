//! The reverse channel: the panel driving an FCast session.
//!
//! Same shape as DLNA's, for the same reason — the sender pushed a URL and the
//! receiver *is* the player, so a finger on the panel acts on our own pipeline. The
//! extra obligation here is the broadcast: every connected sender is told what the
//! finger did (`PlaybackUpdate` / `VolumeUpdate` in its own protocol version), or a
//! phone that started the cast keeps showing "playing" after the room paused it and
//! its pause button becomes a toggle back to playing — exactly the disagreement
//! [`castaway_core::ControlTxn`] being absolute exists to prevent.
//!
//! Unlike DLNA this protocol *does* have a playlist, so next/previous appear: they
//! move the playlist this crate owns, exactly as a sender's `SetPlaylistItem` does.

use std::fmt;
use std::sync::Arc;

use castaway_core::{
    ControlCapabilities, ControlTxn, CoreError, PlaybackEnd, RemoteControl, SessionEvent,
    SessionSink,
};
use tracing::{debug, info};

use crate::adapter::Shared;
use crate::session::SenderCommand;

/// A [`castaway_core::RemoteControl`] over the live FCast player.
pub struct FCastRemote {
    shared: Arc<Shared>,
    sink: SessionSink,
}

impl FCastRemote {
    pub(crate) fn new(shared: Arc<Shared>, sink: SessionSink) -> Self {
        Self { shared, sink }
    }

    /// What an FCast session lets the panel do.
    ///
    /// Play/pause/stop/seek/volume/mute are what `RenderPipeline::control` honours
    /// for a URL session (DLNA's reasoning, unchanged). Next/previous are offered
    /// because FCast has a playlist to move through — they refuse at issue time when
    /// the current load is a single URL, the same answer a sender's out-of-range
    /// `SetPlaylistItem` gets.
    #[must_use]
    pub const fn capabilities() -> ControlCapabilities {
        ControlCapabilities::PLAY
            .or(ControlCapabilities::PAUSE)
            .or(ControlCapabilities::STOP)
            .or(ControlCapabilities::SEEK)
            .or(ControlCapabilities::VOLUME)
            .or(ControlCapabilities::MUTE)
            .or(ControlCapabilities::NEXT)
            .or(ControlCapabilities::PREVIOUS)
    }
}

impl fmt::Debug for FCastRemote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FCastRemote").finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl RemoteControl for FCastRemote {
    fn capabilities(&self) -> ControlCapabilities {
        Self::capabilities()
    }

    async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
        debug!(?txn, "fcast: transport from the panel");
        // Every verb goes through the same player the senders' commands do, so the
        // state senders are told about and the state the room sees cannot diverge.
        let events = match txn {
            ControlTxn::Play => self.shared.apply(None, SenderCommand::Resume),
            ControlTxn::Pause => self.shared.apply(None, SenderCommand::Pause),
            ControlTxn::Stop => self.shared.apply(None, SenderCommand::Stop),
            ControlTxn::Seek(target) => self.shared.apply(None, SenderCommand::Seek(target)),
            ControlTxn::Volume(level) => self
                .shared
                .apply(None, SenderCommand::SetVolume(f64::from(level.position()))),
            // FCast has no mute on the wire; the pipeline handles it and senders'
            // volume sliders are left alone.
            ControlTxn::Mute(muted) => {
                return self
                    .sink
                    .emit(SessionEvent::Control(ControlTxn::Mute(muted)))
                    .await
                    .map_err(|e| CoreError::Adapter(format!("fcast control: {e}")));
            }
            ControlTxn::Next => self.shared.step(true),
            ControlTxn::Previous => self.shared.step(false),
            other => {
                // Unreachable through `issue`, which checks the capability set
                // first. Refusing keeps a direct caller honest.
                return Err(CoreError::UnsupportedControl(format!("{other:?}")));
            }
        };
        let events = events.map_err(|refusal| CoreError::UnsupportedControl(refusal.message))?;
        for event in events {
            self.sink
                .emit(event)
                .await
                .map_err(|e| CoreError::Adapter(format!("fcast control: {e}")))?;
        }
        Ok(())
    }

    async fn media_ended(&self, end: PlaybackEnd) -> Result<(), CoreError> {
        // The receiver is the player, so this is where the playlist advances: the
        // next item's `Play` goes to the pipeline and every sender hears
        // `MediaItemEnd` (the event the reference sender subscribes to on every
        // connection) plus the new item's start.
        info!(%end, "fcast: the pipeline finished with the item");
        let events = self.shared.media_ended(&end);
        let advances = events
            .iter()
            .any(|e| matches!(e, SessionEvent::Play { .. }));
        for event in events {
            let begins = matches!(event, SessionEvent::Play { .. });
            self.sink
                .emit(event)
                .await
                .map_err(|e| CoreError::Adapter(format!("fcast media end: {e}")))?;
            if begins {
                // The next item is a fresh hold on the screen; it needs the
                // surface again (proto-cast's lesson).
                let remote = Arc::new(Self::new(Arc::clone(&self.shared), self.sink.clone()));
                self.sink
                    .emit(SessionEvent::ControlSurface(remote))
                    .await
                    .map_err(|e| CoreError::Adapter(format!("fcast media end: {e}")))?;
            }
        }
        if advances {
            debug!("fcast: playlist advanced");
        }
        Ok(())
    }
}
