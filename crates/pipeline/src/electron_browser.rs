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
//!
//! ## Two windows
//!
//! The subprocess owns one offscreen window per [`Surface`]: the idle widget (the
//! clock), navigated once at startup and never again, and the cast page (YouTube
//! leanback), navigated per cast. One window used to play both parts, which meant
//! opening a cast flashed the clock through it and ending one reloaded the clock from
//! scratch. Now each window's paints carry its surface and land on its own compositor
//! layer, each window follows its own panel answer (`RenderLoop::page_view`), and the
//! only coupling left is the panel's own rule: the widget slot shows one thing at a
//! time, and a demoted page outranks the clock there — `page_view(false)` answers
//! `None` for the duration, so the clock's frames are released unimported rather than
//! fought over the layer.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName as _};

/// The two ends of the control channel.
///
/// This used to be the child's stdin and stdout, and on Linux that worked. It cannot work
/// on Windows: Electron's *main process* `process.stdin` is unusable there — GUI-subsystem
/// startup loses the piped handle, so Node emits `end` immediately and never delivers a
/// byte (electron#4218, #10580, #11680, #22809, open for a decade). Measured on the panel:
/// `shutting down` — main.js's stdin `end` handler — arrived before `ready`, while the
/// parent was demonstrably still holding the pipe open and never wrote to it again.
///
/// So the protocol runs over a local socket, the same on both platforms: a Unix socket
/// under the runtime directory, a named pipe under `\\.\pipe\`. `GenericFilePath` maps
/// both from an ordinary path string, which is also exactly what Node's `net.connect`
/// takes, so the two sides agree on one spelling.
///
/// Moving off stdio buys a second thing worth having: stdout goes back to being
/// diagnostics. It was the control channel *and* whatever Chromium decided to print, which
/// is a framing hazard the protocol had to be careful about — and on Windows Chromium
/// writes `\r\n` and a stray leading blank line (electron#12578, seen in the probe).
type WireSend = interprocess::local_socket::SendHalf;
type WireRecv = interprocess::local_socket::RecvHalf;

/// How long the browser gets to connect back before we call the install broken.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Bind the control socket and return the address to hand the child.
///
/// The address is per-process and per-spawn: a respawned browser gets a fresh socket, so a
/// previous child that has not finished dying cannot connect to the new listener and be
/// mistaken for it. On Unix the socket lives under the runtime directory and interprocess
/// unlinks it on drop; on Windows `\\.\pipe\` names are not filesystem entries and go away
/// with the handle.
fn bind_control_socket() -> Result<(String, interprocess::local_socket::Listener), PipelineError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "castaway-browser-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );

    #[cfg(windows)]
    let address = format!(r"\\.\pipe\{unique}");
    // The cache directory rather than state: a socket outlives nothing, and the crate's
    // own rule for cache is "may be deleted at any time", which is exactly right for a
    // file whose only job is to exist while two processes are both running.
    #[cfg(not(windows))]
    let address = {
        let dir = castaway_paths::host().cache();
        std::fs::create_dir_all(dir).map_err(|e| {
            PipelineError::GpuInit(format!("browser socket directory {}: {e}", dir.display()))
        })?;
        dir.join(format!("{unique}.sock"))
            .to_string_lossy()
            .into_owned()
    };

    let name = address
        .clone()
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| PipelineError::GpuInit(format!("browser socket name {address}: {e}")))?;
    let listener = ListenerOptions::new()
        .name(name)
        // A socket file left by a killed receiver would otherwise make every subsequent
        // start fail with AddrInUse — a panel that never comes back after one bad exit.
        .try_overwrite(true)
        .create_sync()
        .map_err(|e| PipelineError::GpuInit(format!("binding browser socket {address}: {e}")))?;
    Ok((address, listener))
}

/// Wait for the browser to connect, giving up if it dies or takes too long.
///
/// Polls rather than blocking in `accept`, because a browser that exits during startup —
/// a missing DLL, a refused GPU — would otherwise leave this blocked forever on a
/// connection that is never coming. Noticing the exit is the difference between a clear
/// error and a hung panel.
fn accept_within(
    listener: &interprocess::local_socket::Listener,
    timeout: std::time::Duration,
    child: &mut Child,
) -> Result<interprocess::local_socket::Stream, PipelineError> {
    listener
        .set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Accept)
        .map_err(|e| PipelineError::GpuInit(format!("browser socket nonblocking: {e}")))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(stream) => {
                // Back to blocking: the reader thread wants a plain blocking read, and the
                // accepted stream inherits the listener's mode on some platforms.
                stream.set_nonblocking(false).map_err(|e| {
                    PipelineError::GpuInit(format!("browser socket back to blocking: {e}"))
                })?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                return Err(PipelineError::GpuInit(format!(
                    "browser did not connect: {e}"
                )))
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(PipelineError::GpuInit(format!(
                "browser exited before connecting ({status})"
            )));
        }
        if std::time::Instant::now() >= deadline {
            return Err(PipelineError::GpuInit(format!(
                "browser did not connect within {timeout:?}"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

use tracing::{debug, error, info, trace, warn};

/// Where the fd addon (`castaway-browser-fd`'s cdylib, #271) is, if anywhere.
///
/// The env override first — the Nix wrapper and the tests both know exactly where they
/// put it — then beside the binary and up to two directories above it, and each of those
/// directories' `lib/`. That covers a cargo build (`target/debug/deps/<test>` with the
/// cdylib at `target/debug/`), an addon installed next to `castaway`, and the Unix
/// install layout crane produces, where the binary is `<prefix>/bin/castaway` and its
/// cdylib `<prefix>/lib/` (#308). `None` is not an error: the spawner logs it and the
/// reach-in path carries the session.
#[cfg(unix)]
fn locate_fd_addon() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("CASTAWAY_BROWSER_FD_ADDON") {
        return Some(path.into());
    }
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .skip(1)
        .take(3)
        .flat_map(|dir| {
            [
                dir.join(castaway_browser_fd::ADDON_SONAME),
                dir.join("lib").join(castaway_browser_fd::ADDON_SONAME),
            ]
        })
        .find(|path| path.exists())
}

use crate::adblock_engine::SharedBlocker;
use crate::audio_decode::PcmBlock;
use crate::browser::{BrowserCommand, BrowserRole, BrowserView};
use crate::browser_proto::{
    encode, FdTransport, FromBrowser, LineFramer, PixelOrder, PlaneInfo, Surface, ToBrowser,
};
use crate::error::PipelineError;
use crate::hwaccel::remote_handle::{ProcessRef, RemoteHandle};

/// The panel surface a browser window's page occupies. Fixed per window: the page window
/// is always a session's surface, the widget window always Home's own furniture — which
/// is the two-window split's whole point. `browser_proto::Surface` names the *window* on
/// the wire; this maps it to what the panel calls the thing that window shows.
const fn panel_surface(surface: Surface) -> crate::panel::Surface {
    match surface {
        Surface::Page => crate::panel::Surface::CastPage,
        Surface::Widget => crate::panel::Surface::IdleWidget,
    }
}

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
    wire: Arc<Mutex<Option<WireSend>>>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.wire.lock() {
            if let Some(wire) = guard.as_mut() {
                if let Ok(bytes) = encode(&ToBrowser::Release { id: self.id }) {
                    let _ = wire.write_all(&bytes);
                    let _ = wire.flush();
                }
            }
        }
    }
}

/// A fault the reader thread observed, and where.
#[derive(Debug, Clone)]
struct Fault {
    /// The window it happened in, or `None` for the process itself (the control socket
    /// closed).
    /// Recovery is scoped by this: a crashing cast page reloads the page, not the clock.
    surface: Option<Surface>,
    /// What happened, for the log.
    reason: String,
}

/// What the reader thread has observed, for the main thread to act on.
#[derive(Debug, Default)]
struct Health {
    /// A renderer died, or a load failed hard enough to warrant recovery.
    fault: Mutex<Option<Fault>>,
    /// The browser told us it could not produce GPU frames.
    software_fallback: std::sync::atomic::AtomicBool,
    /// Frames the browser dropped because we were behind.
    drops: AtomicU64,
    /// How many audio blocks have been handed to the sink.
    audio_blocks: AtomicU64,
    /// The two clocks the browser session has, paired on the reader thread (#278).
    ///
    /// Behind a mutex rather than atomics because it is state with history — a baseline
    /// and a freshness mark — and both message kinds that feed it arrive on the one
    /// reader thread, so the lock is never contended.
    skew: Mutex<crate::av_skew::SkewGauge>,
    /// A/V drift in milliseconds since the session's first audio/paint pairing (#278).
    ///
    /// Positive means the picture's clock has gained on the sound's. Named like AirPlay's
    /// `av_skew_ms` deliberately — but note it is *drift*, not the absolute offset: the
    /// paint timestamp and the media element's `currentTime` share no origin, so the
    /// session-start offset is measured (over the gauge's settling window, #318) and
    /// subtracted (see [`crate::av_skew`]). The old
    /// direct subtraction reported the difference of the two origins — `17455` holding
    /// constant for 90 s — which is a number about Chromium's clock bookkeeping, not
    /// about sync.
    av_skew_ms: std::sync::atomic::AtomicI64,
    /// Whether [`Health::av_skew_ms`] has ever been written.
    ///
    /// The atomic defaults to 0, which is indistinguishable from a measured perfect
    /// sync — so "a skew exists" cannot be inferred from its value, and it used to be
    /// inferred from `audio_blocks` instead. That counter moves on widget audio and on
    /// page audio alike, and a skew is only stored when the gauge pairs a *page* paint
    /// carrying a media time with fresh page audio, so the log asserted `av_skew_ms = 0`
    /// for the whole life of an audio-only page and for the window before the first such
    /// pairing.
    av_skew_seen: std::sync::atomic::AtomicBool,
    /// Frames imported from descriptors the browser *passed* (`SCM_RIGHTS`, #271).
    scm_frames: AtomicU64,
    /// Frames imported by *reaching in* (`pidfd_getfd`/`DuplicateHandle`).
    ///
    /// The pair is what makes the transport observable from a test: "it painted" says
    /// nothing about how the descriptors travelled, and the whole point of #271 is that
    /// the reach-in path is a policy dependency the shipped arrangement must not have.
    pulled_frames: AtomicU64,
}

impl Health {
    fn set_fault(&self, surface: Option<Surface>, reason: String) {
        if let Ok(mut slot) = self.fault.lock() {
            slot.get_or_insert(Fault { surface, reason });
        }
    }

    fn take_fault(&self) -> Option<Fault> {
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

/// The browser's way into the panel's mix.
///
/// The page is a source like any other since #111 — it used to hold a device of its own,
/// which is how page audio came to bypass the panel's volume entirely. `Gain` is
/// documented as the last thing before the device *because* the panel has one pair of
/// speakers, and page audio went straight past it: YouTube Lounge's `setVolume` converted
/// cleanly into a `ControlTxn::Volume`, landed on a gain the browser never consulted, and
/// did nothing — on the surface most likely to be playing (#86). It cannot recur, because
/// there is no longer a second place for samples to go.
struct BrowserAudio {
    input: crate::mixer::MixInput,
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
    /// The plane's descriptor, already ours, when the browser passed it with
    /// `SCM_RIGHTS` (#271). `None` means reach in with [`ProcessRef::pull`]. Owned
    /// here so a paint that is superseded or dropped closes it on the way out.
    #[cfg(unix)]
    scm_fd: Option<std::os::fd::OwnedFd>,
}

/// One pending slot per window. Each is deliberately a single slot: an older frame
/// superseded before it was drawn is worthless, and holding it would only add lag
/// (ground rule 4). Separate slots because the windows paint at unrelated rates — a
/// 60 fps page must not be able to supersede the clock's once-a-second frame out of a
/// shared slot.
#[derive(Default)]
struct PendingPaints {
    widget: Option<PendingPaint>,
    page: Option<PendingPaint>,
}

impl PendingPaints {
    fn slot(&mut self, surface: Surface) -> &mut Option<PendingPaint> {
        match surface {
            Surface::Widget => &mut self.widget,
            Surface::Page => &mut self.page,
        }
    }
}

/// The browser subprocess: spawn, protocol, supervision.
pub struct Electron {
    child: Child,
    wire: Arc<Mutex<Option<WireSend>>>,
    /// Newest painted frame per window, not yet imported.
    pending: Arc<Mutex<PendingPaints>>,
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
    /// cell is handed over so the reader thread can answer queries without a hop through
    /// the main thread — blocking decisions are latency-critical and the cell is
    /// `Send + Sync`. It is the *cell*, not an engine: every query reads the engine as
    /// of that moment, which is what makes the daily refresh land on a running browser
    /// instead of at the next process start (#239).
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the child cannot be spawned or never says `ready`.
    pub fn spawn(
        program: &std::path::Path,
        app_dir: &std::path::Path,
        adblock: SharedBlocker,
        mixer: Option<&Arc<crate::mixer::AudioMixer>>,
        user_agent: &str,
        waker: castaway_core::Waker,
    ) -> Result<Self, PipelineError> {
        // Listen *before* spawning, so the child cannot lose a race to connect.
        let (address, listener) = bind_control_socket()?;
        let mut command = Command::new(program);
        command
            .arg(app_dir)
            .env("CASTAWAY_USER_AGENT", user_agent)
            .env("CASTAWAY_BROWSER_SOCKET", &address)
            // Chromium's own logging is voluminous and, on Windows, punctuated with stray
            // blank lines and CRLF. It used to share a channel with the protocol, where one
            // stray line could desynchronize framing; now both are just diagnostics.
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        // The fd plane (#271): the production frame-descriptor transport, SCM_RIGHTS on
        // a socket beside the control one. Bound before the spawn for the same reason
        // the control listener is, and best-effort at every step — a bind failure, a
        // missing addon or a host app that never connects all degrade to the
        // `pidfd_getfd` reach-in path, logged, not to a browser that cannot start.
        #[cfg(unix)]
        let fd_table = Arc::new(crate::electron_fd_plane::FdTable::default());
        #[cfg(unix)]
        let fd_listener = match crate::electron_fd_plane::bind(&address) {
            Ok((path, fd_listener)) => {
                command.env("CASTAWAY_BROWSER_FD_SOCKET", &path);
                match locate_fd_addon() {
                    Some(addon) => {
                        command.env("CASTAWAY_BROWSER_FD_ADDON", &addon);
                        debug!(target: "castaway::browser", addon = %addon.display(), "fd plane: addon staged");
                    }
                    None => info!(
                        target: "castaway::browser",
                        "fd plane: no {} beside the binary; frames will use pidfd_getfd (#271)",
                        castaway_browser_fd::ADDON_SONAME
                    ),
                }
                Some((path, fd_listener))
            }
            Err(e) => {
                warn!(target: "castaway::browser", error = %e, "fd plane: bind failed; using pidfd_getfd");
                None
            }
        };

        let mut child = command.spawn().map_err(|e| {
            PipelineError::GpuInit(format!("spawning browser {}: {e}", program.display()))
        })?;

        #[cfg(unix)]
        if let Some((path, fd_listener)) = fd_listener {
            // Detached on purpose: it parks in accept for at most CONNECT_TIMEOUT and
            // then either follows the connection to its EOF (the child's exit) or ends.
            if let Err(e) = crate::electron_fd_plane::serve(
                fd_listener,
                path,
                Arc::clone(&fd_table),
                CONNECT_TIMEOUT,
            ) {
                warn!(target: "castaway::browser", error = %e, "fd plane: thread failed; using pidfd_getfd");
            }
        }

        let stream = match accept_within(&listener, CONNECT_TIMEOUT, &mut child) {
            Ok(s) => s,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let (rx, tx) = stream.split();
        let wire = Arc::new(Mutex::new(Some(tx)));

        let pending: Arc<Mutex<PendingPaints>> = Arc::new(Mutex::new(PendingPaints::default()));
        // A fresh input, because a respawned browser is a new source; the mixer keeps the
        // device across the respawn.
        let audio: Arc<Mutex<Option<BrowserAudio>>> =
            Arc::new(Mutex::new(mixer.map(|mixer| BrowserAudio {
                input: mixer.input(crate::mixer::Backpressure::Live),
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
                let wire = Arc::clone(&wire);
                let audio = Arc::clone(&audio);
                let probes = Arc::clone(&probes);
                #[cfg(unix)]
                let fd_table = Arc::clone(&fd_table);
                move || {
                    reader_loop(
                        rx,
                        &Wiring {
                            pending: &pending,
                            health: &health,
                            wire: &wire,
                            adblock: &adblock,
                            audio: &audio,
                            probes: &probes,
                            ready_tx: &ready_tx,
                            waker: &waker,
                            #[cfg(unix)]
                            fd_table: &fd_table,
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
            wire,
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
        if let Ok(mut guard) = self.wire.lock() {
            if let Some(wire) = guard.as_mut() {
                if wire.write_all(&bytes).and_then(|()| wire.flush()).is_err() {
                    // The browser died; the reader thread will have posted the fault.
                    debug!(target: "castaway::browser", "browser control channel closed");
                }
            }
        }
    }

    /// Take a window's newest painted frame, if one arrived since the last call.
    fn take_paint(&self, surface: Surface) -> Option<PendingPaint> {
        self.pending
            .lock()
            .ok()
            .and_then(|mut p| p.slot(surface).take())
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

    /// How imported frames got their descriptors: `(passed, pulled)` — passed with the
    /// paint over `SCM_RIGHTS` (#271) versus reached in for with
    /// `pidfd_getfd`/`DuplicateHandle`.
    ///
    /// What the browser tests assert on: with the addon staged, a painting session must
    /// show `passed > 0, pulled == 0`, which is the claim "the import no longer depends
    /// on ptrace policy or the direct-child arrangement" made observable.
    #[must_use]
    pub fn fd_transport_counts(&self) -> (u64, u64) {
        (
            self.health.scm_frames.load(Ordering::Relaxed),
            self.health.pulled_frames.load(Ordering::Relaxed),
        )
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
        surface: Surface,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<String, PipelineError> {
        let id = self.next_probe.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut probes) = self.probes.lock() {
            probes.insert(id, tx);
        }
        self.send(&ToBrowser::Probe {
            surface,
            id,
            expression: expression.to_string(),
        });
        rx.recv_timeout(timeout)
            .map_err(|_| PipelineError::GpuInit(format!("probe {id} went unanswered")))
    }

    /// A/V drift in milliseconds since the session's first audio/paint pairing (#278):
    /// positive when the picture's clock has gained on the sound's.
    ///
    /// *Drift*, not the absolute lip-sync offset — the paint timestamp and the media
    /// element's `currentTime` share no origin, so the offset at the first pairing is
    /// measured and subtracted (see [`crate::av_skew`]). Zero through the gauge's
    /// settling window by construction, and a session whose clocks both run at real time
    /// stays there.
    ///
    /// `None` until a page frame and a page audio block have both carried a media clock,
    /// which for a page with no media element is never — a clock page is not out of sync,
    /// it simply has no sound to be out of sync with.
    ///
    /// Gated on a skew having been *stored*, not on an audio block having been seen: 0 is
    /// both the atomic's default and a real measurement, so nothing else can tell them
    /// apart.
    #[must_use]
    pub fn av_skew_ms(&self) -> Option<i64> {
        self.health
            .av_skew_seen
            .load(Ordering::Relaxed)
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
        // Drop our end of the socket: a host app blocked on a read sees EOF and exits
        // even if it mishandled `quit`.
        if let Ok(mut guard) = self.wire.lock() {
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
        // The reader thread ends when the socket closes, which the child's exit guarantees.
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
    pending: &'a Arc<Mutex<PendingPaints>>,
    health: &'a Arc<Health>,
    wire: &'a Arc<Mutex<Option<WireSend>>>,
    adblock: &'a SharedBlocker,
    audio: &'a Arc<Mutex<Option<BrowserAudio>>>,
    probes: &'a Arc<Mutex<std::collections::HashMap<u64, std::sync::mpsc::Sender<String>>>>,
    ready_tx: &'a std::sync::mpsc::Sender<u32>,
    /// Wakes the kiosk loop (#59): a stored paint and a posted fault are both consumed
    /// by the main-thread pump, which no longer runs unless something asks it to.
    waker: &'a castaway_core::Waker,
    /// Where SCM_RIGHTS deliveries land (#271), for a paint marked `scm` to claim.
    #[cfg(unix)]
    fd_table: &'a Arc<crate::electron_fd_plane::FdTable>,
}

/// Read the browser's half of the control socket until it closes, dispatching each message.
///
/// On its own thread rather than the render loop because blocking decisions must not wait
/// for the next frame: a page stalls on a pending request, so answering at 60 Hz would
/// make every blocked resource cost up to 16 ms.
fn reader_loop(rx: WireRecv, w: &Wiring<'_>) {
    let mut framer = LineFramer::default();
    let mut reader = BufReader::new(rx);
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
    debug!(target: "castaway::browser", "browser control channel closed");
    // The process, not a window: both surfaces are gone with it.
    w.health
        .set_fault(None, "browser control channel closed".into());
    w.waker.wake();
}

fn handle(msg: FromBrowser, w: &Wiring<'_>) {
    let Wiring {
        pending,
        health,
        wire,
        adblock,
        audio,
        probes,
        ready_tx,
        waker,
        #[cfg(unix)]
        fd_table,
    } = *w;
    match msg {
        FromBrowser::Ready { pid } => {
            let _ = ready_tx.send(pid);
        }
        FromBrowser::Paint {
            surface,
            id,
            format,
            width,
            height,
            media_time,
            modifier,
            fd_transport,
            planes,
        } => {
            // The paint's timestamp, paired against the page audio's media clock by the
            // gauge (#278) — *not* subtracted from it directly: the two are on unrelated
            // origins, and the raw difference was a constant about Chromium's clock
            // bookkeeping, not about sync. Recorded on every frame because a skew that
            // only appears under load is the one that matters. Page frames only: the
            // widget has no media element, and letting a clock frame's zero into the
            // pair would read as a wild sync excursion. The clock is read here, once, at
            // the actor boundary; the gauge itself is pure (#208).
            if surface == Surface::Page && media_time > 0.0 {
                if let Ok(mut gauge) = health.skew.lock() {
                    if let Some(skew) = gauge.video(ms(media_time), std::time::Instant::now()) {
                        health.av_skew_ms.store(skew, Ordering::Relaxed);
                        health.av_skew_seen.store(true, Ordering::Relaxed);
                    }
                }
            }
            trace!(
                target: "castaway::browser",
                ?surface,
                id,
                width,
                height,
                planes = planes.len(),
                ?fd_transport,
                "paint received"
            );
            // Claim the passed descriptors *first*, whatever else is wrong with the
            // paint: they are ours the moment they were sent, and every early return
            // below must close them (dropping the Vec does) rather than leave them
            // parked in the table until eviction (#271).
            //
            // The wait is for a thread-schedule, not a network: main.js sends the
            // descriptors before it writes the paint line, so they are already in the
            // kernel by the time this reader decodes it. A miss after half a second
            // means the fd-plane connection died; the frame is released un-imported and
            // the browser paints on.
            #[cfg(unix)]
            let scm_fds: Option<Vec<std::os::fd::OwnedFd>> = match fd_transport {
                FdTransport::Process => None,
                FdTransport::Scm => {
                    match fd_table.take(id, std::time::Duration::from_millis(500)) {
                        Some(fds) if !fds.is_empty() => Some(fds),
                        _ => {
                            warn!(
                                target: "castaway::browser",
                                id,
                                "paint says scm but its descriptors never arrived; dropping the frame"
                            );
                            release(wire, id);
                            return;
                        }
                    }
                }
            };
            #[cfg(not(unix))]
            if fd_transport == FdTransport::Scm {
                // The host app only opens the fd plane on Linux, so this is a peer this
                // build does not understand. The handle number is unusable either way.
                warn!(target: "castaway::browser", id, "scm paint on a platform with no fd plane");
                release(wire, id);
                return;
            }
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
            let superseded = pending.lock().ok().and_then(|mut slots| {
                slots.slot(surface).replace(PendingPaint {
                    id,
                    format,
                    width,
                    height,
                    modifier,
                    plane,
                    // One plane (checked above), so the first descriptor is the frame.
                    #[cfg(unix)]
                    scm_fd: scm_fds
                        .and_then(|mut fds| (!fds.is_empty()).then(|| fds.swap_remove(0))),
                })
            });
            // The frame we just displaced was never drawn; return its buffer at once
            // rather than making the browser wait out our frame interval for it.
            if let Some(old) = superseded {
                release(wire, old.id);
            }
            waker.wake();
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
                     and the browser layer will stay empty (D36/#64)"
                );
            }
        }
        FromBrowser::ScriptletQuery { id, url } => {
            reply(wire, &scriptlet_answer(adblock, id, &url));
        }
        FromBrowser::AdblockQuery {
            id,
            url,
            source,
            kind,
        } => {
            reply(wire, &adblock_answer(adblock, id, &url, &source, &kind));
        }
        FromBrowser::LoadEnd {
            surface,
            url,
            status,
        } => {
            debug!(target: "castaway::browser", ?surface, %url, ?status, "load end");
        }
        FromBrowser::LoadError {
            surface,
            url,
            error,
        } => {
            warn!(target: "castaway::browser", ?surface, %url, %error, "load error");
            health.set_fault(Some(surface), format!("load failed: {error}"));
            waker.wake();
        }
        FromBrowser::RenderGone { surface, reason } => {
            warn!(target: "castaway::browser", ?surface, %reason, "render process gone");
            health.set_fault(Some(surface), format!("render process {reason}"));
            waker.wake();
        }
        FromBrowser::Audio {
            surface,
            pcm,
            channels,
            sample_rate,
            media_time,
            paused,
        } => {
            // The lip-sync pair is page audio against page frames; a widget that
            // somehow made a sound still gets mixed, but must not pollute the clock.
            // A paused element's clock is not running, so its tail block un-marks the
            // gauge rather than sitting fresh enough to pair with the paints that
            // follow (#278).
            if surface == Surface::Page {
                if let Ok(mut gauge) = health.skew.lock() {
                    if paused {
                        gauge.pause();
                    } else {
                        gauge.audio(ms(media_time), std::time::Instant::now());
                    }
                }
            }
            if paused {
                return;
            }
            let Some(samples) = decode_pcm(&pcm) else {
                warn!(target: "castaway::browser", "audio block did not decode");
                return;
            };
            health.audio_blocks.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut guard) = audio.lock() {
                if let Some(sink) = guard.as_mut() {
                    // Started lazily on the first block: the format is the page's to
                    // choose, and a device opened before anything plays is a device held
                    // open for a page that may never play.
                    if !sink.started {
                        if let Err(e) = sink.input.format(sample_rate, channels) {
                            warn!(target: "castaway::browser", error = %e, "browser audio format");
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
                        // The page's media clock, carried straight through. NOT the
                        // clock a paint's `media_time` is on — that is the compositor's
                        // own timestamp, on an origin of Chromium's choosing — which is
                        // why `av_skew_ms` pairs the two through a gauge that removes
                        // the origin difference rather than subtracting them (#278).
                        pts: std::time::Duration::from_secs_f64(media_time.max(0.0)),
                    };
                    // No gain here any more, and that is the point: the mixer applies the
                    // panel's one volume to the sum, so page audio cannot miss it.
                    if let Err(e) = sink.input.write(&block) {
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

/// The answer to one `adblock-query`, read through the shared cell at the moment of the
/// query.
///
/// `current()` per query rather than per session is the fix for #239: an engine held
/// across queries is a snapshot, and the daily refresh would only take effect at the next
/// respawn. Pure — the cell in, the wire message out — so the seam the boot snapshot used
/// to hide in is the one the tests below drive.
fn adblock_answer(
    adblock: &SharedBlocker,
    id: u64,
    url: &str,
    source: &str,
    kind: &str,
) -> ToBrowser {
    let block = adblock.current().should_block(url, source, kind);
    ToBrowser::AdblockVerdict { id, block }
}

/// The answer to one `scriptlet-query`, read through the shared cell like
/// [`adblock_answer`] — a refreshed list's new `##+js(...)` rules reach the very next
/// navigation (#239).
fn scriptlet_answer(adblock: &SharedBlocker, id: u64, url: &str) -> ToBrowser {
    let source = adblock.current().injected_script(url).unwrap_or_default();
    ToBrowser::ScriptletSource { id, source }
}

fn release(wire: &Arc<Mutex<Option<WireSend>>>, id: u64) {
    reply(wire, &ToBrowser::Release { id });
}

fn reply(wire: &Arc<Mutex<Option<WireSend>>>, msg: &ToBrowser) {
    let Ok(bytes) = encode(msg) else { return };
    if let Ok(mut guard) = wire.lock() {
        if let Some(wire) = guard.as_mut() {
            let _ = wire.write_all(&bytes);
            let _ = wire.flush();
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
    /// The idle widget's URL. `None` when no widget is configured — or when the widget
    /// itself proved unrecoverable and was given up on. Its window, when this is set,
    /// permanently owns [`Surface::Widget`]; it is never navigated anywhere else.
    widget: Option<String>,
    widget_started: bool,
    /// What the page window is showing, `None` when no cast is up.
    ///
    /// The only thing about either page this host still decides, because it is the only
    /// part it knows: which URL each window was told to load. Where the pages *go* is
    /// the panel's answer (`RenderLoop::page_view`), which is why there is no
    /// `BrowserRole` here any more — and, since the two-window split, no `is_cast`
    /// either: which window is the session's is fixed at the wire ([`panel_surface`]),
    /// not a mode the host flips.
    page_url: Option<String>,
    /// Each window's view as last applied: the viewport its subprocess window was told
    /// to rasterize at, and the layer it maps onto. `None` when that page is not on the
    /// glass. Indexed by [`Surface`] via [`Views::slot`].
    ///
    /// Compared against the panel's answer once per pump; a difference is a resize round
    /// trip, which is why this is remembered rather than recomputed into the subprocess
    /// every frame.
    views: Views,
    ladder: RecoveryLadder,
    gave_up: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Which window holds the left button, and therefore owns the pointer even where it
    /// strays outside its viewport. `None` when the button is up.
    left_down: Option<Surface>,
    /// Touch contacts a window owns — the ones whose press landed inside its viewport,
    /// mapped to the window that took the press. Tracked per id because ownership is a
    /// property of the whole contact, not of each event.
    ///
    /// Keyed by [`input_touch::ContactId`], so the panel's own fingers and every remote
    /// peer's occupy one map without their raw ids colliding. The value carries the
    /// browser-facing id as well, because the protocol to the child speaks a flat `u32`
    /// (it becomes a CDP touch point) and the child must not have to know what an origin
    /// is — see [`ElectronHost::next_wire_contact`].
    contacts: std::collections::HashMap<input_touch::ContactId, TrackedContact>,
    /// The counter behind the browser-facing contact ids.
    next_wire_contact: u32,
    /// When the next session line is due.
    next_report: std::time::Instant,
}

/// A live contact, as the host tracks it: which window took the press, and the flat id
/// the child knows it by.
#[derive(Clone, Copy)]
struct TrackedContact {
    /// The window that owns the contact until it ends — recorded once, at the press, so
    /// a drag that wanders off still ends where it began.
    surface: Surface,
    /// What the child calls it. See [`ElectronHost::next_wire_contact`].
    wire: u32,
}

/// The applied view per window. A struct rather than a map for the same reason
/// [`PendingPaints`] is: two windows is the whole population, and `slot` keeps every
/// user total over it.
#[derive(Default, Clone, Copy)]
struct Views {
    widget: Option<BrowserView>,
    page: Option<BrowserView>,
}

impl Views {
    fn slot(&mut self, surface: Surface) -> &mut Option<BrowserView> {
        match surface {
            Surface::Widget => &mut self.widget,
            Surface::Page => &mut self.page,
        }
    }

    const fn get(&self, surface: Surface) -> Option<BrowserView> {
        match surface {
            Surface::Widget => self.widget,
            Surface::Page => self.page,
        }
    }
}

/// A recovery due at a time, for a window (or for the process when `surface` is `None`).
#[derive(Debug, Clone, Copy)]
struct Retry {
    at: std::time::Instant,
    surface: Option<Surface>,
}

/// The recovery ladder, as arithmetic: what one look at the fault slot should do, given
/// only the attempt count, the pending retry and the clock.
///
/// Extracted from [`ElectronHost::recover`] so it can be table-tested (#235): the ladder
/// is what stops a page that kills three renderers running from cycling through the
/// fourth forever, and the give-up arm is what tells DIAL to stop advertising a page
/// that is not there — and until this existed, both ran only against a real dead
/// Electron on the deploy target. `recover` keeps everything that touches a process or
/// the render loop; this keeps everything that decides. Same split as `mixer::plan`.
#[derive(Debug, Default)]
struct RecoveryLadder {
    /// Faults taken without a healthy stretch in between. [`Self::forgive`] is what a
    /// healthy stretch calls.
    attempts: u32,
    /// A recovery scheduled but not yet due.
    retry: Option<Retry>,
}

/// What the ladder decided.
#[derive(Debug)]
enum Recovery {
    /// Nothing owed: no fault, no retry due.
    Idle,
    /// The fault is within budget; a retry has been scheduled for later.
    Scheduled(Fault),
    /// The fault exhausted the budget: stop rebuilding this surface and say so.
    GiveUp(Fault),
    /// A scheduled retry has come due: rebuild the surface (`None` = everything).
    Rebuild(Option<Surface>),
}

impl RecoveryLadder {
    /// Take one step: absorb `fault` if there is one, and surface whatever has come due.
    fn next(&mut self, fault: Option<Fault>, now: std::time::Instant) -> Recovery {
        if let Some(fault) = fault {
            self.attempts += 1;
            if self.attempts > RECOVERY_ATTEMPTS {
                // The budget is spent. The count starts over so a *different* page can
                // still be recovered later; the caller is the one who knows whether the
                // surface changed.
                self.retry = None;
                self.attempts = 0;
                return Recovery::GiveUp(fault);
            }
            self.retry = Some(Retry {
                at: now + RECOVERY_DELAY,
                surface: fault.surface,
            });
            return Recovery::Scheduled(fault);
        }
        match self.retry {
            Some(retry) if now >= retry.at => {
                self.retry = None;
                Recovery::Rebuild(retry.surface)
            }
            _ => Recovery::Idle,
        }
    }

    /// A healthy stretch: past faults stop counting against the future.
    fn forgive(&mut self) {
        self.attempts = 0;
        self.retry = None;
    }
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
    /// The cell whose *current* engine answers blocking and scriptlet queries. Shared
    /// with the daily refresh, so a respawned browser — like a running one — is always
    /// on the newest lists (#239).
    pub adblock: SharedBlocker,
    /// The panel's mixer, which the page's audio joins as one more source. `None` leaves
    /// the browser silent. See [`BrowserAudio`] for what a device of its own cost.
    pub mixer: Option<Arc<crate::mixer::AudioMixer>>,
    /// The user agent leanback keys off.
    pub user_agent: String,
    /// The kiosk-loop waker a respawned browser's reader thread wakes with (#59).
    pub waker: castaway_core::Waker,
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
            widget: None,
            widget_started: false,
            page_url: None,
            views: Views::default(),
            ladder: RecoveryLadder::default(),
            gave_up: None,
            left_down: None,
            contacts: std::collections::HashMap::new(),
            next_wire_contact: 0,
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

    /// Whether a window has a page worth showing at all.
    fn has_page(&self, surface: Surface) -> bool {
        match surface {
            Surface::Widget => self.widget.is_some(),
            Surface::Page => self.page_url.is_some(),
        }
    }

    /// Queue a command on the browser, if there is one.
    fn send(&self, msg: &ToBrowser) {
        if let Some(e) = &self.electron {
            e.send(msg);
        }
    }

    /// Follow the panel: put each window's page where the panel says it goes.
    ///
    /// The fold. Minimizing and restoring used to be methods here that mutated a role this
    /// host owned, which meant the page's placement and every *other* surface's placement
    /// were two machines that had to be kept in agreement — and were not. Now the panel
    /// decides for all of them and this reacts, once per pump, to a difference it can see —
    /// once per *window*, since each has its own answer: the clock's does not move when a
    /// cast opens fullscreen over it, which is why the cast no longer disturbs it.
    ///
    /// A placement change is a *resize* round trip rather than a transform: the page lays
    /// itself out at the size it will be shown, so the subprocess has to re-rasterize. The
    /// old texture is dropped rather than stretched across the new rect, and the layer comes
    /// back with the first paint at the right size (stale-sized paints are released
    /// unimported in the meantime — see `import_frame`). Coming back onto the glass is the
    /// same round trip even at an unchanged size, because the host app answers a resize
    /// with a forced repaint — which is what fills the layer of a page with no reason to
    /// damage itself, like the clock between ticks.
    fn follow_panel(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        for surface in [Surface::Widget, Surface::Page] {
            let want = self
                .has_page(surface)
                .then(|| render.page_view(surface == Surface::Page))
                .flatten();
            let had = self.views.get(surface);
            if want == had {
                continue;
            }
            *self.views.slot(surface) = want;
            // Layer *or* size changed: either way what is composited under the old view is
            // a texture belonging to a placement that is over. Only *this window's* old
            // layer, though — the other window's picture is exactly what must survive.
            if let Some(had) = had {
                render.clear_browser_layer(had.layer);
            }
            if let Some(view) = want {
                self.send(&ToBrowser::Resize {
                    surface,
                    width: view.rect.width,
                    height: view.rect.height,
                });
            }
        }
    }

    /// Track the kiosk surface size so both browser viewports match.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size = (width, height);
        // The viewports follow from the panel's placements on a surface this size, so the
        // round trips are `follow_panel`'s on the next pump rather than a second set here.
        self.views = Views::default();
    }

    /// One per-frame tick on the main thread: apply commands, recover if needed, and
    /// import whatever the browser painted.
    pub fn pump(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        if !self.widget_started {
            self.widget_started = true;
            self.show_widget(render);
        }
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                BrowserCommand::CastPlatform(port) => {
                    self.send(&ToBrowser::CastPlatform { port });
                }
                BrowserCommand::Navigate(url) => self.show_page(render, &url),
                BrowserCommand::Hide => self.hide_page(render),
            }
        }
        // Where each page belongs is the panel's answer, recomputed every pump so a
        // navigation, a session starting or a demote does not need this host to be told.
        self.follow_panel(render);
        self.recover(render);
        self.import_frames(render);
        self.report();
    }

    /// Pull each window's newest painted frame across and hand it to the compositor.
    fn import_frames(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        for surface in [Surface::Widget, Surface::Page] {
            self.import_frame(render, surface);
        }
    }

    /// Import one window's pending frame, routing it to that window's layer.
    fn import_frame(&mut self, render: &mut crate::render_pipeline::RenderLoop, surface: Surface) {
        let Some(electron) = &self.electron else {
            return;
        };
        #[cfg_attr(not(unix), allow(unused_mut))]
        let Some(mut paint) = electron.take_paint(surface) else {
            return;
        };

        let borrow = InFlight {
            id: paint.id,
            wire: Arc::clone(&electron.wire),
        };
        // Nothing is meant to be showing in this window. A blanked page still paints —
        // about:blank is a white frame, importing it would put a fullscreen white layer
        // where "hidden" should be — and after a dismiss the old page's last paints are
        // still arriving.
        if !self.has_page(surface) {
            return; // `borrow` drops here, releasing the frame.
        }
        let Some(view) = self.views.get(surface) else {
            // Not on the glass: the panel answered `Hidden`, a card owns the slot, or —
            // for the clock — a demoted page does. The window keeps painting; the frames
            // go back.
            return; // `borrow` drops here, releasing the frame.
        };
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
        // The frame's buffer, as a descriptor of ours. Passed with the paint where the
        // fd plane is up (#271); reached in for with `pidfd_getfd`/`DuplicateHandle`
        // otherwise. The counters are what let a test assert *which* — "it painted"
        // says nothing about how the descriptors travelled.
        #[cfg(unix)]
        let local = match paint.scm_fd.take() {
            Some(fd) => {
                electron.health.scm_frames.fetch_add(1, Ordering::Relaxed);
                fd
            }
            None => match electron.process.pull(RemoteHandle(paint.plane.fd)) {
                Ok(handle) => {
                    electron
                        .health
                        .pulled_frames
                        .fetch_add(1, Ordering::Relaxed);
                    handle
                }
                Err(e) => {
                    warn!(target: "castaway::browser", error = %e, "could not fetch the frame's buffer");
                    return; // `borrow` drops here, releasing the frame.
                }
            },
        };
        #[cfg(not(unix))]
        let local = match electron.process.pull(RemoteHandle(paint.plane.fd)) {
            Ok(handle) => {
                electron
                    .health
                    .pulled_frames
                    .fetch_add(1, Ordering::Relaxed);
                handle
            }
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

    /// Put a failed window back if something died, or give up loudly — on that window,
    /// not on both. The clock surviving a crashing cast page (and vice versa) is half of
    /// what the two-window split buys.
    fn recover(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        let fault = self.electron.as_ref().and_then(|e| e.health.take_fault());
        let mut surface = match self.ladder.next(fault, std::time::Instant::now()) {
            Recovery::Idle => return,
            Recovery::Scheduled(fault) => {
                warn!(
                    target: "castaway::browser",
                    fault = %fault.reason,
                    surface = ?fault.surface,
                    attempt = self.ladder.attempts,
                    retry_in = ?RECOVERY_DELAY,
                    "recovering the browser"
                );
                return;
            }
            Recovery::Rebuild(surface) => surface,
            Recovery::GiveUp(fault) => {
                error!(
                    target: "castaway::browser",
                    fault = %fault.reason,
                    surface = ?fault.surface,
                    attempts = RECOVERY_ATTEMPTS,
                    "giving up on the failing browser surface"
                );
                match fault.surface {
                    // Reloading the widget forever is wrong when the widget is what
                    // fails: the panel would cycle through the same crash for good. And
                    // dropping it must not end a cast that is perfectly healthy — so no
                    // `gave_up` here either; that hook is DIAL's, and DIAL is attached
                    // to the page.
                    Some(Surface::Widget) => {
                        error!(
                            target: "castaway::browser",
                            "the idle widget is what failed; leaving its slot empty"
                        );
                        self.widget = None;
                        render.set_surface(panel_surface(Surface::Widget), false);
                        self.send(&ToBrowser::Blank {
                            surface: Surface::Widget,
                        });
                        if let Some(view) = self.views.slot(Surface::Widget).take() {
                            render.clear_browser_layer(view.layer);
                        }
                    }
                    Some(Surface::Page) => {
                        self.hide_page(render);
                        if let Some(gave_up) = self.gave_up.clone() {
                            gave_up();
                        }
                    }
                    // The process: everything is gone, and unrecoverable.
                    None => {
                        self.hide_page(render);
                        render.set_surface(panel_surface(Surface::Widget), false);
                        self.views = Views::default();
                        render.clear_browser();
                        if let Some(gave_up) = self.gave_up.clone() {
                            gave_up();
                        }
                    }
                }
                return;
            }
        };

        // A dead *process* needs respawning, not just re-navigating — the distinction the
        // embedded path never had to make, because there the process was ours.
        let dead = self
            .electron
            .as_mut()
            .is_none_or(|e| matches!(e.child.try_wait(), Ok(Some(_)) | Err(_)));
        if dead {
            self.views = Views::default();
            render.clear_browser();
            if let Some(old) = self.electron.take() {
                old.shutdown();
            }
            match Electron::spawn(
                &self.respawn.program,
                &self.respawn.app_dir,
                self.respawn.adblock.clone(),
                self.respawn.mixer.as_ref(),
                &self.respawn.user_agent,
                self.respawn.waker.clone(),
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
            // A fresh process has neither window; both must come back whatever the
            // fault named.
            surface = None;
        }

        if matches!(surface, Some(Surface::Widget) | None) {
            self.show_widget(render);
        }
        if matches!(surface, Some(Surface::Page) | None) {
            if let Some(url) = self.page_url.clone() {
                info!(target: "castaway::browser", %url, "reloading the page after a fault");
                self.show_page(render, &url);
            }
        }
    }

    /// Navigate the widget window to the configured attract URL, if there is one.
    fn show_widget(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        let Some(url) = self.widget.clone() else {
            return;
        };
        render.set_surface(panel_surface(Surface::Widget), true);
        let view = render.page_view(false);
        *self.views.slot(Surface::Widget) = view;
        let rect = view.map_or_else(
            || BrowserRole::AttractWidget.view(self.size).rect,
            |v| v.rect,
        );
        self.send(&ToBrowser::Navigate {
            surface: Surface::Widget,
            url,
            width: rect.width,
            height: rect.height,
        });
    }

    /// Dismiss the cast page. The widget window is untouched: its clock has been live
    /// underneath the whole time, so "give the panel back" is now nothing more than
    /// taking the page's surface down and letting `follow_panel` hand the slot back.
    fn hide_page(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        render.set_surface(panel_surface(Surface::Page), false);
        if self.page_url.take().is_some() {
            self.send(&ToBrowser::Blank {
                surface: Surface::Page,
            });
        }
        if let Some(view) = self.views.slot(Surface::Page).take() {
            render.clear_browser_layer(view.layer);
        }
    }

    /// Show a cast page — the widget window is not involved, which is why opening a
    /// cast no longer flashes the clock through it.
    fn show_page(&mut self, render: &mut crate::render_pipeline::RenderLoop, url: &str) {
        if self.page_url.as_deref() != Some(url) {
            self.ladder.forgive();
        }
        self.page_url = Some(url.to_string());
        render.set_surface(panel_surface(Surface::Page), true);
        // A cast page arriving *is* a session starting, so it claims the panel on the same
        // terms as every other one — including being declined while somebody is mid-tap.
        // Without this a DIAL launch would arrive demoted whenever the shell happened to
        // have the glass, because nothing else in this path asks for it.
        render.rest_panel_if_idle();
        let view = render.page_view(true);
        *self.views.slot(Surface::Page) = view;
        let rect = view.map_or_else(|| BrowserRole::Fullscreen.view(self.size).rect, |v| v.rect);
        self.send(&ToBrowser::Navigate {
            surface: Surface::Page,
            url: url.to_string(),
            width: rect.width,
            height: rect.height,
        });
    }

    /// The window a fresh input belongs to, with its applied view. The page wins while
    /// it is on the glass — fullscreen it covers the clock, demoted the clock's own
    /// answer is `None` — so there is at most one owner for a point on the panel.
    fn input_target(&self) -> Option<(Surface, BrowserView)> {
        for surface in [Surface::Page, Surface::Widget] {
            if let (true, Some(view)) = (self.has_page(surface), self.views.get(surface)) {
                return Some((surface, view));
            }
        }
        None
    }

    /// A window's viewport for mapping events of a contact it already owns. Falls back
    /// to the window's resting shape when it is momentarily off the glass, so the end of
    /// a drag still lands somewhere coherent.
    fn view_rect(&self, surface: Surface) -> crate::attract::InsetRect {
        self.views.get(surface).map_or_else(
            || {
                let role = match surface {
                    Surface::Widget => BrowserRole::AttractWidget,
                    Surface::Page => BrowserRole::Fullscreen,
                };
                role.view(self.size).rect
            },
            |v| v.rect,
        )
    }

    /// Map a normalized panel coordinate into a window's view pixels, clamped. For a
    /// contact that window already owns.
    fn to_view(&self, surface: Surface, x: f32, y: f32) -> (f32, f32) {
        crate::browser::to_view_px(self.view_rect(surface), self.size, x, y)
    }

    /// The window under a fresh input, or `None` if neither owns that point. For
    /// deciding whether an input belongs to a browser window at all.
    ///
    /// A page that is not on the glass owns nothing: the panel answered `Hidden`, or a
    /// card — or the demoted cast page — took the slot, so what the finger is touching
    /// is whatever is there instead.
    fn hit_target(&self, x: f32, y: f32) -> Option<Surface> {
        let (surface, view) = self.input_target()?;
        crate::browser::hit_view_px(view.rect, self.size, x, y).map(|_| surface)
    }

    /// The next browser-facing contact id.
    ///
    /// The panel's routing keys contacts on [`input_touch::ContactId`], which carries an
    /// origin; the protocol to the child speaks a flat `u32` because it becomes a CDP
    /// touch point id, and CDP has never heard of an origin. Rather than packing the two
    /// halves into 32 bits — which would put a width limit on both, and silently alias
    /// once either overflowed it — each contact is handed a fresh number for its
    /// lifetime, freed when it ends.
    ///
    /// Wrapping is not a collision: it takes 2^32 contacts to come back around, and only
    /// a contact still down at that point could clash.
    fn next_wire_contact(&mut self) -> u32 {
        let id = self.next_wire_contact;
        self.next_wire_contact = self.next_wire_contact.wrapping_add(1);
        id
    }

    /// Tell the child that these contacts have stopped, each in its own window.
    ///
    /// Shared by [`input_touch::InputSink::cancel_all`] and
    /// [`input_touch::InputSink::cancel_origin`], which differ only in which contacts
    /// they select — so the message they send has one spelling.
    fn cancel_tracked(&mut self, contacts: Vec<TrackedContact>) {
        for tracked in contacts {
            self.send(&ToBrowser::Touch {
                surface: tracked.surface,
                id: tracked.wire,
                phase: crate::browser_proto::TouchPhase::Cancel,
                x: 0.0,
                y: 0.0,
            });
        }
    }

    /// When this host next needs the kiosk loop to run it without being asked: a
    /// scheduled recovery's due time (#59). Everything else it does is either driven by
    /// a wake (paints and faults, from the reader thread; commands, from their senders)
    /// or is a reaction to panel state that only moves when the loop runs anyway.
    #[must_use]
    pub fn next_due(&self) -> Option<std::time::Instant> {
        self.ladder.retry.map(|r| r.at)
    }

    /// One structured line every 5 s while the browser has audio, mirroring the mirroring
    /// path's own session log — because "is it in sync" must be answerable from the
    /// journal rather than from someone standing in front of the panel.
    ///
    /// The cadence is best-effort under demand-driven pacing (#59): the line is written
    /// on the first pump at least 5 s after the last one. A video session paints — and
    /// therefore wakes — continuously, so the cadence holds where sync matters; a page
    /// playing audio under a static picture logs only as often as something else runs
    /// the loop, which is a diagnostic delayed, not a frame dropped.
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

    /// Ask the cast page a question. Test-only; see [`Electron::probe`].
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if there is no browser or it does not answer.
    pub fn probe(
        &self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<String, PipelineError> {
        self.probe_on(Surface::Page, expression, timeout)
    }

    /// Ask a named window's page a question. Test-only, like [`Self::probe`] — it is
    /// how a test sees that the clock kept its state while a cast came and went.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if there is no browser or it does not answer.
    pub fn probe_on(
        &self,
        surface: Surface,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<String, PipelineError> {
        self.electron
            .as_ref()
            .ok_or_else(|| PipelineError::GpuInit("no browser to probe".into()))?
            .probe(surface, expression, timeout)
    }

    /// Whether the browser is delivering GPU frames.
    #[must_use]
    pub fn is_software_fallback(&self) -> bool {
        self.electron
            .as_ref()
            .is_some_and(Electron::is_software_fallback)
    }

    /// See [`Electron::fd_transport_counts`]. `(0, 0)` with no browser.
    #[must_use]
    pub fn fd_transport_counts(&self) -> (u64, u64) {
        self.electron
            .as_ref()
            .map_or((0, 0), Electron::fd_transport_counts)
    }

    /// See [`Electron::av_skew_ms`] (#278). `None` with no browser.
    #[must_use]
    pub fn av_skew_ms(&self) -> Option<i64> {
        self.electron.as_ref().and_then(Electron::av_skew_ms)
    }

    /// See [`Electron::audio_blocks`]. `0` with no browser.
    #[must_use]
    pub fn audio_blocks(&self) -> u64 {
        self.electron.as_ref().map_or(0, Electron::audio_blocks)
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
        // A contact belongs to a window only if it *started* inside that window's
        // viewport, and then it belongs to it until it ends. Two failures this prevents,
        // and the second is the nastier one:
        //
        //  - When the target is the idle screen's clock card, a touch anywhere on the
        //    65-inch panel used to be clamped into that corner and delivered to the page.
        //  - Deciding per-event instead of per-contact would drop the *end* of a drag
        //    that wandered off the card, and the page would believe a finger was down for
        //    the rest of the session.
        //
        // Ownership is recorded as *which window*, not merely "ours": the panel can move
        // the page mid-drag, and the end of that drag must still go to the page rather
        // than being re-aimed at whatever owns the glass by then.
        let owned: Option<TrackedContact> = match event.phase {
            TouchPhase::Down => {
                let target = self.hit_target(event.x, event.y);
                target.map(|surface| {
                    let tracked = TrackedContact {
                        surface,
                        wire: self.next_wire_contact(),
                    };
                    self.contacts.insert(event.id, tracked);
                    tracked
                })
            }
            TouchPhase::Move => self.contacts.get(&event.id).copied(),
            TouchPhase::Up | TouchPhase::Cancel => self.contacts.remove(&event.id),
        };
        let Some(tracked) = owned else { return };
        let (x, y) = self.to_view(tracked.surface, event.x, event.y);
        self.send(&ToBrowser::Touch {
            surface: tracked.surface,
            id: tracked.wire,
            phase: event.phase.into(),
            x,
            y,
        });
    }

    fn cancel_all(&mut self) {
        // Every contact a window thinks is down, told to stop — each in the window that
        // owns it. Without this a page keeps a phantom finger for the life of the
        // session: the browser host holds a contact map keyed by id and only an end or a
        // cancel removes an entry.
        let contacts: Vec<TrackedContact> = self.contacts.drain().map(|(_, t)| t).collect();
        self.cancel_tracked(contacts);
        self.left_down = None;
    }

    fn cancel_origin(&mut self, origin: input_touch::InputOrigin) {
        // The same thing for one device only. A remote peer that drops mid-drag must not
        // take the panel's own fingers — or a second peer's — down with it, which is
        // exactly what `cancel_all` would do.
        //
        // `left_down` is deliberately untouched: it tracks the panel's own mouse, and a
        // remote's clicks arrive as contacts rather than through the pointer path.
        let doomed: Vec<input_touch::ContactId> = self
            .contacts
            .keys()
            .copied()
            .filter(|id| id.is_from(origin))
            .collect();
        let tracked: Vec<TrackedContact> = doomed
            .into_iter()
            .filter_map(|id| self.contacts.remove(&id))
            .collect();
        self.cancel_tracked(tracked);
    }

    fn key(&mut self, key: input_touch::Key) {
        // Keys have no position, so they go to the window that owns input — the page
        // while it is on the glass, the widget otherwise — which is also where anything
        // focusable lives. No window on the glass means nothing to type at (#260).
        let Some((surface, _)) = self.input_target() else {
            return;
        };
        self.send(&ToBrowser::Key {
            surface,
            key: key.into(),
        });
    }

    fn text(&mut self, text: &str) {
        let Some((surface, _)) = self.input_target() else {
            return;
        };
        self.send(&ToBrowser::InsertText {
            surface,
            text: text.to_owned(),
        });
    }

    fn pointer(&mut self, event: input_touch::PointerEvent) {
        use crate::browser_proto::PointerKind;
        use input_touch::PointerEvent;
        match event {
            PointerEvent::Move { x, y } => {
                // While a button is held its window owns the pointer wherever it goes;
                // otherwise a hover only counts inside the target's viewport.
                let surface = match self.left_down {
                    Some(surface) => surface,
                    None => match self.hit_target(x, y) {
                        Some(surface) => surface,
                        None => return,
                    },
                };
                let (x, y) = self.to_view(surface, x, y);
                self.send(&ToBrowser::Pointer {
                    surface,
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
                // Press must land inside some window; release is delivered iff the press
                // was one's, and to that one, for the same reason a touch's end is.
                let surface = if down {
                    let Some(surface) = self.hit_target(x, y) else {
                        return;
                    };
                    self.left_down = Some(surface);
                    surface
                } else {
                    let Some(surface) = self.left_down.take() else {
                        return;
                    };
                    surface
                };
                let (x, y) = self.to_view(surface, x, y);
                self.send(&ToBrowser::Pointer {
                    surface,
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
                // A scroll goes to whatever is under the cursor, so it has to be over
                // the target window.
                let Some(surface) = self.hit_target(x, y) else {
                    return;
                };
                let (x, y) = self.to_view(surface, x, y);
                self.send(&ToBrowser::Wheel {
                    surface,
                    x,
                    y,
                    dx,
                    dy,
                });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adblock_engine::AdBlocker;

    /// A refresh changes the verdict the *next* query gets — no respawn anywhere.
    ///
    /// This is #239 at the seam it regressed on: the reader thread used to hold a
    /// boot-time `Arc<AdBlocker>`, so the daily refresh swapped an engine nothing read
    /// and refreshed lists only took effect at process restart. `answers` here is the
    /// reader thread's own handle, cloned at spawn the way `Electron::spawn` clones it.
    #[test]
    fn a_refresh_changes_the_next_adblock_verdict_without_a_respawn() {
        let shared = SharedBlocker::new(AdBlocker::from_list_text("||before.example^\n"));
        let answers = shared.clone(); // what the reader thread holds for the process's life
        let page = "https://site.test/";

        assert_eq!(
            adblock_answer(&answers, 1, "https://before.example/a.js", page, "script"),
            ToBrowser::AdblockVerdict { id: 1, block: true }
        );
        assert_eq!(
            adblock_answer(&answers, 2, "https://after.example/a.js", page, "script"),
            ToBrowser::AdblockVerdict {
                id: 2,
                block: false
            }
        );

        // The daily refresh lands.
        shared.install(AdBlocker::from_list_text("||after.example^\n"));

        assert_eq!(
            adblock_answer(&answers, 3, "https://after.example/a.js", page, "script"),
            ToBrowser::AdblockVerdict { id: 3, block: true },
            "the refreshed rules must answer the very next query"
        );
        assert_eq!(
            adblock_answer(&answers, 4, "https://before.example/a.js", page, "script"),
            ToBrowser::AdblockVerdict {
                id: 4,
                block: false
            },
            "and the superseded rules must be gone"
        );
    }

    /// The scriptlet path reads the same cell: a refresh that brings a new `##+js(...)`
    /// rule (and its body) reaches the next navigation's injection query.
    #[test]
    fn a_refresh_changes_the_next_scriptlet_injection_without_a_respawn() {
        use adblock::resources::{MimeType, Resource, ResourceType};
        use base64::Engine as _;

        let shared = SharedBlocker::new(AdBlocker::with_defaults());
        let answers = shared.clone();

        let before = scriptlet_answer(&answers, 1, "https://example.com/");
        assert_eq!(
            before,
            ToBrowser::ScriptletSource {
                id: 1,
                source: String::new()
            },
            "the boot lists have nothing to inject here"
        );

        // The refresh brings a rule and the body it names (`probe.js`: a `##+js(probe)`
        // rule resolves to the resource with the `.js` extension).
        let mut refreshed = AdBlocker::from_list_text("example.com##+js(probe, hello)\n");
        refreshed.use_resources(vec![Resource {
            name: "probe.js".to_string(),
            aliases: vec![],
            kind: ResourceType::Mime(MimeType::ApplicationJavascript),
            content: base64::prelude::BASE64_STANDARD
                .encode("function probe(a) { console.log('probe:' + a); }"),
            dependencies: vec![],
            permission: Default::default(),
        }]);
        shared.install(refreshed);

        match scriptlet_answer(&answers, 2, "https://example.com/") {
            ToBrowser::ScriptletSource { id: 2, source } => assert!(
                source.contains("probe") && source.contains("hello"),
                "the refreshed rule's scriptlet must reach the next navigation: {source}"
            ),
            other => panic!("wrong answer shape: {other:?}"),
        }
    }

    fn fault(surface: Option<Surface>) -> Option<Fault> {
        Some(Fault {
            surface,
            reason: "a test renderer died".into(),
        })
    }

    /// The whole ladder, in chosen instants. Until #235 every arm of this ran only
    /// against a real dead Electron on the deploy target — the give-up arm is what stops
    /// a page that kills three renderers running from cycling through the fourth
    /// forever, and the `gave_up` hook downstream of it is what tells DIAL to stop
    /// advertising a page that is not there.
    #[test]
    fn three_faults_are_retried_and_the_fourth_gives_up() {
        let mut ladder = RecoveryLadder::default();
        let t0 = std::time::Instant::now();

        for attempt in 1..=RECOVERY_ATTEMPTS {
            let step = ladder.next(fault(Some(Surface::Page)), t0);
            assert!(
                matches!(step, Recovery::Scheduled(_)),
                "attempt {attempt} should schedule, got {step:?}"
            );
            // Not yet due: nothing happens however often the loop looks.
            assert!(matches!(ladder.next(None, t0), Recovery::Idle));
            // Due: the rebuild comes out, once.
            let due = t0 + RECOVERY_DELAY;
            assert!(matches!(
                ladder.next(None, due),
                Recovery::Rebuild(Some(Surface::Page))
            ));
            assert!(
                matches!(ladder.next(None, due), Recovery::Idle),
                "a served retry must not fire twice"
            );
        }

        // The budget is spent: the next fault is a give-up, and no retry is scheduled.
        let step = ladder.next(fault(Some(Surface::Page)), t0);
        assert!(matches!(step, Recovery::GiveUp(_)), "got {step:?}");
        assert!(matches!(
            ladder.next(None, t0 + RECOVERY_DELAY * 10),
            Recovery::Idle
        ));
    }

    #[test]
    fn a_healthy_stretch_forgives_the_past() {
        // A new page navigation resets the count: three faults across three different
        // pages are not one page failing three times.
        let mut ladder = RecoveryLadder::default();
        let t0 = std::time::Instant::now();
        for _ in 0..RECOVERY_ATTEMPTS {
            let _ = ladder.next(fault(None), t0);
        }
        ladder.forgive();
        assert!(
            matches!(ladder.next(fault(None), t0), Recovery::Scheduled(_)),
            "after forgiveness the ladder starts over rather than giving up"
        );
    }

    #[test]
    fn a_process_fault_rebuilds_everything() {
        // `None` surface is the control socket closing: both windows are gone, and the
        // rebuild must say so rather than naming the last window that happened to fail.
        let mut ladder = RecoveryLadder::default();
        let t0 = std::time::Instant::now();
        assert!(matches!(
            ladder.next(fault(None), t0),
            Recovery::Scheduled(_)
        ));
        assert!(matches!(
            ladder.next(None, t0 + RECOVERY_DELAY),
            Recovery::Rebuild(None)
        ));
    }
}
