//! The browser, out of process (D36).
//!
//! Replaces `cef_browser`'s embedded runtime with a subprocess castaway supervises. The
//! shape above this module is deliberately unchanged — [`ElectronHost::pump`] is called
//! once per frame from the kiosk's main thread and pushes a layer into the compositor,
//! exactly as `BrowserHost::pump` did — because the point of the port is a different
//! *runtime*, not a different render loop.
//!
//! What genuinely changed, and why each one is an improvement rather than a translation:
//!
//! - **The browser cannot take us down.** A wedged or crashed browser is a child process
//!   to reap and restart; live AirPlay and DLNA sessions never notice. Embedded, a
//!   browser-process fault was *our* fault.
//! - **Frames stay on the GPU.** CEF's accelerated OSR is unusable upstream, so the CEF
//!   path took a 33 MB CPU copy per 4K frame. Here a paint carries a handle and the
//!   pixels are imported ([`crate::hwaccel::remote_handle`] →
//!   `import_single_plane`) without leaving the device.
//! - **Blocking is answered from Rust.** CEF's resource callback was synchronous, which
//!   is why the adblock engine had to live in-process and be `Send + Sync`. Electron's
//!   `webRequest` is async, so the engine stays here and answers over the protocol —
//!   the local surface remains ours (D36's condition).
//! - **`main()` is ours again.** No re-exec: the binary is not also Chromium's subprocess
//!   entry point, so the bootstrap-must-be-first ordering and the Nix wrapper-identity
//!   constraint are both gone.
//!
//! ## The frame ordering rule
//!
//! A painted frame is *borrowed*. `Release` may only be sent once the GPU has finished
//! sampling, because the import aliases Chromium's own buffer — releasing early is the
//! tearing bug `hwaccel::dmabuf` documents for VA-API surfaces, in a different costume.
//! [`InFlight`] is what holds the borrow. It rides into the compositor as part of the
//! imported texture's owner, so wgpu's own resource tracking drops it — and sends the
//! release — exactly when the last submission sampling the texture has retired.
//!
//! It must not be held any longer than that. The browser stops *sending* paints once
//! [`crate::browser_proto::MAX_INFLIGHT_FRAMES`] are unreleased, so a consumer that
//! sits on borrows until "the
//! next paint arrives" deadlocks the pipeline: the producer is waiting for a release the
//! consumer will only send on a paint that is never coming. That consumer was this file,
//! once — the freeze arrived with the third frame and read as "the page went black".

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info, trace, warn};

use crate::adblock_engine::AdBlocker;
use crate::audio_decode::PcmBlock;
use crate::audio_out::{AudioOut, AudioOutputFactory};
use crate::browser::{BrowserCommand, BrowserRole};
use crate::browser_proto::{encode, FromBrowser, LineFramer, PixelOrder, PlaneInfo, ToBrowser};
use crate::error::PipelineError;
use crate::hwaccel::remote_handle::{ProcessRef, RemoteHandle};

/// A user agent that makes YouTube serve its TV interface.
///
/// Carried over verbatim from the CEF path: leanback keys off this, and a mismatch
/// silently yields the mobile site on a 4K panel.
pub const TV_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/122.0.0.0 Safari/537.36 CrKey/1.54.250320";

/// How many times to rebuild a page before concluding the problem is not transient.
///
/// Unchanged from the CEF host, and for the same reason: a renderer crash usually is
/// transient, but a page that kills three renderers running will kill the fourth, and a
/// panel cycling through that forever is worse than a blank one.
const RECOVERY_ATTEMPTS: u32 = 3;

/// Wait before a recovery attempt, so a page that fails instantly (no DNS at boot,
/// captive portal) cannot burn the whole budget inside one second.
const RECOVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

/// How long to wait for the child to exit on `Quit` before killing it.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// One frame borrowed from the browser, and the release that returns it.
///
/// Dropping this sends the release. That is the whole lifetime rule expressed as
/// ownership, so "forgot to release" is not reachable and "released too early" is a
/// visible `drop`. On a successful import it is boxed into the texture's owner, which
/// wgpu drops when the last submission sampling the texture retires — the earliest
/// moment the release is true.
#[derive(Debug)]
struct InFlight {
    id: u64,
    /// The channel back to the browser. `Weak`-ish by hand: if the browser is gone the
    /// release is pointless, so a send failure here is not worth reporting.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.stdin.lock() {
            if let Some(stdin) = guard.as_mut() {
                if let Ok(bytes) = encode(&ToBrowser::Release { id: self.id }) {
                    let _ = stdin.write_all(&bytes);
                    let _ = stdin.flush();
                }
            }
        }
    }
}

/// What the reader thread has observed, for the main thread to act on.
#[derive(Debug, Default)]
struct Health {
    /// A renderer died, or a load failed hard enough to warrant recovery.
    fault: Mutex<Option<String>>,
    /// The browser told us it could not produce GPU frames.
    software_fallback: std::sync::atomic::AtomicBool,
    /// Frames the browser dropped because we were behind.
    drops: AtomicU64,
    /// The media clock of the most recent audio block, in milliseconds.
    ///
    /// Milliseconds-as-integer because it is read from the render thread while the reader
    /// thread writes it, and an `f64` cannot be atomic. Precision beyond a millisecond is
    /// not meaningful for lip-sync anyway — the threshold where a viewer notices is tens
    /// of milliseconds.
    audio_ms: std::sync::atomic::AtomicI64,
    /// How many audio blocks have been handed to the sink.
    audio_blocks: AtomicU64,
    /// Video media time minus audio media time, in milliseconds — the lip-sync error.
    ///
    /// Positive means the picture is ahead of the sound. Named the same as AirPlay's
    /// `av_skew_ms` on purpose: it is the same quantity, and a panel with two protocols
    /// reporting sync differently is a panel nobody can compare.
    av_skew_ms: std::sync::atomic::AtomicI64,
}

impl Health {
    fn set_fault(&self, reason: String) {
        if let Ok(mut slot) = self.fault.lock() {
            slot.get_or_insert(reason);
        }
    }

    fn take_fault(&self) -> Option<String> {
        self.fault.lock().ok().and_then(|mut f| f.take())
    }
}

/// Seconds on the media clock → milliseconds, saturating.
///
/// A media time is never large enough to overflow, but `as` on a `f64` that somehow is
/// (a NaN from a corrupt message, say) is undefined-ish rather than merely wrong, and a
/// wrapped timestamp would read as a wild sync error rather than a bad input.
fn ms(seconds: f64) -> i64 {
    if !seconds.is_finite() {
        return 0;
    }
    let millis = seconds * 1000.0;
    // `as` on an out-of-range float saturates in Rust, but saying so explicitly documents
    // that a wild value becomes a clamp rather than a wrapped timestamp — which would
    // read as an enormous sync error instead of a bad input.
    #[allow(clippy::cast_possible_truncation)]
    {
        millis as i64
    }
}

/// Base64 → interleaved `f32`.
///
/// The page sends native-endian `f32` bytes; a length that is not a multiple of four
/// means a truncated block, and playing three quarters of a sample as if it were a whole
/// one is a click. Refusing is the quieter failure.
fn decode_pcm(b64: &str) -> Option<Vec<f32>> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    if bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// The browser's audio output, opened on the first block that arrives.
struct BrowserAudio {
    out: Box<dyn AudioOut>,
    started: bool,
}

/// A painted frame waiting to be imported on the render thread.
struct PendingPaint {
    id: u64,
    format: PixelOrder,
    width: u32,
    height: u32,
    modifier: u64,
    plane: PlaneInfo,
}

/// The browser subprocess: spawn, protocol, supervision.
pub struct Electron {
    child: Child,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    /// Newest painted frame not yet imported. Deliberately a single slot: an older frame
    /// superseded before it was drawn is worthless, and holding it would only add lag
    /// (ground rule 4).
    pending: Arc<Mutex<Option<PendingPaint>>>,
    health: Arc<Health>,
    process: ProcessRef,
    reader: Option<std::thread::JoinHandle<()>>,
    probes: Arc<Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<String>>>>,
    next_probe: AtomicU64,
}

impl Electron {
    /// Spawn the browser and wait for it to announce itself.
    ///
    /// `program` is the Electron binary, `app_dir` the host app directory. The adblock
    /// engine is handed over so the reader thread can answer queries without a hop
    /// through the main thread — blocking decisions are latency-critical and the engine
    /// is `Send + Sync`.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the child cannot be spawned or never says `ready`.
    pub fn spawn(
        program: &std::path::Path,
        app_dir: &std::path::Path,
        adblock: Arc<AdBlocker>,
        audio_out: Option<&AudioOutputFactory>,
        user_agent: &str,
    ) -> Result<Self, PipelineError> {
        let mut child = Command::new(program)
            .arg(app_dir)
            .env("CASTAWAY_USER_AGENT", user_agent)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Chromium's own logging goes to the terminal/journal rather than through the
            // protocol: it is voluminous, and mixing it into the control channel would
            // mean one stray line could desynchronize framing.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                PipelineError::GpuInit(format!("spawning browser {}: {e}", program.display()))
            })?;

        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PipelineError::GpuInit("browser has no stdout".into()))?;

        let pending: Arc<Mutex<Option<PendingPaint>>> = Arc::new(Mutex::new(None));
        // A factory rather than a device: a respawned browser takes a *fresh* output, the
        // same way each session does, because two writers on one device fight rather
        // than mix.
        let audio: Arc<Mutex<Option<BrowserAudio>>> =
            Arc::new(Mutex::new(audio_out.map(|make| BrowserAudio {
                out: make(),
                started: false,
            })));
        let health = Arc::new(Health::default());
        let probes: Arc<Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<String>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u32>();

        let reader = std::thread::Builder::new()
            .name("browser-reader".into())
            .spawn({
                let pending = Arc::clone(&pending);
                let health = Arc::clone(&health);
                let stdin = Arc::clone(&stdin);
                let audio = Arc::clone(&audio);
                let probes = Arc::clone(&probes);
                move || {
                    reader_loop(
                        stdout,
                        &Wiring {
                            pending: &pending,
                            health: &health,
                            stdin: &stdin,
                            adblock: &adblock,
                            audio: &audio,
                            probes: &probes,
                            ready_tx: &ready_tx,
                        },
                    );
                }
            })
            .map_err(|e| PipelineError::GpuInit(format!("browser reader thread: {e}")))?;

        // A browser that never says `ready` is a broken install, and waiting forever for
        // it would hang startup on a path the receiver awaits.
        let pid = ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|_| {
                let _ = child.kill();
                PipelineError::GpuInit("browser did not report ready within 30s".into())
            })?;

        let process = ProcessRef::open_for(&child, pid)?;
        let electron = Self {
            child,
            stdin,
            pending,
            health,
            process,
            reader: Some(reader),
            probes,
            next_probe: AtomicU64::new(0),
        };
        info!(target: "castaway::browser", pid, "browser up");
        Ok(electron)
    }

    /// Queue a command. Failures are logged, not returned: every caller's only recourse
    /// is the recovery path, which the reader thread already drives.
    fn send(&self, msg: &ToBrowser) {
        let Ok(bytes) = encode(msg) else {
            warn!(target: "castaway::browser", ?msg, "could not encode a browser command");
            return;
        };
        if let Ok(mut guard) = self.stdin.lock() {
            if let Some(stdin) = guard.as_mut() {
                if stdin
                    .write_all(&bytes)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    // The browser died; the reader thread will have posted the fault.
                    debug!(target: "castaway::browser", "browser stdin closed");
                }
            }
        }
    }

    /// Take the newest painted frame, if one arrived since the last call.
    fn take_paint(&self) -> Option<PendingPaint> {
        self.pending.lock().ok().and_then(|mut p| p.take())
    }

    /// Whether the browser reported it cannot deliver GPU frames.
    #[must_use]
    pub fn is_software_fallback(&self) -> bool {
        self.health.software_fallback.load(Ordering::Relaxed)
    }

    /// Frames the browser dropped because we were not keeping up.
    #[must_use]
    pub fn drops(&self) -> u64 {
        self.health.drops.load(Ordering::Relaxed)
    }

    /// Evaluate an expression in the page and wait for its value.
    ///
    /// For tests: it is how "the page saw the touch" gets asserted instead of "we sent
    /// one". Not on the kiosk path — nothing in normal operation needs to interrogate
    /// the page.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the browser does not answer within `timeout`.
    pub fn probe(
        &self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<String, PipelineError> {
        let id = self.next_probe.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut probes) = self.probes.lock() {
            probes.insert(id, tx);
        }
        self.send(&ToBrowser::Probe {
            id,
            expression: expression.to_string(),
        });
        rx.recv_timeout(timeout)
            .map_err(|_| PipelineError::GpuInit(format!("probe {id} went unanswered")))
    }

    /// Lip-sync error in milliseconds: video media time minus audio media time.
    ///
    /// `None` until both a frame and an audio block have carried a media clock, which for
    /// a page with no media element is never — a clock page is not out of sync, it simply
    /// has no sound to be out of sync with.
    #[must_use]
    pub fn av_skew_ms(&self) -> Option<i64> {
        (self.health.audio_blocks.load(Ordering::Relaxed) > 0)
            .then(|| self.health.av_skew_ms.load(Ordering::Relaxed))
    }

    /// Audio blocks handed to the mixer.
    #[must_use]
    pub fn audio_blocks(&self) -> u64 {
        self.health.audio_blocks.load(Ordering::Relaxed)
    }

    /// Ask the browser to quit, then make sure it did.
    ///
    /// Process cleanup is the half of the subprocess model that is easy to get wrong: a
    /// browser left behind holds the GPU, the audio device and the profile lock, and the
    /// next start fails in a way that looks nothing like "the last one is still running".
    /// So this escalates rather than hoping.
    pub fn shutdown(mut self) {
        self.send(&ToBrowser::Quit);
        // Close stdin: a host app blocked on a read sees EOF and exits even if it
        // mishandled `quit`.
        if let Ok(mut guard) = self.stdin.lock() {
            *guard = None;
        }

        let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    info!(target: "castaway::browser", ?status, drops = self.drops(), "browser exited");
                    break;
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                _ => {
                    warn!(
                        target: "castaway::browser",
                        "browser did not exit within {SHUTDOWN_GRACE:?}; killing it"
                    );
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // The reader thread ends when stdout closes, which the child's exit guarantees.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Electron {
    fn drop(&mut self) {
        // A panic on the render thread must not leave a browser running. `shutdown` is
        // the graceful path; this is the backstop, and it is deliberately blunt.
        if matches!(self.child.try_wait(), Ok(None)) {
            warn!(target: "castaway::browser", "browser still running at drop; killing");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Everything the reader thread dispatches into.
///
/// One struct rather than seven parameters threaded through two functions: they are all
/// the same thing — the state a message can touch — and passing them individually made
/// both signatures unreadable and neither more correct.
struct Wiring<'a> {
    pending: &'a Arc<Mutex<Option<PendingPaint>>>,
    health: &'a Arc<Health>,
    stdin: &'a Arc<Mutex<Option<ChildStdin>>>,
    adblock: &'a Arc<AdBlocker>,
    audio: &'a Arc<Mutex<Option<BrowserAudio>>>,
    probes: &'a Arc<Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<String>>>>,
    ready_tx: &'a std::sync::mpsc::Sender<u32>,
}

/// Read the browser's stdout until it closes, dispatching each message.
///
/// On its own thread rather than the render loop because blocking decisions must not wait
/// for the next frame: a page stalls on a pending request, so answering at 60 Hz would
/// make every blocked resource cost up to 16 ms.
fn reader_loop(stdout: std::process::ChildStdout, w: &Wiring<'_>) {
    let mut framer = LineFramer::default();
    let mut reader = BufReader::new(stdout);
    loop {
        let consumed = {
            let Ok(buf) = reader.fill_buf() else { break };
            if buf.is_empty() {
                break;
            }
            for msg in framer.push(buf) {
                match msg {
                    Ok(msg) => {
                        handle(msg, w);
                    }
                    Err(e) => {
                        warn!(target: "castaway::browser", error = %e, "browser protocol");
                    }
                }
            }
            buf.len()
        };
        reader.consume(consumed);
    }
    debug!(target: "castaway::browser", "browser stdout closed");
    w.health.set_fault("browser stdout closed".into());
}

fn handle(msg: FromBrowser, w: &Wiring<'_>) {
    let Wiring {
        pending,
        health,
        stdin,
        adblock,
        audio,
        probes,
        ready_tx,
    } = *w;
    match msg {
        FromBrowser::Ready { pid } => {
            let _ = ready_tx.send(pid);
        }
        FromBrowser::Paint {
            id,
            format,
            width,
            height,
            media_time,
            modifier,
            planes,
        } => {
            // The picture's place on the media clock, against which the audio block's
            // own `media_time` is the lip-sync error. Recorded on every frame because a
            // skew that only appears under load is the one that matters.
            if media_time > 0.0 {
                let video_ms = ms(media_time);
                let audio_ms = health.audio_ms.load(Ordering::Relaxed);
                if audio_ms != 0 {
                    health
                        .av_skew_ms
                        .store(video_ms - audio_ms, Ordering::Relaxed);
                }
            }
            trace!(
                target: "castaway::browser",
                id,
                width,
                height,
                planes = planes.len(),
                "paint received"
            );
            let Some(&plane) = planes.first() else {
                warn!(target: "castaway::browser", id, "paint with no planes");
                return;
            };
            if planes.len() > 1 {
                // A compressed/tiled modifier can carry a metadata plane. Refusing is
                // right — importing plane 0 alone would render plausible garbage — and
                // saying so names the thing to implement rather than "it went black".
                warn!(
                    target: "castaway::browser",
                    planes = planes.len(),
                    "multi-plane browser frame; only single-plane BGRA is implemented"
                );
                return;
            }
            let modifier = modifier
                .as_deref()
                .map_or(0, |m| m.parse::<u64>().unwrap_or(0));
            let superseded = pending.lock().ok().and_then(|mut slot| {
                slot.replace(PendingPaint {
                    id,
                    format,
                    width,
                    height,
                    modifier,
                    plane,
                })
            });
            // The frame we just displaced was never drawn; return its buffer at once
            // rather than making the browser wait out our frame interval for it.
            if let Some(old) = superseded {
                release(stdin, old.id);
            }
        }
        FromBrowser::Dropped { total } => {
            health.drops.store(total, Ordering::Relaxed);
        }
        FromBrowser::NoTexture { detail } => {
            if !health.software_fallback.swap(true, Ordering::Relaxed) {
                error!(
                    target: "castaway::browser",
                    %detail,
                    "browser cannot produce GPU frames; the zero-copy path is unavailable \
                     and the browser layer will stay empty (D36/Q40)"
                );
            }
        }
        FromBrowser::ScriptletQuery { id, url } => {
            let source = adblock.injected_script(&url).unwrap_or_default();
            reply(stdin, &ToBrowser::ScriptletSource { id, source });
        }
        FromBrowser::AdblockQuery {
            id,
            url,
            source,
            kind,
        } => {
            let block = adblock.should_block(&url, &source, &kind);
            reply(stdin, &ToBrowser::AdblockVerdict { id, block });
        }
        FromBrowser::LoadEnd { url, status } => {
            debug!(target: "castaway::browser", %url, ?status, "load end");
        }
        FromBrowser::LoadError { url, error } => {
            warn!(target: "castaway::browser", %url, %error, "load error");
            health.set_fault(format!("load failed: {error}"));
        }
        FromBrowser::RenderGone { reason } => {
            warn!(target: "castaway::browser", %reason, "render process gone");
            health.set_fault(format!("render process {reason}"));
        }
        FromBrowser::Audio {
            pcm,
            channels,
            sample_rate,
            media_time,
            paused,
        } => {
            if paused {
                return;
            }
            let Some(samples) = decode_pcm(&pcm) else {
                warn!(target: "castaway::browser", "audio block did not decode");
                return;
            };
            health.audio_ms.store(ms(media_time), Ordering::Relaxed);
            health.audio_blocks.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut guard) = audio.lock() {
                if let Some(sink) = guard.as_mut() {
                    // Started lazily on the first block: the format is the page's to
                    // choose, and a device opened before anything plays is a device held
                    // open for a page that may never play.
                    if !sink.started {
                        if let Err(e) = sink.out.start(sample_rate, channels) {
                            warn!(target: "castaway::browser", error = %e, "browser audio device");
                            return;
                        }
                        sink.started = true;
                        info!(
                            target: "castaway::browser",
                            sample_rate, channels, "browser audio into the mixer"
                        );
                    }
                    let block = PcmBlock {
                        samples,
                        channels,
                        sample_rate,
                        // The page's media clock, carried straight through. This is the
                        // same clock a paint's `media_time` is on, which is what makes
                        // the skew in `av_skew_ms` a real measurement rather than a
                        // comparison of two unrelated timelines.
                        pts: std::time::Duration::from_secs_f64(media_time.max(0.0)),
                    };
                    if let Err(e) = sink.out.write(&block) {
                        warn!(target: "castaway::browser", error = %e, "browser audio write");
                    }
                }
            }
        }
        FromBrowser::ProbeResult { id, value } => {
            if let Ok(mut probes) = probes.lock() {
                if let Some(tx) = probes.remove(&id) {
                    let _ = tx.send(value);
                }
            }
        }
        FromBrowser::Log { level, message } => match level.as_str() {
            "error" => error!(target: "castaway::browser", "{message}"),
            "warn" => warn!(target: "castaway::browser", "{message}"),
            _ => debug!(target: "castaway::browser", "{message}"),
        },
    }
}

fn release(stdin: &Arc<Mutex<Option<ChildStdin>>>, id: u64) {
    reply(stdin, &ToBrowser::Release { id });
}

fn reply(stdin: &Arc<Mutex<Option<ChildStdin>>>, msg: &ToBrowser) {
    let Ok(bytes) = encode(msg) else { return };
    if let Ok(mut guard) = stdin.lock() {
        if let Some(stdin) = guard.as_mut() {
            let _ = stdin.write_all(&bytes);
            let _ = stdin.flush();
        }
    }
}

/// Drives the browser from the kiosk's main thread.
///
/// The same role `BrowserHost` played for CEF, and pumped from the same place, so the
/// render loop did not have to learn anything new.
pub struct ElectronHost {
    electron: Option<Electron>,
    /// How to respawn: kept so a browser that dies can be replaced rather than mourned.
    respawn: RespawnSpec,
    commands: std::sync::mpsc::Receiver<BrowserCommand>,
    size: (u32, u32),
    role: BrowserRole,
    widget: Option<String>,
    widget_started: bool,
    current_url: Option<String>,
    recovery_attempts: u32,
    retry_at: Option<std::time::Instant>,
    gave_up: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Whether the idle widget is currently yielded to something that outranks it — a
    /// shell screen other than Home, or a session surface. A per-pump mirror of
    /// [`crate::render_pipeline::RenderLoop::attract_widget_covered`], held only so
    /// input routing agrees with the compositor (which skips drawing the covered layer
    /// on its own — see `LayerId::yields_to`). Always false in
    /// [`BrowserRole::Fullscreen`]; a cast surface is never covered.
    widget_covered: bool,
    /// Whether the left button is held, and therefore whether the browser owns the
    /// pointer even where it strays outside its viewport.
    left_down: bool,
    /// Touch contacts the browser owns — the ones whose press landed inside its
    /// viewport. Tracked per id because ownership is a property of the whole contact,
    /// not of each event.
    contacts: std::collections::HashSet<u32>,
    /// When the next session line is due.
    next_report: std::time::Instant,
}

/// What [`ElectronHost`] needs to bring a browser back.
///
/// Public because it is also what `new` takes: the same five things describe how to start
/// a browser and how to start it *again*, and threading them separately made the
/// constructor an eight-argument seam nobody could read.
pub struct RespawnSpec {
    /// The Electron binary.
    pub program: std::path::PathBuf,
    /// The host app directory.
    pub app_dir: std::path::PathBuf,
    /// The engine that answers blocking and scriptlet queries.
    pub adblock: Arc<AdBlocker>,
    /// Where the page's audio goes. `None` leaves the browser silent.
    pub audio_out: Option<AudioOutputFactory>,
    /// The user agent leanback keys off.
    pub user_agent: String,
}

impl ElectronHost {
    /// Wrap a spawned browser.
    #[must_use]
    pub fn new(
        electron: Electron,
        respawn: RespawnSpec,
        commands: std::sync::mpsc::Receiver<BrowserCommand>,
    ) -> Self {
        Self {
            electron: Some(electron),
            respawn,
            commands,
            size: (1920, 1080),
            role: BrowserRole::Fullscreen,
            widget: None,
            widget_started: false,
            current_url: None,
            recovery_attempts: 0,
            retry_at: None,
            gave_up: None,
            widget_covered: false,
            left_down: false,
            contacts: std::collections::HashSet::new(),
            next_report: std::time::Instant::now(),
        }
    }

    /// Register what to do when the browser cannot be recovered, so DIAL stops reporting
    /// a page that is not there.
    #[must_use]
    pub fn on_recovery_failed(mut self, f: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.gave_up = Some(f);
        self
    }

    /// Paint `url` into the attract scene's card while nothing is casting.
    #[must_use]
    pub fn with_attract_widget(mut self, url: &str) -> Self {
        self.widget = Some(url.to_string());
        self
    }

    /// Leave a fullscreen page, if that is what the browser is showing. Returns whether
    /// there was one to leave, so a back control knows the press is spent.
    ///
    /// The home gesture's missing half. Bringing the shell forward *demotes* video to a
    /// corner, but a fullscreen page has no demoted form — it is opaque, above the
    /// shell, and stays there, so the gesture fired and nothing visibly happened.
    /// Leaving a page means leaving it: same endpoint a DIAL stop reaches, returning
    /// the idle widget (or nothing). The widget role is left alone — it *is* the home
    /// screen's own content.
    pub fn dismiss_fullscreen(&mut self, render: &mut crate::render_pipeline::RenderLoop) -> bool {
        if self.role == BrowserRole::Fullscreen {
            self.hide(render);
            return true;
        }
        false
    }

    /// Track the kiosk surface size so the browser viewport matches.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size = (width, height);
        let rect = self.role.view(self.size).rect;
        if let Some(e) = &self.electron {
            e.send(&ToBrowser::Resize {
                width: rect.width,
                height: rect.height,
            });
        }
    }

    /// One per-frame tick on the main thread: apply commands, recover if needed, and
    /// import whatever the browser painted.
    pub fn pump(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        if !self.widget_started {
            self.widget_started = true;
            if let Some(url) = self.widget.clone() {
                self.show(render, &url, BrowserRole::AttractWidget);
            }
        }
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                BrowserCommand::Navigate(url) => self.show(render, &url, BrowserRole::Fullscreen),
                BrowserCommand::Hide => self.hide(render),
            }
        }
        // Mirror the render loop's per-frame verdict, for input's sake alone: the
        // *drawing* of a covered widget is the compositor's business (it skips the
        // layer — see `LayerId::yields_to`), but "does a touch on that rect belong to
        // the page" is answered here, and it must agree with what is on the glass.
        self.widget_covered =
            self.role == BrowserRole::AttractWidget && render.attract_widget_covered();
        self.recover(render);
        self.import_frame(render);
        self.report();
    }

    /// Pull the newest painted frame across and hand it to the compositor.
    fn import_frame(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        let Some(electron) = &self.electron else {
            return;
        };
        let Some(paint) = electron.take_paint() else {
            return;
        };

        let borrow = InFlight {
            id: paint.id,
            stdin: Arc::clone(&electron.stdin),
        };
        // Nothing is meant to be showing. A blanked page still paints — about:blank is
        // a white frame, importing it would put a fullscreen white layer where "hidden"
        // should be — and after a dismiss the old page's last paints are still arriving.
        if self.current_url.is_none() {
            return; // `borrow` drops here, releasing the frame.
        }
        let view = self.role.view(self.size);
        // A frame sized for a viewport we are no longer showing: the paint raced a role
        // change or a resize. Stretching one frame of the old thing across the new rect
        // is worse than one frame of nothing.
        if (paint.width, paint.height) != (view.rect.width, view.rect.height) {
            debug!(
                target: "castaway::browser",
                got = format_args!("{}x{}", paint.width, paint.height),
                want = format_args!("{}x{}", view.rect.width, view.rect.height),
                "dropping a stale-sized paint"
            );
            return; // `borrow` drops here, releasing the frame.
        }
        let local = match electron.process.pull(RemoteHandle(paint.plane.fd)) {
            Ok(handle) => handle,
            Err(e) => {
                warn!(target: "castaway::browser", error = %e, "could not fetch the frame's buffer");
                return; // `borrow` drops here, releasing the frame.
            }
        };

        match render.import_browser_frame(
            crate::hwaccel::FrameGeometry {
                width: paint.width,
                height: paint.height,
                format: paint.format.texture_format(),
            },
            paint.modifier,
            crate::hwaccel::PlaneSpan {
                offset: paint.plane.offset,
                pitch: paint.plane.stride,
            },
            local,
            // The borrow travels with the texture: wgpu drops it — sending the release —
            // when the last submission sampling this frame retires. Holding it here
            // until "the next paint" instead is the deadlock the module docs describe.
            Box::new(borrow),
            view.transform,
            view.layer,
        ) {
            Ok(()) => trace!(
                target: "castaway::browser",
                id = paint.id,
                width = paint.width,
                height = paint.height,
                layer = ?view.layer,
                "frame imported"
            ),
            Err(e) => warn!(
                target: "castaway::browser",
                error = %e,
                width = paint.width,
                height = paint.height,
                modifier = format_args!("{:#x}", paint.modifier),
                stride = paint.plane.stride,
                offset = paint.plane.offset,
                "browser frame import failed"
            ),
        }
    }

    /// Put the page back if it died, or give up loudly.
    fn recover(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        let fault = self.electron.as_ref().and_then(|e| e.health.take_fault());
        if let Some(fault) = fault {
            self.recovery_attempts += 1;
            if self.recovery_attempts > RECOVERY_ATTEMPTS {
                error!(
                    target: "castaway::browser",
                    %fault,
                    attempts = self.recovery_attempts - 1,
                    url = %self.current_url.clone().unwrap_or_default(),
                    "giving up on the page; returning the panel to the idle screen"
                );
                self.retry_at = None;
                self.recovery_attempts = 0;
                // Falling back to the idle widget is wrong when the idle widget is what
                // failed: the panel would cycle through the same crash forever.
                if self.current_url.as_deref() == self.widget.as_deref() {
                    error!(
                        target: "castaway::browser",
                        "the idle widget is what failed; leaving the browser layer empty"
                    );
                    self.clear(render);
                } else {
                    self.hide(render);
                }
                if let Some(gave_up) = self.gave_up.clone() {
                    gave_up();
                }
                return;
            }
            warn!(
                target: "castaway::browser",
                %fault,
                attempt = self.recovery_attempts,
                retry_in = ?RECOVERY_DELAY,
                "recovering the browser"
            );
            self.retry_at = Some(std::time::Instant::now() + RECOVERY_DELAY);
        }

        let Some(due) = self.retry_at else { return };
        if std::time::Instant::now() < due {
            return;
        }
        self.retry_at = None;

        // A dead *process* needs respawning, not just re-navigating — the distinction the
        // embedded path never had to make, because there the process was ours.
        let dead = self
            .electron
            .as_mut()
            .is_none_or(|e| matches!(e.child.try_wait(), Ok(Some(_)) | Err(_)));
        if dead {
            self.clear(render);
            if let Some(old) = self.electron.take() {
                old.shutdown();
            }
            match Electron::spawn(
                &self.respawn.program,
                &self.respawn.app_dir,
                Arc::clone(&self.respawn.adblock),
                self.respawn.audio_out.as_ref(),
                &self.respawn.user_agent,
            ) {
                Ok(e) => {
                    info!(target: "castaway::browser", "browser respawned");
                    self.electron = Some(e);
                }
                Err(e) => {
                    error!(target: "castaway::browser", error = %e, "could not respawn the browser");
                    return;
                }
            }
        }

        let Some(url) = self.current_url.clone() else {
            return;
        };
        let role = self.role;
        info!(target: "castaway::browser", %url, "reloading after a fault");
        self.show(render, &url, role);
    }

    /// Give the panel back: the idle widget if there is one, otherwise nothing.
    fn hide(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        match self.widget.clone() {
            Some(url) => self.show(render, &url, BrowserRole::AttractWidget),
            None => {
                if let Some(e) = &self.electron {
                    e.send(&ToBrowser::Blank);
                }
                self.clear(render);
            }
        }
    }

    /// Drop the browser layer and every borrow behind it.
    ///
    /// The borrows live inside the layer textures, so dropping the layers is what
    /// releases the frames — once wgpu has retired the submissions still sampling them.
    fn clear(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        self.current_url = None;
        render.clear_browser();
    }

    /// Point the browser at `url` in `role`.
    fn show(
        &mut self,
        render: &mut crate::render_pipeline::RenderLoop,
        url: &str,
        role: BrowserRole,
    ) {
        let rect = role.view(self.size).rect;
        if self.role != role {
            // The layer transform changes with the role but the texture is still the old
            // viewport's size; drop it rather than stretching one frame of the wrong
            // thing across the new rect.
            render.clear_browser();
        }
        self.role = role;
        if self.current_url.as_deref() != Some(url) {
            self.recovery_attempts = 0;
            self.retry_at = None;
        }
        self.current_url = Some(url.to_string());
        if let Some(e) = &self.electron {
            e.send(&ToBrowser::Navigate {
                url: url.to_string(),
                width: rect.width,
                height: rect.height,
            });
        }
    }

    /// Map a normalized panel coordinate into browser view pixels, clamped. For a
    /// contact the browser already owns.
    fn to_view(&self, x: f32, y: f32) -> (f32, f32) {
        crate::browser::to_view_px(self.role.view(self.size).rect, self.size, x, y)
    }

    /// Map a normalized panel coordinate, or `None` if it is outside the viewport. For
    /// deciding whether an input belongs to the browser at all.
    ///
    /// A covered widget owns nothing: its rect is still where it always was, but what
    /// the finger is touching is whatever covered it.
    fn hit_view(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        if self.widget_covered {
            return None;
        }
        crate::browser::hit_view_px(self.role.view(self.size).rect, self.size, x, y)
    }

    /// One structured line every 5 s while the browser has audio, mirroring the mirroring
    /// path's own session log — because "is it in sync" must be answerable from the
    /// journal rather than from someone standing in front of the panel.
    fn report(&mut self) {
        let Some(electron) = &self.electron else {
            return;
        };
        let now = std::time::Instant::now();
        if now < self.next_report {
            return;
        }
        self.next_report = now + std::time::Duration::from_secs(5);
        if let Some(skew) = electron.av_skew_ms() {
            info!(
                target: "castaway::browser",
                av_skew_ms = skew,
                audio_blocks = electron.audio_blocks(),
                drops = electron.drops(),
                "browser session"
            );
        }
    }

    /// Ask the page a question. Test-only; see [`Electron::probe`].
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if there is no browser or it does not answer.
    pub fn probe(
        &self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<String, PipelineError> {
        self.electron
            .as_ref()
            .ok_or_else(|| PipelineError::GpuInit("no browser to probe".into()))?
            .probe(expression, timeout)
    }

    /// Whether the browser is delivering GPU frames.
    #[must_use]
    pub fn is_software_fallback(&self) -> bool {
        self.electron
            .as_ref()
            .is_some_and(Electron::is_software_fallback)
    }

    /// Stop the browser. Call on the main thread after the kiosk event loop exits.
    ///
    /// Any borrows still riding compositor textures release into a closing pipe;
    /// [`InFlight`] ignores that failure, and the child is exiting anyway.
    pub fn shutdown(mut self) {
        if let Some(e) = self.electron.take() {
            e.shutdown();
        }
    }
}

/// Routed panel input → the browser, over the protocol.
///
/// The kiosk delivers normalized events; this maps them into the browser's view space and
/// sends them. Chromium does its own gesture recognition on the far side, so scroll and
/// fling behave as they did embedded.
impl input_touch::InputSink for ElectronHost {
    fn touch(&mut self, event: input_touch::TouchEvent) {
        use input_touch::TouchPhase;
        // A contact belongs to the browser only if it *started* inside the viewport, and
        // then it belongs to it until it ends. Two failures this prevents, and the second
        // is the nastier one:
        //
        //  - When the browser is the idle screen's clock card, a touch anywhere on the
        //    65-inch panel used to be clamped into that corner and delivered to the page.
        //  - Deciding per-event instead of per-contact would drop the *end* of a drag
        //    that wandered off the card, and the page would believe a finger was down for
        //    the rest of the session.
        let owned = match event.phase {
            TouchPhase::Down => {
                let inside = self.hit_view(event.x, event.y).is_some();
                if inside {
                    self.contacts.insert(event.id);
                }
                inside
            }
            TouchPhase::Move => self.contacts.contains(&event.id),
            TouchPhase::Up | TouchPhase::Cancel => self.contacts.remove(&event.id),
        };
        if !owned {
            return;
        }
        let (x, y) = self.to_view(event.x, event.y);
        if let Some(e) = &self.electron {
            e.send(&ToBrowser::Touch {
                id: event.id,
                phase: event.phase.into(),
                x,
                y,
            });
        }
    }

    fn cancel_all(&mut self) {
        // Every contact the browser thinks is down, told to stop. Without this a page
        // keeps a phantom finger for the life of the session: the browser host holds a
        // contact map keyed by id and only an end or a cancel removes an entry.
        let ids: Vec<u32> = self.contacts.drain().collect();
        if ids.is_empty() {
            return;
        }
        let Some(e) = &self.electron else { return };
        for id in ids {
            e.send(&ToBrowser::Touch {
                id,
                phase: crate::browser_proto::TouchPhase::Cancel,
                x: 0.0,
                y: 0.0,
            });
        }
        self.left_down = false;
    }

    fn pointer(&mut self, event: input_touch::PointerEvent) {
        use crate::browser_proto::PointerKind;
        use input_touch::PointerEvent;
        let Some(e) = &self.electron else { return };
        match event {
            PointerEvent::Move { x, y } => {
                // While a button is held the browser owns the pointer wherever it goes;
                // otherwise a hover only counts inside the viewport.
                if !self.left_down && self.hit_view(x, y).is_none() {
                    return;
                }
                let (x, y) = self.to_view(x, y);
                e.send(&ToBrowser::Pointer {
                    kind: PointerKind::Move,
                    x,
                    y,
                });
            }
            PointerEvent::Button {
                x, y, down, button, ..
            } => {
                if button != input_touch::PointerButton::Left {
                    return;
                }
                // Press must land inside; release is delivered iff the press was ours,
                // for the same reason a touch's end is.
                if down && self.hit_view(x, y).is_none() {
                    return;
                }
                if !down && !self.left_down {
                    return;
                }
                self.left_down = down;
                let (x, y) = self.to_view(x, y);
                let Some(e) = &self.electron else { return };
                e.send(&ToBrowser::Pointer {
                    kind: if down {
                        PointerKind::Down
                    } else {
                        PointerKind::Up
                    },
                    x,
                    y,
                });
            }
            PointerEvent::Wheel { x, y, dx, dy } => {
                // A scroll goes to whatever is under the cursor, so it has to be over us.
                if self.hit_view(x, y).is_none() {
                    return;
                }
                let (x, y) = self.to_view(x, y);
                let Some(e) = &self.electron else { return };
                e.send(&ToBrowser::Wheel { x, y, dx, dy });
            }
        }
    }
}

impl ProcessRef {
    /// Open a reference to the browser, preferring the platform's cheapest route.
    ///
    /// The two platforms want opposite inputs — Linux a pid, Windows the process handle
    /// `CreateProcess` already returned — so the `cfg` lives here rather than leaking
    /// into the spawn path.
    fn open_for(child: &Child, pid: u32) -> Result<Self, PipelineError> {
        #[cfg(unix)]
        {
            let _ = child;
            Self::open(pid)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsHandle as _, OwnedHandle};
            let _ = pid;
            let handle: OwnedHandle = child.as_handle().try_clone_to_owned().map_err(|e| {
                PipelineError::GpuInit(format!("cloning the browser process handle: {e}"))
            })?;
            Ok(Self::from_child_handle(handle))
        }
    }
}
