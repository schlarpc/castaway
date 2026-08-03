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
/// How long after a touch the panel counts as in use, for the idle return (#27).
const IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long a requested video/card clear is held before it takes effect, so a control
/// point that "seeks" by stopping and re-loading the item (VLC's live streams do this
/// on every scrub) replaces the picture instead of flashing the idle screen between the
/// STOP and the new item's first frame. Long enough for a network re-open; short enough
/// that a genuine stop still reads as prompt.
const CLEAR_GRACE: std::time::Duration = std::time::Duration::from_millis(1200);

/// How the screen being navigated away from goes.
///
/// Two shapes, and which one it is comes from whether the navigation had a *place*. A screen
/// that was opened out of a tile has to go back into that tile, or the way out contradicts the
/// way in and the tile stops meaning anything. A screen with no origin leaves along the
/// navigation axis, which is the shared-axis pattern and the only honest answer when there is
/// nowhere in particular to go.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Leaving {
    /// Travel one panel-width along the axis, shrinking to a card as it goes.
    Slide {
        /// -1.0 for a push (recede left), 1.0 for a pop (return right).
        direction: f32,
    },
    /// Shrink into the rect it was opened out of, and fade only once it is nearly there.
    Into(crate::panel::NormRect),
    /// Hold still and fade out early, because the *arriving* screen is the one animating —
    /// it is growing out of a tile over the top of this one. Material's container transform
    /// crossfades exactly this way round: outgoing content goes in the first third, incoming
    /// arrives over the rest.
    Yield,
}

/// A navigation being animated.
#[derive(Debug, Clone, Copy)]
struct Transition {
    /// How the outgoing screen leaves.
    leaving: Leaving,
    /// The spring that carries it there once nobody is holding it.
    ///
    /// The same kind of spring every other surface on the panel uses, from the same
    /// [`crate::motion::Choreography`] table. It was a hand-rolled velocity decay plus a
    /// proportional pull — a second integrator with its own feel and its own settle thresholds,
    /// which is exactly the "two mechanisms for one thing" this work exists to remove.
    spring: crate::motion::Spring,
    /// 1.0 = the outgoing screen fills the panel, 0.0 = it is gone.
    progress: f32,
    /// Progress per second, carried from the finger so a flick keeps going.
    velocity: f32,
    /// Where it is heading once nobody is holding it.
    target: f32,
    /// While a finger is on the glass the progress is *its*, not the clock's.
    driven: bool,
}

/// What to do to the stack if a navigation springs back.
#[derive(Debug, Clone)]
enum Undo {
    /// It pushed; pop it again.
    Pop,
    /// It popped or went home; put these back, each with the rect it grew out of.
    Restore(Vec<(crate::shell::Screen, Option<crate::panel::NormRect>)>),
}

/// How much of a container transform the outgoing screen has to be gone by.
///
/// Material's crossfade split, and it is asymmetric on purpose: the content being replaced goes
/// in the first third or so while the arriving content takes the rest. Both fading over the
/// whole duration is what produces the double-exposure look that reads as a dissolve rather than
/// as one thing becoming another.
const OUTGOING_FADE: f32 = 0.4;

/// How small the card gets on its way out. Not to nothing: it shrinks *into a preview*,
/// so what you flicked away is still legible as it goes.
const CARD_MIN_SCALE: f32 = 0.82;

/// How fast the finger has to be moving for a flick to win over where it let go.
const FLICK: f32 = 1.6;

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
    /// Set or refresh the Home screen.
    ///
    /// Carries the *model*, like `NowPlaying` and for the same reasons: a 4K RGBA buffer
    /// is 33 MB down a bounded channel, and only the render thread knows the true surface
    /// size. The idle screen used to be rasterised once at startup at a hardcoded
    /// 3840x2160 and GPU-upscaled to whatever the panel actually was; now it is drawn at
    /// the size it will be shown, and can change while the receiver is running (D38).
    Home(Box<crate::attract::AttractScene>),
    /// Push a shell screen — a picker the app built, in answer to a tile press.
    PushScreen(Box<crate::shell::Screen>),
    /// Replace the screen on top, or push if at Home. What a picker's own refreshes use,
    /// so `back` stays one step regardless of how many times the list updated.
    ReplaceScreen(Box<crate::shell::Screen>),
    /// Go back one shell screen.
    ShellBack,
    /// Put the panel back to its resting arrangement: the shell at Home, and the glass
    /// handed to whatever is playing.
    ///
    /// Both ends of a session send this, and they mean the same thing by it. A session
    /// *ending* wants the idle screen back (#27); a session *starting* wants the panel it
    /// is about to fill — and without that, a source that restarts while the shell happens
    /// to be forward stays minimised for ever, because nothing else ever hands the glass
    /// back. Both are declined the same way, by the same predicate: not while someone is
    /// using the panel.
    RestPanel,
    /// Attach a consumer of composited frames (#18). Sent as a command rather than set
    /// on the loop directly because the loop lives on the main thread and everything
    /// that wants to tap it does not.
    AddTap(Box<dyn crate::tap::OutputTap>),
    /// Start duplicating the output as a web stream (#101). Separate from
    /// [`Self::AddTap`] because the tap's coded size is derived from the panel's, and the
    /// panel's size is something only the loop knows.
    #[cfg(feature = "stream")]
    StartStream {
        /// Where segments are published and where requests are counted.
        state: Arc<crate::stream::LiveStream>,
        /// Rate, size cap, bitrate, and how long the tap outlives its last viewer.
        config: crate::stream::StreamConfig,
        /// The panel's sound, if this build has an audio path. `None` is a video-only
        /// stream, which is a whole stream rather than a degraded one.
        audio: Option<Arc<crate::stream::StreamAudio>>,
    },
    /// The URL that was opened turned out to be audio-only, with whatever the container
    /// tags said about it. The surface answers with a now-playing card rather than a
    /// black screen over music.
    #[cfg(feature = "ffmpeg")]
    UrlAudioOnly(Box<crate::ffmpeg_decode::MediaLayout>),
}

/// The sending half of the render channel: two lanes, chosen by the command itself.
///
/// Video frames ride a small bounded lane and are dropped when the loop is behind —
/// for live media, latency beats completeness (architecture §6). Everything else is a
/// *state transition*, and a dropped transition desynchronises the panel from its
/// sessions: `RestPanel` used to ride the same depth-3 lane as the frames, so a cast
/// that arrived while the lane happened to be full simply never took the screen, and a
/// `PushScreen` could turn a tile press into nothing. Transitions therefore ride an
/// unbounded lane that cannot refuse them. The routing lives *here, on the type*, so no
/// call site can put a transition on the lossy lane or a frame on the unbounded one.
#[derive(Clone)]
pub struct RenderTx {
    frames: SyncSender<RenderCommand>,
    control: std::sync::mpsc::Sender<RenderCommand>,
    /// Wakes the kiosk loop, which sleeps between frames (#59). Every send wakes it:
    /// a command sitting in a channel nobody is spinning on is otherwise invisible.
    waker: castaway_core::Waker,
}

impl RenderTx {
    /// Send a command down its lane.
    ///
    /// A refused frame is dropped by design. A transition cannot be refused; the only
    /// way it goes nowhere is a disconnected receiver, which means the render loop
    /// itself is gone and there is no panel left to desynchronise.
    pub fn send(&self, cmd: RenderCommand) {
        match cmd {
            RenderCommand::Video(_) => drop(self.frames.try_send(cmd)),
            control => drop(self.control.send(control)),
        }
        self.waker.wake();
    }

    /// Send a video frame, answering whether the render loop is still there.
    ///
    /// `false` only on disconnect — a full lane just drops the frame. The decode and
    /// mirror loops use the answer to stop pushing at a loop that is gone.
    pub fn send_frame(&self, frame: DecodedFrame) -> bool {
        let alive = !matches!(
            self.frames.try_send(RenderCommand::Video(frame)),
            Err(TrySendError::Disconnected(_))
        );
        self.waker.wake();
        alive
    }

    /// The waker every sender on this channel shares. For the app to hand to producers
    /// that bypass the channel (the exit flag, the browser command lane) so their events
    /// reach a sleeping loop too.
    #[must_use]
    pub fn waker(&self) -> castaway_core::Waker {
        self.waker.clone()
    }
}

/// The receiving half: what [`RenderLoop`] drains every pump.
pub struct RenderRx {
    frames: Receiver<RenderCommand>,
    control: Receiver<RenderCommand>,
    waker: castaway_core::Waker,
}

impl RenderRx {
    /// The next command, transitions first.
    ///
    /// Control-before-frames means a frame can be applied after a `ClearVideo` that was
    /// sent later; that is absorbed by the clear being *scheduled* (`CLEAR_GRACE`), not
    /// immediate, so a straggler frame is cleared with everything else. The opposite
    /// order would be worse: frames delaying the transition that explains them.
    pub fn try_recv(&self) -> Option<RenderCommand> {
        match self.control.try_recv() {
            Ok(cmd) => Some(cmd),
            Err(_) => self.frames.try_recv().ok(),
        }
    }

    /// Throw away every video frame queued right now, and answer how many there were.
    ///
    /// For the end of an item: those frames are pixels of something that is over, and the
    /// loop is about to take the layer down. Only the frame lane is touched — a control
    /// command is never stale in this sense, and dropping one would lose a transition.
    pub fn discard_queued_frames(&self) -> usize {
        let mut dropped = 0;
        while self.frames.try_recv().is_ok() {
            dropped += 1;
        }
        dropped
    }

    /// Block up to `timeout` for the next command. Two receivers share the timeout by
    /// polling in millisecond slices — only tests block here, so coarse is fine.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<RenderCommand> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(cmd) = self.try_recv() {
                return Some(cmd);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The waker the senders on this channel share, for the kiosk loop to arm with its
    /// real wake mechanism once the event loop exists (#59).
    #[must_use]
    pub fn waker(&self) -> castaway_core::Waker {
        self.waker.clone()
    }
}

/// A render channel: a bounded frame lane of `frame_depth` and an unbounded
/// transition lane. See [`RenderTx`] for why there are two.
#[must_use]
pub fn render_channel(frame_depth: usize) -> (RenderTx, RenderRx) {
    let (frames_tx, frames_rx) = sync_channel(frame_depth.max(1));
    let (control_tx, control_rx) = std::sync::mpsc::channel();
    let waker = castaway_core::Waker::new();
    (
        RenderTx {
            frames: frames_tx,
            control: control_tx,
            waker: waker.clone(),
        },
        RenderRx {
            frames: frames_rx,
            control: control_rx,
            waker,
        },
    )
}

/// Asks the render thread to keep duplicating what it is showing, as a web stream (#101).
///
/// Cheap to clone and cheap to hold: nothing is encoded, and no readback happens, until
/// something actually fetches the playlist. [`Self::ensure_running`] is what a request
/// calls, and it is idempotent — a page with a player on it fires several at once.
#[cfg(feature = "stream")]
#[derive(Clone)]
pub struct StreamHandle {
    tx: RenderTx,
    state: Arc<crate::stream::LiveStream>,
    config: crate::stream::StreamConfig,
    /// The panel's sound. Created once by the app because the factory it tees is installed
    /// once, where the tap comes and goes with whoever is watching.
    audio: Option<Arc<crate::stream::StreamAudio>>,
}

#[cfg(feature = "stream")]
impl std::fmt::Debug for StreamHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHandle")
            .field("status", &self.state.status())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "stream")]
impl StreamHandle {
    /// What is currently on offer: the playlist, the segments, and why there is none.
    #[must_use]
    pub fn stream(&self) -> &Arc<crate::stream::LiveStream> {
        &self.state
    }

    /// Note a request and start the stream if it is not already running.
    ///
    /// The touch is the point: it is what keeps the tap alive, so "somebody is watching"
    /// is derived from requests actually arriving rather than from a subscriber count
    /// somebody has to remember to decrement.
    pub fn ensure_running(&self) {
        self.state.touch(std::time::Instant::now());
        if self.state.claim() {
            // The stream's coded size depends on the panel's, which only the render loop
            // knows — so the loop builds the tap, and this says no more than "start".
            self.tx.send(RenderCommand::StartStream {
                state: Arc::clone(&self.state),
                config: self.config,
                audio: self.audio.clone(),
            });
        }
    }
}

/// Asks the render thread to capture what it is showing.
#[derive(Clone)]
pub struct ScreenshotHandle {
    tx: RenderTx,
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
        self.tx.send(RenderCommand::AddTap(Box::new(tap)));
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
    tx: RenderTx,
    /// Stop flag for the currently-running decode thread, so a new `Play`/`stop`
    /// preempts the old one.
    active: Mutex<Option<Arc<AtomicBool>>>,
    /// Held for exactly as long as a session is active, so the panel does not idle out
    /// from under something that is playing (#109). Any session — audio, video, mirror,
    /// browser — because all of them mean somebody is using it.
    awake: Mutex<Option<crate::keepawake::KeepAwake>>,
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
    /// Where audio goes. `None` opens the default device.
    ///
    /// A factory rather than a device, because each session takes its own. It is also
    /// the only way to observe that a session's audio *reached* an output, which is the
    /// assertion that would have caught mirror audio being silently discarded.
    #[cfg(feature = "audio")]
    audio_output: Mutex<Option<crate::audio_out::AudioOutputFactory>>,
    /// Called when another source takes the screen, so a browser that is covering the
    /// panel gives it back.
    ///
    /// A callback rather than a `BrowserCommand` sender because the browser lives behind
    /// the `electron` feature and the pipeline should not: the pipeline's concern is "somebody
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
    /// A second handle on the command channel.
    ///
    /// For the parts of `app` that drive the *shell* rather than playback — they need to
    /// push screens without owning the pipeline, and the pipeline is moved into the
    /// session manager at startup.
    pub fn commands(&self) -> RenderTx {
        self.tx.clone()
    }

    /// The process-wide render-loop waker (#59). Everything that queues work for the
    /// kiosk outside the render channel — the exit flag, the browser command lane, the
    /// browser subprocess itself — takes a clone, so the loop can sleep between frames
    /// without any of them going unnoticed.
    #[must_use]
    pub fn waker(&self) -> castaway_core::Waker {
        self.tx.waker()
    }

    pub fn new(depth: usize) -> (Self, RenderRx) {
        let (tx, rx) = render_channel(depth);
        (
            Self {
                tx,
                active: Mutex::new(None),
                awake: Mutex::new(None),
                hw: HwPreference::Auto,
                card: Mutex::new(crate::nowplaying_card::NowPlayingCard::default()),
                playback: Arc::new(Mutex::new(None)),
                ends: Mutex::new(None),
                release_screen: Mutex::new(None),
                #[cfg(feature = "audio")]
                audio_output: Mutex::new(None),
                #[cfg(feature = "audio")]
                gain: Arc::new(crate::audio_session::Gain::default()),
            },
            rx,
        )
    }

    /// The panel's one volume.
    ///
    /// Shared rather than copied, and this is the whole point of it: everything that
    /// writes samples to a device applies the same `Gain`, so a level set over AVRCP is
    /// the level YouTube plays at. The browser is a second writer on a second stream and
    /// used to reach its device without passing here at all (#86).
    #[cfg(feature = "audio")]
    #[must_use]
    pub fn gain(&self) -> Arc<crate::audio_session::Gain> {
        Arc::clone(&self.gain)
    }

    /// Use `factory` for audio output instead of the default device.
    #[cfg(feature = "audio")]
    #[must_use]
    pub fn with_audio_output(self, factory: crate::audio_out::AudioOutputFactory) -> Self {
        if let Ok(mut slot) = self.audio_output.lock() {
            *slot = Some(factory);
        }
        self
    }

    /// A fresh output device for one session.
    #[cfg(feature = "audio")]
    fn audio_output(&self) -> Box<dyn crate::audio_out::AudioOut> {
        self.audio_output
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|f| f()))
            .unwrap_or_else(crate::audio_session::default_output)
    }
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

    /// A handle on the output duplicate (#101), taken for the same reason as
    /// [`Self::screenshot_handle`] and costing the same nothing until something asks.
    #[cfg(feature = "stream")]
    #[must_use]
    pub fn stream_handle(
        &self,
        config: crate::stream::StreamConfig,
        audio: Option<Arc<crate::stream::StreamAudio>>,
    ) -> StreamHandle {
        StreamHandle {
            tx: self.tx.clone(),
            state: Arc::new(crate::stream::LiveStream::new(&config)),
            config,
            audio,
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
        self.tx
            .send(RenderCommand::NowPlaying(Box::new(guard.clone())));
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

    /// Ask for the panel a starting session is about to fill.
    ///
    /// The counterpart of the idle return in [`Self::stop`], and declined on the same
    /// terms — the render loop keeps someone who is using the panel where they are. What
    /// it fixes: nothing else ever took the shell out of the foreground, so a source that
    /// ended and restarted (a phone reclaiming Spotify, a preempted sender pressing play)
    /// came back minimised into a corner with no way to know it had.
    fn claim_panel(&self) {
        self.tx.send(RenderCommand::RestPanel);
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
        // Let the display idle again. A session taking over re-acquires through
        // `set_active` a moment later; the gap is sub-millisecond and no idle timer has
        // that resolution.
        if let Ok(mut awake) = self.awake.lock() {
            *awake = None;
        }
        // Whatever decode thread is still alive belongs to a session that is over, so
        // retire its ticket: a thread that had already passed its stop-flag check must
        // not end the session that is taking the screen right now.
        if let Some(report) = self.end_report() {
            report.invalidate();
        }
    }

    /// Begin a session: mint its stop flag and hold the panel awake for it.
    ///
    /// The two are deliberately the same call, and that is the whole point. Every session
    /// needs a stop flag — without one, preemption cannot reach it — so a new source type
    /// cannot obtain one without also taking the keep-awake guard. There is no separate
    /// step to forget.
    ///
    /// This replaced a `set_active(flag)` that each caller invoked by convention, which is
    /// exactly the shape that goes wrong the first time somebody adds a session type and
    /// does not know the convention exists. `preempt` drops both together for the same
    /// reason (#109).
    #[must_use]
    fn begin_session(&self) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut guard) = self.active.lock() {
            *guard = Some(Arc::clone(&stop));
        }
        if let Ok(mut awake) = self.awake.lock() {
            *awake = Some(crate::keepawake::KeepAwake::acquire());
        }
        stop
    }
}

#[async_trait]
impl Pipeline for RenderPipeline {
    async fn play(&self, source: MediaUri, start: Option<Duration>) -> Result<(), CoreError> {
        self.release_screen();
        self.claim_panel();
        self.preempt();
        let stop = self.begin_session();
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
                self.audio_output(),
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
            tx.send(RenderCommand::ClearVideo);
            tx.send(RenderCommand::ClearNowPlaying);

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
        self.claim_panel();
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
                        if !tx.send_frame(frame) {
                            break;
                        }
                    }
                });
                Ok(())
            }
            FrameSource::Encoded(rx) => {
                info!("render pipeline: MIRROR (encoded frames → decode → compositor)");
                let stop = self.begin_session();
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
                            self.audio_output(),
                            Arc::clone(&stop),
                            Arc::clone(&self.gain),
                            // No failure report, for the same reason this path does not
                            // preempt: a mirror's audio and its picture share a stop
                            // flag, so ending the session over a refused output would
                            // take the screen down too. Losing the sound and keeping the
                            // mirror is the better half of a bad trade; the ERROR the
                            // session logs is what says so.
                            None,
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
            // left on screen would keep playing its own sound over it.
            self.release_screen();
            self.claim_panel();
            // Preempt first: the flag slot holds whichever session is live, video or
            // audio, because only one source may own the output at a time.
            self.preempt();
            let stop = self.begin_session();
            // Taken after `preempt`, so it is this session's ticket. `play` has always
            // done this; the audio path never did, which is why an audio session that
            // could not open the device had no way to say so and the source streamed on
            // into nothing.
            let ends = self.end_report().map(|r| {
                let ticket = r.ticket();
                (r, ticket)
            });
            let failed: Option<crate::audio_session::SessionFailed> =
                ends.map(|(report, ticket)| {
                    Box::new(move |why: String| report.report(ticket, PlaybackEnd::Failed(why)))
                        as crate::audio_session::SessionFailed
                });
            match source {
                FrameSource::Encoded(rx) => {
                    crate::audio_session::spawn(
                        rx,
                        format,
                        config,
                        self.audio_output(),
                        stop,
                        Arc::clone(&self.gain),
                        failed,
                    );
                    Ok(())
                }
                // Already decoded (Spotify): `format` is what the adapter negotiated, but
                // each block restates it, so the session takes it from the samples.
                FrameSource::Pcm(rx) => {
                    crate::audio_session::spawn_pcm(
                        rx,
                        self.audio_output(),
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
            let _ = (source, format, config);
            // A typed refusal rather than a silent success: a build without the `audio`
            // feature has no decoder at all, and a phone that pairs, streams and plays
            // to silence is the worst possible thing to diagnose.
            Err(CoreError::Pipeline(
                "this build has no audio support; rebuild with the `audio` feature".into(),
            ))
        }
    }

    async fn now_playing(&self, snapshot: castaway_core::NowPlaying) -> Result<(), CoreError> {
        self.publish_card(|card| {
            // The track that just started was, a moment ago, the head of the queue.
            // Sources are not obliged to re-send the queue on a natural advance —
            // Spotify's cluster updates announce edits, not progress — so the card
            // advances its own copy: showing a track both as "playing" and as "up next"
            // reads as the queue having stalled. Matched on title+artist because that
            // is all a `QueueItem` carries; a queue with genuine duplicates loses one
            // occurrence, which is the truthful outcome anyway.
            let now_head = card.up_next.first().is_some_and(|next| {
                Some(next.title.as_str()) == snapshot.title.as_deref()
                    && next.artist == snapshot.artist
            });
            if now_head {
                card.up_next.remove(0);
            }
            card.track = snapshot;
        });
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
                    // Both numbers, because they are the two that used to be confused:
                    // where the sender's slider is, and what the samples get multiplied
                    // by (#85). A log showing only one cannot tell you the taper ran.
                    info!(
                        position = level.position(),
                        amplitude = self.gain.level(),
                        "render pipeline: volume"
                    );
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
                self.tx.send(RenderCommand::ClearVideo);
                if let Ok(mut guard) = self.card.lock() {
                    *guard = crate::nowplaying_card::NowPlayingCard::default();
                }
                self.tx.send(RenderCommand::ClearNowPlaying);
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
        self.tx.send(RenderCommand::ClearVideo);
        if let Ok(mut guard) = self.card.lock() {
            *guard = crate::nowplaying_card::NowPlayingCard::default();
        }
        self.tx.send(RenderCommand::ClearNowPlaying);
        // Nothing is playing and nobody is connected, so put the panel back where a
        // person expects to find it (#27). The render loop declines if the panel was
        // touched recently — an ending session is no reason to close a screen someone is
        // reading.
        self.tx.send(RenderCommand::RestPanel);
        info!("render pipeline: STOP");
        Ok(())
    }
}

/// Decode `uri` into render commands until EOF or `stop` is set.
#[allow(clippy::too_many_arguments)]
fn decode_into(
    uri: &str,
    hw: HwPreference,
    tx: &RenderTx,
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
                    tx.send(RenderCommand::UrlAudioOnly(Box::new(layout.clone())));
                }
            },
            |frame| {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                // Drop on full (bounded queue) but stop if the render loop is gone.
                tx.send_frame(frame)
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
    tx: &RenderTx,
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
                tx.send_frame(frame)
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
    rx: RenderRx,
    osd: Option<crate::osd::OsdController>,
    /// A navigation in progress: how much of the outgoing screen is still showing, and
    /// whether a finger is driving it.
    ///
    /// The screen being left is a *card*: it moves and shrinks, and the destination is
    /// underneath it the whole time. Placing it off-surface costs nothing — the GPU
    /// clips geometry outside the viewport, so a card three-quarters of the way out is
    /// three-quarters drawn. Both are transform-only, which is a 32-byte uniform write
    /// per frame rather than a 33 MB re-raster (D38).
    transition: Option<Transition>,
    /// How to put the stack back if a driven navigation is abandoned half-way.
    transition_undo: Option<Undo>,
    /// What the panel is presenting: which screens are stacked, which surfaces are up, and
    /// who has the glass. The authority — this loop derives every placement, every
    /// suppression and every hit test from it rather than keeping opinions of its own
    /// (see [`crate::panel`]).
    panel: crate::panel::Panel,
    /// What the panel last answered for each surface: its placement, or `None` when it was
    /// not on the panel at all. A change detector, not state — placement is recomputed every
    /// pump, because it depends on which screen is current and no navigation should have to
    /// remember to re-place layers. A difference here is what starts a motion.
    placed: [Option<crate::panel::Placement>; crate::panel::Surface::ALL.len()],
    /// Where each surface actually is on its way to where the panel says it goes, and the
    /// floor's own recession. The continuous half of the model (see [`crate::motion`]).
    motions: crate::motion::Motions,
    /// The floor: the shell pushing back and dimming under a session that has the glass.
    floor: crate::motion::Floor,
    /// Whether anything is still moving, so the kiosk knows to ask for another frame.
    animating: bool,
    /// The mascot overlay's own placement, if she is up. Kept so the floor's recession can be
    /// *composed* with it: she is a sub-rect of the scene, not a full-panel layer, so handing
    /// her the floor's transform directly would stretch line art across the whole panel.
    mascot_base: Option<Transform>,
    /// When the panel was last touched, so an ending session does not yank someone out
    /// of a screen they are reading (#27).
    last_touch: Option<std::time::Instant>,
    /// When a requested video-layer clear takes effect, unless a frame cancels it. The
    /// deferral exists for control points that "seek" by stopping and re-loading the
    /// item: an immediate clear bared the idle screen for the gap between the two.
    video_clear_due: Option<std::time::Instant>,
    /// The now-playing card's counterpart of [`Self::video_clear_due`].
    card_clear_due: Option<std::time::Instant>,
    /// The side the close badge was last rasterized at; `0` when it is not up.
    close_badge_side: u32,
    /// Where this loop reads the time. Injectable so the deadlines below — the strip's
    /// anchor, the deferred clears, the demand calculation — can be asserted against a
    /// number a test chose rather than against how fast the host happened to be (#156).
    clock: crate::render_clock::RenderClock,
    /// The card as last rasterized, with the surface size it was rasterized at.
    ///
    /// Position now republishes once a second (the transport strip's clock syncs on
    /// it), and the card draws no position — so this is what lets the per-second
    /// update repaint the small strip without re-rasterizing a card of identical
    /// pixels that is tens of megabytes at 4K.
    card_shown: Option<(Box<crate::nowplaying_card::NowPlayingCard>, (u32, u32))>,
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
    /// The decoded size of the video currently on the glass.
    ///
    /// Kept because placement happens in [`Self::apply_motion`], which runs on motion
    /// steps and reflows and has no frame in hand — while the only thing that knows the
    /// frame's shape is the frame itself. `None` until one arrives, and updated when it
    /// changes, which on a mirroring phone is every rotation.
    video_size: Option<(u32, u32)>,
}

/// Place `inner` inside `outer`: the transform a sub-rect layer needs when the surface it
/// belongs to has itself been moved.
///
/// Both are rect placements in the same normalized space, so this is the obvious composition
/// — but it is worth a name, because doing it the other way round is the bug that stretches a
/// mascot across a panel.
fn compose(outer: Transform, inner: Transform) -> Transform {
    Transform {
        scale_x: outer.scale_x * inner.scale_x,
        scale_y: outer.scale_y * inner.scale_y,
        offset_x: outer.offset_x + outer.scale_x * inner.offset_x,
        offset_y: outer.offset_y + outer.scale_y * inner.offset_y,
    }
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
    /// The position as it *moves*, between the readings the source publishes.
    ///
    /// Sources state a position roughly once a second; this is what makes the bar slide
    /// between them instead of stepping. It also owns the reconciliation, which is the
    /// part with the trap in it — see [`crate::projection`] (#165).
    projection: crate::projection::Projection,
    /// The position last painted, so a repaint happens when the readout changes rather
    /// than on every frame.
    painted: Option<Duration>,
    /// Where the finger currently dragging the scrub track is, as a fraction (#97).
    ///
    /// An override rather than a write into the model: the model is what the *source*
    /// said, and a drag has not asked the source for anything yet. Cleared on the lift,
    /// when the real seek goes out, and on a cancel, when it does not.
    preview: Option<f32>,
}

impl TransportState {
    /// The position as of now: the projection, which is what the source said carried
    /// forward and reconciled against every reading since.
    fn live_position(&self, now: std::time::Instant) -> Option<Duration> {
        self.model.position?;
        Some(self.projection.at(now))
    }

    /// How far the bar moves for one pixel of travel, in time.
    ///
    /// The repaint rate limit for a *playing* track, and the reason a sliding bar is not
    /// simply "repaint every frame". A pixel is the smallest visible change, so anything
    /// finer is a full-strip raster nobody can see; and because it scales with the item's
    /// length, a three-minute track asks for far fewer frames than a one-minute one
    /// without either being told a number.
    ///
    /// `None` when there is no scrub track drawn — a live stream, or a source that
    /// reports no duration — in which case only the elapsed readout changes and the
    /// second boundary is the whole rate limit.
    fn pixel_of_travel(&self) -> Option<Duration> {
        let total = self.model.duration?;
        let width = f64::from(self.layout.track_touch?.w);
        if width < 1.0 {
            return None;
        }
        Some(total.div_f64(width))
    }

    /// The position the strip should be showing right now.
    ///
    /// The finger while one is down, the clock otherwise — and the finger wins for as
    /// long as it is there, including on a *paused* track. A drag over a pause used to
    /// repaint not once a second but never, because the clock is the only thing that ever
    /// asked for a repaint and a paused clock does not tick.
    fn showing(&self, now: std::time::Instant) -> Option<Duration> {
        match self.preview {
            Some(fraction) => Some(self.model.duration?.mul_f32(fraction)),
            // Not gated on playback being active, and it cannot be. A paused track's
            // position is constant, so this asks for at most one repaint and then stops
            // — but that one repaint is the one that puts the bar back where the music is
            // when a drag over a *paused* track is cancelled. Returning nothing here left
            // the panel showing a preview of a gesture that never happened.
            None => self.live_position(now),
        }
    }

    /// Whether what is painted has fallen behind what should be showing.
    ///
    /// Two rate limits, because there are two things being drawn. A clock changes at most
    /// once a second and the strip is a full raster, so repainting it per frame would be
    /// waste. A bar under a finger has to keep up with the finger, so every movement
    /// counts — that is the "frame-rate, not second-rate" #97 asks for, and it applies
    /// only while a drag is actually in flight.
    fn stale(&self, target: Duration) -> bool {
        let Some(painted) = self.painted else {
            return true;
        };
        if self.preview.is_some() {
            return painted != target;
        }
        // The readout, which changes on the whole second.
        if painted.as_secs() != target.as_secs() {
            return true;
        }
        // …and the bar, which moves continuously. Before this the second boundary was the
        // only thing that asked for a repaint, so the bar advanced in one-second hops —
        // fine on a phone, conspicuous on a two-metre panel (#165).
        match self.pixel_of_travel() {
            Some(pixel) => painted.abs_diff(target) >= pixel,
            None => false,
        }
    }
}

impl RenderLoop {
    /// Build a render loop over an existing compositor and the pipeline's receiver.
    #[must_use]
    pub fn new(compositor: WgpuCompositor, rx: RenderRx) -> Self {
        Self {
            compositor,
            rx,
            osd: None,
            transition: None,
            transition_undo: None,
            panel: crate::panel::Panel::new(),
            placed: [None; crate::panel::Surface::ALL.len()],
            motions: crate::motion::Motions::default(),
            floor: crate::motion::Floor::default(),
            animating: false,
            mascot_base: None,
            last_touch: None,
            video_clear_due: None,
            video_size: None,
            card_clear_due: None,
            close_badge_side: 0,
            clock: crate::render_clock::RenderClock::monotonic(),
            card_shown: None,
            taps: Vec::new(),
            failed_imports: 0,
            transport: None,
        }
    }

    /// Run this loop against `clock` instead of the monotonic one.
    ///
    /// For tests only — nothing in production installs anything but the monotonic clock.
    #[must_use]
    pub fn with_clock(mut self, clock: crate::render_clock::RenderClock) -> Self {
        self.clock = clock;
        self
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
        let now = self.clock.now();
        let update = match &mut self.osd {
            Some(controller) => controller.poll(now, surface),
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
    pub fn offscreen(width: u32, height: u32, rx: RenderRx) -> Result<Self, PipelineError> {
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
    ///
    /// `new_item` says the card is for a *different track*, not a fresh reading of the
    /// same one. It is the difference between a discontinuity and a measurement, and the
    /// projection needs to be told which it is getting: a track advancing from the phone
    /// publishes 0:00, and absorbing that as drift would leave the new song's bar showing
    /// the moment the last one had reached.
    fn set_transport(
        &mut self,
        model: &crate::transport::TransportModel,
        w: u32,
        h: u32,
        new_item: bool,
    ) {
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

        // Only an actually-*new* reading is fed to the projection. The card republishes
        // for reasons that have nothing to do with playback — a queue update, the device
        // naming itself — carrying the position it last stated, and treating one of those
        // as a measurement would rewind the scrubber by however long had passed since the
        // real reading. But matching on position alone confused "the same reading again"
        // with "a new track that also starts at 0:00": advancing a track from the phone
        // kept the old track's anchor, and the scrubber read 1:20 over a song at 0:00.
        // Any field of the reading changing — state, duration, shuffle — is a fresh
        // reading.
        let now = self.clock.now();
        let running = model.state.is_active();
        let position = model.position.unwrap_or(Duration::ZERO);
        let projection = match self.transport.as_ref() {
            Some(prev) if prev.model == *model && !new_item => prev.projection,
            Some(prev) if !new_item => {
                let mut projection = prev.projection;
                projection.observe(position, now, running, model.duration);
                projection
            }
            _ => crate::projection::Projection::new(position, now, running, model.duration),
        };
        // A drag survives the source republishing under it, and it has to: sources publish
        // for reasons that have nothing to do with the finger — a position tick, a queue
        // update — and dropping the preview on one would make the bar snap back to the
        // music halfway through a scrub, which reads as the panel fighting the hand.
        let preview = self.transport.as_ref().and_then(|prev| prev.preview);

        match self.paint_transport(model, pw, ph, placement.1, h) {
            Ok(()) => {
                self.transport = Some(TransportState {
                    layout: crate::transport::layout(model, pw, ph),
                    model: model.clone(),
                    placement,
                    projection,
                    painted: model.position,
                    preview,
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
        let now = self.clock.now();
        let Some(live) = state.showing(now) else {
            return;
        };
        if !state.stale(live) {
            return;
        }

        // The position carries the preview rather than a separate fraction reaching the
        // painter, so the elapsed *readout* follows the finger too. Reading 1:07 while the
        // bar sits at a third of a three-minute track is two answers to one question.
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

    /// Whether a session surface currently holds the whole panel — the state in which
    /// the home pill stays dimly present rather than fading out, because it is the one
    /// exit affordance every app view shares.
    #[must_use]
    pub fn session_fullscreen(&self) -> bool {
        matches!(self.panel.focus(), crate::panel::Focus::Session)
    }

    /// Whether the mascot overlay would actually be drawn this frame — present and
    /// not suppressed or yielded away. For tests.
    #[must_use]
    pub fn mascot_on_glass(&self) -> bool {
        self.compositor.has_layer(LayerId::MascotOverlay)
            && !self.compositor.hidden(LayerId::MascotOverlay)
    }

    /// The strip's live position estimate: the last published reading plus however
    /// long playback has run since. For tests and logs.
    #[must_use]
    pub fn transport_position(&self) -> Option<Duration> {
        let now = self.clock.now();
        self.transport
            .as_ref()
            .and_then(|state| state.live_position(now))
    }

    /// What a touch at panel-normalized `(x, y)` means, if the strip is on screen.
    ///
    /// Returns the transaction rather than the hit: the caller is the input router and
    /// has no business knowing about scrub fractions, and the mapping needs the model
    /// this loop is holding anyway.
    ///
    /// A preview is *applied* here rather than returned, for the same reason: it is a
    /// picture this loop owes the glass, not a decision the router has to relay. Takes
    /// `&mut self` for that, and the drag's repaint lands on the next tick.
    pub fn transport_action(
        &mut self,
        x: f32,
        y: f32,
        phase: crate::transport::TouchPhase,
    ) -> Option<castaway_core::ControlTxn> {
        use crate::transport::StripTouch;
        let state = self.transport.as_ref()?;
        if self.strip_covered(x, y) {
            return None;
        }
        let (sw, sh) = self.compositor.target_size();
        let (lx, ly) = crate::transport::to_strip_local(x, y, sw, sh);
        match state.layout.touch(lx, ly, phase)? {
            StripTouch::Preview(fraction) => {
                if let Some(state) = self.transport.as_mut() {
                    state.preview = Some(fraction);
                }
                None
            }
            StripTouch::Act(hit) => {
                // The lift commits, so the picture stops being a guess and goes back to
                // being the source's reading — which the seek is about to move anyway.
                self.clear_scrub_preview();
                let state = self.transport.as_ref()?;
                state.model.action(hit)
            }
        }
    }

    /// The strip's model as the source last described it, if a strip is on screen.
    ///
    /// For tests, which need the same layout the loop is hit-testing against — deriving
    /// one from a model of their own would pass while the real one was wrong.
    #[must_use]
    pub fn transport_model(&self) -> Option<crate::transport::TransportModel> {
        self.transport.as_ref().map(|s| s.model.clone())
    }

    /// Where the finger dragging the scrub track is, if one is.
    ///
    /// The input router reads it to know whether the press it just offered was taken as a
    /// scrub, and so whether the contact's later moves belong to the strip.
    #[must_use]
    pub fn scrub_preview(&self) -> Option<f32> {
        self.transport.as_ref().and_then(|s| s.preview)
    }

    /// Stop previewing, without seeking.
    ///
    /// For a cancel: a contact that was lost — the phone dropped off Wi-Fi mid-drag —
    /// did not *finish* the gesture, so the bar goes back to the source's reading and
    /// nothing is asked of the sender.
    pub fn clear_scrub_preview(&mut self) {
        if let Some(state) = self.transport.as_mut() {
            state.preview = None;
        }
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
        if self.strip_covered(x, y) {
            return false;
        }
        let (sw, sh) = self.compositor.target_size();
        let (lx, ly) = crate::transport::to_strip_local(x, y, sw, sh);
        state.layout.hit(lx, ly).is_some()
    }

    /// Whether the transport strip is covered here, and so neither owns a press nor acts on
    /// one.
    ///
    /// One answer, shared: the two used to carry the same check independently, with a comment
    /// saying it had to be repeated rather than assumed — and the moment the rule changed,
    /// only one of them changed with it, so a covered strip stopped *owning* presses and
    /// carried on *acting* on them.
    ///
    /// The strip sits below video, so a session that publishes metadata *and* pixels — a DLNA
    /// video with tags, say — used to leave an invisible strip swallowing the bottom of the
    /// glass and answering presses aimed at whatever was actually on screen.
    ///
    /// Answered from the model, not from the live layers, for the reason
    /// [`crate::panel::Panel::covered_by_any`] gives: a strip must not become answerable for
    /// the 300 ms a video spends arriving over it. And only for what is *above* it —
    /// `LayerId::Transport` is drawn directly on the card it belongs to, so that card
    /// covering this point is no reason for its own controls to stop answering.
    fn strip_covered(&self, x: f32, y: f32) -> bool {
        const ABOVE_THE_STRIP: [crate::panel::Surface; 2] = [
            crate::panel::Surface::Video,
            crate::panel::Surface::CastPage,
        ];
        self.panel.covered_by_any(x, y, &ABOVE_THE_STRIP)
            || self
                .compositor
                .covered_above(crate::compositor::LayerId::Transport, x, y)
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
            opacity: 1.0,
            transform: Transform::default(),
        });
        Ok(())
    }

    /// Upload a browser frame (BGRA8, as a CPU paint delivers it) as the `Browser`
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
        layer: LayerId,
    ) -> Result<(), PipelineError> {
        self.compositor.upload_texture_regions(
            layer,
            width,
            height,
            TexelFormat::Bgra8,
            bgra,
            dirty,
        )?;
        self.compositor.upsert_layer(Layer {
            id: layer,
            opacity: 1.0,
            transform,
        });
        Ok(())
    }

    /// Import a browser frame's GPU buffer and make it the browser layer.
    ///
    /// The zero-copy counterpart of [`Self::upload_browser`], which took a CPU copy of
    /// every frame because CEF's accelerated offscreen path was unusable upstream (#44). At
    /// 4K that copy was 33 MB per frame; this is a handful of driver objects.
    ///
    /// `handle` is the browser's buffer, already pulled into this process. It is consumed
    /// here, and `borrow` travels with it into the texture's owner: wgpu drops the pair
    /// when the last submission sampling the texture retires, which is what tells the
    /// caller's release logic (see `electron_browser::InFlight`) the browser may recycle
    /// the pixels.
    ///
    /// # Errors
    /// [`PipelineError::GpuImport`] if this device cannot import external memory or the
    /// geometry is one the single-plane path does not describe.
    #[cfg(feature = "hwaccel")]
    #[allow(clippy::too_many_arguments)]
    pub fn import_browser_frame(
        &mut self,
        geometry: crate::hwaccel::FrameGeometry,
        modifier: u64,
        span: crate::hwaccel::PlaneSpan,
        handle: crate::hwaccel::remote_handle::LocalHandle,
        borrow: Box<dyn std::any::Any + Send + Sync>,
        transform: Transform,
        layer: LayerId,
    ) -> Result<(), PipelineError> {
        let (texture, owner) = self
            .compositor
            .import_browser_frame(geometry, modifier, span, handle, borrow)?;
        self.compositor.adopt_rgba_texture(layer, texture, owner);
        self.compositor.upsert_layer(Layer {
            id: layer,
            opacity: 1.0,
            transform,
        });
        Ok(())
    }

    /// The pixel size of the shell screen's texture, if one is composited.
    ///
    /// For the test that the screen is drawn at the surface size rather than stretched
    /// to it — the failure is invisible on the 4K panel this ships on and wrong
    /// everywhere else.
    #[must_use]
    pub fn shell_layer_size(&self) -> Option<(u32, u32)> {
        self.compositor.layer_size(LayerId::Attract)
    }

    /// The pixel size of any layer's texture, if it has one. For the tests that ask
    /// "which surface is this picture actually on" — minimize/restore moves the browser
    /// between two layers, and presence alone cannot tell them apart.
    #[must_use]
    pub fn layer_size(&self, id: LayerId) -> Option<(u32, u32)> {
        self.compositor.layer_size(id)
    }

    /// Scroll the current screen, if it is something that scrolls.
    ///
    /// `dy` is a panel-normalized drag: positive is a finger moving down, which reveals
    /// what is *above*, the way a sheet of paper moves under a hand.
    pub fn shell_scroll(&mut self, dy: f32) -> bool {
        let (_, h) = self.compositor.target_size();
        let h = h.max(1);
        let Some(stack) = self.panel.stack_mut() else {
            return false;
        };
        let crate::shell::Screen::Picker(picker) = stack.current_mut() else {
            return false;
        };
        let l = crate::picker::layout(picker, 1, h);
        if picker.items.len() <= l.visible {
            return false;
        }
        let rows = -dy * h as f32 / l.row_step;
        picker.scroll_by(rows, l.visible);
        self.repaint_shell();
        true
    }

    /// Whether the shell screen at this point is one that scrolls, and has somewhere to
    /// scroll to.
    #[must_use]
    pub fn shell_scrollable(&self, x: f32, y: f32) -> bool {
        if self
            .compositor
            .covered_above(crate::compositor::LayerId::Attract, x, y)
        {
            return false;
        }
        let Some(crate::shell::Screen::Picker(picker)) = self.panel.current() else {
            return false;
        };
        let (_, h) = self.compositor.target_size();
        picker.items.len() > crate::picker::layout(picker, 1, h.max(1)).visible
    }

    /// Note that a finger touched the panel.
    ///
    /// Used by the idle policy: an ending session returns the panel Home, but not out
    /// from under someone who is using it.
    pub fn note_touch(&mut self) {
        self.last_touch = Some(self.clock.now());
    }

    /// Bring the shell in front, or hand the screen back to what is playing.
    ///
    /// The shell draws *below* video, so navigating while something plays would
    /// otherwise be invisible. Rather than hiding the video — someone pressing Home in
    /// the middle of a film has not asked for it to stop — it is demoted to a corner,
    /// which is what the compositor's PiP transform has been there for since the
    /// beginning (architecture §4).
    pub fn set_shell_foreground(&mut self, front: bool) {
        let moved = if front {
            self.panel.hand_to_shell()
        } else {
            self.panel.hand_to_session()
        };
        if moved {
            self.reflow_surfaces();
        }
    }

    /// Whether the shell has the glass.
    #[must_use]
    pub fn shell_foreground(&self) -> bool {
        self.panel.focus() == crate::panel::Focus::Shell
    }

    /// Whether there is a session surface to hand the glass back to.
    ///
    /// What the edge drag asks before it starts animating: with nothing playing, dragging
    /// the shell aside at Home uncovers nothing and is not a gesture.
    #[must_use]
    pub const fn can_hand_back(&self) -> bool {
        self.panel.can_hand_back()
    }

    /// What the panel is presenting, for the browser host and the kiosk's input routing.
    #[must_use]
    pub const fn panel(&self) -> &crate::panel::Panel {
        &self.panel
    }

    /// Record that a surface is up, or gone, and re-place everything if that changed the
    /// answer.
    ///
    /// The one way presence enters the model. Called where a surface really appears or
    /// leaves: a first video frame, a published card, a page shown or hidden, a deferred
    /// clear expiring.
    pub fn set_surface(&mut self, surface: crate::panel::Surface, present: bool) {
        if self.panel.set_surface(surface, present) {
            self.reflow_surfaces();
        }
    }

    /// Go one step out. See [`crate::panel::Left`].
    pub fn panel_back(&mut self) -> crate::panel::Left {
        let left = self.panel.back();
        match left {
            // A screen change is a navigation and gets the fade; the others only move
            // layers, which `reflow_surfaces` does.
            crate::panel::Left::Screen => self.repaint_shell(),
            crate::panel::Left::Demoted | crate::panel::Left::Nothing => {}
        }
        self.reflow_surfaces();
        left
    }

    /// Hand the glass back to what is playing. Returns whether anything moved.
    pub fn panel_restore(&mut self) -> bool {
        let moved = self.panel.hand_to_session();
        if moved {
            self.reflow_surfaces();
        }
        moved
    }

    /// Point every surface at where the panel says it goes.
    ///
    /// Sets *targets*, and nothing else: what a surface does about a new target — travel,
    /// arrive from an origin, leave — is [`crate::motion`]'s, and the layers are written by
    /// [`Self::tick_motion`] once per frame. Splitting it this way is what makes every
    /// placement change an animation for free: a caller that changes the panel's state does
    /// not have to know, or say, how the result should move.
    ///
    /// Called whenever an input to the answer changes — focus, the screen stack, a surface
    /// appearing — and once per pump as a standing fact, because navigation happens in
    /// several places and none of them should have to remember this.
    fn reflow_surfaces(&mut self) {
        use crate::motion::{Choreography, Step};
        use crate::panel::Surface;
        let no_shell = self.panel.depth() == 0;
        for (i, surface) in Surface::ALL.into_iter().enumerate() {
            let want = self.panel.placement(surface);
            let present = self.panel.surfaces().has(surface);
            let had = self.placed.get(i).copied().unwrap_or(None);
            let now = present.then_some(want);
            if had == now {
                continue;
            }
            if let Some(slot) = self.placed.get_mut(i) {
                *slot = now;
            }
            // No session surface has a spatial origin today, and saying so plainly beats a
            // field that reads as if one might: a surface being *summoned* out of the corner is
            // a `Moving` step that carries its own velocity and wants no origin, and every
            // genuine arrival — a phone casting, a DIAL launch, a track starting — reaches the
            // panel across an async round trip that no touch survives. The screens are the ones
            // with origins, and they carry theirs on the navigation (`ScreenStack`).
            //
            // What would change this: a launch whose *whole* path is local, like a GameStream
            // app started from a picker row, if the row's rect were threaded through to the
            // session it starts.
            let origin = crate::motion::Origin::Nowhere;
            let step = Step {
                from: had,
                to: now,
                origin,
            };
            let spring = Choreography::spring(step);
            let target = self.target_for(surface, want, present);
            let Some(motion) = self.motions.get_mut(surface) else {
                continue;
            };
            if no_shell {
                // Nobody is watching a renderer with no shell — see `Motion::snap`.
                motion.snap(target);
            } else {
                match (had, now) {
                    // Arriving on the panel: placed at its origin before its first frame, so
                    // there is no flash of the destination first.
                    //
                    // Unless it is still *here* — a surface part-way through its exit that comes
                    // back (a preemption, a stop immediately followed by a play) reverses from
                    // wherever it has got to, keeping its velocity. Restarting it from an origin
                    // it has already left is a jump, and it is the one interruption the whole
                    // per-component spring arrangement exists to make free.
                    (None, Some(_)) if !motion.drawn() => motion.enter(target, origin, spring),
                    (Some(_), None) => motion.leave(spring),
                    _ => motion.move_to(spring),
                }
            }
        }
    }

    /// Where `surface` is heading, and how visible it should be there.
    fn target_for(
        &self,
        surface: crate::panel::Surface,
        want: crate::panel::Placement,
        present: bool,
    ) -> crate::motion::Target {
        use crate::motion::{Choreography, Target};
        use crate::panel::{demoted_rect, NormRect, Placement};
        let (w, h) = self.compositor.target_size();
        let resting = match want {
            Placement::Panel => NormRect::FULL,
            Placement::Widget | Placement::Hidden => {
                demoted_rect(surface, w, h).unwrap_or(NormRect::FULL)
            }
        };
        if !present || matches!(want, Placement::Hidden) {
            // Leaving, or pushed off by a screen: inward and out, the mirror of arrival.
            return Target {
                rect: resting.scaled(Choreography::EXIT_SCALE),
                opacity: 0.0,
            };
        }
        Target {
            rect: resting,
            opacity: 1.0,
        }
    }

    /// Advance every motion by `dt` and write the layers. Returns whether anything is still
    /// moving, so the kiosk knows to ask for another frame.
    ///
    /// The one place a layer transform is written for a session surface. Everything else sets
    /// targets.
    pub fn tick_motion(&mut self, dt: std::time::Duration) -> bool {
        use crate::motion::Phase;
        use crate::panel::Surface;
        let dt = dt.as_secs_f32();
        let mut moving = false;
        for surface in Surface::ALL {
            let want = self.panel.placement(surface);
            let present = self.panel.surfaces().has(surface);
            let target = self.target_for(surface, want, present);
            let Some(motion) = self.motions.get_mut(surface) else {
                continue;
            };
            if motion.phase() == Phase::Absent {
                continue;
            }
            moving |= motion.step(target, dt);
        }
        let recessed = self.panel.focus() == crate::panel::Focus::Session;
        moving |= self.floor.step(recessed, dt);
        self.apply_motions();
        self.animating = moving;
        moving
    }

    /// Write every layer from where its motion currently has it, without advancing time.
    ///
    /// Called after stepping *and* before presenting, so the frame on the glass always matches
    /// the motion state. That matters most on the frame an entrance begins: the motion is
    /// placed at its origin during a reflow, and presenting before applying would show one
    /// frame at the destination first — a flash, and one flash is worse than no animation.
    fn apply_motions(&mut self) {
        use crate::panel::Surface;
        for surface in Surface::ALL {
            let Some(motion) = self.motions.get_mut(surface) else {
                continue;
            };
            let (frame, opacity, drawn) = (motion.frame(), motion.opacity(), motion.drawn());
            self.apply_motion(surface, frame, opacity, drawn);
        }
        self.apply_floor();
    }

    /// Put one surface's layer where its motion currently has it.
    fn apply_motion(
        &mut self,
        surface: crate::panel::Surface,
        frame: crate::panel::NormRect,
        opacity: f32,
        drawn: bool,
    ) {
        use crate::panel::Surface;
        let layer = match surface {
            Surface::Video => LayerId::Video,
            Surface::Card => LayerId::NowPlaying,
            // The browser's own layers are placed by the browser host, which owns the
            // viewport the page rasterizes into — a placement change there is a *resize*
            // round trip, not a transform (see `ElectronHost::follow_panel`). Animating the
            // transform underneath it would stretch a page rasterized for the other rect,
            // which is worse than not animating it at all until phase 2 gives the compositor
            // a source rect of its own.
            Surface::CastPage | Surface::IdleWidget => return,
        };
        if !self.compositor.has_layer(layer) {
            return;
        }
        self.compositor.set_suppressed(layer, !drawn);
        if !drawn {
            return;
        }
        // Video is the one layer with a shape of its own — the decoded frame's, which the
        // compositor did not choose — so it is *fitted* into its slot with the remainder
        // matted, rather than stretched across it. The layer keeps the whole slot: the
        // bars belong to it, so nothing underneath can show through them.
        if layer == LayerId::Video {
            let (pw, ph) = self.compositor.target_size();
            let source = match self.video_size {
                Some((vw, vh)) => crate::compositor::contain_source(
                    (frame.w * pw as f32, frame.h * ph as f32),
                    (vw as f32, vh as f32),
                ),
                None => crate::compositor::FULL_SOURCE,
            };
            self.compositor.set_source(layer, source);
        }
        // The card is authored at the surface's own shape, while the widget slot is a
        // hard-coded 16:9 — so on a panel that is not 16:9 a demoted card is stretched today.
        // A no-op on one that is.
        if layer == LayerId::NowPlaying {
            let (pw, ph) = self.compositor.target_size();
            self.compositor.set_source(
                layer,
                crate::compositor::cover_source(
                    (frame.w * pw as f32, frame.h * ph as f32),
                    (pw as f32, ph as f32),
                ),
            );
        }
        self.compositor.upsert_layer(Layer {
            id: layer,
            opacity,
            transform: Transform {
                scale_x: frame.w,
                scale_y: frame.h,
                offset_x: frame.x,
                offset_y: frame.y,
            },
        });
        // The strip belongs to the card and is unusably small at anything but full size.
        if layer == LayerId::NowPlaying {
            self.compositor
                .set_suppressed(LayerId::Transport, frame.w < 0.999);
        }
    }

    /// Place the shell where the floor's motion has it: pushed back and dimmed under a
    /// session that has the glass, flat and bright when it has the panel.
    ///
    /// Also called at the end of a repaint, so a freshly rasterized screen lands in the
    /// floor's current placement instead of at full size for one frame.
    fn apply_floor(&mut self) {
        let Some((rect, dim)) = self.floor.placement() else {
            return;
        };
        let panel = self.compositor.target_size();
        let floor = Transform {
            scale_x: rect.w,
            scale_y: rect.h,
            offset_x: rect.x,
            offset_y: rect.y,
        };
        if self.compositor.has_layer(LayerId::Attract) {
            self.compositor
                .set_radius(LayerId::Attract, self.floor.radius());
            // Clip, do not stretch. The tiles are square and the panel is 16:9, so a screen
            // growing out of a tile would be compressed 44% horizontally for the first third of
            // its travel. A no-op once it has arrived, since a full-panel layer is exactly the
            // panel's shape — which is why it can be applied unconditionally.
            self.compositor.set_source(
                LayerId::Attract,
                crate::compositor::cover_source(
                    (rect.w * panel.0 as f32, rect.h * panel.1 as f32),
                    (panel.0 as f32, panel.1 as f32),
                ),
            );
            self.compositor.upsert_layer(Layer {
                id: LayerId::Attract,
                opacity: dim,
                transform: floor,
            });
        }
        // Composed, not replaced: she is a sub-rect of the scene and moves *with* it.
        if let Some(base) = self.mascot_base {
            if self.compositor.has_layer(LayerId::MascotOverlay) {
                self.compositor.upsert_layer(Layer {
                    id: LayerId::MascotOverlay,
                    opacity: dim * (1.0 - self.slot_veil()),
                    transform: compose(floor, base),
                });
            }
        }
    }

    /// How far a session has expanded past the slot, `0.0`..=`1.0`.
    ///
    /// What the mascot's visibility follows, and the arithmetic behind "she leans on the
    /// *slot*". At `0.0` the occupant is at its demoted size — in the slot, which is the card
    /// frame she is leaning over, so she stays and her arms land in front of it. At `1.0` it
    /// fills the panel, which is not the slot at all and no place for an ornament to be drawn
    /// over, so she is gone.
    ///
    /// A matter of degree rather than of presence, which is what makes it smooth for free:
    /// she fades out exactly as fast as the occupant grows, and back exactly as fast as it
    /// shrinks or leaves. Presence alone was a hard hide, and it popped.
    ///
    /// Video is included and rarely contributes: it demotes to the PiP corner rather than the
    /// slot, so a demoted video leaves her alone — correctly, since it is nowhere near her —
    /// while a full-screen one hides her like anything else that has taken the panel.
    fn slot_veil(&self) -> f32 {
        let (w, h) = self.compositor.target_size();
        crate::panel::Surface::ALL
            .into_iter()
            .filter(|s| s.is_session())
            .map(|surface| {
                let motion = self.motions.get(surface);
                if !motion.drawn() {
                    return 0.0;
                }
                let demoted = crate::panel::demoted_rect(surface, w, h)
                    .map_or(1.0, |r| r.w)
                    .clamp(0.0, 0.999);
                // How far between its demoted width and the whole panel it currently is.
                let expansion = (motion.frame().w - demoted) / (1.0 - demoted);
                expansion.clamp(0.0, 1.0) * motion.opacity()
            })
            .fold(0.0_f32, f32::max)
            .clamp(0.0, 1.0)
    }

    /// How visible the mascot is right now, for tests: the veil applied to the floor's dim.
    #[must_use]
    pub fn mascot_opacity(&self) -> Option<f32> {
        if !self.compositor.has_layer(LayerId::MascotOverlay) {
            return None;
        }
        let dim = self.floor.placement().map_or(1.0, |(_, dim)| dim);
        Some(dim * (1.0 - self.slot_veil()))
    }

    /// Whether the placement of any surface has moved since it was last applied.
    fn placement_moved(&self) -> bool {
        crate::panel::Surface::ALL
            .into_iter()
            .enumerate()
            .any(|(i, surface)| {
                let present = self.panel.surfaces().has(surface);
                let now = present.then(|| self.panel.placement(surface));
                self.placed.get(i).copied().unwrap_or(None) != now
            })
    }

    /// Where the demoted video sits, in panel-normalized coordinates. `None` when it is
    /// not demoted, or when there is no video.
    #[must_use]
    pub fn pip_rect(&self) -> Option<(f32, f32, f32, f32)> {
        use crate::panel::Surface;
        if !self.panel.placement(Surface::Video).is_widget() {
            return None;
        }
        let (w, h) = self.compositor.target_size();
        crate::panel::demoted_rect(Surface::Video, w, h).map(|r| (r.x, r.y, r.w, r.h))
    }

    /// Whether a panel-normalized point is on the demoted video.
    #[must_use]
    pub fn hit_pip(&self, x: f32, y: f32) -> bool {
        self.pip_rect()
            .is_some_and(|(ox, oy, sx, sy)| x >= ox && y >= oy && x <= ox + sx && y <= oy + sy)
    }

    /// Where the floor — the shell, and the screen it is showing — currently sits, and how
    /// bright it is. For tests, and for the browser host's own viewport arithmetic.
    #[must_use]
    pub fn floor_placement(&self) -> Option<(crate::panel::NormRect, f32)> {
        self.floor.placement()
    }

    /// Where the screen being navigated away from currently sits. `None` when no navigation is
    /// animating.
    ///
    /// For tests: "did it go back into the tile it came out of" is the assertion that keeps
    /// the way out from contradicting the way in, and it cannot be made from outside without
    /// being able to watch the outgoing layer.
    #[must_use]
    pub fn outgoing_screen_rect(&self) -> Option<crate::panel::NormRect> {
        let t = self.compositor.layer_transform(LayerId::ShellPrev)?;
        Some(crate::panel::NormRect {
            x: t.offset_x,
            y: t.offset_y,
            w: t.scale_x,
            h: t.scale_y,
        })
    }

    /// Where the now-playing card actually is right now, mid-animation included.
    ///
    /// For tests: "did it travel or did it teleport" cannot be asked from outside without
    /// being able to watch it, and it is the difference this whole module exists for.
    #[must_use]
    pub fn card_frame(&self) -> crate::panel::NormRect {
        self.motions.get(crate::panel::Surface::Card).frame()
    }

    /// How visible the now-playing card is right now. For tests, like [`Self::card_frame`].
    #[must_use]
    pub fn card_opacity(&self) -> f32 {
        self.motions.get(crate::panel::Surface::Card).opacity()
    }

    /// Whether a panel-normalized point is on the minimized now-playing card.
    #[must_use]
    pub fn hit_minimized_card(&self, x: f32, y: f32) -> bool {
        use crate::panel::Surface;
        if !self.panel.placement(Surface::Card).is_widget() {
            return false;
        }
        let (w, h) = self.compositor.target_size();
        crate::panel::demoted_rect(Surface::Card, w, h).is_some_and(|r| r.contains(x, y))
    }

    /// Draw the home pill and place it as the shell's overlay layer.
    ///
    /// # Errors
    /// [`PipelineError`] if the pill cannot be rasterized or uploaded.
    pub fn draw_home_pill(&mut self) -> Result<(), PipelineError> {
        let (w, h) = self.compositor.target_size();
        let (w, h) = (w.max(1), h.max(1));
        let (rgba, rect) = crate::overlay::render_pill(w, h)?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (pw, ph) = (rect.w.ceil() as u32, rect.h.ceil() as u32);
        self.compositor.upload_texture(
            LayerId::ShellOverlay,
            pw,
            ph,
            TexelFormat::Rgba8Srgb,
            &rgba,
        )?;
        self.compositor.upsert_layer(Layer {
            id: LayerId::ShellOverlay,
            opacity: 1.0,
            // One texel per device pixel, like every other authored surface.
            transform: Transform {
                scale_x: rect.w / w as f32,
                scale_y: rect.h / h as f32,
                offset_x: rect.x / w as f32,
                offset_y: rect.y / h as f32,
            },
        });
        Ok(())
    }

    /// Fade the pill without redrawing it — a uniform write, not a 4K upload.
    pub fn set_home_pill_opacity(&mut self, opacity: f32) {
        if !self.compositor.has_layer(LayerId::ShellOverlay) {
            return;
        }
        let (w, h) = self.compositor.target_size();
        let (w, h) = (w.max(1), h.max(1));
        let rect = crate::overlay::pill_rect(w, h);
        self.compositor.upsert_layer(Layer {
            id: LayerId::ShellOverlay,
            opacity: opacity.clamp(0.0, 1.0),
            transform: Transform {
                scale_x: rect.w / w as f32,
                scale_y: rect.h / h as f32,
                offset_x: rect.x / w as f32,
                offset_y: rect.y / h as f32,
            },
        });
    }

    /// Drop the pill.
    pub fn clear_home_pill(&mut self) {
        self.compositor.remove_layer(LayerId::ShellOverlay);
    }

    /// Whether the home pill is currently composited.
    #[must_use]
    pub fn home_pill_present(&self) -> bool {
        self.compositor.has_layer(LayerId::ShellOverlay)
    }

    /// Whether a browser frame is currently composited.
    ///
    /// Exists for the end-to-end test: "did a frame become a layer" is the question that
    /// separates a working import from a working *product*, and it cannot be answered
    /// from outside without this.
    #[must_use]
    pub fn browser_layer_present(&self) -> bool {
        self.compositor.has_layer(LayerId::BrowserWidget)
            || self.compositor.has_layer(LayerId::BrowserFullscreen)
    }

    /// Remove the browser's layers (browser hidden).
    ///
    /// Both — the whole-browser cases: shutdown, respawn, unrecoverable process death.
    /// With one window per surface, everything short of that is per-layer
    /// ([`Self::clear_browser_layer`]): taking a dismissed cast down must not take the
    /// live clock with it.
    pub fn clear_browser(&mut self) {
        self.compositor.remove_layer(LayerId::BrowserWidget);
        self.compositor.remove_layer(LayerId::BrowserFullscreen);
    }

    /// Remove one of the browser's two layers, leaving the other's picture alone.
    ///
    /// Anything that is not a browser layer is refused rather than removed: this method
    /// exists for the browser host, and a typo'd `LayerId` silently deleting the video
    /// layer would be a far stranger bug than a clear that visibly did not happen.
    pub fn clear_browser_layer(&mut self, id: LayerId) {
        if id.is_browser() {
            self.compositor.remove_layer(id);
        }
    }

    /// Where a browser window's page belongs right now: the viewport to rasterize into,
    /// the layer it maps onto, and whether it is on the glass at all.
    ///
    /// The whole of what the browser host used to decide for itself. It kept a
    /// `BrowserRole` it mutated on minimize/restore, plus a per-pump copy of a
    /// "widget covered" verdict it had to remember to refresh — a second state machine over
    /// the same slot as the now-playing card, coordinated by convention. Now the panel
    /// answers, the host follows, and the two cannot drift.
    ///
    /// `cast` distinguishes the two pages — and, since the two-window split, the two
    /// *windows*: a session's page (leanback, a Cast receiver), which can own the panel
    /// and be restored, and the idle screen's clock, which is Home's own furniture. Both
    /// windows exist at once and each follows its own answer, so the answers must never
    /// overlap: at most one of them names the widget slot at a time.
    #[must_use]
    pub fn page_view(&self, cast: bool) -> Option<crate::browser::BrowserView> {
        let surface = if cast {
            crate::panel::Surface::CastPage
        } else {
            crate::panel::Surface::IdleWidget
        };
        let role = match self.panel.placement(surface) {
            crate::panel::Placement::Panel => crate::browser::BrowserRole::Fullscreen,
            crate::panel::Placement::Widget => crate::browser::BrowserRole::AttractWidget,
            crate::panel::Placement::Hidden => return None,
        };
        if role == crate::browser::BrowserRole::AttractWidget {
            // A card in the slot outranks a page in it. Declared on the layer rather than
            // in the model, because it is a fact about depth (`LayerId::yields_to`) — but
            // the host needs it as an answer too, so that a touch on the slot goes to
            // whatever is actually drawn there.
            if LayerId::BrowserWidget
                .yields_to()
                .iter()
                .any(|&l| self.compositor.has_layer(l))
            {
                return None;
            }
            // A demoted cast page outranks the clock in the same hole. With one browser
            // window this could not collide — the clock was simply not loaded while a
            // cast existed — but the clock's window now paints continuously, and two
            // windows answering "the slot" would fight over one layer, frame against
            // frame. The clock's window keeps painting; its frames are dropped
            // unimported until the slot is its own again.
            if !cast
                && self.panel.placement(crate::panel::Surface::CastPage)
                    == crate::panel::Placement::Widget
            {
                return None;
            }
        }
        Some(role.view(self.compositor.target_size()))
    }

    /// One demand-driven frame (#59): drain pending commands, advance every animation by
    /// `dt`, present once, and answer when the next frame is owed.
    ///
    /// Commands are applied *before* the ticks so a motion a command just started is
    /// stepped — and counted as animating — in this same frame; the old
    /// ticks-then-pump order left the loop believing it was idle on exactly the frame
    /// something began to move.
    pub fn frame(&mut self, dt: std::time::Duration) -> crate::demand::Demand {
        // Clamped, because under demand-driven pacing `dt` is "time since the last
        // frame", and after an idle sleep that is minutes: a spring stepped by minutes
        // diverges rather than settles. A motion that begins after a long sleep starts
        // from its beginning, which is also what a person watching expects.
        let dt = dt.min(std::time::Duration::from_millis(50));
        while let Some(cmd) = self.rx.try_recv() {
            self.apply(cmd);
        }
        self.tick_transition(dt);
        self.tick_motion(dt);
        self.tick_transport();
        self.update_osd();
        self.present_and_serve_taps();
        self.demand(self.clock.now())
    }

    /// When this loop next owes the glass a frame, from standing facts alone.
    ///
    /// No flag anywhere is set to "schedule" a redraw; everything time-driven in the
    /// loop is either *continuous* (a spring mid-flight, a driven transition, a tap
    /// consuming output) or *scheduled* (a deferred clear, a banner's TTL, the
    /// transport clock's next visible second), and this reads both kinds off the state
    /// that already exists. Events from outside arrive by waking the kiosk instead.
    #[must_use]
    pub fn demand(&self, now: std::time::Instant) -> crate::demand::Demand {
        use crate::demand::Demand;
        // A tap holds the loop at display rate while attached: a screenshot retires
        // itself after one present, and a future HLS/DASH tap (#18 phase 2) wants a
        // steady cadence anyway.
        if self.transition.is_some() || self.animating || !self.taps.is_empty() {
            return Demand::Frame;
        }
        [
            self.video_clear_due,
            self.card_clear_due,
            self.transport_due(now),
            self.osd
                .as_ref()
                .and_then(crate::osd::OsdController::next_change),
        ]
        .into_iter()
        .flatten()
        .min()
        .map_or(Demand::Idle, Demand::At)
    }

    /// When the transport strip next changes what it shows.
    ///
    /// Two things move at two rates, and the earlier of them is the answer. The elapsed
    /// *readout* changes on the whole second. The *bar* slides, and the smallest change
    /// worth a frame is one pixel of travel — which scales with the item's length, so a
    /// three-minute track asks for far fewer frames than a one-minute one and neither is
    /// told a number.
    ///
    /// This is the half of #165 without which the projection would be invisible: the
    /// event loop sleeps on `Demand`, so a bar that is *modelled* as sliding but only
    /// woken for on the second still steps once a second on the glass.
    fn transport_due(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        let state = self.transport.as_ref()?;
        let live = state.live_position(now)?;
        let to_boundary = Duration::from_secs(1)
            .saturating_sub(Duration::from_nanos(u64::from(live.subsec_nanos())));
        let bar = state
            .pixel_of_travel()
            .and_then(|pixel| state.projection.time_to_advance(now, pixel));
        // A paused projection asks for nothing at all, and then neither does the readout:
        // a clock that is not running has no boundary to cross.
        let readout = state.projection.running().then_some(to_boundary);
        [readout, bar].into_iter().flatten().min().map(|d| now + d)
    }

    /// Drain all pending commands (non-blocking) and present once. Returns the number of
    /// video frames applied this pump.
    pub fn pump(&mut self) -> usize {
        let mut applied = 0;
        while let Some(cmd) = self.rx.try_recv() {
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
        if let Some(cmd) = self.rx.recv_timeout(timeout) {
            if self.apply(cmd) {
                applied += 1;
            }
            while let Some(cmd) = self.rx.try_recv() {
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
    /// Execute a scheduled layer clear whose grace has run out (see `ClearVideo`).
    fn run_due_clears(&mut self) {
        let now = self.clock.now();
        // A due clear starts the *exit*; it does not drop the layer. Removing it here is
        // what made a card vanish rather than leave, and it is why `Phase::Leaving` exists:
        // the layer is retired by `retire_finished` once the motion has actually gone.
        if self.video_clear_due.is_some_and(|due| now >= due) {
            self.video_clear_due = None;
            self.set_surface(crate::panel::Surface::Video, false);
        }
        if self.card_clear_due.is_some_and(|due| now >= due) {
            self.card_clear_due = None;
            self.set_surface(crate::panel::Surface::Card, false);
        }
        self.retire_finished();
    }

    /// Drop the layers of surfaces whose motion has finished leaving.
    ///
    /// The other half of a deferred clear. A surface is composited for as long as it is
    /// visible — including the whole of its exit — and only then does its texture go.
    fn retire_finished(&mut self) {
        use crate::motion::Phase;
        use crate::panel::Surface;
        for surface in Surface::ALL {
            if self.panel.surfaces().has(surface) {
                continue;
            }
            if self.motions.get(surface).phase() != Phase::Absent {
                continue;
            }
            match surface {
                Surface::Video => self.compositor.remove_layer(LayerId::Video),
                Surface::Card => {
                    self.compositor.remove_layer(LayerId::NowPlaying);
                    // The strip belongs to the card. Leaving it would offer controls for a
                    // session that has ended, wired to a remote that has been dropped.
                    self.compositor.remove_layer(LayerId::Transport);
                    self.transport = None;
                    self.card_shown = None;
                }
                Surface::CastPage | Surface::IdleWidget => {}
            }
        }
    }

    /// The close badge, derived fresh each frame like the placements themselves: on
    /// the glass exactly while a closable surface sits demoted in a visible slot, at
    /// that surface's corner. A standing fact, not a transition anyone has to run —
    /// which is what keeps it from ever outliving the thing it would close.
    fn place_close_badge(&mut self) {
        let (w, h) = self.compositor.target_size();
        let shown = crate::panel::Surface::ALL.into_iter().find(|s| {
            s.closable() && self.panel.placement(*s).is_widget() && self.panel.widget_slot_visible()
        });
        let rect = shown.and_then(|s| crate::panel::close_rect(s, w, h));
        let Some(rect) = rect else {
            self.compositor.remove_layer(LayerId::CloseAffordance);
            self.close_badge_side = 0;
            return;
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let side = (rect.w * w.max(1) as f32).round().max(8.0) as u32;
        if side != self.close_badge_side {
            let rgba = crate::overlay::render_close_badge(side);
            if let Err(e) = self.compositor.upload_texture(
                LayerId::CloseAffordance,
                side,
                side,
                TexelFormat::Rgba8Srgb,
                &rgba,
            ) {
                error!(error = %e, "failed to draw the close badge");
                return;
            }
            self.close_badge_side = side;
        }
        self.compositor.upsert_layer(Layer {
            id: LayerId::CloseAffordance,
            opacity: 1.0,
            transform: Transform {
                scale_x: rect.w,
                scale_y: rect.h,
                offset_x: rect.x,
                offset_y: rect.y,
            },
        });
    }

    fn present_and_serve_taps(&mut self) {
        self.run_due_clears();
        // Every placement, recomputed and handed down each frame, so it is a standing fact
        // rather than a transition somebody has to catch — and applied only when the answer
        // moved, so the uniform writes stay one-per-change. Precedence *between* two
        // surfaces in the same slot is not decided here: the compositor derives that from
        // `LayerId::yields_to`.
        //
        // The idle widget page leaves with its surface; the mascot does not follow it.
        // She leans on the slot's card *frame*, which is baked into the Home floor and
        // visible whether or not a page occupies the hole — tying her to the page was
        // the bug where coming Home with the one browser off being YouTube showed her
        // torso (in the floor texture) and suppressed the rest of her.
        let widget_hidden = !self
            .panel
            .placement(crate::panel::Surface::IdleWidget)
            .visible()
            || !self.panel.surfaces().has(crate::panel::Surface::IdleWidget);
        self.compositor
            .set_suppressed(LayerId::BrowserWidget, widget_hidden);
        self.compositor
            .set_suppressed(LayerId::MascotOverlay, !self.panel.widget_slot_visible());
        self.place_close_badge();
        if self.placement_moved() {
            self.reflow_surfaces();
        }
        // The layers, from wherever the motions have got to. Idempotent uniform writes, and
        // the reason a presented frame can never disagree with the animation.
        self.apply_motions();
        if self.taps.is_empty() {
            self.compositor.present();
            return;
        }
        let now = self.clock.now();
        // Ask everyone first, then read back once per *distinct* shape. Two screenshots
        // racing, or a screenshot taken while the stream is running, must not cost two
        // full-surface copies — and which taps can share one is a fact only this loop has,
        // which is why the compositor is handed the deduplicated list rather than working
        // it out itself.
        let mut shapes: Vec<crate::tap::FrameWant> = Vec::new();
        let mut served: Vec<(usize, usize)> = Vec::new();
        for (i, tap) in self.taps.iter_mut().enumerate() {
            let Some(want) = tap.wants_frame(now) else {
                continue;
            };
            let slot = shapes.iter().position(|w| *w == want).unwrap_or_else(|| {
                shapes.push(want);
                shapes.len() - 1
            });
            served.push((i, slot));
        }
        let captured = self.compositor.present_and_capture(&shapes);
        for (tap_index, slot) in served {
            let (Some(Some(frame)), Some(tap)) = (captured.get(slot), self.taps.get_mut(tap_index))
            else {
                continue;
            };
            tap.on_frame(&frame.as_tapped());
        }
        self.taps.retain(|t| !t.finished());
    }

    /// Apply one command. Returns true if it was a video frame.
    fn apply(&mut self, cmd: RenderCommand) -> bool {
        match cmd {
            RenderCommand::Video(frame) => {
                // A frame arriving is the session speaking; whatever clear was pending
                // belonged to the item this one replaces.
                self.video_clear_due = None;
                // Two ways pixels reach the video layer: uploaded from system memory
                // (software decode) or imported in place from a surface the decoder
                // produced on the GPU (hwaccel). Only the second one avoids the copy.
                let landed = match &frame.image {
                    FrameImage::Cpu { format, data } => {
                        // Video samples are gamma-encoded (BT.709/601 transfer, which the
                        // NV12 shader already treats as sRGB) — the sampler must decode
                        // them before the compositor blends in linear, or the sRGB
                        // swapchain re-encodes on store and the picture reaches the panel
                        // washed out. The same double-encode the hardware path fixed.
                        let format = match format {
                            PixelFormat::Bgra8 => TexelFormat::Bgra8Srgb,
                            // Planar YUV is converted by swscale in the decoder; if a
                            // frame slips through (or a future variant appears), treat
                            // the bytes as RGBA (better a wrong frame than a panic).
                            _ => TexelFormat::Rgba8Srgb,
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
                    // The frame's shape decides how the layer is fitted, and it changes
                    // under us: a phone being turned over re-encodes at the other
                    // orientation and the next frame is a different shape entirely.
                    // Re-place the layer when it moves, not just when the panel does.
                    let size = (frame.width, frame.height);
                    if self.video_size != Some(size) {
                        debug!(
                            width = frame.width,
                            height = frame.height,
                            "render loop: video shape changed; refitting"
                        );
                        self.video_size = Some(size);
                        self.apply_motions();
                    }
                    // Placed by the panel, not unconditionally full-screen: a cast that
                    // starts while someone is navigating arrives demoted rather than
                    // covering what they are reading.
                    self.set_surface(crate::panel::Surface::Video, true);
                    return true;
                }
                false
            }
            RenderCommand::NowPlaying(card) => {
                // A live card cancels a pending clear, exactly as a video frame does.
                self.card_clear_due = None;
                // Rendered here rather than upstream: the metadata is a few hundred bytes
                // and the pixels are tens of megabytes, so the channel carries the small
                // one and this thread — which owns the surface size — makes the big one.
                let (w, h) = self.card_size();
                // …and only when the pixels would differ. Position ticks once a second
                // and the card does not draw it — that is the strip's job — so a card
                // that changed in nothing but position skips the big raster entirely.
                let same_pixels = self
                    .card_shown
                    .as_ref()
                    .is_some_and(|(prev, size)| *size == (w, h) && prev.visual_eq(&card));
                if !same_pixels {
                    match crate::nowplaying_card::render(&card, w, h) {
                        Ok(rgba) => {
                            if let Err(e) = self.set_now_playing(w, h, &rgba) {
                                error!(error = %e, "failed to draw the now-playing card");
                            }
                        }
                        Err(e) => error!(error = %e, "failed to render the now-playing card"),
                    }
                }
                // A card that draws differently is a different track, which is the same
                // question `same_pixels` asks and a strictly narrower one — it must not
                // pick up a mere resize.
                let new_item = self
                    .card_shown
                    .as_ref()
                    .is_none_or(|(prev, _)| !prev.visual_eq(&card));
                self.set_transport(&card.transport(), w, h, new_item);
                self.card_shown = Some((card.clone(), (w, h)));
                // A card published while someone is on the home screen arrives
                // minimized rather than snatching the panel from under them.
                self.set_surface(crate::panel::Surface::Card, true);
                false
            }
            RenderCommand::Home(scene) => {
                self.set_home(*scene);
                false
            }
            RenderCommand::PushScreen(screen) => {
                self.shell_push(*screen);
                false
            }
            RenderCommand::ReplaceScreen(screen) => {
                if self.panel.stack().is_some() {
                    self.panel.replace_top(*screen);
                    self.repaint_shell();
                    self.reflow_surfaces();
                }
                false
            }
            RenderCommand::ShellBack => {
                self.shell_back();
                false
            }
            RenderCommand::RestPanel => {
                self.rest_panel_if_idle();
                false
            }
            #[cfg(feature = "stream")]
            RenderCommand::StartStream {
                state,
                config,
                audio,
            } => {
                let (width, height) =
                    crate::stream::stream_size(self.compositor.target_size(), config.max_height);
                info!(width, height, "starting the output stream");
                self.taps.push(Box::new(crate::stream::StreamTap::new(
                    state, config, width, height, audio,
                )));
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
                // Deferred, like `ClearVideo` and for the same seek-shaped reason; the
                // strip goes with the card when the grace runs out.
                self.card_clear_due = Some(self.clock.now() + CLEAR_GRACE);
                false
            }
            RenderCommand::ClearVideo => {
                // The item is over, so the frames of it still queued are frames of
                // something that has ended. Dropping them is not an optimisation: the
                // control lane is drained before the frame lane, so a clear is always
                // seen *before* the stragglers it refers to — and since applying a frame
                // cancels a pending clear (below), leaving them queued means the last one
                // cancels this clear and nothing ever re-arms it. The panel then holds the
                // final frame of a finished cast for the life of the session, which is
                // exactly the frozen-frame bug the grace was added to fix, arriving
                // through the grace instead of around it. Found by #98's first
                // end-to-end run against a real compositor.
                self.rx.discard_queued_frames();

                // Only *then*, and not yet. A control point that cannot seek in-band
                // restarts the item — VLC's live stream sends STOP then a fresh LOAD for
                // every scrub — and clearing here bared the idle screen for the
                // half-second between them. The clear is scheduled instead, and a frame
                // of the *next* item cancels it; a stop that really is the end of the
                // session clears when the grace runs out, which is too brief to read as
                // the frozen frame it would otherwise be.
                self.video_clear_due = Some(self.clock.now() + CLEAR_GRACE);
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
        // Redraw the shell at the new size rather than letting the GPU stretch what was
        // drawn for the old one. The idle screen is a dithered gradient and fine text;
        // upscaling smears both, which is exactly the banding the dither exists to avoid.
        if let Err(e) = self.paint_screen() {
            warn!(error = %e, "failed to redraw the shell screen after a resize");
        }
    }

    /// Set or refresh the Home screen, keeping whatever is stacked above it.
    ///
    /// Refreshing must not navigate: Home is rebuilt whenever the receiver's state
    /// changes, and a host appearing on the LAN should not close a picker someone is
    /// reading.
    pub fn set_home(&mut self, scene: crate::attract::AttractScene) {
        self.panel.set_home(scene);
        self.repaint_shell();
        self.reflow_surfaces();
    }

    /// Push a screen on top of the current one, from nowhere in particular.
    pub fn shell_push(&mut self, screen: crate::shell::Screen) {
        self.shell_push_from(screen, None);
    }

    /// Push a screen on top of the current one, recording what it grew out of.
    ///
    /// `from` is the rect of the thing that was pressed. It is both what the screen's own
    /// entrance animates out of and — because the stack remembers it — what `back` shrinks it
    /// back into, so the way out cannot contradict the way in.
    pub fn shell_push_from(
        &mut self,
        screen: crate::shell::Screen,
        from: Option<crate::panel::NormRect>,
    ) {
        if self.panel.stack().is_none() {
            return;
        }
        // Going deeper. With a place to come from, the *arriving* screen is what moves —
        // growing out of the tile — and the one being left holds still and fades early. With
        // no place, the pair travels along the axis instead.
        let (_, panel_h) = self.compositor.target_size();
        let (leaving, from_rect, spring, radius) = match from {
            Some(rect) => (
                Leaving::Yield,
                rect,
                crate::motion::Choreography::container(),
                // The tile's own corner, in device pixels, flattening as the screen grows: a
                // square-cornered rectangle emerging from a rounded one reads as two objects
                // rather than one expanding.
                rect.h * panel_h as f32 * crate::attract::TILE_RADIUS,
            ),
            None => (
                Leaving::Slide { direction: -1.0 },
                crate::motion::Choreography::off_panel(-1.0),
                crate::motion::Choreography::shared_axis(),
                0.0,
            ),
        };
        self.begin_transition(false, leaving);
        self.transition_undo = Some(Undo::Pop);
        self.panel.push_from(screen, from);
        self.repaint_shell();
        // After the repaint, so the layer it launches is already the new screen's texture.
        self.floor.launch(from_rect, spring, radius);
        self.reflow_surfaces();
    }

    /// Go back one screen. Returns whether anything moved.
    ///
    /// Screens only: this is the shell's own step, not the panel's. One press "out" is
    /// [`Self::panel_back`], which leaves a fullscreen session first.
    pub fn shell_back(&mut self) -> bool {
        if self.panel.depth() <= 1 {
            return false;
        }
        // Before the stack changes, so the screen being left is still the current one
        // and can be drawn into its own layer to travel away — and so its origin is still
        // readable, which is what says where it has to go back to.
        let leaving = match self.panel.current_origin() {
            Some(rect) => Leaving::Into(rect),
            None => Leaving::Slide { direction: 1.0 },
        };
        self.begin_transition(false, leaving);
        self.transition_undo = Some(Undo::Restore(self.panel.above_screens()));
        let moved = self.panel.pop_screen();
        if moved {
            self.repaint_shell();
            // The screen underneath arrives too: from off-panel along the axis, or — when the
            // one leaving is shrinking back into a tile — from that tile's place, so the pair
            // reads as one movement rather than two.
            let (from, spring) = match leaving {
                Leaving::Slide { .. } => (
                    crate::motion::Choreography::off_panel(1.0),
                    crate::motion::Choreography::shared_axis(),
                ),
                // Home was already whole behind it; launching it from a hair inside itself
                // gives `carried` something to interpolate for a driven drag without any
                // visible movement of its own.
                Leaving::Into(_) | Leaving::Yield => (
                    crate::panel::NormRect::FULL,
                    crate::motion::Choreography::container(),
                ),
            };
            self.floor.launch(from, spring, 0.0);
            self.reflow_surfaces();
        }
        moved
    }

    /// Return to Home. Returns whether anything moved.
    pub fn shell_home(&mut self) -> bool {
        let deep = self.panel.depth() > 1;
        if deep {
            // Home is a return, whatever route got here: the screen goes back where it came
            // from if it came from somewhere, and otherwise off the way it arrived.
            let leaving = match self.panel.current_origin() {
                Some(rect) => Leaving::Into(rect),
                None => Leaving::Slide { direction: 1.0 },
            };
            self.begin_transition(false, leaving);
            self.transition_undo = Some(Undo::Restore(self.panel.above_screens()));
        }
        self.panel.go_home();
        if deep {
            self.repaint_shell();
        }
        self.reflow_surfaces();
        deep
    }

    /// Put the panel back to its resting arrangement — Home, glass handed to whatever is
    /// playing — unless somebody is using it.
    ///
    /// The idle return (#27) and a starting session's claim on the glass, which are the same
    /// act and are declined on the same terms: neither end of a session is a reason to close
    /// a picker out from under someone. Public because a page that arrives outside the
    /// `Pipeline` trait — a DIAL launch, which comes in as a browser command — is a session
    /// starting too, and has to claim the panel the same way.
    pub fn rest_panel_if_idle(&mut self) {
        if self.last_touch.is_some_and(|t| t.elapsed() < IDLE_GRACE) {
            // `info`, not `debug`: this is the whole explanation for "I cast something
            // and the panel didn't change", and the on-disk log only keeps `info` up.
            info!("shell: a session claimed the panel, but it was touched recently; staying put");
            return;
        }
        let deep = self.panel.depth() > 1;
        if deep {
            let leaving = match self.panel.current_origin() {
                Some(rect) => Leaving::Into(rect),
                None => Leaving::Slide { direction: 1.0 },
            };
            self.begin_transition(false, leaving);
            self.transition_undo = Some(Undo::Restore(self.panel.above_screens()));
        }
        self.panel.rest();
        if deep {
            self.repaint_shell();
        }
        self.reflow_surfaces();
    }

    /// How deep the shell is; `1` is Home. For tests and logs.
    #[must_use]
    pub fn shell_depth(&self) -> usize {
        self.panel.depth()
    }

    /// What a panel-normalized touch would hit on the current shell screen.
    ///
    /// `None` when the shell is covered there: its screens sit at the bottom of the
    /// stack, so a cast, a fullscreen browser or the now-playing card is in front of
    /// them and owns that part of the glass. Same rule the transport strip follows.
    #[must_use]
    pub fn shell_hit(&self, x: f32, y: f32) -> Option<crate::shell::ScreenHit> {
        match self.panel_hit(x, y) {
            crate::panel::PanelHit::Shell(hit) => Some(hit),
            crate::panel::PanelHit::Restore(_)
            | crate::panel::PanelHit::Close(_)
            | crate::panel::PanelHit::Miss => None,
        }
    }

    /// What a panel-normalized press means. The one routing answer — see
    /// [`crate::panel::Panel::hit`].
    ///
    /// The compositor's occlusion check is passed in as the veto for surfaces the model
    /// does not describe: the transport strip, the OSD, the mascot's arms.
    #[must_use]
    pub fn panel_hit(&self, x: f32, y: f32) -> crate::panel::PanelHit {
        let (w, h) = self.compositor.target_size();
        self.panel.hit(w, h, x, y, |x, y| {
            self.compositor
                .covered_above(crate::compositor::LayerId::Attract, x, y)
        })
    }

    /// Begin a crossfade away from whatever is currently drawn.
    ///
    /// Called *before* the stack changes, so the outgoing screen is still the current one
    /// and can be rasterised into its own layer.
    fn begin_transition(&mut self, driven: bool, leaving: Leaving) {
        // Re-rasterise rather than copy: the compositor has no texture-to-texture copy,
        // and this happens once per navigation — a human action, not a frame.
        let Some(screen) = self.panel.current() else {
            return;
        };
        let (w, h) = self.compositor.target_size();
        let (w, h) = (w.max(1), h.max(1));
        let rgba = match self.render_screen(screen, w, h) {
            Ok(rgba) => rgba,
            Err(e) => {
                warn!(error = %e, "could not draw the outgoing screen; navigating without a fade");
                return;
            }
        };
        if self
            .compositor
            .upload_texture(LayerId::ShellPrev, w, h, TexelFormat::Rgba8Srgb, &rgba)
            .is_err()
        {
            return;
        }
        self.compositor.upsert_layer(Layer {
            id: LayerId::ShellPrev,
            opacity: 1.0,
            transform: Transform::default(),
        });
        self.transition = Some(Transition {
            leaving,
            spring: match leaving {
                // A screen going back where it came from is the deliberate one, watched all the
                // way; the axis slide is the ordinary one.
                Leaving::Into(_) | Leaving::Yield => crate::motion::Choreography::container(),
                Leaving::Slide { .. } => crate::motion::Choreography::shared_axis(),
            },
            progress: 1.0,
            velocity: 0.0,
            target: 0.0,
            driven,
        });
        self.apply_transition(1.0);
    }

    /// Advance an unattended transition. Returns whether anything is still animating.
    pub fn tick_transition(&mut self, dt: std::time::Duration) -> bool {
        let Some(t) = self.transition.as_mut() else {
            return false;
        };
        if t.driven {
            return true;
        }
        let dt = dt.as_secs_f32().min(0.05);
        // One spring, taking the finger's speed as its initial velocity — which is what a
        // spring is *for*, and what makes a flick keep going after the hand has stopped without
        // a separate decay term to tune against a separate pull term.
        let (p, v) = t.spring.step(t.progress, t.velocity, t.target, dt);
        t.progress = p;
        t.velocity = v;
        let target = t.target;
        if crate::motion::Spring::settled(p, v, target) {
            if target >= 0.5 {
                // Sprung back: the navigation is undone, and the card is whole again.
                self.undo_transition();
            } else {
                self.end_transition();
            }
            return false;
        }
        self.apply_transition(p.clamp(0.0, 1.2));
        true
    }

    /// Set a driven transition's progress directly, from a finger's travel.
    ///
    /// `shown` is how much of the *outgoing* screen should still be visible, so a drag
    /// that is half-way leaves the panel half-way — and letting go without finishing
    /// puts it back, which is what makes the gesture feel like it is attached to the
    /// hand rather than a switch that fires.
    pub fn drive_transition(&mut self, shown: f32, velocity: f32) {
        let shown = shown.clamp(0.0, 1.0);
        if let Some(t) = self.transition.as_mut() {
            t.progress = shown;
            t.velocity = velocity;
            t.driven = true;
        }
        self.apply_transition(shown);
        // The screen being navigated *to* comes with the finger as well. Only the outgoing one
        // moved before, so a half-completed swipe slid one screen aside to reveal the next
        // already sitting there whole — which reads as two unrelated things rather than as one
        // navigation being carried.
        let recessed = self.panel.focus() == crate::panel::Focus::Session;
        if let Some(at) = self.floor.carried(shown, recessed) {
            self.floor.drive(at);
            self.apply_floor();
        }
    }

    /// Let go: where it ends up is decided here, from where it was released and how fast
    /// it was moving.
    ///
    /// A flick wins over position — someone who threw the card away meant it, even if
    /// they let go early — and otherwise it goes wherever it was more than half way to.
    pub fn release_transition(&mut self) {
        if let Some(t) = self.transition.as_mut() {
            t.driven = false;
            t.target = if t.velocity <= -FLICK {
                0.0
            } else if t.velocity >= FLICK {
                1.0
            } else {
                f32::from(t.progress > 0.5)
            };
        }
    }

    /// Whether a navigation is animating.
    #[must_use]
    pub const fn transitioning(&self) -> bool {
        self.transition.is_some()
    }

    /// The outgoing screen's late fade: opaque for most of the travel, then a smooth
    /// ease out over the last stretch.
    ///
    /// This was `(p * 4.0).clamp(0.0, 1.0)` — a slope-4 linear ramp with hard corners
    /// at both ends, which is a visible pop on a two-metre panel (the largest single
    /// frame-to-frame opacity step at 60 fps was ~0.22). A smoothstep over a slightly
    /// longer tail has zero slope at both ends, so the fade starts and finishes
    /// without a corner, and its worst step is well under half the old one.
    fn fade_late(p: f32) -> f32 {
        let t = (p / 0.45).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    /// Place the outgoing screen for `p`: whole at 1.0, gone at 0.0.
    fn apply_transition(&mut self, p: f32) {
        let leaving = self
            .transition
            .map_or(Leaving::Slide { direction: 1.0 }, |t| t.leaving);
        let (transform, opacity) = match leaving {
            Leaving::Slide { direction } => {
                let scale = CARD_MIN_SCALE + (1.0 - CARD_MIN_SCALE) * p;
                // Centred as it shrinks, then carried a whole panel-width on its way out.
                let centre = (1.0 - scale) / 2.0;
                let travel = (1.0 - p) * direction;
                (
                    Transform {
                        scale_x: scale,
                        scale_y: scale,
                        offset_x: centre + travel,
                        offset_y: centre,
                    },
                    // Only fading at the very end, so the card stays a card rather than a
                    // ghost.
                    Self::fade_late(p),
                )
            }
            Leaving::Into(rect) => {
                // Straight back into the thing it was opened out of. Linear in `p` because
                // `p` is already the spring's own curve.
                let lerp = |a: f32, b: f32| b + (a - b) * p;
                // …and rounding as it goes, the mirror of the growth that opened it.
                let (_, h) = self.compositor.target_size();
                let radius = rect.h * h as f32 * crate::attract::TILE_RADIUS * (1.0 - p);
                self.compositor.set_radius(LayerId::ShellPrev, radius);
                (
                    Transform {
                        scale_x: lerp(1.0, rect.w),
                        scale_y: lerp(1.0, rect.h),
                        offset_x: lerp(0.0, rect.x),
                        offset_y: lerp(0.0, rect.y),
                    },
                    // Late, so it is still legible as the screen right up to the tile.
                    Self::fade_late(p),
                )
            }
            // Held still; the arriving screen is the one moving.
            Leaving::Yield => (
                Transform::default(),
                // Gone early, so what remains of the transition is the new screen growing
                // over an empty panel rather than through a ghost of the old one — but
                // eased, not a linear ramp with corners at both ends.
                {
                    let t = ((p - (1.0 - OUTGOING_FADE)) / OUTGOING_FADE).clamp(0.0, 1.0);
                    t * t * (3.0 - 2.0 * t)
                },
            ),
        };
        self.compositor.upsert_layer(Layer {
            id: LayerId::ShellPrev,
            opacity,
            transform,
        });
    }

    /// A navigation that sprang back: put the stack where it was and drop the card.
    fn undo_transition(&mut self) {
        if let Some(undo) = self.transition_undo.take() {
            match undo {
                Undo::Pop => {
                    self.panel.pop_screen();
                }
                Undo::Restore(screens) => self.panel.restore_screens(screens),
            }
            self.repaint_shell();
            self.reflow_surfaces();
        }
        self.end_transition();
    }

    fn end_transition(&mut self) {
        self.compositor.set_radius(LayerId::ShellPrev, 0.0);
        self.compositor.remove_layer(LayerId::ShellPrev);
        self.transition = None;
        self.transition_undo = None;
    }

    /// Rasterise a screen, whichever kind it is.
    fn render_screen(
        &self,
        screen: &crate::shell::Screen,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>, PipelineError> {
        match screen {
            crate::shell::Screen::Home(scene) => crate::attract::render(scene, w, h),
            crate::shell::Screen::Service(sc) => crate::service::render(sc, w, h),
            crate::shell::Screen::Picker(p) => crate::picker::render(p, w, h),
        }
    }

    fn repaint_shell(&mut self) {
        if let Err(e) = self.paint_screen() {
            error!(error = %e, "failed to draw the shell screen");
        }
    }

    /// Rasterise the current shell screen at the true surface size and install it.
    ///
    /// No-op when no screen has been set — a renderer driven only by casts (the offscreen
    /// test harness, a headless tap) never has one, and should not get a background.
    fn paint_screen(&mut self) -> Result<(), PipelineError> {
        let Some(screen) = self.panel.current() else {
            return Ok(());
        };
        let (w, h) = self.compositor.target_size();
        let (w, h) = (w.max(1), h.max(1));
        let rgba = self.render_screen(screen, w, h)?;
        // The mascot's foreground half rides its own layer above the widget's page (see
        // `LayerId::MascotOverlay`). Rasterised alongside Home and carried while other
        // screens are up — suppression keeps it off them — so coming back to Home does
        // not redraw her.
        if let crate::shell::Screen::Home(scene) = screen {
            match crate::attract::render_mascot_overlay(scene, w, h) {
                Some((pixels, rect)) => {
                    self.compositor.upload_texture(
                        LayerId::MascotOverlay,
                        rect.width,
                        rect.height,
                        // Authored art: sRGB in, sRGB out.
                        TexelFormat::Rgba8Srgb,
                        &pixels,
                    )?;
                    self.mascot_base = Some(rect.transform(w, h));
                    self.compositor.upsert_layer(Layer {
                        id: LayerId::MascotOverlay,
                        opacity: 1.0,
                        transform: rect.transform(w, h),
                    });
                }
                None => {
                    self.mascot_base = None;
                    self.compositor.remove_layer(LayerId::MascotOverlay);
                }
            }
        }
        self.set_attract(w, h, &rgba)?;
        // A freshly rasterized screen must land in the floor's *current* placement. Without
        // this it appears at full size for one frame and then jumps, which is the snap this
        // whole module removes everywhere else.
        self.apply_floor();
        Ok(())
    }
}

#[cfg(all(test, feature = "ffmpeg"))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The outgoing screen's fade must be a curve, not a ramp with corners: at 60 fps
    /// no frame-to-frame opacity step may pop. This is the regression test for the old
    /// `(p * 4.0).clamp(0.0, 1.0)`, whose worst step at 60 fps of a 400 ms transition
    /// was ~0.22 — a visible blink on a two-metre panel.
    #[test]
    #[cfg(feature = "kiosk")]
    fn the_late_fade_is_continuous_and_reaches_both_ends() {
        assert_eq!(RenderLoop::fade_late(0.0), 0.0);
        assert!((RenderLoop::fade_late(1.0) - 1.0).abs() < f32::EPSILON);
        let mut prev = RenderLoop::fade_late(0.0);
        // p sweeps 0→1 as a ~400 ms spring would at 60 fps: about 24 frames.
        for i in 1..=24 {
            let p = i as f32 / 24.0;
            let now = RenderLoop::fade_late(p);
            assert!(now >= prev, "the fade never reverses");
            assert!(
                now - prev < 0.15,
                "a {:.3} step between adjacent frames is a pop, not a fade",
                now - prev
            );
            prev = now;
        }
    }

    #[tokio::test]
    async fn a_track_starting_advances_the_cards_own_queue() {
        // Sources are not obliged to re-send the queue on a natural advance — Spotify's
        // cluster updates announce edits, not progress — so when the playing track is
        // the head of the card's queue, the card shifts its own copy rather than
        // showing one song as both "playing" and "up next".
        let (pipe, _rx) = RenderPipeline::new(4);
        pipe.up_next(vec![
            castaway_core::QueueItem::new("Aerodynamic"),
            castaway_core::QueueItem::new("Voyager"),
        ])
        .await
        .unwrap();

        let mut started = castaway_core::NowPlaying::default();
        started.title = Some("Aerodynamic".into());
        pipe.now_playing(started).await.unwrap();
        let card = pipe.card();
        assert_eq!(card.up_next.len(), 1);
        assert_eq!(card.up_next[0].title, "Voyager");

        // A track that is *not* the head says nothing about the queue.
        let mut unrelated = castaway_core::NowPlaying::default();
        unrelated.title = Some("Contact".into());
        pipe.now_playing(unrelated).await.unwrap();
        assert_eq!(
            pipe.card().up_next.len(),
            1,
            "unrelated track ate the queue"
        );
    }

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
        while let Some(cmd) = rx.try_recv() {
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
                    None,
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
        while let Some(cmd) = rx.try_recv() {
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
    #[cfg(all(feature = "audio", feature = "ffmpeg"))]
    use std::sync::Arc;

    /// An output that remembers whether anything was actually played through it.
    #[cfg(all(feature = "audio", feature = "ffmpeg"))]
    #[derive(Default)]
    struct Speaker {
        frames: std::sync::atomic::AtomicU64,
    }

    #[cfg(all(feature = "audio", feature = "ffmpeg"))]
    impl crate::audio_out::AudioOut for Arc<Speaker> {
        fn start(&mut self, _rate: u32, _channels: u16) -> Result<(), crate::error::PipelineError> {
            Ok(())
        }
        fn write(
            &mut self,
            block: &crate::audio_decode::PcmBlock,
        ) -> Result<(), crate::error::PipelineError> {
            self.frames.fetch_add(
                block.frame_count() as u64,
                std::sync::atomic::Ordering::SeqCst,
            );
            Ok(())
        }
        fn stop(&mut self) {}
    }

    #[cfg(all(feature = "audio", feature = "ffmpeg"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mirrors_audio_reaches_the_output_on_the_render_path_too() {
        // The same assertion as `null::tests`, against the pipeline that actually had the
        // bug: this one took the field as `_audio` and discarded every frame. The null
        // build having the test is not enough, because the null build is not the one that
        // runs on the panel.
        //
        // No GPU is needed: `new` hands back the receiver the render loop would consume,
        // and this test simply holds it.
        let rate = 44_100;
        let frames = crate::audio_decode::tests::encode(
            castaway_core::AudioCodec::Sbc,
            rate,
            &crate::audio_decode::tests::sine(rate, 44_100),
        );
        if frames.is_empty() {
            eprintln!("this ffmpeg build has no SBC encoder; skipping");
            return;
        }

        let (atx, arx) = tokio::sync::mpsc::channel(frames.len() + 1);
        for frame in frames {
            atx.send(frame).await.unwrap();
        }
        drop(atx);

        let speaker = Arc::new(Speaker::default());
        let for_factory = Arc::clone(&speaker);
        let (pipeline, _rx) = RenderPipeline::new(4);
        let pipeline =
            pipeline.with_audio_output(Arc::new(move || Box::new(Arc::clone(&for_factory))));

        let (_vtx, vrx) = tokio::sync::mpsc::channel(1);
        pipeline
            .mirror(
                castaway_core::FrameSource::Encoded(vrx),
                Some(castaway_core::MirrorAudio {
                    source: castaway_core::FrameSource::Encoded(arx),
                    format: crate::audio_decode::tests::format(rate, 2),
                    config: None,
                }),
            )
            .await
            .unwrap();

        for _ in 0..200 {
            if speaker.frames.load(std::sync::atomic::Ordering::SeqCst) > u64::from(rate) / 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let played = speaker.frames.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            played > u64::from(rate) / 2,
            "only {played} frames of the mirror's audio reached the output"
        );
    }
}
