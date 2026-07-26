//! The session manager: arbitrates a single active source, drives the [`Pipeline`], and
//! fires [`DisplayControl`] on session start. It's one *producer* on the OSD channel
//! (posting "Now casting from …"), not the OSD owner — the overlay is a shared subsystem
//! ([`crate::osd`]) many sources feed.
//!
//! Policy today is **last-writer-wins**: a new `Play`/`Mirror` from any source
//! preempts whatever is active (matching how casting UIs behave — the newest sender
//! takes the screen). Main+PiP arbitration is a future extension point.

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::adapter::{SourceId, SourceMessage};
use crate::control::RemoteControl;
use crate::display::{DisplayControl, DisplayInput};
use crate::error::CoreError;
use crate::event::SessionEvent;
use crate::osd::{OsdMessage, OsdSink};
use crate::pipeline::Pipeline;
use crate::source::SourceDescription;

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
    remote: Option<Arc<dyn RemoteControl>>,
    description: SourceDescription,
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
            remote: None,
            description: SourceDescription::new(),
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

    /// What is known about the connected sender: name, address, negotiated codec.
    #[must_use]
    pub const fn description(&self) -> &SourceDescription {
        &self.description
    }

    /// A handle for driving the *active sender*, if it published one.
    ///
    /// This is what the touch UI reaches for when a finger lands on pause: the panel
    /// doesn't pause the pipeline, it tells the phone to stop sending. Cleared whenever
    /// the session ends or is preempted, so a stale handle can't outlive its session and
    /// send commands to a phone that walked out of the room.
    #[must_use]
    pub fn remote(&self) -> Option<&Arc<dyn RemoteControl>> {
        self.remote.as_ref()
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
            SessionEvent::Audio {
                source: frames,
                format,
            } => {
                self.begin_session(&source).await?;
                info!(%source, %format, "session: audio");
                self.pipeline.play_audio(frames, format).await
            }
            SessionEvent::NowPlaying(snapshot) => {
                if self.active.as_ref() == Some(&source) {
                    self.pipeline.now_playing(snapshot).await
                } else {
                    // Metadata from a backgrounded source would overwrite the active
                    // card; drop it rather than let a preempted phone win the screen.
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::UpNext(items) => {
                if self.active.as_ref() == Some(&source) {
                    info!(%source, queued = items.len(), "session: up next");
                    self.pipeline.up_next(items).await
                } else {
                    // Same reasoning as the metadata case: a backgrounded source must not
                    // rewrite the queue the room is looking at.
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::SourceInfo(info) => {
                if self.active.as_ref() == Some(&source) {
                    // Merged, not replaced: each update knows only the fact it just
                    // learned, and a codec update must not erase the device's name.
                    self.description = std::mem::take(&mut self.description).merged(info);
                    let description = self.description.clone();
                    if let Some(osd) = &self.osd {
                        osd.show(OsdMessage::banner(
                            format!("Now playing from {description}"),
                            self.config.osd_ttl,
                        ));
                    }
                    self.pipeline.source_info(description).await
                } else {
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::ControlSurface(remote) => {
                if self.active.as_ref() == Some(&source) {
                    info!(%source, caps = ?remote.capabilities(), "session: control surface up");
                    self.remote = Some(remote);
                    Ok(())
                } else {
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
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
                    self.remote = None;
                    self.description = SourceDescription::new();
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
            // The outgoing source's control handle and description die with its session
            // — the new source publishes its own if it has any.
            self.remote = None;
            self.description = SourceDescription::new();
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
    use crate::ControlCapabilities;

    #[derive(Default)]
    struct Counts {
        play: AtomicUsize,
        stop: AtomicUsize,
        control: AtomicUsize,
        audio: AtomicUsize,
        audio_format: std::sync::Mutex<Option<crate::types::AudioFormat>>,
        snapshots: std::sync::Mutex<Vec<crate::NowPlaying>>,
        description: std::sync::Mutex<SourceDescription>,
        up_next: std::sync::Mutex<Vec<crate::QueueItem>>,
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
        async fn play_audio(
            &self,
            _s: crate::types::FrameSource,
            format: crate::types::AudioFormat,
        ) -> Result<(), CoreError> {
            self.0.audio.fetch_add(1, Ordering::SeqCst);
            *self.0.audio_format.lock().expect("poisoned") = Some(format);
            Ok(())
        }
        async fn now_playing(&self, snapshot: crate::NowPlaying) -> Result<(), CoreError> {
            self.0.snapshots.lock().expect("poisoned").push(snapshot);
            Ok(())
        }
        async fn up_next(&self, items: Vec<crate::QueueItem>) -> Result<(), CoreError> {
            *self.0.up_next.lock().expect("poisoned") = items;
            Ok(())
        }
        async fn source_info(&self, info: SourceDescription) -> Result<(), CoreError> {
            *self.0.description.lock().expect("poisoned") = info;
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

    /// Stands in for a phone's AVRCP target: records what the receiver told it to do.
    #[derive(Debug, Default)]
    struct FakeRemote {
        caps: ControlCapabilities,
        sent: std::sync::Mutex<Vec<ControlTxn>>,
    }

    #[async_trait::async_trait]
    impl crate::RemoteControl for FakeRemote {
        fn capabilities(&self) -> ControlCapabilities {
            self.caps
        }
        async fn issue_unchecked(&self, txn: ControlTxn) -> Result<(), CoreError> {
            self.sent.lock().expect("poisoned").push(txn);
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
    async fn the_queue_reaches_the_pipeline_from_the_active_source() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Spotify, "a");
        mgr.handle(play_msg(&a)).await.unwrap();
        mgr.handle(SourceMessage {
            source: a.clone(),
            event: SessionEvent::UpNext(vec![
                crate::QueueItem::new("Aerodynamic").with_artist("Daft Punk")
            ]),
        })
        .await
        .unwrap();
        let queued = counts.up_next.lock().expect("poisoned");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].title, "Aerodynamic");
    }

    #[tokio::test]
    async fn a_backgrounded_source_cannot_rewrite_the_queue() {
        // Same rule as metadata: whoever is on screen owns the screen. A preempted phone
        // still pushing cluster updates must not replace the queue the room is reading.
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Spotify, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        mgr.handle(play_msg(&a)).await.unwrap();
        let res = mgr
            .handle(SourceMessage {
                source: b,
                event: SessionEvent::UpNext(vec![crate::QueueItem::new("Intruder")]),
            })
            .await;
        assert!(matches!(res, Err(CoreError::NoActiveSession(_))));
        assert!(counts.up_next.lock().expect("poisoned").is_empty());
    }

    fn audio_msg(src: &SourceId) -> SourceMessage {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        SourceMessage {
            source: src.clone(),
            event: SessionEvent::Audio {
                source: crate::types::FrameSource::Encoded(rx),
                format: crate::types::AudioFormat::from_hz(48_000, 2)
                    .expect("48 kHz stereo is a format"),
            },
        }
    }

    #[tokio::test]
    async fn an_audio_session_starts_the_pipeline_and_takes_metadata() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let src = SourceId::new(ProtocolKind::Bluetooth, "aa:bb:cc:dd:ee:ff");
        mgr.handle(audio_msg(&src)).await.unwrap();
        assert_eq!(counts.audio.load(Ordering::SeqCst), 1);
        // Q25: the negotiated rate must survive the trip to the pipeline. Losing it here
        // is inaudible in a test and plays 9% slow on a real 48 kHz aptX stream.
        assert_eq!(
            *counts.audio_format.lock().unwrap(),
            crate::types::AudioFormat::from_hz(48_000, 2),
        );

        // Text first, artwork second — the two-snapshot shape cover art actually has.
        let text = crate::NowPlaying::default()
            .with_title("Bloom")
            .with_artist("Beach House");
        mgr.handle(SourceMessage {
            source: src.clone(),
            event: SessionEvent::NowPlaying(text.clone()),
        })
        .await
        .unwrap();
        mgr.handle(SourceMessage {
            source: src,
            event: SessionEvent::NowPlaying(text.with_artwork(crate::Artwork::new(
                crate::ImageFormat::Jpeg,
                bytes::Bytes::from_static(&[0xff, 0xd8]),
            ))),
        })
        .await
        .unwrap();

        let snaps = counts.snapshots.lock().unwrap();
        assert_eq!(snaps.len(), 2);
        assert!(snaps[0].artwork.is_none());
        assert!(snaps[1].artwork.is_some());
        assert!(
            snaps[0].is_same_item(&snaps[1]),
            "art is not a track change"
        );
    }

    #[tokio::test]
    async fn metadata_from_a_backgrounded_source_cannot_hijack_the_card() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Bluetooth, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        mgr.handle(audio_msg(&a)).await.unwrap();
        mgr.handle(play_msg(&b)).await.unwrap(); // b preempts a
        let res = mgr
            .handle(SourceMessage {
                source: a,
                event: SessionEvent::NowPlaying(crate::NowPlaying::default().with_title("stale")),
            })
            .await;
        assert!(matches!(res, Err(CoreError::NoActiveSession(_))));
        assert!(counts.snapshots.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_control_surface_is_published_then_dies_with_its_session() {
        // The reverse channel's lifetime rule: a handle must never outlive the session
        // that produced it, or the panel ends up pausing a phone that left the room.
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Bluetooth, "a");
        mgr.handle(audio_msg(&a)).await.unwrap();
        assert!(mgr.remote().is_none(), "no handle before AVCTP connects");

        mgr.handle(SourceMessage {
            source: a.clone(),
            event: SessionEvent::ControlSurface(Arc::new(FakeRemote {
                caps: ControlCapabilities::TRANSPORT,
                sent: std::sync::Mutex::new(Vec::new()),
            })),
        })
        .await
        .unwrap();

        let remote = mgr.remote().expect("published").clone();
        assert!(remote.capabilities().supports(&ControlTxn::Pause));
        remote.issue(ControlTxn::Pause).await.unwrap();
        // Seek is outside TRANSPORT, so the capability set refuses it before the wire.
        assert!(matches!(
            remote.issue(ControlTxn::Seek(Duration::from_secs(5))).await,
            Err(CoreError::UnsupportedControl(_))
        ));

        mgr.handle(SourceMessage {
            source: a,
            event: SessionEvent::End,
        })
        .await
        .unwrap();
        assert!(
            mgr.remote().is_none(),
            "handle cleared when the session ended"
        );
    }

    #[tokio::test]
    async fn preemption_drops_the_previous_sources_control_surface() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Bluetooth, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        mgr.handle(audio_msg(&a)).await.unwrap();
        mgr.handle(SourceMessage {
            source: a,
            event: SessionEvent::ControlSurface(Arc::new(FakeRemote::default())),
        })
        .await
        .unwrap();
        assert!(mgr.remote().is_some());
        mgr.handle(play_msg(&b)).await.unwrap();
        assert!(mgr.remote().is_none());
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
