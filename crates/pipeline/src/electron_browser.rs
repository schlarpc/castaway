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
//! [`InFlight`] is what holds the borrow, and it is dropped only after the compositor has
//! adopted the texture and the previous frame's submission has retired.

use std::collections::VecDeque;
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info, warn};

use crate::adblock_engine::AdBlocker;
use crate::browser::{BrowserCommand, BrowserRole};
use crate::browser_proto::{
    encode, FromBrowser, LineFramer, PixelOrder, PlaneInfo, ToBrowser, MAX_INFLIGHT_FRAMES,
};
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
/// visible `drop`.
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
        let health = Arc::new(Health::default());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<u32>();

        let reader = std::thread::Builder::new()
            .name("browser-reader".into())
            .spawn({
                let pending = Arc::clone(&pending);
                let health = Arc::clone(&health);
                let stdin = Arc::clone(&stdin);
                move || reader_loop(stdout, &pending, &health, &stdin, &adblock, &ready_tx)
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

/// Read the browser's stdout until it closes, dispatching each message.
///
/// On its own thread rather than the render loop because blocking decisions must not wait
/// for the next frame: a page stalls on a pending request, so answering at 60 Hz would
/// make every blocked resource cost up to 16 ms.
fn reader_loop(
    stdout: std::process::ChildStdout,
    pending: &Arc<Mutex<Option<PendingPaint>>>,
    health: &Arc<Health>,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    adblock: &Arc<AdBlocker>,
    ready_tx: &std::sync::mpsc::Sender<u32>,
) {
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
                        handle(msg, pending, health, stdin, adblock, ready_tx);
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
    health.set_fault("browser stdout closed".into());
}

fn handle(
    msg: FromBrowser,
    pending: &Arc<Mutex<Option<PendingPaint>>>,
    health: &Arc<Health>,
    stdin: &Arc<Mutex<Option<ChildStdin>>>,
    adblock: &Arc<AdBlocker>,
    ready_tx: &std::sync::mpsc::Sender<u32>,
) {
    match msg {
        FromBrowser::Ready { pid } => {
            let _ = ready_tx.send(pid);
        }
        FromBrowser::Paint {
            id,
            format,
            width,
            height,
            modifier,
            planes,
        } => {
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
    /// Frames handed to the compositor and not yet retired, oldest first.
    ///
    /// More than one because the compositor may still be sampling the previous frame
    /// when the next arrives; holding a short queue is what makes release-after-retire
    /// expressible without stalling on the GPU.
    inflight: VecDeque<InFlight>,
    left_down: bool,
}

/// What [`ElectronHost`] needs to bring a browser back.
struct RespawnSpec {
    program: std::path::PathBuf,
    app_dir: std::path::PathBuf,
    adblock: Arc<AdBlocker>,
    user_agent: String,
}

impl ElectronHost {
    /// Wrap a spawned browser.
    #[must_use]
    pub fn new(
        electron: Electron,
        program: std::path::PathBuf,
        app_dir: std::path::PathBuf,
        adblock: Arc<AdBlocker>,
        user_agent: String,
        commands: std::sync::mpsc::Receiver<BrowserCommand>,
    ) -> Self {
        Self {
            electron: Some(electron),
            respawn: RespawnSpec {
                program,
                app_dir,
                adblock,
                user_agent,
            },
            commands,
            size: (1920, 1080),
            role: BrowserRole::Fullscreen,
            widget: None,
            widget_started: false,
            current_url: None,
            recovery_attempts: 0,
            retry_at: None,
            gave_up: None,
            inflight: VecDeque::new(),
            left_down: false,
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
        self.recover(render);
        self.import_frame(render);
    }

    /// Pull the newest painted frame across and hand it to the compositor.
    fn import_frame(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        let Some(electron) = &self.electron else {
            return;
        };
        let Some(paint) = electron.take_paint() else {
            return;
        };

        // Retire borrows the compositor is provably done with before taking another, so
        // the browser's pool cannot be starved by our queue growing without bound.
        while self.inflight.len() >= MAX_INFLIGHT_FRAMES {
            self.inflight.pop_front();
        }

        let borrow = InFlight {
            id: paint.id,
            stdin: Arc::clone(&electron.stdin),
        };
        let local = match electron.process.pull(RemoteHandle(paint.plane.fd)) {
            Ok(handle) => handle,
            Err(e) => {
                warn!(target: "castaway::browser", error = %e, "could not fetch the frame's buffer");
                return; // `borrow` drops here, releasing the frame.
            }
        };

        let view = self.role.view(self.size);
        match render.import_browser_frame(
            crate::hwaccel::FrameGeometry {
                width: paint.width,
                height: paint.height,
                format: paint.format.texture_format(),
            },
            paint.modifier,
            local,
            view.transform,
            view.z,
        ) {
            Ok(()) => self.inflight.push_back(borrow),
            Err(e) => warn!(target: "castaway::browser", error = %e, "browser frame import failed"),
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
            self.inflight.clear();
            self.clear(render);
            if let Some(old) = self.electron.take() {
                old.shutdown();
            }
            match Electron::spawn(
                &self.respawn.program,
                &self.respawn.app_dir,
                Arc::clone(&self.respawn.adblock),
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
    fn clear(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        self.current_url = None;
        self.inflight.clear();
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
            self.inflight.clear();
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

    /// Map a normalized panel coordinate into browser view pixels.
    fn to_view(&self, x: f32, y: f32) -> (f32, f32) {
        crate::browser::to_view_px(self.role.view(self.size).rect, self.size, x, y)
    }

    /// Whether the browser is delivering GPU frames.
    #[must_use]
    pub fn is_software_fallback(&self) -> bool {
        self.electron
            .as_ref()
            .is_some_and(Electron::is_software_fallback)
    }

    /// Stop the browser and release every borrowed frame. Call on the main thread after
    /// the kiosk event loop exits.
    pub fn shutdown(mut self) {
        // Borrows first: releasing after the child is gone writes into a closed pipe.
        self.inflight.clear();
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

    fn pointer(&mut self, event: input_touch::PointerEvent) {
        use crate::browser_proto::PointerKind;
        use input_touch::PointerEvent;
        let Some(e) = &self.electron else { return };
        match event {
            PointerEvent::Move { x, y } => {
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
