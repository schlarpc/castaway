//! The real [`Pipeline`]: wires a `Play(url)` to decode → GPU compositor → present, and
//! forwards decoded mirror frames straight to the compositor. Threading follows
//! architecture §6: the compositor/GPU lives on ONE render thread (the [`RenderLoop`],
//! driven by the kiosk's winit loop or, in tests, pumped directly); decode runs on its
//! own blocking thread; this [`RenderPipeline`] is the tokio-side handle that connects
//! them over a bounded channel that **drops frames when full** (latency > freshness).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use castaway_core::{
    ControlTxn, CoreError, DecodedFrame, FrameImage, FrameSource, MediaUri, Pipeline, PixelFormat,
    PlaybackEnd, PlaybackProgress, PlaybackReport,
};
use tracing::{debug, error, info, warn};

use crate::compositor::{Compositor, DirtyRect, Layer, LayerId, Transform};
use crate::error::PipelineError;
use crate::hwaccel::HwPreference;
use crate::wgpu_compositor::{TexelFormat, WgpuCompositor};

/// A command sent from the tokio/decode side to the render thread. (OSD is a separate
/// channel — see [`castaway_core::osd`] / [`crate::osd`] — so any source can post it.)
pub enum RenderCommand {
    /// Upload a decoded frame as the video layer.
    Video(DecodedFrame),
    /// Drop the video layer (playback stopped).
    ClearVideo,
    /// Show or update the now-playing card. Carries the metadata rather than pixels: a
    /// 4K RGBA buffer is 33 MB and this is a few hundred bytes that reproduce it.
    NowPlaying(Box<crate::nowplaying_card::NowPlayingCard>),
    /// Drop the card (the session ended).
    ClearNowPlaying,
    /// Attach a consumer of composited frames (Q30). Sent as a command rather than set
    /// on the loop directly because the loop lives on the main thread and everything
    /// that wants to tap it does not.
    AddTap(Box<dyn crate::tap::OutputTap>),
    /// The URL that was opened turned out to be audio-only, with whatever the container
    /// tags said about it. The surface answers with a now-playing card rather than a
    /// black screen over music.
    #[cfg(feature = "ffmpeg")]
    UrlAudioOnly(Box<crate::ffmpeg_decode::MediaLayout>),
}

/// Asks the render thread to capture what it is showing.
#[derive(Clone)]
pub struct ScreenshotHandle {
    tx: SyncSender<RenderCommand>,
}

impl std::fmt::Debug for ScreenshotHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenshotHandle").finish_non_exhaustive()
    }
}

impl ScreenshotHandle {
    /// Capture the next composited frame as a PNG.
    ///
    /// Blocking, with a deadline: the answer arrives on the render thread's own schedule,
    /// and a panel that has stopped presenting — a wedged compositor, a build with no
    /// renderer — must fail the request rather than hang the caller forever.
    ///
    /// # Errors
    /// [`PipelineError::Surface`] if the render thread is gone or does not answer in
    /// time, or whatever the capture itself failed with.
    pub fn capture(&self, timeout: Duration) -> Result<Vec<u8>, PipelineError> {
        let (tap, rx) = crate::tap::ScreenshotTap::new();
        self.tx
            .try_send(RenderCommand::AddTap(Box::new(tap)))
            .map_err(|_| {
                PipelineError::Surface("the render thread is not accepting work".into())
            })?;
        rx.recv_timeout(timeout).map_err(|_| {
            PipelineError::Surface("the render thread did not present in time".into())
        })?
    }
}

/// The URL session in flight: where it has got to, and how long it turns out to be.
///
/// One value rather than two fields on the pipeline because they are only ever read
/// together, and because they are established at different moments by different threads —
/// the clock at `play`, the length once the decode thread has opened the container — so
/// keeping them adjacent is what stops a stale length being reported against a fresh
/// clock.
struct UrlPlayback {
    clock: Arc<crate::clock::MediaClock>,
    /// Where a `Seek` is left for the decode thread to pick up.
    seek: Arc<crate::seek::SeekControl>,
    /// Filled in from the container, on the decode thread, before the first frame.
    /// [`None`] for a live stream, which has no end and must not be given one.
    duration: Arc<Mutex<Option<Duration>>>,
}

/// Where the pipeline reports that an item finished or failed, and the guard that keeps a
/// late decode thread from ending somebody else's session.
///
/// The guard is not paranoia. A decode thread checks its stop flag and *then* reports;
/// between those two instants another source can take the screen, and an unguarded report
/// would tear down the session that just started — a cast that ends itself for no visible
/// reason, at random, once in a while. So each session takes a ticket and every preemption
/// moves the counter on.
struct EndReport {
    tx: tokio::sync::mpsc::Sender<PlaybackEnd>,
    current: AtomicU64,
}

impl EndReport {
    /// The ticket the session starting now should quote when it ends.
    fn ticket(&self) -> u64 {
        self.current.load(Ordering::SeqCst)
    }

    /// Retire the current ticket, so whoever holds it can no longer end a session.
    fn invalidate(&self) {
        self.current.fetch_add(1, Ordering::SeqCst);
    }

    /// Report an end, if the reporting session is still the current one.
    fn report(&self, ticket: u64, end: PlaybackEnd) {
        if self.ticket() != ticket {
            debug!(
                ?end,
                "playback end from a session that has already been replaced"
            );
            return;
        }
        // `try_send` rather than `blocking_send`: this runs on a decode thread, ends are
        // one per session against a channel with room for several, and a library crate
        // must not have a panicking send on a runtime-reachable path (ground rule 7).
        if let Err(e) = self.tx.try_send(end) {
            warn!(error = %e, "nothing took the end-of-media report");
        }
    }
}

/// Reads where the URL session has got to, without owning the pipeline that plays it.
///
/// Exists for the same reason [`ScreenshotHandle`] does: by the time an adapter wants to
/// answer "how far through is this", the pipeline has been moved into the session manager
/// and there is no `&self` left anywhere. A DLNA control point polls `GetPositionInfo`
/// about once a second for the whole item, so this has to be cheap and it has to be
/// reachable from an HTTP handler.
#[derive(Clone)]
pub struct PlaybackHandle {
    playback: Arc<Mutex<Option<UrlPlayback>>>,
}

impl std::fmt::Debug for PlaybackHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackHandle")
            .field("playing", &self.progress().is_some())
            .finish()
    }
}

impl PlaybackReport for PlaybackHandle {
    fn progress(&self) -> Option<PlaybackProgress> {
        let held = self.playback.lock().ok()?;
        let session = held.as_ref()?;
        // `None` before the first frame or the first audio block, and that is the honest
        // answer: a control point asking during the fetch should be told nothing rather
        // than zero, which it would draw as "at the start" of an item that has not begun.
        let position = session.clock.now()?;
        let duration = session.duration.lock().ok().and_then(|d| *d);
        Some(PlaybackProgress { position, duration })
    }
}

/// The tokio-side pipeline handle. Implements [`Pipeline`]; owns decode threads and the
/// sender half of the render channel.
pub struct RenderPipeline {
    tx: SyncSender<RenderCommand>,
    /// Stop flag for the currently-running decode thread, so a new `Play`/`stop`
    /// preempts the old one.
    active: Mutex<Option<Arc<AtomicBool>>>,
    /// Whether decode threads may use hardware decode. A *runtime* setting, never a
    /// compile-time one — see [`crate::hwaccel`].
    hw: HwPreference,
    /// The card as last sent. Held because its two halves arrive on separate calls and
    /// each render needs both — not as a cache for someone else to read.
    card: Mutex<crate::nowplaying_card::NowPlayingCard>,
    /// The URL session in flight, so transport control and the position readout can both
    /// reach it.
    ///
    /// `Pause` on this path is not a message to a sender — we *are* the player — so it
    /// is applied by freezing the clock, which halts the video thread and the audio
    /// thread and, through the bounded queue between them, the demuxer as well.
    ///
    /// An `Arc` because [`PlaybackHandle`] shares it: the pipeline is moved into the
    /// session manager and the adapter that needs the position is not.
    playback: Arc<Mutex<Option<UrlPlayback>>>,
    /// Where an item that finished or failed gets reported, if anything asked to hear.
    ends: Mutex<Option<Arc<EndReport>>>,
    /// Called when another source takes the screen, so a browser that is covering the
    /// panel gives it back.
    ///
    /// A callback rather than a `BrowserCommand` sender because the browser lives behind
    /// the `cef` feature and the pipeline should not: the pipeline's concern is "somebody
    /// else is casting now", and what that means for a browser is the app's business.
    release_screen: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Output gain, shared with whichever audio session is live.
    ///
    /// Owned by the pipeline rather than by a session because it has to outlive them:
    /// the panel has one volume, and a level set over AVRCP should still be the level
    /// when the next source starts.
    #[cfg(feature = "audio")]
    gain: Arc<crate::audio_session::Gain>,
}

impl RenderPipeline {
    /// Create the pipeline and the receiver the [`RenderLoop`] consumes. `depth` bounds
    /// the in-flight frame queue; when full, new frames are dropped (drop-late).
    ///
    /// Hardware decode is attempted when the build and the box support it; see
    /// [`Self::with_hw_preference`] to pin it either way.
    #[must_use]
    pub fn new(depth: usize) -> (Self, Receiver<RenderCommand>) {
        let (tx, rx) = sync_channel(depth.max(1));
        (
            Self {
                tx,
                active: Mutex::new(None),
                hw: HwPreference::Auto,
                card: Mutex::new(crate::nowplaying_card::NowPlayingCard::default()),
                playback: Arc::new(Mutex::new(None)),
                ends: Mutex::new(None),
                release_screen: Mutex::new(None),
                #[cfg(feature = "audio")]
                gain: Arc::new(crate::audio_session::Gain::default()),
            },
            rx,
        )
    }

    /// A handle for asking the render thread for a screenshot.
    ///
    /// Cheap and clonable, and deliberately separate from the pipeline itself: by the
    /// time anything wants a screenshot the pipeline has been moved into the session
    /// manager, and an HTTP handler has no business holding that.
    #[must_use]
    pub fn screenshot_handle(&self) -> ScreenshotHandle {
        ScreenshotHandle {
            tx: self.tx.clone(),
        }
    }

    /// A reader of the URL session's position and length.
    ///
    /// Taken before the pipeline is handed to the session manager, for the same reason as
    /// [`Self::screenshot_handle`]: an adapter that has to answer "how far through is
    /// this" has no business owning a pipeline, and after the move there is nothing to
    /// ask anyway.
    #[must_use]
    pub fn playback_handle(&self) -> PlaybackHandle {
        PlaybackHandle {
            playback: Arc::clone(&self.playback),
        }
    }

    /// Report finished and failed items to `tx`.
    ///
    /// Without this the decode thread logged and exited and told nobody, so a DLNA
    /// control point went on reading PLAYING / OK for a URL the box could not fetch, and a
    /// queued playlist waiting for the item to end waited for the life of the process.
    pub fn set_playback_ends(&self, tx: tokio::sync::mpsc::Sender<PlaybackEnd>) {
        if let Ok(mut held) = self.ends.lock() {
            *held = Some(Arc::new(EndReport {
                tx,
                current: AtomicU64::new(0),
            }));
        }
    }

    fn end_report(&self) -> Option<Arc<EndReport>> {
        self.ends.lock().ok().and_then(|held| held.clone())
    }

    /// The card as it currently stands. For tests and diagnostics.
    #[must_use]
    pub fn card(&self) -> crate::nowplaying_card::NowPlayingCard {
        self.card.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Update one half of the card and publish the whole thing.
    ///
    /// Both halves arrive on separate calls — metadata per track, the device once per
    /// session — and the surface needs both, so each update re-sends the pair rather than
    /// the piece that changed.
    fn publish_card(&self, edit: impl FnOnce(&mut crate::nowplaying_card::NowPlayingCard)) {
        let Ok(mut guard) = self.card.lock() else {
            return;
        };
        edit(&mut guard);
        let _ = self
            .tx
            .try_send(RenderCommand::NowPlaying(Box::new(guard.clone())));
    }

    /// Pin the hardware-decode choice.
    ///
    /// [`HwPreference::HardwareOnly`] is the useful one for diagnosis: it turns a silent
    /// downgrade into a hard error, which is the only way to notice that hwaccel stopped
    /// working — everything still plays without it, just on the CPU.
    #[must_use]
    pub const fn with_hw_preference(mut self, preference: HwPreference) -> Self {
        self.hw = preference;
        self
    }

    /// Register what to do when another source takes the screen.
    ///
    /// The bug this closes: `BrowserCommand` was produced from exactly one place, the
    /// DIAL launch handler, and nothing else ever sent `Hide` — while DIAL `DELETE`, the
    /// only thing that did, is something no real sender sends (D28). So the first YouTube
    /// cast owned the panel permanently: a later DLNA or Cast video decoded and
    /// composited *underneath* an opaque leanback page at z=5, and the attract scene
    /// never came back.
    pub fn set_screen_release(&self, release: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut held) = self.release_screen.lock() {
            *held = Some(release);
        }
    }

    /// Hand the panel back from whatever is covering it, if anything is.
    fn release_screen(&self) {
        let release = self
            .release_screen
            .lock()
            .ok()
            .and_then(|held| held.clone());
        if let Some(release) = release {
            release();
        }
    }

    fn preempt(&self) {
        if let Ok(mut guard) = self.active.lock() {
            if let Some(flag) = guard.take() {
                flag.store(true, Ordering::SeqCst);
            }
        }
        // Whatever decode thread is still alive belongs to a session that is over, so
        // retire its ticket: a thread that had already passed its stop-flag check must
        // not end the session that is taking the screen right now.
        if let Some(report) = self.end_report() {
            report.invalidate();
        }
    }

    fn set_active(&self, flag: Arc<AtomicBool>) {
        if let Ok(mut guard) = self.active.lock() {
            *guard = Some(flag);
        }
    }
}

#[async_trait]
impl Pipeline for RenderPipeline {
    async fn play(&self, source: MediaUri, start: Option<Duration>) -> Result<(), CoreError> {
        self.release_screen();
        self.preempt();
        let stop = Arc::new(AtomicBool::new(false));
        self.set_active(stop.clone());
        info!(%source, ?start, "render pipeline: PLAY (decode → compositor)");

        let tx = self.tx.clone();
        let uri = source.to_string();
        let hw = self.hw;

        // The session's clock, and the audio sink that drives it. Both live here rather
        // than inside the decoder because only this type owns the gain and the output —
        // and because a build with no audio feature must still compile to *something*
        // that plays the picture.
        let clock = Arc::new(crate::clock::MediaClock::new());
        let seek = Arc::new(crate::seek::SeekControl::new());
        // A start offset is a seek that happens before the first frame. Cast `LOAD` and
        // AirPlay both carry one — "resume where I was" — and it used to be accepted and
        // then quietly ignored, so resuming a film restarted it.
        if let Some(start) = start.filter(|s| !s.is_zero()) {
            seek.request(start);
        }
        // Empty until the container has been opened, which happens on the decode thread.
        // A control point polling in the meantime is told the length is unknown, which is
        // true, rather than zero, which it would draw as a bar with no room in it.
        let duration = Arc::new(Mutex::new(None));
        if let Ok(mut held) = self.playback.lock() {
            *held = Some(UrlPlayback {
                clock: Arc::clone(&clock),
                seek: Arc::clone(&seek),
                duration: Arc::clone(&duration),
            });
        }
        // Taken after `preempt`, so it is this session's number and not the outgoing
        // session's.
        let ends = self.end_report().map(|r| {
            let ticket = r.ticket();
            (r, ticket)
        });
        #[cfg(all(feature = "ffmpeg", feature = "audio"))]
        let audio_tx = {
            let (atx, arx) = std::sync::mpsc::sync_channel(crate::ffmpeg_decode::AUDIO_QUEUE);
            crate::audio_session::spawn_pcm(
                arx,
                crate::audio_session::default_output(),
                Arc::clone(&stop),
                Arc::clone(&self.gain),
                Some(crate::audio_session::PacedSession {
                    clock: Arc::clone(&clock),
                    seek: Arc::clone(&seek),
                }),
            );
            Some(atx)
        };
        #[cfg(not(all(feature = "ffmpeg", feature = "audio")))]
        let audio_tx = {
            // Said once per session rather than never: a build without the feature plays
            // video silently, and silence that nobody announced is the failure mode this
            // whole path was suffering from in the first place.
            warn!(
                "this build has no audio support, so playback will be silent; rebuild \
                 with the `audio` feature"
            );
            None
        };

        // Decode is blocking + thread-affine → dedicated OS thread, never the runtime.
        std::thread::spawn(move || {
            let result = decode_into(&uri, hw, &tx, &stop, &clock, &seek, &duration, audio_tx);

            // Preemption is not completion. When another source has taken the screen the
            // stop flag is what ended this decode, and the layers on screen belong to
            // whoever took it — clearing them here would blank the new session.
            if stop.load(Ordering::SeqCst) {
                debug!(%uri, "decode ended: preempted");
                return;
            }

            let end = match result {
                Ok(()) => {
                    info!(%uri, "decode ended: media finished");
                    PlaybackEnd::Finished
                }
                // The URL was unreachable, the server refused it, or it held nothing this
                // build can decode. Named at `warn` because it is the whole explanation
                // for a panel that accepted a cast and showed nothing.
                Err(e) => {
                    warn!(%uri, error = %e, "decode ended: playback failed");
                    PlaybackEnd::Failed(e.to_string())
                }
            };

            // Either way the item is over, so the screen goes back to idle. Without this
            // the last decoded frame stayed frozen on a two-metre panel indefinitely, and
            // a failed fetch left the attract scene up with nothing saying why.
            let _ = tx.try_send(RenderCommand::ClearVideo);
            let _ = tx.try_send(RenderCommand::ClearNowPlaying);

            // …and tell whoever pushed the URL. Clearing the screen is what the room sees;
            // this is what the phone sees, and without it a control point read PLAYING / OK
            // forever and a queued playlist never advanced past the first track.
            if let Some((report, ticket)) = ends {
                report.report(ticket, end);
            }
        });
        Ok(())
    }

    async fn mirror(
        &self,
        video: FrameSource,
        audio: Option<castaway_core::MirrorAudio>,
    ) -> Result<(), CoreError> {
        self.release_screen();
        self.preempt();
        match video {
            // A mirror session is pixels by definition. PCM reaching here means an
            // adapter routed an audio-only source down the video path, which would
            // otherwise show as a black screen rather than as the wiring mistake it is.
            FrameSource::Pcm(_) => Err(CoreError::Pipeline(
                "a mirror session cannot be PCM audio; use play_audio".into(),
            )),
            FrameSource::Url(uri) => self.play(uri, None).await,
            FrameSource::Decoded(mut rx) => {
                info!("render pipeline: MIRROR (decoded frames → compositor)");
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    while let Some(frame) = rx.recv().await {
                        // Drop on full — live mirroring favors latency over freshness.
                        if let Err(TrySendError::Disconnected(_)) =
                            tx.try_send(RenderCommand::Video(frame))
                        {
                            break;
                        }
                    }
                });
                Ok(())
            }
            FrameSource::Encoded(rx) => {
                info!("render pipeline: MIRROR (encoded frames → decode → compositor)");
                let stop = Arc::new(AtomicBool::new(false));
                self.set_active(stop.clone());
                // The audio half shares the video's stop flag, because it is the same
                // session: ending one has to end the other. It also deliberately does
                // *not* go through `play_audio`, which preempts — a mirror announcing
                // its audio that way would tear down its own picture.
                #[cfg(feature = "audio")]
                if let Some(audio) = audio {
                    if let FrameSource::Encoded(arx) = audio.source {
                        info!(format = %audio.format, "render pipeline: MIRROR audio");
                        crate::audio_session::spawn(
                            arx,
                            audio.format,
                            audio.config,
                            crate::audio_session::default_output(),
                            Arc::clone(&stop),
                            Arc::clone(&self.gain),
                        );
                    } else {
                        warn!("mirror audio is not encoded frames; ignoring it");
                    }
                }
                #[cfg(not(feature = "audio"))]
                let _ = audio;
                let tx = self.tx.clone();
                let hw = self.hw;
                // Same reasoning as `play`: decode blocks, so it gets an OS thread of its
                // own rather than a runtime worker.
                std::thread::spawn(move || {
                    if let Err(e) = decode_mirror(rx, hw, &tx, &stop) {
                        warn!(error = %e, "mirror decode ended with error");
                    }
                });
                Ok(())
            }
        }
    }

    async fn play_audio(
        &self,
        source: FrameSource,
        format: castaway_core::AudioFormat,
        config: Option<bytes::Bytes>,
    ) -> Result<(), CoreError> {
        #[cfg(feature = "audio")]
        {
            // A source that is only audio still takes the session, and a YouTube page
            // left on screen would keep playing its own sound over it — CEF's audio goes
            // straight to the system device, not through our mixer.
            self.release_screen();
            // Preempt first: the flag slot holds whichever session is live, video or
            // audio, because only one source may own the output at a time.
            self.preempt();
            let stop = Arc::new(AtomicBool::new(false));
            self.set_active(Arc::clone(&stop));
            match source {
                FrameSource::Encoded(rx) => {
                    crate::audio_session::spawn(
                        rx,
                        format,
                        config,
                        crate::audio_session::default_output(),
                        stop,
                        Arc::clone(&self.gain),
                    );
                    Ok(())
                }
                // Already decoded (Spotify): `format` is what the adapter negotiated, but
                // each block restates it, so the session takes it from the samples.
                FrameSource::Pcm(rx) => {
                    crate::audio_session::spawn_pcm(
                        rx,
                        crate::audio_session::default_output(),
                        stop,
                        Arc::clone(&self.gain),
                        // Bluetooth/Spotify PCM: the sender is the clock, there is no
                        // video to synchronise, and a seek is the phone's business — so
                        // there is no paced session to share.
                        None,
                    );
                    Ok(())
                }
                FrameSource::Url(_) | FrameSource::Decoded(_) => Err(CoreError::Pipeline(
                    "an audio session must arrive as encoded or PCM frames".into(),
                )),
            }
        }
        #[cfg(not(feature = "audio"))]
        {
            let _ = (source, format);
            // A typed refusal rather than a silent success: a build without the `audio`
            // feature has no decoder at all, and a phone that pairs, streams and plays
            // to silence is the worst possible thing to diagnose.
            Err(CoreError::Pipeline(
                "this build has no audio support; rebuild with the `audio` feature".into(),
            ))
        }
    }

    async fn now_playing(&self, snapshot: castaway_core::NowPlaying) -> Result<(), CoreError> {
        self.publish_card(|card| card.track = snapshot);
        Ok(())
    }

    async fn controls(
        &self,
        capabilities: castaway_core::ControlCapabilities,
    ) -> Result<(), CoreError> {
        info!(?capabilities, "render pipeline: CONTROLS");
        self.publish_card(|card| card.controls = capabilities);
        Ok(())
    }

    async fn up_next(&self, items: Vec<castaway_core::QueueItem>) -> Result<(), CoreError> {
        info!(queued = items.len(), "render pipeline: UP NEXT");
        self.publish_card(|card| card.up_next = items);
        Ok(())
    }

    async fn source_info(&self, source: castaway_core::SourceDescription) -> Result<(), CoreError> {
        // The device line above the track: who is connected, and over what.
        info!(%source, "render pipeline: SOURCE");
        self.publish_card(|card| card.source = source);
        Ok(())
    }

    async fn control(&self, txn: ControlTxn) -> Result<(), CoreError> {
        match txn {
            // Volume and mute land on the output gain. Everything a phone does with its
            // rocker arrives here, and it used to be logged and dropped — which is how a
            // receiver ends up pinned at full scale on a phone that has handed us
            // absolute-volume control and stopped attenuating locally.
            ControlTxn::Volume(level) => {
                #[cfg(feature = "audio")]
                {
                    self.gain.set(level);
                    info!(level = self.gain.level(), "render pipeline: volume");
                }
                #[cfg(not(feature = "audio"))]
                let _ = level;
                Ok(())
            }
            ControlTxn::Mute(muted) => {
                #[cfg(feature = "audio")]
                {
                    self.gain.set_muted(muted);
                    info!(muted, "render pipeline: mute");
                }
                #[cfg(not(feature = "audio"))]
                let _ = muted;
                Ok(())
            }
            // Pause and resume a URL session by freezing its clock. Everything downstream
            // is already waiting on it: the video thread holds each frame until its turn,
            // the audio thread stops feeding the device, and the demuxer stalls behind the
            // bounded queue between them. One flag stops the whole chain in step, which is
            // what makes resuming land where it left off rather than lurching.
            ControlTxn::Pause | ControlTxn::Play => {
                let paused = matches!(txn, ControlTxn::Pause);
                let held = self
                    .playback
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().map(|s| Arc::clone(&s.clock)));
                match held {
                    Some(clock) => {
                        clock.set_paused(paused);
                        info!(paused, "render pipeline: transport");
                        Ok(())
                    }
                    // Nothing is playing from a URL. Mirroring and audio-only sessions are
                    // driven by their sender, which pauses at its end, so there is nothing
                    // here to act on and saying so is better than a silent success.
                    None => Err(CoreError::UnsupportedControl(format!("{txn:?}"))),
                }
            }
            // Seek is left for the decode thread rather than done here, because moving a
            // demuxer is a blocking libav call and this is a runtime worker. Returning
            // once it is *requested* rather than once it has happened is also what the
            // caller wants: an AVTransport `Seek` is answered synchronously over HTTP, and
            // a control point that waited for a network seek to complete would time out.
            ControlTxn::Seek(position) => {
                let held = self
                    .playback
                    .lock()
                    .ok()
                    .and_then(|guard| guard.as_ref().map(|s| Arc::clone(&s.seek)));
                match held {
                    Some(seek) => {
                        seek.request(position);
                        info!(?position, "render pipeline: seek");
                        Ok(())
                    }
                    None => Err(CoreError::UnsupportedControl(format!("{txn:?}"))),
                }
            }
            // Stop tears the session down rather than merely freezing it. This used to
            // fall through to the refusal below while `proto-dlna` advertised STOP to the
            // panel and mapped the AVTransport `Stop` action onto it — so pressing stop,
            // on the phone or on the glass, moved the transport state to STOPPED and left
            // the video playing with sound, with both views then agreeing on a lie. The
            // only way out was to cast something else.
            ControlTxn::Stop => {
                self.preempt();
                if let Ok(mut held) = self.playback.lock() {
                    *held = None;
                }
                let _ = self.tx.try_send(RenderCommand::ClearVideo);
                if let Ok(mut guard) = self.card.lock() {
                    *guard = crate::nowplaying_card::NowPlayingCard::default();
                }
                let _ = self.tx.try_send(RenderCommand::ClearNowPlaying);
                info!("render pipeline: STOP (transport)");
                Ok(())
            }
            // Next, previous, shuffle, repeat, set-queue: a renderer handed one URL has no
            // playlist for any of them to move through. Refused rather than logged as a
            // success so a caller can tell the difference — and so the panel does not draw
            // a button that does nothing.
            other => {
                info!(?other, "render pipeline: CONTROL (unsupported)");
                Err(CoreError::UnsupportedControl(format!("{other:?}")))
            }
        }
    }

    async fn stop(&self) -> Result<(), CoreError> {
        self.preempt();
        // Release the clock with the session: a resumed pause on a session that has ended
        // would otherwise unfreeze threads that are already gone.
        if let Ok(mut held) = self.playback.lock() {
            *held = None;
        }
        let _ = self.tx.try_send(RenderCommand::ClearVideo);
        if let Ok(mut guard) = self.card.lock() {
            *guard = crate::nowplaying_card::NowPlayingCard::default();
        }
        let _ = self.tx.try_send(RenderCommand::ClearNowPlaying);
        info!("render pipeline: STOP");
        Ok(())
    }
}

/// Decode `uri` into render commands until EOF or `stop` is set.
#[allow(clippy::too_many_arguments)]
fn decode_into(
    uri: &str,
    hw: HwPreference,
    tx: &SyncSender<RenderCommand>,
    stop: &Arc<AtomicBool>,
    clock: &crate::clock::MediaClock,
    seek: &crate::seek::SeekControl,
    duration: &Mutex<Option<Duration>>,
    audio_tx: Option<std::sync::mpsc::SyncSender<castaway_core::PcmFrame>>,
) -> Result<(), PipelineError> {
    #[cfg(feature = "ffmpeg")]
    {
        crate::ffmpeg_decode::decode_av(
            uri,
            hw,
            clock,
            Some(seek),
            audio_tx,
            &|| stop.load(Ordering::SeqCst),
            |layout| {
                // How long the item is, as soon as anyone can know: the container is the
                // only party that has it, and a control point's progress bar is drawn from
                // it. Absent for a live stream, which is exactly the case that must not be
                // given a length.
                if let Ok(mut held) = duration.lock() {
                    *held = layout.duration;
                }
                // A file with no video is music, not a failure. Tell the surface so it
                // puts a card up instead of leaving the idle scene under silence — and
                // carry whatever the container's tags said, since a bare URL from Cast or
                // AirPlay brings no metadata of its own.
                if !layout.has_video {
                    let _ = tx.try_send(RenderCommand::UrlAudioOnly(Box::new(layout.clone())));
                }
            },
            |frame| {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                // Drop on full (bounded queue) but stop if the render loop is gone.
                !matches!(
                    tx.try_send(RenderCommand::Video(frame)),
                    Err(TrySendError::Disconnected(_))
                )
            },
        )
    }
    #[cfg(not(feature = "ffmpeg"))]
    {
        let _ = (uri, hw, tx, stop, clock, seek, duration, audio_tx);
        Err(PipelineError::Decode(
            "decode requires the `ffmpeg` feature".into(),
        ))
    }
}

/// Decode an encoded mirror stream into render commands until the adapter hangs up, the
/// render loop goes away, or `stop` is set.
///
/// The codec is only known once a frame has arrived — [`castaway_core::EncodedFrame`]
/// carries it per frame — so the first frame both chooses the decoder and starts it.
///
/// `stop` is observed between frames, so preempting a *silent* mirror does not end this
/// thread until the sender speaks again or drops the channel. That is fine for a live
/// mirror, which by definition keeps sending; the thread is parked on `blocking_recv`,
/// not spinning.
fn decode_mirror(
    #[allow(unused_mut)] mut rx: tokio::sync::mpsc::Receiver<castaway_core::EncodedFrame>,
    hw: HwPreference,
    tx: &SyncSender<RenderCommand>,
    stop: &Arc<AtomicBool>,
) -> Result<(), PipelineError> {
    #[cfg(feature = "ffmpeg")]
    {
        // This runs on a plain OS thread, never a runtime worker, so blocking here is
        // allowed — see the `std::thread::spawn` that calls us.
        let Some(first) = rx.blocking_recv() else {
            return Ok(());
        };
        let Some(codec) = first.video_codec else {
            return Err(PipelineError::Decode(
                "mirror stream delivered a frame with no video codec".into(),
            ));
        };

        let mut queued = Some(first);
        crate::ffmpeg_decode::decode_stream(
            codec,
            hw,
            || {
                if stop.load(Ordering::SeqCst) {
                    return None;
                }
                queued.take().or_else(|| rx.blocking_recv())
            },
            |frame| {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                // Drop on full (bounded queue) but stop if the render loop is gone.
                !matches!(
                    tx.try_send(RenderCommand::Video(frame)),
                    Err(TrySendError::Disconnected(_))
                )
            },
        )
    }
    #[cfg(not(feature = "ffmpeg"))]
    {
        let _ = (rx, hw, tx, stop);
        Err(PipelineError::Decode(
            "mirror decode requires the `ffmpeg` feature".into(),
        ))
    }
}

/// The render-thread side: owns the GPU compositor and applies render commands, then
/// presents. The kiosk's winit loop calls [`Self::pump`] each frame; tests call
/// [`Self::pump_blocking`].
pub struct RenderLoop {
    compositor: WgpuCompositor,
    rx: Receiver<RenderCommand>,
    osd: Option<crate::osd::OsdController>,
    has_video: bool,
    /// Consumers of the composited output — screenshots, and later a stream tee. Empty
    /// on the default path, which is the point: a readback is a full-surface copy.
    taps: Vec<Box<dyn crate::tap::OutputTap>>,
    /// Consecutive GPU-surface imports that failed on a device which claimed to support
    /// them. The decode thread cannot see these — it hands over surfaces and never hears
    /// back — so past a threshold the render thread records the verdict where the *next*
    /// session's decoder will find it.
    failed_imports: u32,
    /// The transport strip currently on screen, if any.
    transport: Option<TransportState>,
}

/// The transport strip's state on the render thread.
///
/// It is kept here rather than re-sent per tick because the scrubber has to *move*
/// between metadata updates. Sources report a position roughly once a second, and a bar
/// that only ever advances when a message arrives visibly stutters; worse, a source that
/// reports position and nothing else would republish the whole card each time, which at
/// 4K is a 33 MB upload for a number (`proto-spotify::session` refused to do that, and
/// was right to).
struct TransportState {
    /// The model as the source last described it.
    model: crate::transport::TransportModel,
    /// The layout that model produced, in strip-local pixels — kept so a touch can be
    /// tested against exactly what was drawn.
    layout: crate::transport::Layout,
    /// Where the strip sits on the surface: `(x, y, w, h)` in pixels.
    placement: (f32, f32, f32, f32),
    /// When `model.position` was taken, so elapsed time can be added to it.
    taken_at: std::time::Instant,
    /// The position last painted, so a repaint happens when the readout changes rather
    /// than on every frame.
    painted: Option<Duration>,
}

impl TransportState {
    /// The position as of now: what the source said, plus the time since it said it.
    ///
    /// Only advanced while playback is actually active — a paused track whose position
    /// crept forward would be a scrubber that lies, and lies in the direction that makes
    /// a seek land somewhere the user did not ask for.
    fn live_position(&self) -> Option<Duration> {
        let base = self.model.position?;
        if !self.model.state.is_active() {
            return Some(base);
        }
        let advanced = base + self.taken_at.elapsed();
        Some(match self.model.duration {
            Some(total) => advanced.min(total),
            None => advanced,
        })
    }
}

impl RenderLoop {
    /// Build a render loop over an existing compositor and the pipeline's receiver.
    #[must_use]
    pub fn new(compositor: WgpuCompositor, rx: Receiver<RenderCommand>) -> Self {
        Self {
            compositor,
            rx,
            osd: None,
            has_video: false,
            taps: Vec::new(),
            failed_imports: 0,
            transport: None,
        }
    }

    /// Attach an OSD controller (consumes the core OSD channel and draws banners).
    #[must_use]
    pub fn with_osd(mut self, controller: crate::osd::OsdController) -> Self {
        self.osd = Some(controller);
        self
    }

    /// Drive the OSD overlay: poll the controller and update the OSD layer.
    fn update_osd(&mut self) {
        // Passed in rather than remembered by the controller, so it tracks a window resize
        // for free and the banner is always rasterized at the panel's real pixel scale.
        let surface = self.compositor.target_size();
        let update = match &mut self.osd {
            Some(controller) => controller.poll(std::time::Instant::now(), surface),
            None => return,
        };
        match update {
            crate::osd::OsdUpdate::Show(banner) => {
                if self
                    .compositor
                    .upload_texture(
                        LayerId::Osd,
                        banner.width,
                        banner.height,
                        // Authored colour: sRGB in, sRGB out. See `TexelFormat::Rgba8`.
                        TexelFormat::Rgba8Srgb,
                        &banner.rgba,
                    )
                    .is_ok()
                {
                    // The banner is a tight image; its transform is what puts it in the
                    // bottom-center of the surface.
                    self.compositor.upsert_layer(Layer {
                        id: LayerId::Osd,
                        z: 10,
                        opacity: 1.0,
                        transform: banner.transform,
                    });
                }
            }
            crate::osd::OsdUpdate::Clear => self.compositor.remove_layer(LayerId::Osd),
            crate::osd::OsdUpdate::Unchanged => {}
        }
    }

    /// Build an offscreen render loop (headless — for tests / capture).
    ///
    /// # Errors
    /// [`PipelineError`] if the GPU can't be acquired.
    pub fn offscreen(
        width: u32,
        height: u32,
        rx: Receiver<RenderCommand>,
    ) -> Result<Self, PipelineError> {
        Ok(Self::new(WgpuCompositor::new_offscreen(width, height)?, rx))
    }

    /// Read back the composited image (offscreen only).
    ///
    /// # Errors
    /// [`PipelineError`] if not offscreen or the readback fails.
    pub fn read_rgba(&self) -> Result<Vec<u8>, PipelineError> {
        self.compositor.read_rgba()
    }

    /// Install the idle/attract background (shown when no video layer is present; a
    /// playing video covers it since it sits below `z=0`).
    ///
    /// # Errors
    /// [`PipelineError`] if the image can't be uploaded.
    /// The size to draw the card at: the surface itself, so text is crisp rather than
    /// upscaled. Clamped because a zero-sized surface exists briefly during startup.
    fn card_size(&self) -> (u32, u32) {
        let (w, h) = self.compositor.target_size();
        (w.max(640), h.max(360))
    }

    /// Install (or remove) the transport strip for `model`.
    ///
    /// Painting the strip is cheap next to the card — a fraction of the surface, no cover
    /// art, no text layout beyond two clock readings — which is the whole reason it is a
    /// separate layer and can be repainted every second.
    fn set_transport(&mut self, model: &crate::transport::TransportModel, w: u32, h: u32) {
        if model.is_empty() {
            self.compositor.remove_layer(LayerId::Transport);
            self.transport = None;
            return;
        }
        let placement = crate::transport::placement(w, h);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = (
            placement.2.round().max(1.0) as u32,
            placement.3.round().max(1.0) as u32,
        );

        // Keep the existing timestamp when the source has repeated a position it already
        // gave us. The card republishes for reasons that have nothing to do with playback
        // — a queue update, the device naming itself — and restamping on those would rewind
        // the scrubber by however long had passed since the real reading.
        let taken_at = match self.transport.as_ref() {
            Some(prev) if prev.model.position == model.position => prev.taken_at,
            _ => std::time::Instant::now(),
        };

        match self.paint_transport(model, pw, ph, placement.1, h) {
            Ok(()) => {
                self.transport = Some(TransportState {
                    layout: crate::transport::layout(model, pw, ph),
                    model: model.clone(),
                    placement,
                    taken_at,
                    painted: model.position,
                });
            }
            Err(e) => error!(error = %e, "failed to draw the transport strip"),
        }
    }

    /// Rasterize `model` into the strip's texture and place the layer.
    ///
    /// `strip_y`/`surface_h` are only for the background: the strip continues the card's
    /// gradient rather than sitting on it, so it has to know which slice of the ramp it
    /// covers. Anything else draws a visible band across a two-metre screen.
    fn paint_transport(
        &mut self,
        model: &crate::transport::TransportModel,
        width: u32,
        height: u32,
        strip_y: f32,
        surface_h: u32,
    ) -> Result<(), PipelineError> {
        let (top, bottom) =
            crate::nowplaying_card::background_span(strip_y / surface_h.max(1) as f32, 1.0);
        let rgba = crate::transport::render(model, width, height, top, bottom)?;
        self.compositor.upload_texture(
            LayerId::Transport,
            width,
            height,
            TexelFormat::Rgba8Srgb,
            &rgba,
        )?;
        let (x, y, w, h) = crate::transport::placement(
            self.compositor.target_size().0,
            self.compositor.target_size().1,
        );
        let (sw, sh) = self.compositor.target_size();
        self.compositor.upsert_layer(Layer {
            id: LayerId::Transport,
            // Above the card, below video.
            z: -4,
            opacity: 1.0,
            transform: Transform {
                scale_x: w / sw.max(1) as f32,
                scale_y: h / sh.max(1) as f32,
                offset_x: x / sw.max(1) as f32,
                offset_y: y / sh.max(1) as f32,
            },
        });
        Ok(())
    }

    /// Advance the scrubber if a visible second has passed.
    ///
    /// Gated on the *rendered* value changing rather than on a timer: the readout is
    /// whole seconds and the bar is a few hundred pixels wide, so repainting faster than
    /// the display changes is upload for no visible difference.
    ///
    /// The source's own reading stays the base — only what is painted moves — so a
    /// snapshot arriving mid-second corrects the drift instead of compounding with it.
    fn tick_transport(&mut self) {
        let Some(state) = self.transport.as_ref() else {
            return;
        };
        if !state.model.state.is_active() {
            return;
        }
        let Some(live) = state.live_position() else {
            return;
        };
        if state.painted.map(|p| p.as_secs()) == Some(live.as_secs()) {
            return;
        }

        let mut painting = state.model.clone();
        painting.position = Some(live);
        let (_, y, w, h) = state.placement;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
        let surface_h = self.compositor.target_size().1;
        match self.paint_transport(&painting, pw, ph, y, surface_h) {
            Ok(()) => {
                if let Some(state) = self.transport.as_mut() {
                    state.painted = Some(live);
                }
            }
            Err(e) => debug!(error = %e, "transport strip tick did not repaint"),
        }
    }

    /// What a touch at panel-normalized `(x, y)` means, if the strip is on screen.
    ///
    /// Returns the transaction rather than the hit: the caller is the input router and
    /// has no business knowing about scrub fractions, and the mapping needs the model
    /// this loop is holding anyway.
    #[must_use]
    pub fn transport_action(
        &self,
        x: f32,
        y: f32,
        phase: crate::transport::TouchPhase,
    ) -> Option<castaway_core::ControlTxn> {
        let state = self.transport.as_ref()?;
        let (sw, sh) = self.compositor.target_size();
        let (lx, ly) = crate::transport::to_strip_local(x, y, sw, sh);
        let hit = state.layout.hit_for(lx, ly, phase)?;
        state.model.action(hit)
    }

    /// Whether a panel-normalized point is over the transport strip at all.
    ///
    /// The input router needs this separately from [`RenderLoop::transport_action`]: a
    /// touch that lands on the strip but produces no transaction — the scrub track on a
    /// source that cannot seek — must still be *consumed*, or it falls through to the
    /// browser underneath and scrolls a page nobody was looking at.
    #[must_use]
    pub fn transport_owns(&self, x: f32, y: f32) -> bool {
        let Some(state) = self.transport.as_ref() else {
            return false;
        };
        let (sw, sh) = self.compositor.target_size();
        let (lx, ly) = crate::transport::to_strip_local(x, y, sw, sh);
        state.layout.hit(lx, ly).is_some()
    }

    /// Install the now-playing card as its own layer.
    ///
    /// # Errors
    /// [`PipelineError`] if the texture cannot be uploaded.
    pub fn set_now_playing(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), PipelineError> {
        self.compositor.upload_texture(
            LayerId::NowPlaying,
            width,
            height,
            // Authored colour: sRGB in, sRGB out. See `TexelFormat::Rgba8`.
            TexelFormat::Rgba8Srgb,
            rgba,
        )?;
        self.compositor.upsert_layer(Layer {
            id: LayerId::NowPlaying,
            z: -5,
            opacity: 1.0,
            transform: Transform::default(),
        });
        Ok(())
    }

    pub fn set_attract(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), PipelineError> {
        self.compositor.upload_texture(
            LayerId::Attract,
            width,
            height,
            // Authored colour: sRGB in, sRGB out. See `TexelFormat::Rgba8`.
            TexelFormat::Rgba8Srgb,
            rgba,
        )?;
        self.compositor.upsert_layer(Layer {
            id: LayerId::Attract,
            z: -10,
            opacity: 1.0,
            transform: Transform::default(),
        });
        Ok(())
    }

    /// Upload a CEF browser frame (BGRA8, as `on_paint` delivers) as the `Browser`
    /// compositor layer. `bgra` is always the complete frame; only the `dirty` regions are
    /// written to the GPU (native BGRA, no CPU swizzle), falling back to a full upload on
    /// first paint or resize. `transform` and `z` come from the browser's role — fullscreen
    /// above the video, or the attract scene's inset widget below it — so this layer's
    /// placement is the caller's decision, not a constant here.
    ///
    /// # Errors
    /// [`PipelineError::InvalidFrame`] if the buffer is undersized.
    pub fn upload_browser(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
        dirty: &[DirtyRect],
        transform: Transform,
        z: i32,
    ) -> Result<(), PipelineError> {
        self.compositor.upload_texture_regions(
            LayerId::Browser,
            width,
            height,
            TexelFormat::Bgra8,
            bgra,
            dirty,
        )?;
        self.compositor.upsert_layer(Layer {
            id: LayerId::Browser,
            z,
            opacity: 1.0,
            transform,
        });
        Ok(())
    }

    /// Remove the browser layer (browser hidden).
    pub fn clear_browser(&mut self) {
        self.compositor.remove_layer(LayerId::Browser);
    }

    /// Drain all pending commands (non-blocking) and present once. Returns the number of
    /// video frames applied this pump.
    pub fn pump(&mut self) -> usize {
        let mut applied = 0;
        while let Ok(cmd) = self.rx.try_recv() {
            if self.apply(cmd) {
                applied += 1;
            }
        }
        self.tick_transport();
        self.update_osd();
        self.present_and_serve_taps();
        applied
    }

    /// Block up to `timeout` for at least one command, apply it (and any others queued),
    /// then present. Returns how many video frames were applied. Used by tests where the
    /// decode thread races the render loop.
    pub fn pump_blocking(&mut self, timeout: Duration) -> usize {
        let mut applied = 0;
        if let Ok(cmd) = self.rx.recv_timeout(timeout) {
            if self.apply(cmd) {
                applied += 1;
            }
            while let Ok(cmd) = self.rx.try_recv() {
                if self.apply(cmd) {
                    applied += 1;
                }
            }
        }
        self.tick_transport();
        self.update_osd();
        self.present_and_serve_taps();
        applied
    }

    /// Attach a consumer of composited frames.
    ///
    /// Costs nothing until it asks for a frame, and is dropped when it says it is
    /// finished — a screenshot retires itself after one capture.
    pub fn add_tap(&mut self, tap: Box<dyn crate::tap::OutputTap>) {
        self.taps.push(tap);
    }

    /// Present, reading the frame back only if some tap asked for it.
    ///
    /// The question is put to every tap *before* the copy, because the copy is a full
    /// surface — 33 MB at 4K — and doing it speculatively would cost more than the rest
    /// of the frame. One readback serves everyone who said yes.
    fn present_and_serve_taps(&mut self) {
        if self.taps.is_empty() {
            self.compositor.present();
            return;
        }
        let now = std::time::Instant::now();
        let mut wanted = Vec::with_capacity(self.taps.len());
        for (i, tap) in self.taps.iter_mut().enumerate() {
            if tap.wants_frame(now) {
                wanted.push(i);
            }
        }
        let captured = self.compositor.present_and_capture(!wanted.is_empty());
        if let Some(rgba) = captured {
            let (width, height) = self.compositor.target_size();
            let frame = crate::tap::TappedFrame::Rgba {
                width,
                height,
                data: &rgba,
            };
            for i in wanted {
                if let Some(tap) = self.taps.get_mut(i) {
                    tap.on_frame(&frame);
                }
            }
        }
        self.taps.retain(|t| !t.finished());
    }

    /// Apply one command. Returns true if it was a video frame.
    fn apply(&mut self, cmd: RenderCommand) -> bool {
        match cmd {
            RenderCommand::Video(frame) => {
                // Two ways pixels reach the video layer: uploaded from system memory
                // (software decode, CEF) or imported in place from a surface the decoder
                // produced on the GPU (hwaccel). Only the second one avoids the copy.
                let landed = match &frame.image {
                    FrameImage::Cpu { format, data } => {
                        let format = match format {
                            PixelFormat::Bgra8 => TexelFormat::Bgra8,
                            // Planar YUV is converted by swscale in the decoder; if a
                            // frame slips through (or a future variant appears), treat
                            // the bytes as RGBA (better a wrong frame than a panic).
                            _ => TexelFormat::Rgba8,
                        };
                        self.compositor.upload_texture(
                            LayerId::Video,
                            frame.width,
                            frame.height,
                            format,
                            data,
                        )
                    }
                    FrameImage::Gpu(surface) => self.compositor.import_surface(
                        LayerId::Video,
                        frame.width,
                        frame.height,
                        surface,
                    ),
                };
                if let Err(e) = &landed {
                    warn!(error = %e, "render loop: dropping a frame the compositor could not take");
                    if matches!(frame.image, FrameImage::Gpu(_)) {
                        self.note_failed_import();
                    }
                } else {
                    self.failed_imports = 0;
                }
                if landed.is_ok() {
                    if !self.has_video {
                        self.compositor.upsert_layer(Layer {
                            id: LayerId::Video,
                            z: 0,
                            opacity: 1.0,
                            transform: Transform::default(),
                        });
                        self.has_video = true;
                    }
                    return true;
                }
                false
            }
            RenderCommand::NowPlaying(card) => {
                // Rendered here rather than upstream: the metadata is a few hundred bytes
                // and the pixels are tens of megabytes, so the channel carries the small
                // one and this thread — which owns the surface size — makes the big one.
                let (w, h) = self.card_size();
                match crate::nowplaying_card::render(&card, w, h) {
                    Ok(rgba) => {
                        if let Err(e) = self.set_now_playing(w, h, &rgba) {
                            error!(error = %e, "failed to draw the now-playing card");
                        }
                    }
                    Err(e) => error!(error = %e, "failed to render the now-playing card"),
                }
                self.set_transport(&card.transport(), w, h);
                false
            }
            RenderCommand::AddTap(tap) => {
                self.taps.push(tap);
                false
            }
            #[cfg(feature = "ffmpeg")]
            RenderCommand::UrlAudioOnly(layout) => {
                // Music from a URL. Everything the container knew, so the card says what
                // is playing rather than sitting blank over sound — and a duration, so the
                // scrubber has something honest to draw against.
                let mut track =
                    castaway_core::NowPlaying::new(castaway_core::PlaybackState::Playing);
                track.title = layout.title.clone();
                track.artist = layout.artist.clone();
                track.album = layout.album.clone();
                track.duration = layout.duration;
                let (w, h) = self.card_size();
                let card = crate::nowplaying_card::NowPlayingCard {
                    track,
                    ..Default::default()
                };
                match crate::nowplaying_card::render(&card, w, h) {
                    Ok(rgba) => {
                        if let Err(e) = self.set_now_playing(w, h, &rgba) {
                            error!(error = %e, "failed to draw the music card");
                        }
                    }
                    Err(e) => error!(error = %e, "failed to render the music card"),
                }
                false
            }
            RenderCommand::ClearNowPlaying => {
                self.compositor.remove_layer(LayerId::NowPlaying);
                // The strip belongs to the card. Leaving it would offer controls for a
                // session that has ended, wired to a remote that has been dropped.
                self.compositor.remove_layer(LayerId::Transport);
                self.transport = None;
                false
            }
            RenderCommand::ClearVideo => {
                self.compositor.remove_layer(LayerId::Video);
                self.has_video = false;
                false
            }
        }
    }

    /// Count an import failure, and past a short run conclude that this device cannot
    /// really do it after all.
    ///
    /// The threshold matters: a single failure is a dropped frame on a live mirror and
    /// invisible, but a steady stream of them means the decoder is doing hardware work
    /// whose output lands nowhere — strictly worse than decoding on the CPU. Recording it
    /// is what lets the next session start in software instead of rediscovering this.
    fn note_failed_import(&mut self) {
        const GIVE_UP_AFTER: u32 = 8;
        self.failed_imports += 1;
        if self.failed_imports == GIVE_UP_AFTER {
            crate::hwaccel::mark_import_broken();
        }
    }

    /// Resize the underlying surface (kiosk window resize).
    pub fn resize(&mut self, width: u32, height: u32) {
        self.compositor.resize(width, height);
    }
}

#[cfg(all(test, feature = "ffmpeg"))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_test_clip() -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join("castaway-render-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("testsrc.mp4");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
            ])
            .args(["-pix_fmt", "yuv420p"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        ok.then_some(path)
    }

    #[test]
    fn play_url_decodes_and_composites_pixels() {
        let Some(path) = make_test_clip() else {
            eprintln!("skipping: no ffmpeg CLI");
            return;
        };
        let (pipe, rx) = RenderPipeline::new(4);
        let mut rloop = match RenderLoop::offscreen(64, 48, rx) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };

        // Drive play() on a small runtime; it spawns the decode thread.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let uri = format!("file://{}", path.display());
        rt.block_on(async {
            pipe.play(MediaUri::parse(&uri).unwrap(), None)
                .await
                .unwrap();
        });

        // Pump until a video frame lands (decode thread races us).
        let mut got = 0;
        for _ in 0..50 {
            got += rloop.pump_blocking(Duration::from_millis(200));
            if got > 0 {
                break;
            }
        }
        assert!(got > 0, "expected at least one composited video frame");

        // The composited output must not be all-black (testsrc is colorful).
        let px = rloop.read_rgba().unwrap();
        let non_black = px.chunks_exact(4).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8);
        assert!(non_black, "composited frame should contain color");
    }

    /// A raw Annex-B H.264 stream, split into the per-frame units an adapter pushes.
    fn encoded_h264() -> Option<Vec<castaway_core::EncodedFrame>> {
        let dir = std::env::temp_dir().join("castaway-render-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("testsrc.h264");
        let ok = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=10:duration=1",
            ])
            // No B-frames, as a mirroring sender encodes: reordering costs latency.
            .args(["-pix_fmt", "yuv420p", "-bf", "0", "-f", "h264"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }

        let mut ictx = ffmpeg_next::format::input(&path).ok()?;
        let index = ictx
            .streams()
            .best(ffmpeg_next::media::Type::Video)?
            .index();
        let mut out = Vec::new();
        for (stream, packet) in ictx.packets() {
            if stream.index() != index {
                continue;
            }
            if let Some(data) = packet.data() {
                out.push(castaway_core::EncodedFrame {
                    video_codec: Some(castaway_core::VideoCodec::H264),
                    audio_codec: None,
                    pts: Duration::from_millis(100 * out.len() as u64),
                    keyframe: packet.is_key(),
                    data: bytes::Bytes::copy_from_slice(data),
                });
            }
        }
        Some(out)
    }

    #[test]
    fn encoded_mirror_decodes_and_composites_pixels() {
        let Some(frames) = encoded_h264() else {
            eprintln!("skipping: no ffmpeg CLI");
            return;
        };
        let (pipe, rx) = RenderPipeline::new(4);
        let mut rloop = match RenderLoop::offscreen(64, 48, rx) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: no GPU ({e})");
                return;
            }
        };

        let (tx, frame_rx) = tokio::sync::mpsc::channel(frames.len().max(1));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // This is what a Cast/AirPlay adapter hands over: encoded frames and nothing
            // else — no URL, no container, no codec on the source itself.
            pipe.mirror(FrameSource::Encoded(frame_rx), None)
                .await
                .unwrap();
            for frame in frames {
                tx.send(frame).await.unwrap();
            }
        });
        // Closing the source is what lets the decode thread flush and finish.
        drop(tx);

        let mut got = 0;
        for _ in 0..50 {
            got += rloop.pump_blocking(Duration::from_millis(200));
            if got > 0 {
                break;
            }
        }
        assert!(got > 0, "expected at least one composited mirror frame");

        let px = rloop.read_rgba().unwrap();
        let non_black = px.chunks_exact(4).any(|p| p[0] > 8 || p[1] > 8 || p[2] > 8);
        assert!(non_black, "composited mirror frame should contain color");
    }
}

#[cfg(test)]
mod card_tests {
    #![allow(clippy::unwrap_used)]
    use castaway_core::{MediaUri, NowPlaying, Pipeline as _, SourceDescription};

    use super::{RenderCommand, RenderPipeline};

    #[tokio::test]
    async fn both_halves_of_the_card_are_published_together() {
        // The device and the track arrive on separate calls and the surface needs both.
        // Publishing only the half that changed would blank the other on every update.
        let (pipeline, rx) = RenderPipeline::new(8);
        pipeline
            .source_info(SourceDescription::new().with_display_name("iPhone"))
            .await
            .unwrap();
        pipeline
            .now_playing(NowPlaying::default().with_title("Derezzed"))
            .await
            .unwrap();

        let mut last = None;
        while let Ok(cmd) = rx.try_recv() {
            if let RenderCommand::NowPlaying(card) = cmd {
                last = Some(card);
            }
        }
        let card = last.expect("the card should have been published");
        assert_eq!(card.track.title.as_deref(), Some("Derezzed"));
        assert_eq!(card.source.display_name.as_deref(), Some("iPhone"));
    }

    #[tokio::test]
    async fn any_source_taking_the_session_asks_the_browser_for_the_panel_back() {
        // The D28 shape, from the other side: nothing but DIAL `DELETE` ever hid the
        // leanback page, and nothing sends `DELETE`. So a YouTube cast covered every
        // later source — video decoded underneath an opaque page, and audio-only sources
        // played under YouTube's own sound, which does not even pass through our mixer.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let released = Arc::new(AtomicUsize::new(0));

        let (pipeline, _rx) = RenderPipeline::new(8);
        let counter = Arc::clone(&released);
        pipeline.set_screen_release(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));

        pipeline
            .play(
                MediaUri::parse("http://example.invalid/a.mp4").unwrap(),
                None,
            )
            .await
            .ok();
        assert_eq!(released.load(Ordering::SeqCst), 1, "a video cast");

        // An audio-only source takes the panel too — a page left on screen keeps making
        // noise even when nothing about it is visible. Only meaningful in a build that
        // can play audio at all; without the feature `play_audio` refuses the session, and
        // a refused session should not dismiss anything.
        #[cfg(feature = "audio")]
        {
            let (_tx, rx) = std::sync::mpsc::sync_channel(1);
            pipeline
                .play_audio(
                    castaway_core::FrameSource::Pcm(rx),
                    castaway_core::AudioFormat::from_hz(44_100, 2).unwrap(),
                )
                .await
                .ok();
            assert_eq!(released.load(Ordering::SeqCst), 2, "an audio-only source");
        }
    }

    #[tokio::test]
    async fn stopping_clears_the_card() {
        let (pipeline, rx) = RenderPipeline::new(8);
        pipeline
            .now_playing(NowPlaying::default().with_title("Derezzed"))
            .await
            .unwrap();
        pipeline.stop().await.unwrap();

        let mut cleared = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, RenderCommand::ClearNowPlaying) {
                cleared = true;
            }
        }
        assert!(cleared, "the card must not outlive the session");
        // …and the next session starts blank rather than inheriting the last track.
        assert_eq!(
            pipeline.card(),
            crate::nowplaying_card::NowPlayingCard::default()
        );
    }
}
