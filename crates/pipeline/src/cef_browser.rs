//! CEF (Chromium Embedded Framework) offscreen browser. Renders a web page windowlessly
//! and delivers BGRA frames via `on_paint`, which we hand to the compositor as the
//! `Browser` layer (the doc's §5 "double duty": this same browser hosts YouTube's TV
//! surface for the Lounge path).
//!
//! CEF is multi-process: [`Cef::bootstrap`] must run *first thing* in `main`, because a
//! subprocess launch re-execs this same binary and has to be handled before anything
//! else. The browser process then [`Cef::initialize`]s and pumps [`Cef::pump`] on the
//! main thread. MVP is the CPU `on_paint` path (mature; accelerated shared-texture OSR
//! is buggy upstream — cross-build.md / Q6).
//!
//! This is the FFI boundary to libcef, so `unsafe` is permitted here (ground rule 8);
//! the one `unsafe` block carries a `// SAFETY:` note.
#![allow(
    unsafe_code,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::sync::{Arc, Mutex, OnceLock, RwLock};

use cef::rc::Rc as _;
use cef::{args::Args, *};
use tracing::{debug, info, warn};

use crate::attract::{InsetRect, WidgetSlot};
use crate::cef_adblock::AdBlocker;
use crate::compositor::{DirtyRect, Transform};
use crate::error::PipelineError;

/// The ad blocker, swappable while the browser is running.
///
/// The live client holds its own handle, so a daily refresh that replaced a plain `Arc`
/// would update nothing that is actually blocking requests. Everything reads through this
/// cell instead, and [`crate::filterlists::spawn_daily_refresh`] writes to it.
pub type SharedBlocker = Arc<RwLock<Arc<AdBlocker>>>;

/// A frame painted by CEF: BGRA8, top-down.
#[derive(Clone)]
pub struct CefFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// BGRA8 pixels (`width*height*4`).
    pub bgra: Vec<u8>,
}

/// The accumulated browser frame plus the regions painted since the last consume.
/// `on_paint` copies only the dirty rows in; the consumer uploads only those regions —
/// during video playback that's mostly the video rect, not the whole 4K surface.
#[derive(Default)]
struct FrameAccum {
    width: u32,
    height: u32,
    /// BGRA8, always the complete current frame (`width*height*4`).
    bgra: Vec<u8>,
    /// Painted-but-not-consumed regions.
    dirty: Vec<DirtyRect>,
}

impl FrameAccum {
    /// Merge one `on_paint` into the accumulated frame, copying only the dirty rows.
    fn paint(&mut self, src: &[u8], width: u32, height: u32, rects: &[DirtyRect]) {
        let need = (width as usize) * (height as usize) * 4;
        if self.width != width || self.height != height || self.bgra.len() != need {
            // First paint or resize: take the whole frame.
            self.width = width;
            self.height = height;
            self.bgra.clear();
            self.bgra.extend_from_slice(&src[..need]);
            self.dirty = vec![DirtyRect::full(width, height)];
            return;
        }
        let full = [DirtyRect::full(width, height)];
        let rects = if rects.is_empty() { &full[..] } else { rects };
        let stride = (width as usize) * 4;
        for rect in rects {
            let Some(r) = rect.clamped(width, height) else {
                continue;
            };
            let row_bytes = (r.width as usize) * 4;
            let mut off = (r.y as usize) * stride + (r.x as usize) * 4;
            for _ in 0..r.height {
                self.bgra[off..off + row_bytes].copy_from_slice(&src[off..off + row_bytes]);
                off += stride;
            }
            self.dirty.push(r);
        }
        // Degenerate accumulations (long lists, or most of the frame anyway) collapse
        // to one full-frame write — cheaper than per-rect bookkeeping past that point.
        let area: u64 = self.dirty.iter().map(|r| r.area()).sum();
        if self.dirty.len() > 32 || area.saturating_mul(2) > DirtyRect::full(width, height).area() {
            self.dirty = vec![DirtyRect::full(width, height)];
        }
    }
}

/// A shared accumulator for painted frames. Cloneable; the render handler writes,
/// the compositor/consumer reads.
#[derive(Clone, Default)]
pub struct CefFrameSink {
    inner: Arc<Mutex<FrameAccum>>,
}

impl CefFrameSink {
    /// Clone out the latest complete frame, if any (screenshot/example path).
    #[must_use]
    pub fn latest(&self) -> Option<CefFrame> {
        self.inner.lock().ok().and_then(|g| {
            (!g.bgra.is_empty()).then(|| CefFrame {
                width: g.width,
                height: g.height,
                bgra: g.bgra.clone(),
            })
        })
    }

    /// If any regions were painted since the last consume, hand the accumulated frame
    /// and those regions to `f` and mark them consumed; `None` if nothing changed.
    /// The lock is held across `f` — paints and consumes both happen on the kiosk main
    /// thread (the CEF UI thread under the external pump), so it never contends.
    pub fn consume<R>(&self, f: impl FnOnce(u32, u32, &[u8], &[DirtyRect]) -> R) -> Option<R> {
        let mut g = self.inner.lock().ok()?;
        if g.bgra.is_empty() || g.dirty.is_empty() {
            return None;
        }
        let dirty = std::mem::take(&mut g.dirty);
        Some(f(g.width, g.height, &g.bgra, &dirty))
    }

    /// Drop the accumulated frame (browser hidden) so the next paint starts fresh.
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            *g = FrameAccum::default();
        }
    }

    fn paint(&self, src: &[u8], width: u32, height: u32, rects: &[DirtyRect]) {
        if let Ok(mut g) = self.inner.lock() {
            g.paint(src, width, height, rects);
        }
    }
}

/// The offscreen viewport size, shared with the render handler's `view_rect`.
#[derive(Clone)]
struct ViewSize {
    dims: Arc<Mutex<(u32, u32)>>,
}

// --- Render process: scriptlet injection at document start ---

/// The blocker used *inside the render process* to decide what to inject, together with
/// the cache timestamp it was built from.
///
/// It is a second engine, not the browser process's one, because these are separate
/// processes: CEF re-execs this binary for the renderer, and `on_context_created` fires
/// there. Keyed on the cache stamp so a daily refresh reaches injections too — a renderer
/// can outlive several refreshes, and one that never re-read would keep injecting the
/// rules it started with while the browser process blocked by newer ones.
static RENDER_BLOCKER: OnceLock<Mutex<Option<CachedBlocker>>> = OnceLock::new();

/// An engine and the cache timestamp it was built from.
type CachedBlocker = (Option<std::time::SystemTime>, Arc<AdBlocker>);

/// The engine for this render process, rebuilt only when the cached lists have changed.
///
/// Cache-only, never fetching: a renderer is short-lived relative to the browser process
/// and must not block a page load on the network. The browser process refreshes the cache;
/// this notices.
///
/// Its log lines only exist because `main` installs the tracing subscriber *before*
/// `Cef::bootstrap`: a subprocess never leaves `execute_process`, so a subscriber
/// installed after it would cover the browser process alone and this whole path — the one
/// that does the injecting — would be silent.
fn render_blocker() -> Option<Arc<AdBlocker>> {
    let paths = crate::filterlists::CachePaths::default();
    let stamp = crate::filterlists::cache_stamp(&paths);
    let cell = RENDER_BLOCKER.get_or_init(|| Mutex::new(None));
    let mut slot = cell.lock().ok()?;

    if let Some((built_from, blocker)) = slot.as_ref() {
        if *built_from == stamp {
            return Some(Arc::clone(blocker));
        }
        debug!(target: "castaway::adblock", "cached lists changed; rebuilding for injection");
    }

    let blocker = Arc::new(crate::filterlists::load_cached_only(&paths)?);
    info!(
        target: "castaway::adblock",
        scriptlets = blocker.scriptlet_count(),
        "render process ready to inject"
    );
    *slot = Some((stamp, Arc::clone(&blocker)));
    Some(blocker)
}

#[derive(Clone)]
struct CastawayRenderProcess;

wrap_render_process_handler! {
    struct RenderProcessBuilder {
        handler: CastawayRenderProcess,
    }
    impl RenderProcessHandler {
        /// Runs once per frame, *before* the page's own scripts. This is the whole reason
        /// the render process is involved at all: uBlock Origin's `##+js(...)` rules work
        /// by hooking things like `fetch` and `XMLHttpRequest` before the page can use
        /// them, so injecting later — from the browser process, on load-start — would be
        /// injecting after the thing it needed to intercept had already happened.
        fn on_context_created(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _context: Option<&mut V8Context>,
        ) {
            let Some(frame) = frame else { return };
            let url = CefString::from(&frame.url()).to_string();
            // about:blank and the like carry no rules and no risk.
            if !url.starts_with("http") {
                return;
            }
            let Some(blocker) = render_blocker() else { return };
            let Some(script) = blocker.injected_script(&url) else { return };
            debug!(
                target: "castaway::adblock",
                %url, bytes = script.len(), "injecting scriptlets"
            );
            // The script URL is what shows up in a stack trace from inside the injection,
            // so name it something a person debugging the page will recognise.
            frame.execute_java_script(
                Some(&script.as_str().into()),
                Some(&"castaway://scriptlets".into()),
                0,
            );
        }
    }
}

// --- App: process-wide callbacks (command line tweaks) ---

#[derive(Clone)]
struct CastawayApp;

wrap_app! {
    struct AppBuilder {
        app: CastawayApp,
    }
    impl App {
        /// CEF asks every process for this; only the renderer ever calls into it.
        fn render_process_handler(&self) -> Option<RenderProcessHandler> {
            Some(RenderProcessBuilder::new(CastawayRenderProcess))
        }


        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            if let Some(cl) = command_line {
                // Headless-friendly + no GPU sandbox surprises on the kiosk box.
                cl.append_switch(Some(&"disable-gpu-shader-disk-cache".into()));
                cl.append_switch(Some(&"enable-logging=stderr".into()));
                // We consume the CPU `on_paint` path (accelerated OSR is buggy upstream
                // — cross-build.md Q6). With GPU compositing on, every OSR frame takes a
                // GPU→CPU readback that caps the paint rate well below the requested
                // windowless_frame_rate; software compositing paints straight into the
                // shared-memory buffer and is the CEF-recommended pairing for
                // windowless rendering.
                cl.append_switch(Some(&"disable-gpu".into()));
                cl.append_switch(Some(&"disable-gpu-compositing".into()));
                // A cast receiver is all autoplay: the sender queues a video over the
                // Lounge and the page calls play() with no user gesture behind it. This
                // CEF build already permits that (measured — the self-play harness reaches
                // PLAYING without this switch), but stock Chromium's default policy does
                // not: the same page there buffers and drops back to UNSTARTED, binding
                // and accepting the playlist and then just sitting. Pinned rather than
                // inherited, because there is no user to gesture at a kiosk and the
                // failure mode is silent — and the Windows CEF is a different binary.
                cl.append_switch_with_value(
                    Some(&"autoplay-policy".into()),
                    Some(&"no-user-gesture-required".into()),
                );
                // Widevine, if this build shipped it. Without a CDM every EME-gated
                // stream fails, and it fails *quietly* — the page reports an error to its
                // own console and the panel just does not play, which is indistinguishable
                // from a network problem. The path comes from the environment rather than
                // being compiled in because the CDM is an unfree binary: a build that does
                // not want it simply does not set this, and everything else still works.
                if let Some(path) = widevine_path() {
                    info!(target: "castaway::cef", %path, "widevine cdm");
                    cl.append_switch_with_value(
                        Some(&"widevine-cdm-path".into()),
                        Some(&path.as_str().into()),
                    );
                } else {
                    // Said once, at startup, because the alternative is discovering it
                    // from a leanback console line while standing in front of the panel.
                    debug!(
                        target: "castaway::cef",
                        "no widevine cdm; DRM-protected video will not play"
                    );
                }
            }
        }
    }
}

/// Where this build's Widevine CDM lives, if it has one.
///
/// `CASTAWAY_WIDEVINE_PATH` should be the directory holding `manifest.json` and
/// `_platform_specific/` — the layout Chrome ships and nixpkgs' `widevine-cdm` reproduces
/// under `share/google/chrome/WidevineCdm`. Set by the packaging wrapper next to
/// `CEF_PATH`, for the same reason: it is a runtime lookup, not a build-time one.
fn widevine_path() -> Option<String> {
    let path = std::env::var("CASTAWAY_WIDEVINE_PATH").ok()?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    // Checked rather than trusted: a stale path silently disables DRM, which is the exact
    // failure this is meant to remove.
    if !std::path::Path::new(path).join("manifest.json").is_file() {
        warn!(
            target: "castaway::cef",
            %path,
            "CASTAWAY_WIDEVINE_PATH has no manifest.json; ignoring it"
        );
        return None;
    }
    Some(path.to_owned())
}

// --- RenderHandler: viewport + on_paint ---

#[derive(Clone)]
struct CastawayRenderHandler {
    size: ViewSize,
    sink: CefFrameSink,
}

wrap_render_handler! {
    struct RenderHandlerBuilder {
        handler: CastawayRenderHandler,
    }
    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                let (w, h) = self
                    .handler
                    .size
                    .dims
                    .lock()
                    .map_or((1, 1), |g| *g);
                rect.x = 0;
                rect.y = 0;
                rect.width = w.max(1) as _;
                rect.height = h.max(1) as _;
            }
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            _type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = (width * height * 4) as usize;
            // SAFETY: CEF guarantees `buffer` points to `width*height*4` BGRA bytes for
            // the duration of this callback; the sink copies the dirty rows out before
            // we return.
            let src = unsafe { std::slice::from_raw_parts(buffer, len) };
            let rects: Vec<DirtyRect> = dirty_rects
                .unwrap_or_default()
                .iter()
                .filter(|r| r.x >= 0 && r.y >= 0 && r.width > 0 && r.height > 0)
                .map(|r| DirtyRect {
                    x: r.x as u32,
                    y: r.y as u32,
                    width: r.width as u32,
                    height: r.height as u32,
                })
                .collect();
            self.handler
                .sink
                .paint(src, width as u32, height as u32, &rects);
        }
    }
}

// --- ResourceRequestHandler: ad/tracker blocking on every resource load ---

#[derive(Clone)]
struct ResourceInner {
    adblock: SharedBlocker,
}

wrap_resource_request_handler! {
    struct ResourceRequestHandlerBuilder {
        inner: ResourceInner,
    }
    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let Some(request) = request else {
                return ReturnValue::CONTINUE;
            };
            let url = CefString::from(&request.url()).to_string();
            let source = frame
                .map(|f| CefString::from(&f.url()).to_string())
                .unwrap_or_default();
            let kind = adblock_type(request.resource_type());
            // Cloned out of the cell so the lock is not held across the decision, which
            // is what lets a refresh swap the engine without stalling page loads.
            // A poisoned lock must not turn into a page that loads nothing, so the
            // request is allowed through rather than cancelled.
            let Some(blocker) = self.inner.adblock.read().ok().map(|g| Arc::clone(&g)) else {
                return ReturnValue::CONTINUE;
            };
            if blocker.should_block(&url, &source, kind) {
                ReturnValue::CANCEL
            } else {
                ReturnValue::CONTINUE
            }
        }
    }
}

/// Map CEF's `ResourceType` to an Adblock request-type string.
fn adblock_type(rt: ResourceType) -> &'static str {
    if rt == ResourceType::SCRIPT {
        "script"
    } else if rt == ResourceType::IMAGE || rt == ResourceType::FAVICON {
        "image"
    } else if rt == ResourceType::STYLESHEET {
        "stylesheet"
    } else if rt == ResourceType::FONT_RESOURCE {
        "font"
    } else if rt == ResourceType::MEDIA {
        "media"
    } else if rt == ResourceType::XHR {
        "xmlhttprequest"
    } else if rt == ResourceType::SUB_FRAME {
        "subdocument"
    } else if rt == ResourceType::MAIN_FRAME {
        "document"
    } else if rt == ResourceType::OBJECT {
        "object"
    } else if rt == ResourceType::PING || rt == ResourceType::CSP_REPORT {
        "ping"
    } else {
        "other"
    }
}

// --- RequestHandler: provides the ResourceRequestHandler ---

#[derive(Clone)]
struct RequestInner {
    resource_handler: ResourceRequestHandler,
    health: std::sync::Arc<BrowserHealth>,
}

wrap_request_handler! {
    struct RequestHandlerBuilder {
        inner: RequestInner,
    }
    impl RequestHandler {
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            // The whole reason this handler exists. Without it a dead renderer left the
            // last painted frame frozen on the panel — indistinguishable from a paused
            // video — while DIAL still said `running`.
            tracing::error!(
                target: "castaway::cef",
                status = ?status,
                error_code,
                detail = %error_string.map(ToString::to_string).unwrap_or_default(),
                "the render process died"
            );
            self.inner.health.note_crash();
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            Some(self.inner.resource_handler.clone())
        }
    }
}

// --- DisplayHandler: surface the page's JS console on the `castaway::console` target
// (YouTube leanback logs its player errors there — the only visibility we have into
// "why won't this video play" on a kiosk with no devtools) ---

wrap_display_handler! {
    struct DisplayHandlerBuilder;
    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let msg = message.map(ToString::to_string).unwrap_or_default();
            let src = source.map(ToString::to_string).unwrap_or_default();
            if level == LogSeverity::ERROR || level == LogSeverity::FATAL {
                tracing::warn!(target: "castaway::console", %src, line, "{msg}");
            } else {
                tracing::debug!(target: "castaway::console", %src, line, "{msg}");
            }
            0
        }
    }
}

// --- LoadHandler + renderer-death: the browser's own supervision ---

/// What the browser has told us about its health since the host last looked.
///
/// Set from CEF's callbacks (browser process, on the CEF thread) and read by
/// [`BrowserHost::pump`] on the kiosk main thread, so these are atomics rather than a
/// lock: the pump runs once per redraw and must never block on a handler.
///
/// The failure this exists to catch is entirely silent. A sad-tab renderer crash leaves
/// the *last painted frame* on the panel at z=5 — a still image of a working YouTube page
/// — while DIAL goes on reporting `running` and the screen id stays published. Nobody in
/// the room can tell that from a video that is merely paused.
#[derive(Debug, Default)]
pub struct BrowserHealth {
    /// The render process died. Whatever is on screen is a frozen corpse.
    crashed: std::sync::atomic::AtomicBool,
    /// The main frame failed to load.
    load_failed: std::sync::atomic::AtomicBool,
}

impl BrowserHealth {
    fn note_crash(&self) {
        self.crashed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn note_load_failure(&self) {
        self.load_failed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// A load completed, so whatever went wrong before is over.
    fn note_healthy(&self) {
        self.load_failed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.crashed
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Take the pending complaint, if there is one.
    fn take_fault(&self) -> Option<BrowserFault> {
        use std::sync::atomic::Ordering::SeqCst;
        if self.crashed.swap(false, SeqCst) {
            self.load_failed.store(false, SeqCst);
            return Some(BrowserFault::RendererGone);
        }
        if self.load_failed.swap(false, SeqCst) {
            return Some(BrowserFault::LoadFailed);
        }
        None
    }
}

/// Why the browser needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserFault {
    /// The render process died: sad tab, OOM kill, or `chrome://crash`.
    RendererGone,
    /// The main frame would not load: DNS down at boot, a captive portal, a deploy that
    /// breaks under our pinned TV user agent.
    LoadFailed,
}

wrap_load_handler! {
    struct LoadHandlerBuilder {
        health: std::sync::Arc<BrowserHealth>,
    }
    impl LoadHandler {
        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // Subframes fail all the time on a page like YouTube's — an ad iframe we
            // blocked is a *success* by our lights. Only the main frame means the page
            // is not there. `ABORTED` is not a failure either: it is what a navigation
            // that replaced another one reports, so every `Navigate` produces one.
            let main_frame = frame.as_ref().is_none_or(|f| f.is_main() != 0);
            if !main_frame || error_code == Errorcode::ABORTED {
                return;
            }
            tracing::warn!(
                target: "castaway::cef",
                code = ?error_code,
                url = %failed_url.map(ToString::to_string).unwrap_or_default(),
                "{}",
                error_text.map(ToString::to_string).unwrap_or_default()
            );
            self.health.note_load_failure();
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            http_status_code: ::std::os::raw::c_int,
        ) {
            if frame.as_ref().is_none_or(|f| f.is_main() != 0) {
                debug!(target: "castaway::cef", status = http_status_code, "page loaded");
                self.health.note_healthy();
            }
        }
    }
}

// --- Client: hands CEF our render + request + display + load handlers ---

#[derive(Clone)]
struct ClientInner {
    render_handler: RenderHandler,
    request_handler: RequestHandler,
    display_handler: DisplayHandler,
    load_handler: LoadHandler,
}

wrap_client! {
    struct ClientBuilder {
        inner: ClientInner,
    }
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.inner.render_handler.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.inner.request_handler.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.inner.display_handler.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.inner.load_handler.clone())
        }
    }
}

/// A running CEF instance. Owns the process args + app and drives the message loop.
pub struct Cef {
    args: Args,
    app: App,
    adblock: SharedBlocker,
    user_agent: Option<String>,
}

/// A smart-TV user agent that makes `youtube.com/tv` serve the 10-foot leanback UI
/// (Chromium's default desktop UA gets the normal site).
pub const TV_USER_AGENT: &str =
    "Mozilla/5.0 (SMART-TV; Linux; Tizen 6.0) AppleWebKit/537.36 (KHTML, like Gecko) \
     Version/6.0 TV Safari/537.36";

impl Cef {
    /// Must be called first thing in `main`. Returns `Ok(None)` if this invocation was a
    /// CEF **subprocess** (the caller should exit immediately); `Ok(Some(cef))` in the
    /// browser process to continue.
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if CEF process execution fails unexpectedly.
    pub fn bootstrap() -> Result<Option<Self>, PipelineError> {
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = Args::new();
        let mut app = AppBuilder::new(CastawayApp);

        let ret = execute_process(
            Some(args.as_main_args()),
            Some(&mut app),
            std::ptr::null_mut(),
        );
        if ret >= 0 {
            // This was a subprocess; it has run to completion.
            debug!(code = ret, "cef subprocess finished");
            return Ok(None);
        }
        // Browser process: build the ad blocker (parses rules once).
        let adblock: SharedBlocker = Arc::new(RwLock::new(Arc::new(AdBlocker::with_defaults())));
        Ok(Some(Self {
            args,
            app,
            adblock,
            user_agent: None,
        }))
    }

    /// Replace the ad blocker (e.g. loaded from the real subscriptions).
    ///
    /// Safe to call at any time, including while pages are loading: requests read the
    /// current blocker through a lock rather than holding one from browser-creation time.
    pub fn set_adblock(&mut self, adblock: AdBlocker) {
        if let Ok(mut slot) = self.adblock.write() {
            *slot = Arc::new(adblock);
        }
    }

    /// A handle to the blocker cell, for a refresher to swap into later.
    #[must_use]
    pub fn adblock_handle(&self) -> SharedBlocker {
        self.adblock.clone()
    }

    /// Set the browser user agent (call before [`Self::initialize`]). Use [`TV_USER_AGENT`]
    /// for YouTube leanback.
    pub fn set_user_agent(&mut self, ua: &str) {
        self.user_agent = Some(ua.to_string());
    }

    /// Initialize the CEF browser process (windowless, external message pump).
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if `cef_initialize` fails.
    pub fn initialize(&mut self) -> Result<(), PipelineError> {
        // A STABLE cache dir, not temp_dir(): the profile stores durable choices (e.g.
        // YouTube leanback's "Watch as guest"), and nix-shell TMPDIRs differ per shell,
        // which would re-ask on every kiosk restart.
        let cache = stable_cache_dir();
        let _ = std::fs::create_dir_all(&cache);
        // Point CEF at the flattened distribution's resources (.pak/ICU/locales). The
        // build copies them next to the *crate* binary, but a subprocess/example binary
        // may live elsewhere, so set them explicitly from CEF_PATH when available.
        let (resources_dir, locales_dir) = match std::env::var("CEF_PATH") {
            Ok(p) => (
                CefString::from(p.as_str()),
                CefString::from(format!("{p}/locales").as_str()),
            ),
            Err(_) => (CefString::default(), CefString::default()),
        };
        let settings = Settings {
            no_sandbox: 1,
            windowless_rendering_enabled: 1,
            external_message_pump: 1,
            root_cache_path: CefString::from(&*cache.to_string_lossy()),
            resources_dir_path: resources_dir,
            locales_dir_path: locales_dir,
            user_agent: self
                .user_agent
                .as_deref()
                .map_or_else(CefString::default, CefString::from),
            log_severity: LogSeverity::default(),
            ..Default::default()
        };
        let ok = initialize(
            Some(self.args.as_main_args()),
            Some(&settings),
            Some(&mut self.app),
            std::ptr::null_mut(),
        );
        if ok == 1 {
            info!("CEF initialized (windowless)");
            Ok(())
        } else {
            Err(PipelineError::GpuInit("cef_initialize failed".into()))
        }
    }

    /// Create an offscreen browser rendering `url` at `width`×`height`, painting into
    /// `sink`. Returns the browser handle (kept alive to keep painting).
    ///
    /// # Errors
    /// [`PipelineError::GpuInit`] if the browser can't be created.
    pub fn create_offscreen(
        &self,
        url: &str,
        width: u32,
        height: u32,
        sink: CefFrameSink,
        health: std::sync::Arc<BrowserHealth>,
    ) -> Result<CefBrowser, PipelineError> {
        let size = ViewSize {
            dims: Arc::new(Mutex::new((width, height))),
        };
        let render_handler = RenderHandlerBuilder::new(CastawayRenderHandler {
            size: size.clone(),
            sink,
        });
        let resource_handler = ResourceRequestHandlerBuilder::new(ResourceInner {
            adblock: self.adblock.clone(),
        });
        let request_handler = RequestHandlerBuilder::new(RequestInner {
            resource_handler,
            health: std::sync::Arc::clone(&health),
        });
        let display_handler = DisplayHandlerBuilder::new();
        let load_handler = LoadHandlerBuilder::new(health);
        let mut client = ClientBuilder::new(ClientInner {
            render_handler,
            request_handler,
            display_handler,
            load_handler,
        });

        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            ..Default::default()
        };
        let browser_settings = BrowserSettings {
            // Match the kiosk's 60 Hz present cadence; the compositor's take()-latest
            // slot drops anything the redraw loop doesn't consume.
            windowless_frame_rate: 60,
            ..Default::default()
        };

        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&url.into()),
            Some(&browser_settings),
            None,
            None,
        );
        let browser = browser.ok_or(PipelineError::GpuInit("create browser failed".into()))?;
        info!(%url, width, height, "CEF offscreen browser created");
        Ok(CefBrowser { browser, size })
    }

    /// Pump one iteration of CEF's message loop. Call regularly on the main thread.
    pub fn pump(&self) {
        do_message_loop_work();
    }

    /// Adblock stats so far: `(requests_seen, requests_blocked)`.
    #[must_use]
    pub fn adblock_stats(&self) -> (u64, u64) {
        self.adblock
            .read()
            .map_or((0, 0), |b| (b.seen_count(), b.blocked_count()))
    }

    /// Diagnostic host tally: `(host, seen, blocked)` most-seen first.
    #[must_use]
    pub fn adblock_hosts(&self) -> Vec<(String, u32, u32)> {
        self.adblock
            .read()
            .map(|b| b.host_tally())
            .unwrap_or_default()
    }

    /// Shut CEF down.
    pub fn shutdown(self) {
        shutdown();
    }
}

/// The persistent CEF profile dir: `$XDG_CACHE_HOME/castaway/cef`, else
/// `$HOME/.cache/castaway/cef` (or `%LOCALAPPDATA%` on Windows), else the temp dir.
fn stable_cache_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    base.join("castaway").join("cef")
}

/// A live offscreen browser. Dropping it closes the browser.
pub struct CefBrowser {
    browser: Browser,
    size: ViewSize,
}

impl CefBrowser {
    /// Resize the offscreen viewport.
    pub fn resize(&self, width: u32, height: u32) {
        if let Ok(mut dims) = self.size.dims.lock() {
            *dims = (width, height);
        }
        if let Some(host) = self.browser.host() {
            host.was_resized();
        }
    }

    /// Navigate to a new URL.
    pub fn load_url(&self, url: &str) {
        if let Some(frame) = self.browser.main_frame() {
            frame.load_url(Some(&url.into()));
        }
    }

    /// Ask CEF to close this browser (force-close: no beforeunload prompt — this is a
    /// kiosk, not a document editor).
    pub fn close(&self) {
        if let Some(host) = self.browser.host() {
            host.close_browser(1);
        }
    }
}

/// A command sent from the tokio side (e.g. a DIAL launch) to the main-thread browser.
pub enum BrowserCommand {
    /// Show the browser fullscreen, navigating to `url` (the offscreen browser is
    /// created on first use).
    Navigate(String),
    /// Give the panel back: return to the idle widget if one is configured, else close
    /// the browser and drop its compositor layer (e.g. DIAL stop).
    Hide,
}

/// What the one offscreen browser is currently for. There is exactly one CEF browser, so
/// its two uses are mutually exclusive by construction: a cast takes the panel over, and
/// dismissing it hands the screen back to the idle widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRole {
    /// The idle web widget (the clock) painting into the attract scene's reserved card,
    /// *below* the video layer so a starting cast simply covers it.
    AttractWidget,
    /// A cast surface (YouTube leanback): fills the panel, above the video layer.
    Fullscreen,
}

/// Where a role's browser lives on a `surface`-sized panel: the offscreen viewport CEF
/// rasterizes into (device pixels — the page lays itself out at the size it will actually
/// be shown, instead of a small render upscaled) and the layer that viewport maps onto.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserView {
    /// Viewport rect in device pixels.
    pub rect: InsetRect,
    /// Layer placement: `rect` normalized onto the surface, one texel per device pixel.
    pub transform: Transform,
    /// Layer depth in the compositor stack.
    pub z: i32,
}

impl BrowserRole {
    /// Viewport + layer placement for this role on a `surface`-sized panel.
    #[must_use]
    pub fn view(self, surface: (u32, u32)) -> BrowserView {
        let (w, h) = (surface.0.max(1), surface.1.max(1));
        let full = InsetRect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        // The widget's rect comes from the attract renderer, which draws the card frame
        // from the same call — the two cannot drift. Falling back to fullscreen keeps a
        // slot that reserves nothing from producing a zero-sized viewport.
        let rect = match self {
            Self::AttractWidget => WidgetSlot::RightCard.rect(w, h).unwrap_or(full),
            Self::Fullscreen => full,
        };
        BrowserView {
            rect,
            transform: rect.transform(w, h),
            // Attract is -10 and video is 0, so the idle widget sits between them: no
            // explicit "hide the clock" step, a cast just covers it.
            z: match self {
                Self::AttractWidget => -5,
                Self::Fullscreen => 5,
            },
        }
    }
}

/// Map window-normalized coordinates into a browser view-space pixel position. Free
/// function (not a method) so the inset mapping is testable without a live CEF instance.
fn to_view_px(rect: InsetRect, surface: (u32, u32), x: f32, y: f32) -> (f32, f32) {
    // Clamped into the viewport rather than dropped when outside: CEF still needs the
    // move that leaves the card to end a hover or a drag.
    let vx = (x * surface.0.max(1) as f32 - rect.x as f32).clamp(0.0, rect.width as f32);
    let vy = (y * surface.1.max(1) as f32 - rect.y as f32).clamp(0.0, rect.height as f32);
    (vx, vy)
}

/// Owns the initialized [`Cef`] instance and the lazily-created offscreen browser on the
/// kiosk **main thread** (the thread that called [`Cef::initialize`]). The kiosk calls
/// [`Self::pump`] once per redraw: it applies queued [`BrowserCommand`]s, runs one CEF
/// message-loop iteration, and feeds any newly painted frame to the compositor's
/// `Browser` layer.
pub struct BrowserHost {
    cef: Cef,
    browser: Option<CefBrowser>,
    sink: CefFrameSink,
    commands: std::sync::mpsc::Receiver<BrowserCommand>,
    /// Current kiosk surface size; the browser viewport tracks it.
    size: (u32, u32),
    /// What the browser is currently showing — decides its viewport and its layer.
    role: BrowserRole,
    /// The idle widget's URL (the attract clock), if configured.
    widget: Option<String>,
    /// Whether the idle widget has been brought up yet. It can't be created in
    /// [`Self::new`] (CEF needs the real surface size, which arrives with the window), and
    /// a create failure is logged once rather than retried every frame at 60 Hz.
    widget_started: bool,
    /// Primary mouse button held (so moves carry the drag modifier).
    left_down: bool,
    /// What the handlers have reported about the page.
    health: std::sync::Arc<BrowserHealth>,
    /// The URL the browser is meant to be showing, so a crash can be recovered from.
    current_url: Option<String>,
    /// Consecutive recovery attempts without a successful load in between.
    recovery_attempts: u32,
    /// When the next recovery attempt is due, so a page that fails instantly does not
    /// spin the retry budget away in one frame at 60 Hz.
    retry_at: Option<std::time::Instant>,
    /// Called when recovery is abandoned, so the app can stop telling senders the app is
    /// running. `None` in tests and in builds that do not care.
    gave_up: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

/// How many times to rebuild a page before concluding the problem is not transient.
///
/// A renderer crash usually is transient — a sad tab on one page reloads fine. A page
/// that crashes its renderer three times running is a page we should stop putting on the
/// wall, because the fourth attempt is the same crash and the panel spends its life
/// flickering through it.
const RECOVERY_ATTEMPTS: u32 = 3;

/// Wait before a recovery attempt.
///
/// Not zero: a load that fails instantly (no DNS at boot, captive portal) would otherwise
/// burn the whole budget inside one second, before the network had any chance to come up.
const RECOVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(3);

impl BrowserHost {
    /// Wrap an initialized [`Cef`]. `commands` is the channel other threads use to
    /// navigate/hide the browser.
    #[must_use]
    pub fn new(cef: Cef, commands: std::sync::mpsc::Receiver<BrowserCommand>) -> Self {
        Self {
            cef,
            browser: None,
            sink: CefFrameSink::default(),
            commands,
            size: (1920, 1080),
            role: BrowserRole::Fullscreen,
            widget: None,
            widget_started: false,
            left_down: false,
            health: std::sync::Arc::new(BrowserHealth::default()),
            current_url: None,
            recovery_attempts: 0,
            retry_at: None,
            gave_up: None,
        }
    }

    /// Register what to do when the browser cannot be recovered.
    ///
    /// The app uses this to stop DIAL reporting `running` and to clear the published
    /// screen id. Without it a dead page keeps advertising itself as a live cast target,
    /// which is the half of the crash that senders can see.
    #[must_use]
    pub fn on_recovery_failed(mut self, f: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        self.gave_up = Some(f);
        self
    }

    /// Paint `url` into the attract scene's reserved card while nothing is casting — a
    /// live web widget (the clock) on the idle screen. The same browser is taken over
    /// fullscreen by a cast and handed back here when the cast is dismissed.
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
        if let Some(browser) = &self.browser {
            let rect = self.role.view(self.size).rect;
            browser.resize(rect.width, rect.height);
        }
    }

    /// One per-frame tick (main thread): apply queued commands, pump CEF, upload the
    /// regions painted since the last tick to `render`'s `Browser` layer.
    pub fn pump(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        // Deferred to the first pump, not done in `new`: CEF needs the real surface size
        // to size the widget's viewport, and that only exists once the window is up.
        if !self.widget_started {
            self.widget_started = true;
            if let Some(url) = self.widget.clone() {
                self.show(render, &url, BrowserRole::AttractWidget);
            }
        }
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                BrowserCommand::Navigate(url) => {
                    self.show(render, &url, BrowserRole::Fullscreen);
                }
                // Back to the idle widget rather than a hole in the attract card. The
                // browser is already warm, so it's a navigation, not a re-create.
                BrowserCommand::Hide => self.hide(render),
            }
        }
        self.recover(render);
        self.cef.pump();
        if self.browser.is_some() {
            let view = self.role.view(self.size);
            let uploaded = self.sink.consume(|width, height, bgra, dirty| {
                render.upload_browser(width, height, bgra, dirty, view.transform, view.z)
            });
            if let Some(Err(e)) = uploaded {
                tracing::warn!(error = %e, "browser frame upload failed");
            }
        }
    }

    /// Put the page back if it died, or give up loudly.
    ///
    /// The failure being recovered from is invisible without this: a dead render process
    /// leaves the last painted frame on screen at z=5 — a still of a working page — while
    /// DIAL goes on reporting `running`. Nobody in the room can tell that from a paused
    /// video, and nothing retried, so the panel stayed that way until someone restarted
    /// the receiver.
    fn recover(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        if let Some(fault) = self.health.take_fault() {
            self.recovery_attempts += 1;
            if self.recovery_attempts > RECOVERY_ATTEMPTS {
                tracing::error!(
                    target: "castaway::cef",
                    ?fault,
                    attempts = self.recovery_attempts - 1,
                    url = %self.current_url.clone().unwrap_or_default(),
                    "giving up on the page; returning the panel to the idle screen"
                );
                self.retry_at = None;
                self.recovery_attempts = 0;
                // Hand the screen back rather than leaving a corpse on it, and tell
                // whoever is advertising this page that it is not there any more.
                //
                // Unless the page we gave up on *is* the idle widget: falling back to it
                // would be navigating straight back to the thing that just crashed three
                // times, and the panel would spend its life cycling through it. A blank
                // screen is the honest outcome there.
                if self.current_url.as_deref() == self.widget.as_deref() {
                    tracing::error!(
                        target: "castaway::cef",
                        "the idle widget is what failed; leaving the browser layer empty"
                    );
                    if let Some(browser) = self.browser.take() {
                        browser.close();
                    }
                    self.current_url = None;
                    self.sink.clear();
                    render.clear_browser();
                } else {
                    self.hide(render);
                }
                if let Some(gave_up) = self.gave_up.clone() {
                    gave_up();
                }
                return;
            }
            tracing::warn!(
                target: "castaway::cef",
                ?fault,
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
        let Some(url) = self.current_url.clone() else {
            return;
        };
        let role = self.role;
        // Re-navigating is enough for a renderer crash: CEF keeps the browser object and
        // spawns a new render process for the load. If the browser object itself is gone,
        // `show` recreates it.
        tracing::info!(target: "castaway::cef", %url, "reloading after a fault");
        self.show(render, &url, role);
    }

    /// Give the panel back: the idle widget if there is one, otherwise nothing at all.
    fn hide(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        match self.widget.clone() {
            Some(url) => self.show(render, &url, BrowserRole::AttractWidget),
            None => {
                if let Some(browser) = self.browser.take() {
                    browser.close();
                }
                self.current_url = None;
                self.sink.clear();
                render.clear_browser();
            }
        }
    }

    /// Point the browser at `url` in `role`, creating it on first use.
    fn show(
        &mut self,
        render: &mut crate::render_pipeline::RenderLoop,
        url: &str,
        role: BrowserRole,
    ) {
        let rect = role.view(self.size).rect;
        if self.role != role || self.browser.is_none() {
            // The layer's transform changes with the role, but the texture it holds is
            // still the old viewport's size. Drop it until CEF paints at the new size,
            // rather than stretching one frame of the wrong thing across the new rect.
            self.sink.clear();
            render.clear_browser();
        }
        self.role = role;
        // A deliberate navigation is a fresh start: whatever was failing before is no
        // longer the page we are on, so it should not spend this page's retry budget.
        if self.current_url.as_deref() != Some(url) {
            self.recovery_attempts = 0;
            self.retry_at = None;
        }
        self.current_url = Some(url.to_string());
        match &self.browser {
            Some(browser) => {
                browser.resize(rect.width, rect.height);
                browser.load_url(url);
            }
            None => match self.cef.create_offscreen(
                url,
                rect.width,
                rect.height,
                self.sink.clone(),
                std::sync::Arc::clone(&self.health),
            ) {
                Ok(browser) => self.browser = Some(browser),
                Err(e) => tracing::warn!(error = %e, %url, "browser create failed"),
            },
        }
    }

    /// Map a normalized coordinate to browser view space.
    fn to_view(&self, x: f32, y: f32) -> (f32, f32) {
        to_view_px(self.role.view(self.size).rect, self.size, x, y)
    }

    fn cef_host(&self) -> Option<cef::BrowserHost> {
        self.browser.as_ref().and_then(|b| b.browser.host())
    }

    /// The `EVENTFLAG_LEFT_MOUSE_BUTTON` bit of `cef_event_flags_t` — set on move events
    /// while the primary button is held so Chromium recognizes drags.
    const LEFT_BUTTON_FLAG: u32 = 1 << 4;

    /// Close any open browser and shut CEF down. Call on the main thread after the
    /// kiosk event loop exits.
    pub fn shutdown(mut self) {
        if let Some(browser) = self.browser.take() {
            browser.close();
            // Give CEF a few pump iterations to tear the browser down first.
            for _ in 0..30 {
                self.cef.pump();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let (seen, blocked) = self.cef.adblock_stats();
        info!(seen, blocked, "adblock totals at shutdown");
        self.cef.shutdown();
    }
}

/// Routed panel/window input → CEF injection. The kiosk (input router) delivers
/// normalized events; this maps them to browser view space and hands them to the
/// offscreen browser, which does its own gesture recognition (touch scroll/fling/tap).
impl input_touch::InputSink for BrowserHost {
    fn touch(&mut self, event: input_touch::TouchEvent) {
        use input_touch::TouchPhase;
        let Some(host) = self.cef_host() else { return };
        let (x, y) = self.to_view(event.x, event.y);
        let type_ = match event.phase {
            TouchPhase::Down => TouchEventType::PRESSED,
            TouchPhase::Move => TouchEventType::MOVED,
            TouchPhase::Up => TouchEventType::RELEASED,
            TouchPhase::Cancel => TouchEventType::CANCELLED,
        };
        host.send_touch_event(Some(&TouchEvent {
            id: event.id as i32,
            x,
            y,
            radius_x: 0.0,
            radius_y: 0.0,
            rotation_angle: 0.0,
            pressure: if event.phase == TouchPhase::Up {
                0.0
            } else {
                1.0
            },
            type_,
            modifiers: 0,
            pointer_type: PointerType::TOUCH,
        }));
    }

    fn pointer(&mut self, event: input_touch::PointerEvent) {
        use input_touch::{PointerButton, PointerEvent};
        let Some(host) = self.cef_host() else { return };
        match event {
            PointerEvent::Move { x, y } => {
                let (vx, vy) = self.to_view(x, y);
                let modifiers = if self.left_down {
                    Self::LEFT_BUTTON_FLAG
                } else {
                    0
                };
                host.send_mouse_move_event(
                    Some(&MouseEvent {
                        x: vx as i32,
                        y: vy as i32,
                        modifiers,
                    }),
                    0,
                );
            }
            PointerEvent::Button { x, y, button, down } => {
                let (vx, vy) = self.to_view(x, y);
                let type_ = match button {
                    PointerButton::Left => {
                        self.left_down = down;
                        MouseButtonType::LEFT
                    }
                    PointerButton::Middle => MouseButtonType::MIDDLE,
                    PointerButton::Right => MouseButtonType::RIGHT,
                };
                host.send_mouse_click_event(
                    Some(&MouseEvent {
                        x: vx as i32,
                        y: vy as i32,
                        modifiers: 0,
                    }),
                    type_,
                    i32::from(!down),
                    1,
                );
            }
            PointerEvent::Wheel { x, y, dx, dy } => {
                let (vx, vy) = self.to_view(x, y);
                host.send_mouse_wheel_event(
                    Some(&MouseEvent {
                        x: vx as i32,
                        y: vy as i32,
                        modifiers: 0,
                    }),
                    dx as i32,
                    dy as i32,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn frame(width: u32, height: u32, val: u8) -> Vec<u8> {
        vec![val; (width * height * 4) as usize]
    }

    #[test]
    fn first_paint_takes_full_frame() {
        let mut acc = FrameAccum::default();
        acc.paint(&frame(4, 4, 1), 4, 4, &[]);
        assert_eq!(acc.bgra, frame(4, 4, 1));
        assert_eq!(acc.dirty, vec![DirtyRect::full(4, 4)]);
    }

    #[test]
    fn partial_paint_copies_only_dirty_rows() {
        let mut acc = FrameAccum::default();
        acc.paint(&frame(4, 4, 1), 4, 4, &[]);
        acc.dirty.clear();
        // New source is all-2s, but only a 2×2 rect is declared dirty.
        let rect = DirtyRect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        acc.paint(&frame(4, 4, 2), 4, 4, &[rect]);
        assert_eq!(acc.bgra[0], 1, "outside the rect keeps the old pixels");
        let inside = ((4 + 1) * 4) as usize; // (x=1, y=1)
        assert_eq!(acc.bgra[inside], 2, "inside the rect took the new pixels");
        assert_eq!(acc.dirty, vec![rect]);
    }

    #[test]
    fn majority_dirty_area_collapses_to_full() {
        let mut acc = FrameAccum::default();
        acc.paint(&frame(4, 4, 1), 4, 4, &[]);
        acc.dirty.clear();
        acc.paint(
            &frame(4, 4, 2),
            4,
            4,
            &[DirtyRect {
                x: 0,
                y: 0,
                width: 4,
                height: 3,
            }],
        );
        assert_eq!(acc.dirty, vec![DirtyRect::full(4, 4)]);
    }

    #[test]
    fn resize_resets_to_full_frame() {
        let mut acc = FrameAccum::default();
        acc.paint(&frame(4, 4, 1), 4, 4, &[]);
        acc.paint(&frame(2, 2, 3), 2, 2, &[]);
        assert_eq!(acc.width, 2);
        assert_eq!(acc.bgra, frame(2, 2, 3));
        assert_eq!(acc.dirty, vec![DirtyRect::full(2, 2)]);
    }

    const UHD: (u32, u32) = (3840, 2160);

    /// The idle widget must land inside the attract scene's reserved card and *below* the
    /// video layer — that's what makes a starting cast cover it with no extra bookkeeping.
    #[test]
    fn attract_widget_view_matches_the_reserved_card_and_sits_under_video() {
        let view = BrowserRole::AttractWidget.view(UHD);
        assert_eq!(view.rect, WidgetSlot::RightCard.rect(UHD.0, UHD.1).unwrap());
        assert!(view.rect.width < UHD.0 && view.rect.height < UHD.1);
        assert!(view.z < 0, "the widget belongs under the video layer");
        // One texel per device pixel, so the page isn't resampled.
        assert!((view.transform.scale_x * UHD.0 as f32 - view.rect.width as f32).abs() < 0.01);
    }

    #[test]
    fn fullscreen_view_covers_the_panel_above_video() {
        let view = BrowserRole::Fullscreen.view(UHD);
        assert_eq!(view.rect.width, UHD.0);
        assert_eq!(view.rect.height, UHD.1);
        assert_eq!(view.transform, Transform::default());
        assert!(view.z > 0, "a cast surface covers the video layer");
    }

    /// Input arrives normalized to the *window*; the inset widget's view space is only a
    /// corner of it, so a tap on the card must land at the same spot inside the page.
    #[test]
    fn input_maps_into_the_inset_view_space() {
        let rect = BrowserRole::AttractWidget.view(UHD).rect;
        let center = (
            (rect.x + rect.width / 2) as f32 / UHD.0 as f32,
            (rect.y + rect.height / 2) as f32 / UHD.1 as f32,
        );
        let (vx, vy) = to_view_px(rect, UHD, center.0, center.1);
        assert!((vx - rect.width as f32 / 2.0).abs() <= 1.0, "vx {vx}");
        assert!((vy - rect.height as f32 / 2.0).abs() <= 1.0, "vy {vy}");

        // Outside the card, coordinates clamp to its edges instead of going negative or
        // past the viewport — CEF would otherwise see an out-of-bounds pointer.
        let (lx, ly) = to_view_px(rect, UHD, 0.0, 1.0);
        assert_eq!((lx, ly), (0.0, rect.height as f32));

        // Fullscreen is the identity mapping it always was.
        let full = BrowserRole::Fullscreen.view(UHD).rect;
        assert_eq!(to_view_px(full, UHD, 0.5, 0.25), (1920.0, 540.0));
    }

    #[test]
    fn consume_drains_dirty_and_skips_when_clean() {
        let sink = CefFrameSink::default();
        assert!(sink.consume(|_, _, _, _| ()).is_none(), "empty sink");
        sink.paint(&frame(2, 2, 1), 2, 2, &[]);
        let seen = sink.consume(|w, h, bgra, dirty| (w, h, bgra.len(), dirty.to_vec()));
        assert_eq!(seen, Some((2, 2, 16, vec![DirtyRect::full(2, 2)])));
        assert!(
            sink.consume(|_, _, _, _| ()).is_none(),
            "nothing new since last consume"
        );
        // latest() still serves the accumulated frame after a consume.
        assert_eq!(sink.latest().map(|f| f.bgra), Some(frame(2, 2, 1)));
    }

    #[test]
    fn a_crash_outranks_a_load_failure_and_each_is_reported_once() {
        // The two faults arrive from different CEF callbacks and can overlap: a renderer
        // that dies mid-load reports both. A dead renderer is the more serious of the two
        // and subsumes the other — reporting the load failure as well would spend a
        // recovery attempt on a page that has no process to load into.
        let health = BrowserHealth::default();
        assert_eq!(
            health.take_fault(),
            None,
            "a healthy browser complains about nothing"
        );

        health.note_load_failure();
        health.note_crash();
        assert_eq!(health.take_fault(), Some(BrowserFault::RendererGone));
        assert_eq!(
            health.take_fault(),
            None,
            "the load failure must not be reported a second time behind the crash"
        );

        health.note_load_failure();
        assert_eq!(health.take_fault(), Some(BrowserFault::LoadFailed));
        assert_eq!(health.take_fault(), None, "faults are taken, not left set");
    }

    #[test]
    fn a_successful_load_clears_whatever_went_wrong_before() {
        // Otherwise a page that failed once and then loaded would still burn a recovery
        // attempt, and three transient failures over a week would retire a working page.
        let health = BrowserHealth::default();
        health.note_crash();
        health.note_healthy();
        assert_eq!(health.take_fault(), None);
    }
}
