//! The kiosk output: a `winit` borderless-fullscreen window whose surface the
//! [`WgpuCompositor`] renders into. This owns the winit event loop and therefore must
//! run on the **main thread** (architecture §6) — the tokio runtime and decode threads
//! live elsewhere and feed frames in over the [`RenderLoop`]'s channel.
//!
//! Presenting is driven by continuous redraw; each redraw drains queued frames and
//! composites. Late frames were already dropped at the bounded channel, so the window
//! always shows the freshest available frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use input_touch::{InputSink, PointerButton, PointerEvent, TouchEvent, TouchPhase};
use tracing::{error, info};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

use crate::error::PipelineError;
use crate::osd::OsdController;
use crate::render_pipeline::{RenderCommand, RenderLoop};
use crate::wgpu_compositor::WgpuCompositor;

/// An idle-scene image to show before/between casts: `(width, height, rgba8)`.
pub type AttractImage = (u32, u32, Vec<u8>);

struct KioskApp {
    rx: Option<Receiver<RenderCommand>>,
    attract: Option<AttractImage>,
    osd: Option<OsdController>,
    window: Option<Arc<Window>>,
    render: Option<RenderLoop>,
    /// External shutdown request (ctrl-c / service failure): checked every loop
    /// iteration, since a borderless-fullscreen kiosk has no chrome to close.
    exit: Option<Arc<AtomicBool>>,
    /// Last cursor position in window pixels (winit reports buttons without one).
    cursor: (f64, f64),
    /// Current window size, for normalizing input coordinates.
    size: (u32, u32),
    /// The main-thread CEF host (this loop is CEF's message pump — architecture §6).
    #[cfg(feature = "cef")]
    browser: Option<crate::cef_browser::BrowserHost>,
}

impl KioskApp {
    /// The surface that currently receives input: the CEF browser layer when present.
    /// Future interactive layers (video controls, adapter UIs) slot in here.
    fn input_sink(&mut self) -> Option<&mut dyn InputSink> {
        #[cfg(feature = "cef")]
        {
            self.browser.as_mut().map(|b| b as &mut dyn InputSink)
        }
        #[cfg(not(feature = "cef"))]
        {
            None
        }
    }

    fn route_input(&mut self, event: &WindowEvent) {
        let size = self.size;
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                let (x, y) = normalize(position.x, position.y, size);
                if let Some(sink) = self.input_sink() {
                    sink.pointer(PointerEvent::Move { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = pointer_button(*button) else {
                    return;
                };
                let down = state.is_pressed();
                let (x, y) = normalize(self.cursor.0, self.cursor.1, size);
                if let Some(sink) = self.input_sink() {
                    sink.pointer(PointerEvent::Button { x, y, button, down });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = wheel_pixels(*delta);
                let (x, y) = normalize(self.cursor.0, self.cursor.1, size);
                if let Some(sink) = self.input_sink() {
                    sink.pointer(PointerEvent::Wheel { x, y, dx, dy });
                }
            }
            WindowEvent::Touch(touch) => {
                let event = translate_touch(touch, size);
                if let Some(sink) = self.input_sink() {
                    sink.touch(event);
                }
            }
            _ => {}
        }
    }
}

/// Normalize window-pixel coordinates to `0.0..=1.0`.
fn normalize(x: f64, y: f64, (w, h): (u32, u32)) -> (f32, f32) {
    #[allow(clippy::cast_possible_truncation)]
    (
        ((x / f64::from(w.max(1))).clamp(0.0, 1.0)) as f32,
        ((y / f64::from(h.max(1))).clamp(0.0, 1.0)) as f32,
    )
}

/// A wheel delta in pixels; line deltas use the conventional 40px-per-line.
fn wheel_pixels(delta: winit::event::MouseScrollDelta) -> (f32, f32) {
    use winit::event::MouseScrollDelta;
    match delta {
        MouseScrollDelta::LineDelta(dx, dy) => (dx * 40.0, dy * 40.0),
        #[allow(clippy::cast_possible_truncation)]
        MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
    }
}

fn pointer_button(button: winit::event::MouseButton) -> Option<PointerButton> {
    use winit::event::MouseButton;
    match button {
        MouseButton::Left => Some(PointerButton::Left),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::Right => Some(PointerButton::Right),
        _ => None,
    }
}

/// Map a winit multi-touch contact to a normalized [`TouchEvent`]. winit ids are u64
/// but stay small in practice; wrap into the sink's u32 space keeping distinctness.
fn translate_touch(touch: &winit::event::Touch, size: (u32, u32)) -> TouchEvent {
    let phase = match touch.phase {
        winit::event::TouchPhase::Started => TouchPhase::Down,
        winit::event::TouchPhase::Moved => TouchPhase::Move,
        winit::event::TouchPhase::Ended => TouchPhase::Up,
        winit::event::TouchPhase::Cancelled => TouchPhase::Cancel,
    };
    let (x, y) = normalize(touch.location.x, touch.location.y, size);
    #[allow(clippy::cast_possible_truncation)]
    TouchEvent::new(touch.id as u32, phase, x, y)
}

impl ApplicationHandler for KioskApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("castaway")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!(error = %e, "failed to create kiosk window");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();

        let instance = crate::wgpu_compositor::create_instance();
        let surface = match instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to create wgpu surface");
                event_loop.exit();
                return;
            }
        };
        let compositor =
            match WgpuCompositor::new_for_surface(instance, surface, size.width, size.height) {
                Ok(c) => c,
                Err(e) => {
                    error!(error = %e, "failed to init compositor");
                    event_loop.exit();
                    return;
                }
            };

        if let Some(rx) = self.rx.take() {
            let mut render = RenderLoop::new(compositor, rx);
            if let Some((w, h, rgba)) = self.attract.take() {
                if let Err(e) = render.set_attract(w, h, &rgba) {
                    error!(error = %e, "failed to install attract scene");
                }
            }
            if let Some(osd) = self.osd.take() {
                render = render.with_osd(osd);
            }
            self.render = Some(render);
        }
        #[cfg(feature = "cef")]
        if let Some(host) = &mut self.browser {
            host.resize(size.width, size.height);
        }
        self.size = (size.width, size.height);
        info!(width = size.width, height = size.height, "kiosk window up");
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.route_input(&event);
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.size = (size.width, size.height);
                if let Some(r) = &mut self.render {
                    r.resize(size.width, size.height);
                }
                #[cfg(feature = "cef")]
                if let Some(host) = &mut self.browser {
                    host.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                // CEF first: its message-loop iteration may paint a fresh frame, which
                // then lands in this same redraw's present.
                #[cfg(feature = "cef")]
                if let (Some(host), Some(r)) = (&mut self.browser, &mut self.render) {
                    host.pump(r);
                }
                if let Some(r) = &mut self.render {
                    r.pump();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .exit
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            info!("kiosk: exit requested, closing");
            event_loop.exit();
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Run the kiosk to completion (blocks the calling — main — thread). Consumes render
/// commands from `rx` and displays them fullscreen until the window is closed or `exit`
/// is set (ctrl-c). `attract` is the idle scene shown before/between casts.
///
/// # Errors
/// [`PipelineError`] if the event loop can't be created or run.
pub fn run(
    rx: Receiver<RenderCommand>,
    attract: Option<AttractImage>,
    osd: Option<OsdController>,
    exit: Option<Arc<AtomicBool>>,
) -> Result<(), PipelineError> {
    let mut app = KioskApp {
        rx: Some(rx),
        attract,
        osd,
        window: None,
        render: None,
        exit,
        cursor: (0.0, 0.0),
        size: (1, 1),
        #[cfg(feature = "cef")]
        browser: None,
    };
    run_app(&mut app)
}

/// [`run`], plus a main-thread CEF [`BrowserHost`](crate::cef_browser::BrowserHost)
/// pumped every frame (the kiosk loop is CEF's external message pump). Shuts CEF down
/// after the event loop exits.
///
/// # Errors
/// [`PipelineError`] if the event loop can't be created or run.
#[cfg(feature = "cef")]
pub fn run_with_browser(
    rx: Receiver<RenderCommand>,
    attract: Option<AttractImage>,
    osd: Option<OsdController>,
    exit: Option<Arc<AtomicBool>>,
    browser: crate::cef_browser::BrowserHost,
) -> Result<(), PipelineError> {
    let mut app = KioskApp {
        rx: Some(rx),
        attract,
        osd,
        window: None,
        render: None,
        exit,
        cursor: (0.0, 0.0),
        size: (1, 1),
        browser: Some(browser),
    };
    let result = run_app(&mut app);
    // CEF must be shut down on this (main) thread, after the loop stops pumping it.
    if let Some(host) = app.browser.take() {
        host.shutdown();
    }
    result
}

fn run_app(app: &mut KioskApp) -> Result<(), PipelineError> {
    let event_loop =
        EventLoop::new().map_err(|e| PipelineError::GpuInit(format!("event loop: {e}")))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let result = event_loop
        .run_app(app)
        .map_err(|e| PipelineError::Surface(format!("event loop: {e}")));
    // Release the GPU stack (wgpu surface/instance → EGL displays) and the window while
    // `event_loop` — and with it the Wayland connection — is still alive. `app` outlives
    // this function, and tearing EGL down after the connection closes segfaults in
    // Mesa's Wayland teardown (wl_proxy on a dead wl_display).
    app.render = None;
    app.window = None;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_clamps_and_scales() {
        assert_eq!(normalize(640.0, 360.0, (1280, 720)), (0.5, 0.5));
        assert_eq!(normalize(-10.0, 9999.0, (1280, 720)), (0.0, 1.0));
        // Degenerate size must not divide by zero.
        let (x, y) = normalize(5.0, 5.0, (0, 0));
        assert!(x.is_finite() && y.is_finite());
    }

    #[test]
    fn wheel_lines_become_pixels() {
        use winit::event::MouseScrollDelta;
        assert_eq!(
            wheel_pixels(MouseScrollDelta::LineDelta(0.0, -2.0)),
            (0.0, -80.0)
        );
    }

    #[test]
    fn extra_mouse_buttons_are_dropped() {
        use winit::event::MouseButton;
        assert_eq!(pointer_button(MouseButton::Left), Some(PointerButton::Left));
        assert_eq!(pointer_button(MouseButton::Back), None);
    }
}
