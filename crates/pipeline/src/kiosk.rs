//! The kiosk output: a `winit` borderless-fullscreen window whose surface the
//! [`WgpuCompositor`] renders into. This owns the winit event loop and therefore must
//! run on the **main thread** (architecture §6) — the tokio runtime and decode threads
//! live elsewhere and feed frames in over the [`RenderLoop`]'s channel.
//!
//! Presenting is driven by continuous redraw; each redraw drains queued frames and
//! composites. Late frames were already dropped at the bounded channel, so the window
//! always shows the freshest available frame.

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use tracing::{error, info};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

use crate::error::PipelineError;
use crate::render_pipeline::{RenderCommand, RenderLoop};
use crate::wgpu_compositor::WgpuCompositor;

/// An idle-scene image to show before/between casts: `(width, height, rgba8)`.
pub type AttractImage = (u32, u32, Vec<u8>);

struct KioskApp {
    rx: Option<Receiver<RenderCommand>>,
    attract: Option<AttractImage>,
    window: Option<Arc<Window>>,
    render: Option<RenderLoop>,
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

        let instance = wgpu::Instance::default();
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
            self.render = Some(render);
        }
        info!(width = size.width, height = size.height, "kiosk window up");
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.render {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
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

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

/// Run the kiosk to completion (blocks the calling — main — thread). Consumes render
/// commands from `rx` and displays them fullscreen until the window is closed. `attract`
/// is the idle scene shown before/between casts.
///
/// # Errors
/// [`PipelineError`] if the event loop can't be created or run.
pub fn run(
    rx: Receiver<RenderCommand>,
    attract: Option<AttractImage>,
) -> Result<(), PipelineError> {
    let event_loop =
        EventLoop::new().map_err(|e| PipelineError::GpuInit(format!("event loop: {e}")))?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = KioskApp {
        rx: Some(rx),
        attract,
        window: None,
        render: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| PipelineError::Surface(format!("event loop: {e}")))?;
    Ok(())
}
