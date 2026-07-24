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

use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{args::Args, *};
use tracing::{debug, info};

use crate::cef_adblock::AdBlocker;
use crate::error::PipelineError;

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

/// A shared slot holding the most recent painted frame. Cloneable; the render handler
/// writes, the compositor/consumer reads.
#[derive(Clone, Default)]
pub struct CefFrameSink {
    inner: Arc<Mutex<Option<CefFrame>>>,
}

impl CefFrameSink {
    /// Clone out the latest frame, if any.
    #[must_use]
    pub fn latest(&self) -> Option<CefFrame> {
        self.inner.lock().ok().and_then(|g| g.clone())
    }

    /// Take the latest frame, leaving the slot empty — so a per-frame consumer uploads
    /// each paint exactly once instead of re-uploading a stale frame every tick.
    #[must_use]
    pub fn take(&self) -> Option<CefFrame> {
        self.inner.lock().ok().and_then(|mut g| g.take())
    }

    fn put(&self, frame: CefFrame) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(frame);
        }
    }
}

/// The offscreen viewport size, shared with the render handler's `view_rect`.
#[derive(Clone)]
struct ViewSize {
    dims: Arc<Mutex<(u32, u32)>>,
}

// --- App: process-wide callbacks (command line tweaks) ---

#[derive(Clone)]
struct CastawayApp;

wrap_app! {
    struct AppBuilder {
        app: CastawayApp,
    }
    impl App {
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
            }
        }
    }
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
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let len = (width * height * 4) as usize;
            // SAFETY: CEF guarantees `buffer` points to `width*height*4` BGRA bytes for
            // the duration of this callback; we copy out immediately.
            let bgra = unsafe { std::slice::from_raw_parts(buffer, len) }.to_vec();
            self.handler.sink.put(CefFrame {
                width: width as u32,
                height: height as u32,
                bgra,
            });
        }
    }
}

// --- ResourceRequestHandler: ad/tracker blocking on every resource load ---

#[derive(Clone)]
struct ResourceInner {
    adblock: Arc<AdBlocker>,
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
            if self.inner.adblock.should_block(&url, &source, kind) {
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
}

wrap_request_handler! {
    struct RequestHandlerBuilder {
        inner: RequestInner,
    }
    impl RequestHandler {
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

// --- Client: hands CEF our render + request + display handlers ---

#[derive(Clone)]
struct ClientInner {
    render_handler: RenderHandler,
    request_handler: RequestHandler,
    display_handler: DisplayHandler,
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
    }
}

/// A running CEF instance. Owns the process args + app and drives the message loop.
pub struct Cef {
    args: Args,
    app: App,
    adblock: Arc<AdBlocker>,
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
        let adblock = Arc::new(AdBlocker::with_defaults());
        Ok(Some(Self {
            args,
            app,
            adblock,
            user_agent: None,
        }))
    }

    /// Replace the ad blocker (e.g. loaded from a full EasyList) before creating browsers.
    pub fn set_adblock(&mut self, adblock: AdBlocker) {
        self.adblock = Arc::new(adblock);
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
        let request_handler = RequestHandlerBuilder::new(RequestInner { resource_handler });
        let display_handler = DisplayHandlerBuilder::new();
        let mut client = ClientBuilder::new(ClientInner {
            render_handler,
            request_handler,
            display_handler,
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
        (self.adblock.seen_count(), self.adblock.blocked_count())
    }

    /// Diagnostic host tally: `(host, seen, blocked)` most-seen first.
    #[must_use]
    pub fn adblock_hosts(&self) -> Vec<(String, u32, u32)> {
        self.adblock.host_tally()
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
    /// Close the browser and drop its compositor layer (e.g. DIAL stop).
    Hide,
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
    /// Primary mouse button held (so moves carry the drag modifier).
    left_down: bool,
}

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
            left_down: false,
        }
    }

    /// Track the kiosk surface size so the browser viewport matches.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size = (width, height);
        if let Some(browser) = &self.browser {
            browser.resize(width, height);
        }
    }

    /// One per-frame tick (main thread): apply queued commands, pump CEF, upload the
    /// latest painted frame to `render`'s `Browser` layer.
    pub fn pump(&mut self, render: &mut crate::render_pipeline::RenderLoop) {
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                BrowserCommand::Navigate(url) => self.navigate(&url),
                BrowserCommand::Hide => {
                    if let Some(browser) = self.browser.take() {
                        browser.close();
                    }
                    let _ = self.sink.take();
                    render.clear_browser();
                }
            }
        }
        self.cef.pump();
        if self.browser.is_some() {
            if let Some(frame) = self.sink.take() {
                if let Err(e) = render.upload_browser(frame.width, frame.height, &frame.bgra) {
                    tracing::warn!(error = %e, "browser frame upload failed");
                }
            }
        }
    }

    fn navigate(&mut self, url: &str) {
        match &self.browser {
            Some(browser) => browser.load_url(url),
            None => {
                let (w, h) = self.size;
                match self.cef.create_offscreen(url, w, h, self.sink.clone()) {
                    Ok(browser) => self.browser = Some(browser),
                    Err(e) => tracing::warn!(error = %e, %url, "browser create failed"),
                }
            }
        }
    }

    /// Map a normalized coordinate to browser view space.
    fn to_view(&self, x: f32, y: f32) -> (f32, f32) {
        let (w, h) = self.size;
        (x * w as f32, y * h as f32)
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
