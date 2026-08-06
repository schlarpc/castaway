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

use tracing::{debug, info, warn};

use crate::adapter::{SourceId, SourceMessage};
use crate::control::{ControlCapabilities, RemoteControl};
use crate::display::{DisplayControl, DisplayInput};
use crate::error::CoreError;
use crate::event::{ControlTxn, SessionEvent};
use crate::osd::{OsdMessage, OsdSink};
use crate::pipeline::Pipeline;
use crate::playback::PlaybackEnd;
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
    remote: RemoteHandle,
    /// The active source's touch surface, for a router that owns the panel. Held here as
    /// well as pushed to the pipeline so a router that starts late can ask.
    touch: crate::touch::TouchHandle,
    description: SourceDescription,
    /// The pipeline saying an item ended or failed. Closed unless somebody called
    /// [`SessionManager::with_playback_ends`] — see there for why it is a receiver the manager
    /// owns rather than another `SessionEvent`.
    ended: tokio::sync::mpsc::Receiver<PlaybackEnd>,
}

/// A shared view of the active source's control surface.
///
/// This exists because the manager's own `remote()` was unreachable by construction:
/// `run(self, ..)` consumes the manager, so once the receiver is running there is no
/// `&self` left to ask. Every protocol that published a `ControlSurface` — Spotify's
/// whole `control` module, and AVRCP's — was therefore dead at runtime, and the panel
/// could not drive playback no matter what it advertised.
///
/// Handed out before `run` takes ownership, so a touch handler or an overlay can hold one
/// for the life of the process and always see whoever is currently playing.
#[derive(Clone, Default)]
pub struct RemoteHandle(Arc<std::sync::Mutex<Option<Arc<dyn RemoteControl>>>>);

impl std::fmt::Debug for RemoteHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteHandle")
            .field("present", &self.get().is_some())
            .finish()
    }
}

impl RemoteHandle {
    /// The active source's control surface, if it published one.
    ///
    /// Returns an owned handle rather than a borrow: the caller is on another thread and
    /// the session it belongs to may end while they are using it. A `RemoteControl` that
    /// outlives its session answers with a typed error, which is the honest outcome.
    #[must_use]
    pub fn get(&self) -> Option<Arc<dyn RemoteControl>> {
        self.0.lock().ok()?.clone()
    }

    /// Publish (or clear) the control surface. A poisoned lock is not worth failing an
    /// event over — the next publish recovers it.
    fn set(&self, remote: Option<Arc<dyn RemoteControl>>) {
        if let Ok(mut held) = self.0.lock() {
            *held = remote;
        }
    }
}

impl<P: Pipeline> SessionManager<P> {
    /// Build a session manager over `pipeline`, optionally controlling a display.
    pub fn new(
        pipeline: P,
        display: Option<Box<dyn DisplayControl>>,
        config: SessionConfig,
    ) -> Self {
        // A channel whose sender is dropped on the spot: `recv()` then answers `None`
        // immediately and forever, which `select!` reads as a dead branch. A manager
        // nobody wired a pipeline-end channel into therefore behaves exactly as it did
        // before there was one, with no `Option` to unwrap on the hot path.
        let (_closed, ended) = tokio::sync::mpsc::channel(1);
        Self {
            pipeline,
            display,
            osd: None,
            config,
            active: None,
            remote: RemoteHandle::default(),
            touch: crate::touch::TouchHandle::new(),
            description: SourceDescription::new(),
            ended,
        }
    }

    /// Listen for the pipeline saying that the item it was given ended or failed.
    ///
    /// Separate from the [`SourceMessage`] stream on purpose. That stream is tagged with
    /// the source that produced the event, and the pipeline has no source: it is the one
    /// party in the system that knows a fetch failed and cannot say on whose behalf. The
    /// manager supplies the missing half — it is the only thing that knows which source is
    /// currently active — so the routing lives here rather than in the caller.
    ///
    /// Pair it with [`crate::playback::end_channel`], whose sender goes to the pipeline.
    #[must_use]
    pub fn with_playback_ends(mut self, ends: tokio::sync::mpsc::Receiver<PlaybackEnd>) -> Self {
        self.ended = ends;
        self
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
    pub fn remote(&self) -> Option<Arc<dyn RemoteControl>> {
        self.remote.get()
    }

    /// A handle onto the active source's control surface that outlives [`Self::run`].
    ///
    /// Take this *before* starting the manager: `run` consumes `self`, so this is the
    /// only way anything else can reach the remote once the receiver is up.
    #[must_use]
    pub fn remote_handle(&self) -> RemoteHandle {
        self.remote.clone()
    }

    /// A handle onto the active source's touch surface, for whatever owns the panel.
    ///
    /// Same rule as [`Self::remote_handle`]: take it before `run` consumes `self`.
    #[must_use]
    pub fn touch_handle(&self) -> crate::touch::TouchHandle {
        self.touch.clone()
    }

    /// Consume the event stream until it closes, arbitrating sources.
    ///
    /// Also drains whatever the pipeline says about the item in flight, if anything was
    /// wired to [`Self::with_playback_ends`]. Both are handled by the same task because both
    /// mutate the same active-source state, and a lock around it would be a lock the
    /// actor model exists to avoid.
    pub async fn run(mut self, mut rx: tokio::sync::mpsc::Receiver<SourceMessage>) {
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    if let Err(e) = self.handle(msg).await {
                        warn!(error = %e, "session manager dropped an event");
                    }
                }
                // A dropped sender makes this `None` forever, which disables the branch
                // rather than spinning on it — see `with_playback_ends`.
                Some(end) = self.ended.recv() => {
                    self.media_ended(end).await;
                }
            }
        }
        // Stream closed: best-effort teardown.
        let _ = self.pipeline.stop().await;
    }

    /// The pipeline finished, or failed to play, whatever the active source handed it.
    ///
    /// Tells that source first and ends the session second, and the order is the point:
    /// the source's own protocol state has to be corrected *before* its control surface is
    /// dropped, or a DLNA control point is left polling a transport that still says
    /// PLAYING with nobody able to say otherwise.
    async fn media_ended(&mut self, end: PlaybackEnd) {
        let Some(source) = self.active.clone() else {
            // Nothing is active, so this is the tail of a session that already ended —
            // a decode thread noticing on its own schedule that it was torn down.
            debug!(%end, "session: media ended with no active source");
            return;
        };
        info!(%source, %end, "session: the pipeline finished with the item");
        if let Some(remote) = self.remote.get() {
            if let Err(e) = remote.media_ended(end).await {
                debug!(%source, error = %e, "session: the source would not take the end report");
            }
        }
        if let Err(e) = self
            .handle(SourceMessage {
                source,
                event: SessionEvent::End,
            })
            .await
        {
            warn!(error = %e, "session: could not end the session cleanly");
        }
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
                config,
            } => {
                self.begin_session(&source).await?;
                info!(%source, %format, "session: audio");
                self.pipeline.play_audio(frames, format, config).await
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
                    let caps = remote.capabilities();
                    info!(%source, ?caps, "session: control surface up");
                    self.remote.set(Some(remote));
                    // The surface hears about this too: it is what decides whether the
                    // panel draws transport controls, and which ones.
                    self.pipeline.controls(caps).await
                } else {
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::TouchSurface(surface) => {
                if self.active.as_ref() == Some(&source) {
                    info!(%source, "session: touch surface up");
                    self.touch.set(Some(surface));
                    Ok(())
                } else {
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::TouchSurfaceRevoked => {
                if self.active.as_ref() == Some(&source) {
                    info!(%source, "session: touch surface withdrawn");
                    self.touch.set(None);
                    Ok(())
                } else {
                    // A source that is not active has no surface installed to withdraw,
                    // and saying so is better than clearing the *active* source's.
                    Err(CoreError::NoActiveSession(source.to_string()))
                }
            }
            SessionEvent::HostPage(page) => {
                self.begin_session(&source).await?;
                info!(%source, url = %page.url, title = %page.title, "session: hosting a page");
                self.pipeline.host_page(page).await
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
                    self.remote.set(None);
                    // …and the glass with them. A router still pointed at a session that
                    // ended sends touches into a closed socket at best, and at worst into
                    // a session the person on the panel can no longer see.
                    self.touch.set(None);
                    // Controls go with the session that published them. Leaving them on
                    // screen would offer buttons wired to a peer that has gone.
                    let _ = self.pipeline.controls(ControlCapabilities::NONE).await;
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
            // Ask the outgoing source to stop *itself* before we stop rendering it.
            // Dropping its frames is not the same as it having stopped: a phone that is
            // still streaming keeps its encoder running and keeps burning radio time
            // against the source that just won, and when the winner ends, the loser's
            // audio reappears from wherever it got to. Best-effort — a peer that never
            // advertised Pause refuses, which is exactly what `issue` is for.
            if let Some(remote) = self.remote.get() {
                if let Err(e) = remote.issue(ControlTxn::Pause).await {
                    debug!(%prev, error = %e, "session: preempted source would not pause");
                }
            }
            self.pipeline.stop().await.ok();
            // The outgoing source's control handle, touch surface and description die
            // with its session — the new source publishes its own if it has any.
            self.remote.set(None);
            self.touch.set(None);
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
        controls: std::sync::Mutex<Option<ControlCapabilities>>,
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
            _a: Option<crate::event::MirrorAudio>,
        ) -> Result<(), CoreError> {
            Ok(())
        }
        async fn play_audio(
            &self,
            _s: crate::types::FrameSource,
            format: crate::types::AudioFormat,
            _config: Option<bytes::Bytes>,
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
        async fn controls(&self, capabilities: ControlCapabilities) -> Result<(), CoreError> {
            *self.0.controls.lock().expect("poisoned") = Some(capabilities);
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
        ends: std::sync::Mutex<Vec<crate::playback::PlaybackEnd>>,
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
        async fn media_ended(&self, end: crate::playback::PlaybackEnd) -> Result<(), CoreError> {
            self.ends.lock().expect("poisoned").push(end);
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
                config: None,
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
        // #70: the negotiated rate must survive the trip to the pipeline. Losing it here
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
                ends: std::sync::Mutex::new(Vec::new()),
            })),
        })
        .await
        .unwrap();

        let remote = mgr.remote().expect("published");
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

    /// A touch surface goes up, comes down when the source withdraws it, and does not
    /// come down at somebody else's request.
    ///
    /// The revoke is its own event rather than a session end because the source keeps
    /// streaming — a Miracast source sending `wfd_uibc_setting: disable` is closing an
    /// input channel, not a session (#193). Without it the surface lives until the
    /// session does, which is the wrong lifetime: the panel goes on delivering touch to a
    /// source that has said it is not listening.
    #[tokio::test]
    async fn a_withdrawn_touch_surface_comes_down_without_ending_the_session() {
        struct Surface;
        impl crate::touch::TouchSurface for Surface {
            fn touch(&self, _touch: crate::touch::SurfaceTouch) {}
            fn cancel_all(&self) {}
        }

        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Miracast, "a");
        mgr.handle(audio_msg(&a)).await.unwrap();
        assert!(
            mgr.touch_handle().get().is_none(),
            "nothing before UIBC opens"
        );

        mgr.handle(SourceMessage {
            source: a.clone(),
            event: SessionEvent::TouchSurface(Arc::new(Surface)),
        })
        .await
        .unwrap();
        assert!(
            mgr.touch_handle().get().is_some(),
            "the surface is routable"
        );

        // A *different* source cannot take it down. Otherwise a backgrounded session
        // could blind the active one's glass, which is the same class of hijack
        // `metadata_from_a_backgrounded_source_cannot_hijack_the_card` guards.
        let b = SourceId::new(ProtocolKind::Miracast, "b");
        assert!(mgr
            .handle(SourceMessage {
                source: b,
                event: SessionEvent::TouchSurfaceRevoked,
            })
            .await
            .is_err());
        assert!(
            mgr.touch_handle().get().is_some(),
            "a source that is not active must not clear the active one's surface"
        );

        // The source that put it up takes it down, and the session survives.
        mgr.handle(SourceMessage {
            source: a,
            event: SessionEvent::TouchSurfaceRevoked,
        })
        .await
        .unwrap();
        assert!(mgr.touch_handle().get().is_none(), "withdrawn");
        assert!(
            mgr.active().is_some(),
            "an input channel closed, not a session"
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
    async fn preemption_asks_the_outgoing_source_to_stop_sending() {
        // Dropping a source's frames is not the same as that source having stopped. A
        // preempted phone keeps its encoder running, keeps burning radio time against the
        // source that just won, and reappears mid-track when the winner ends. So the
        // outgoing sender is told, over its own protocol, before we stop rendering it.
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Bluetooth, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        let remote = Arc::new(FakeRemote {
            caps: ControlCapabilities::TRANSPORT,
            sent: std::sync::Mutex::new(Vec::new()),
            ends: std::sync::Mutex::new(Vec::new()),
        });
        mgr.handle(audio_msg(&a)).await.unwrap();
        mgr.handle(SourceMessage {
            source: a,
            event: SessionEvent::ControlSurface(remote.clone()),
        })
        .await
        .unwrap();

        mgr.handle(play_msg(&b)).await.unwrap();
        assert_eq!(
            *remote.sent.lock().unwrap(),
            vec![ControlTxn::Pause],
            "the preempted source should have been told to pause"
        );
    }

    #[tokio::test]
    async fn a_source_that_cannot_pause_does_not_block_the_preemption() {
        // Best-effort: a peer that never advertised Pause refuses, and the new source
        // still takes the screen. A preemption that failed because the *loser* said no
        // would be the worst possible arbitration policy.
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        let a = SourceId::new(ProtocolKind::Bluetooth, "a");
        let b = SourceId::new(ProtocolKind::Cast, "b");
        mgr.handle(audio_msg(&a)).await.unwrap();
        mgr.handle(SourceMessage {
            source: a,
            event: SessionEvent::ControlSurface(Arc::new(FakeRemote {
                caps: ControlCapabilities::NONE,
                sent: std::sync::Mutex::new(Vec::new()),
                ends: std::sync::Mutex::new(Vec::new()),
            })),
        })
        .await
        .unwrap();
        mgr.handle(play_msg(&b)).await.unwrap();
        assert_eq!(mgr.active(), Some(&b));
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

    /// The half of a media-URL session nothing could report: the pipeline knowing the item
    /// is over. A DLNA control point polling `GetTransportInfo` was told PLAYING forever
    /// and a queued playlist waiting on the item to end waited for the life of the
    /// process, so the source that pushed the URL has to be told before its handle is
    /// dropped.
    #[tokio::test]
    async fn a_finished_item_reaches_the_source_that_pushed_it_and_ends_the_session() {
        let counts = Arc::new(Counts::default());
        let (ends, end_rx) = crate::playback::end_channel();
        let mgr = SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default())
            .with_playback_ends(end_rx);
        let remote = Arc::new(FakeRemote {
            caps: ControlCapabilities::PLAY,
            sent: std::sync::Mutex::new(Vec::new()),
            ends: std::sync::Mutex::new(Vec::new()),
        });

        let a = SourceId::new(ProtocolKind::Dlna, "a");
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        tx.send(play_msg(&a)).await.unwrap();
        tx.send(SourceMessage {
            source: a,
            event: SessionEvent::ControlSurface(remote.clone()),
        })
        .await
        .unwrap();
        let running = tokio::spawn(mgr.run(rx));

        // The two inputs are separate channels, so the end has to be sent *after* the
        // surface has demonstrably been taken up — `controls()` is what the manager calls
        // when it accepts one. In production the ordering is not in doubt: the surface is
        // published behind the `Play` and decode ends much later.
        settle(|| counts.controls.lock().expect("poisoned").is_some()).await;

        ends.send(PlaybackEnd::Failed("connection refused".into()))
            .await
            .unwrap();
        settle(|| !remote.ends.lock().expect("poisoned").is_empty()).await;

        assert_eq!(
            &*remote.ends.lock().unwrap(),
            &[PlaybackEnd::Failed("connection refused".into())],
            "the control point has to be able to stop saying PLAYING",
        );
        // …and the session ended with it, rather than leaving a card up over nothing.
        settle(|| counts.stop.load(Ordering::SeqCst) >= 1).await;

        drop(tx);
        drop(ends);
        running.await.unwrap();
    }

    /// Wait for an actor on another task to have got somewhere, or fail the test.
    ///
    /// A bounded spin rather than a fixed sleep: the manager runs as its own task, so
    /// "has it handled that yet" has no synchronous answer, and a sleep long enough to be
    /// reliable on a loaded CI box is long enough to be a waste on every other run.
    async fn settle(mut done: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("the session manager never got there");
    }

    /// A decode thread noticing on its own schedule that it was torn down arrives after
    /// the session it belonged to is gone. Ending "the active session" then would tear
    /// down whoever took the screen in the meantime.
    #[tokio::test]
    async fn an_end_with_nothing_active_is_ignored() {
        let counts = Arc::new(Counts::default());
        let mut mgr =
            SessionManager::new(FakePipeline(counts.clone()), None, SessionConfig::default());
        // Straight at the handler rather than through `run`: the thing under test is the
        // guard, and routing it through a task would make "nothing happened" a race
        // rather than an assertion.
        mgr.media_ended(PlaybackEnd::Finished).await;

        assert!(mgr.active().is_none());
        assert_eq!(counts.stop.load(Ordering::SeqCst), 0);
    }
}
