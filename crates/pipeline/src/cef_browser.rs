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

// --- Client: hands CEF our render + request handlers ---

#[derive(Clone)]
struct ClientInner {
    render_handler: RenderHandler,
    request_handler: RequestHandler,
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
        let cache = std::env::temp_dir().join("castaway-cef-cache");
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
        let mut client = ClientBuilder::new(ClientInner {
            render_handler,
            request_handler,
        });

        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            ..Default::default()
        };
        let browser_settings = BrowserSettings {
            windowless_frame_rate: 30,
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
}
