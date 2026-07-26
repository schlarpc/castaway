//! The real [`Pipeline`]: wires a `Play(url)` to decode → GPU compositor → present, and
//! forwards decoded mirror frames straight to the compositor. Threading follows
//! architecture §6: the compositor/GPU lives on ONE render thread (the [`RenderLoop`],
//! driven by the kiosk's winit loop or, in tests, pumped directly); decode runs on its
//! own blocking thread; this [`RenderPipeline`] is the tokio-side handle that connects
//! them over a bounded channel that **drops frames when full** (latency > freshness).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use castaway_core::{
    ControlTxn, CoreError, DecodedFrame, FrameImage, FrameSource, MediaUri, Pipeline, PixelFormat,
};
use tracing::{error, info, warn};

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

    fn preempt(&self) {
        if let Ok(mut guard) = self.active.lock() {
            if let Some(flag) = guard.take() {
                flag.store(true, Ordering::SeqCst);
            }
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
        self.preempt();
        let stop = Arc::new(AtomicBool::new(false));
        self.set_active(stop.clone());
        info!(%source, ?start, "render pipeline: PLAY (decode → compositor)");

        let tx = self.tx.clone();
        let uri = source.to_string();
        let hw = self.hw;
        // Decode is blocking + thread-affine → dedicated OS thread, never the runtime.
        std::thread::spawn(move || {
            let result = decode_into(&uri, hw, &tx, &stop);
            if let Err(e) = result {
                warn!(error = %e, "decode ended with error");
            }
        });
        Ok(())
    }

    async fn mirror(
        &self,
        video: FrameSource,
        _audio: Option<FrameSource>,
    ) -> Result<(), CoreError> {
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
    ) -> Result<(), CoreError> {
        #[cfg(feature = "audio")]
        {
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
            // Transport against a live decode is a follow-up. Refused rather than logged
            // as success, so a caller can tell the difference — `UnsupportedControl` is
            // the typed way to say "not on this pipeline" (ground rule 7).
            other => {
                info!(?other, "render pipeline: CONTROL (unsupported)");
                Err(CoreError::UnsupportedControl(format!("{other:?}")))
            }
        }
    }

    async fn stop(&self) -> Result<(), CoreError> {
        self.preempt();
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
fn decode_into(
    uri: &str,
    hw: HwPreference,
    tx: &SyncSender<RenderCommand>,
    stop: &Arc<AtomicBool>,
) -> Result<(), PipelineError> {
    #[cfg(feature = "ffmpeg")]
    {
        crate::ffmpeg_decode::decode(uri, hw, |frame| {
            if stop.load(Ordering::SeqCst) {
                return false;
            }
            // Drop on full (bounded queue) but stop if the render loop is gone.
            !matches!(
                tx.try_send(RenderCommand::Video(frame)),
                Err(TrySendError::Disconnected(_))
            )
        })
    }
    #[cfg(not(feature = "ffmpeg"))]
    {
        let _ = (uri, hw, tx, stop);
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
                false
            }
            RenderCommand::AddTap(tap) => {
                self.taps.push(tap);
                false
            }
            RenderCommand::ClearNowPlaying => {
                self.compositor.remove_layer(LayerId::NowPlaying);
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
    use castaway_core::{NowPlaying, Pipeline as _, SourceDescription};

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
