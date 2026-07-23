//! The session manager: arbitrates a single active source, drives the [`Pipeline`], and
//! fires [`DisplayControl`] on session start. It's one *producer* on the OSD channel
//! (posting "Now casting from …"), not the OSD owner — the overlay is a shared subsystem
//! ([`crate::osd`]) many sources feed.
//!
//! Policy today is **last-writer-wins**: a new `Play`/`Mirror` from any source
//! preempts whatever is active (matching how casting UIs behave — the newest sender
//! takes the screen). Main+PiP arbitration is a future extension point.

use std::time::Duration;

use tracing::{info, warn};

use crate::adapter::{SourceId, SourceMessage};
use crate::display::{DisplayControl, DisplayInput};
use crate::error::CoreError;
use crate::event::SessionEvent;
use crate::osd::{OsdMessage, OsdSink};
use crate::pipeline::Pipeline;

/// Configuration the session manager needs that isn't per-event.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The input the receiver's video output is wired to; selected on session start.
    pub output_input: DisplayInput,
    /// How long the "now casting" banner stays up.
    pub osd_ttl: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            output_input: DisplayInput::Hdmi1,
            osd_ttl: Duration::from_secs(4),
        }
    }
}

/// Arbitrates sources and drives the pipeline. Generic over the pipeline and an
/// optional display-control backend so it can be unit-tested with fakes.
pub struct SessionManager<P: Pipeline> {
    pipeline: P,
    display: Option<Box<dyn DisplayControl>>,
    osd: Option<OsdSink>,
    config: SessionConfig,
    active: Option<SourceId>,
}

impl<P: Pipeline> SessionManager<P> {
    /// Build a session manager over `pipeline`, optionally controlling a display.
    pub fn new(
        pipeline: P,
        display: Option<Box<dyn DisplayControl>>,
        config: SessionConfig,
    ) -> Self {
        Self {
            pipeline,
            display,
            osd: None,
            config,
            active: None,
        }
    }

    /// Attach an OSD sink so the manager posts "Now casting from …" banners. Without one,
    /// session start/end simply doesn't touch the overlay.
    #[must_use]
    pub fn with_osd(mut self, osd: OsdSink) -> Self {
        self.osd = Some(osd);
        self
    }

    /// The currently active source, if any.
    #[must_use]
    pub fn active(&self) -> Option<&SourceId> {
        self.active.as_ref()
    }

    /// Consume the event stream until it closes, arbitrating sources.
    pub async fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<SourceMessage>) {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = self.handle(msg).await {
                warn!(error = %e, "session manager dropped an event");
            }
        }
        // Stream closed: best-effort teardown.
        let _ = self.pipeline.stop().await;
    }

    /// Handle a single tagged event. Public for unit testing.
    ///
    /// # Errors
    /// Propagates pipeline/display errors so the caller can log them.
    pub async fn handle(&mut self, msg: SourceMessage) -> Result<(), CoreError> {
        let SourceMessage { source, event } = msg;
        match event {
            SessionEvent::Play { source: uri, start } => {
                self.begin_session(&source).await?;
                info!(%source, %uri, "session: play");
                self.pipeline.play(uri, start).await
            }
            SessionEvent::Mirror { video, audio } => {
                self.begin_session(&source).await?;
                info!(%source, "session: mirror");
                self.pipeline.mirror(video, audio).await
            }
            SessionEvent::Control(txn) => {
                if self.active.as_ref() == Some(&source) {
                    self.pipeline.control(txn).await
                } else {
                    // Control from a backgrounded/unknown source is ignored, not fatal.
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::End => {
                if self.active.as_ref() == Some(&source) {
                    info!(%source, "session: end");
                    self.active = None;
                    if let Some(osd) = &self.osd {
                        osd.clear();
                    }
                    self.pipeline.stop().await
                } else {
                    // End for a source that isn't active — already preempted; no-op.
                    Ok(())
                }
            }
        }
    }

    /// Transition to `source` as the active session, preempting any other. Fires
    /// display power/input + OSD only on an actual (re)start.
    async fn begin_session(&mut self, source: &SourceId) -> Result<(), CoreError> {
        if self.active.as_ref() == Some(source) {
            return Ok(()); // already active; a follow-up Play/Mirror on same source
        }
        if let Some(prev) = &self.active {
            info!(%prev, %source, "session: preempting active source");
            self.pipeline.stop().await.ok();
        } else if let Some(display) = &self.display {
            // Idle → active: wake the panel and select our input.
            display.power_on().await?;
            display.select_input(self.config.output_input).await?;
        }
        self.active = Some(source.clone());
        if let Some(osd) = &self.osd {
            osd.show(OsdMessage::banner(
                format!("Now casting from {source}"),
                self.config.osd_ttl,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::event::ControlTxn;
    use crate::types::{MediaUri, ProtocolKind};

    #[derive(Default)]
    struct Counts {
        play: AtomicUsize,
        stop: AtomicUsize,
        control: AtomicUsize,
    }

    #[derive(Clone)]
    struct FakePipeline(Arc<Counts>);

    #[async_trait::async_trait]
    impl Pipeline for FakePipeline {
        async fn play(&self, _s: MediaUri, _start: Option<Duration>) -> Result<(), CoreError> {
            self.0.play.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn mirror(
            &self,
            _v: crate::types::FrameSource,
            _a: Option<crate::types::FrameSource>,
        ) -> Result<(), CoreError> {
            Ok(())
        }
        async fn control(&self, _txn: ControlTxn) -> Result<(), CoreError> {
            self.0.control.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self) -> Result<(), CoreError> {
            self.0.stop.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeDisplay(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl DisplayControl for FakeDisplay {
        async fn power_on(&self) -> Result<(), CoreError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn power_off(&self) -> Result<(), CoreError> {
            Ok(())
        }
        async fn select_input(&self, _i: DisplayInput) -> Result<(), CoreError> {
            Ok(())
        }
    }

    fn play_msg(src: &SourceId) -> SourceMessage {
        SourceMessage {
            source: src.clone(),
            event: SessionEvent::Play {
                source: MediaUri::parse("https://x/y.mp4").unwrap(),
                start: None,
            },
        }
    }

    #[tokio::test]
    async fn first_play_powers_display_and_sets_osd() {
        let counts = Arc::new(Counts::default());
        let powered = Arc::new(AtomicUsize::new(0));
        let (osd, osd_rx) = crate::osd::osd_channel();
        let mut mgr = SessionManager::new(
            FakePipeline(counts.clone()),
            Some(Box::new(FakeDisplay(powered.clone()))),
            SessionConfig::default(),
        )
        .with_osd(osd);
        let src = SourceId::new(ProtocolKind::Dlna, "a");
        mgr.handle(play_msg(&src)).await.unwrap();
        assert_eq!(counts.play.load(Ordering::SeqCst), 1);
        assert_eq!(powered.load(Ordering::SeqCst), 1);
        assert_eq!(mgr.active(), Some(&src));
        // The session posted a "Now casting from …" banner on the OSD channel.
        match osd_rx.try_recv() {
            Some(crate::osd::OsdCommand::Show(m)) => assert!(m.text.contains("dlna/a")),
            other => panic!("expected an OSD banner, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn new_source_preempts_active() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Dlna, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        mgr.handle(play_msg(&a)).await.unwrap();
        mgr.handle(play_msg(&b)).await.unwrap();
        // Second play preempted the first → one extra stop.
        assert_eq!(counts.stop.load(Ordering::SeqCst), 1);
        assert_eq!(mgr.active(), Some(&b));
    }

    #[tokio::test]
    async fn control_from_inactive_source_is_ignored() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Dlna, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        mgr.handle(play_msg(&a)).await.unwrap();
        let res = mgr
            .handle(SourceMessage {
                source: b,
                event: SessionEvent::Control(ControlTxn::Pause),
            })
            .await;
        assert!(matches!(res, Err(CoreError::NoActiveSession(_))));
        assert_eq!(counts.control.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn end_from_active_source_stops_and_clears() {
        let counts = Arc::new(Counts::default());
        let (osd, osd_rx) = crate::osd::osd_channel();
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default())
                .with_osd(osd);
        let a = SourceId::new(ProtocolKind::Dlna, "a");
        mgr.handle(play_msg(&a)).await.unwrap();
        mgr.handle(SourceMessage {
            source: a,
            event: SessionEvent::End,
        })
        .await
        .unwrap();
        assert_eq!(counts.stop.load(Ordering::SeqCst), 1);
        assert!(mgr.active().is_none());
        // Start posted a Show; End posted a Clear.
        assert!(matches!(
            osd_rx.try_recv(),
            Some(crate::osd::OsdCommand::Show(_))
        ));
        assert_eq!(osd_rx.try_recv(), Some(crate::osd::OsdCommand::Clear));
    }
}
