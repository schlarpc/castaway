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
use tracing::{error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

use crate::error::PipelineError;
use crate::osd::OsdController;
use crate::render_pipeline::{RenderCommand, RenderLoop};
use crate::wgpu_compositor::WgpuCompositor;

/// An idle-scene image to show before/between casts: `(width, height, rgba8)`.
pub type AttractImage = crate::attract::AttractScene;

/// Where a press on the transport strip goes.
///
/// A callback rather than a handle, because the two live on different threads and in
/// different crates: this loop owns the main thread (architecture §6) and the session's
/// `RemoteControl` is an async handle on the tokio runtime. The app closes over both.
pub type ControlSink = Arc<dyn Fn(castaway_core::ControlTxn) + Send + Sync>;

/// Where a shell press the panel cannot answer itself goes (D38).
///
/// Most presses are answered locally — a service tile opens that service's screen, back
/// goes back — with no round trip, because a panel that waits on a network round trip to
/// redraw feels broken. This is for the rest: a tile that means "go and do something",
/// and a picker row, which only `app` can interpret.
pub type ShellSink = Arc<dyn Fn(crate::shell::ShellEvent) + Send + Sync>;

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
    /// Where a press on the transport strip goes. `None` in a build with no session to
    /// drive — the strip is then never drawn either, since nothing publishes capabilities.
    controls: Option<ControlSink>,
    shell_sink: Option<ShellSink>,
    /// When the last frame was, so animations advance on wall time rather than on
    /// however fast this box happens to redraw.
    last_frame: Option<std::time::Instant>,
    /// Whether the current edge contact is dragging a navigation along with it.
    started_drag: bool,
    /// The last position and time of that drag, for the velocity a flick carries.
    drag_sample: Option<(std::time::Instant, f32)>,
    /// Where each contact was last seen, for turning a drag into a scroll.
    drag_last: std::collections::HashMap<u32, f32>,
    /// Live touch contacts, for the edge swipe. Nothing else in the tree tracked these:
    /// ids were carried faithfully to the browser and then forgotten (D38).
    contacts: std::collections::HashMap<u32, crate::overlay::Contact>,
    /// When the home pill was last raised, or `None` when it is not showing.
    pill_since: Option<std::time::Instant>,
    /// Whether the pill's texture is currently a layer, so the fade only uploads once.
    pill_drawn: bool,
    /// The main-thread browser host (this loop is its message pump — architecture §6).
    #[cfg(feature = "electron")]
    browser: Option<crate::electron_browser::ElectronHost>,
}

impl KioskApp {
    /// The surface that currently receives input: the browser layer when present.
    /// Future interactive layers (video controls, adapter UIs) slot in here.
    fn input_sink(&mut self) -> Option<&mut dyn InputSink> {
        #[cfg(feature = "electron")]
        {
            self.browser.as_mut().map(|b| b as &mut dyn InputSink)
        }
        #[cfg(not(feature = "electron"))]
        {
            None
        }
    }

    /// Raise the home pill, or keep it up. Any touch does this: it is the affordance for
    /// someone who does not know the gesture, so it has to appear for someone who does
    /// not know to ask for it.
    fn wake_pill(&mut self) {
        self.pill_since = Some(std::time::Instant::now());
    }

    /// Go back to Home, from the pill or the gesture.
    ///
    /// Cancels whatever the browser thinks is down first. The panel is being taken away
    /// from it mid-touch by definition — the gesture *is* a touch — and a contact that
    /// never ends leaves the page believing a finger is down forever.
    fn go_home(&mut self) {
        if let Some(sink) = self.input_sink() {
            sink.cancel_all();
        }
        if let Some(render) = self.render.as_mut() {
            // One call, and everything that is up follows it: a fullscreen page minimizes
            // into the widget slot, video demotes to its corner, a card to the slot. There
            // used to be a step here that reached into the browser to minimize it by hand,
            // because the page was the one surface the shell's own focus did not move.
            render.shell_home();
            render.set_shell_foreground(true);
            info!("shell: home");
        }
    }

    /// One step out, from wherever the panel is.
    ///
    /// The keyboard twin of the back gesture. The ordering — leave a fullscreen session
    /// before the screen underneath it — is [`crate::panel::Panel::back`]'s, and this
    /// matches on what it spent itself on rather than deciding it: three branches over two
    /// objects, in whatever order they had been written, was how the page and the shell came
    /// to disagree about who had the glass.
    fn back_one_level(&mut self) {
        if let Some(sink) = self.input_sink() {
            sink.cancel_all();
        }
        let Some(render) = self.render.as_mut() else {
            return;
        };
        match render.panel_back() {
            crate::panel::Left::Demoted => info!("shell: escape demoted the cast surface"),
            crate::panel::Left::Screen => info!("shell: escape went back"),
            crate::panel::Left::Nothing => {}
        }
    }

    /// Restore whatever is demoted under a press, if anything. Returns whether the press
    /// was spent doing so.
    ///
    /// A demoted surface is an app in the home screen's furniture, and tapping one means
    /// "bring it back" — never "forward my tap into it at 42% scale". Which of them is
    /// under the finger, and in what order they are offered, is the panel's answer: the
    /// card is checked before the page because it is the one drawn when both exist.
    fn restore_minimized(&mut self, x: f32, y: f32) -> bool {
        let Some(render) = self.render.as_mut() else {
            return false;
        };
        match render.panel_hit(x, y) {
            crate::panel::PanelHit::Restore(surface) => {
                render.panel_restore();
                info!(?surface, "shell: restoring a demoted surface");
                true
            }
            crate::panel::PanelHit::Shell(_) | crate::panel::PanelHit::Miss => false,
        }
    }

    /// Update the pill layer for this frame. Cheap: while it is up and unchanging this
    /// writes one 32-byte uniform, and it only rasterizes when it first appears.
    fn tick_pill(&mut self) {
        let Some(since) = self.pill_since else {
            return;
        };
        let opacity = crate::overlay::pill_opacity(since.elapsed());
        let Some(render) = self.render.as_mut() else {
            return;
        };
        if opacity <= 0.0 {
            render.clear_home_pill();
            self.pill_since = None;
            self.pill_drawn = false;
            return;
        }
        if !self.pill_drawn {
            if let Err(e) = render.draw_home_pill() {
                warn!(error = %e, "could not draw the home pill");
                self.pill_since = None;
                return;
            }
            self.pill_drawn = true;
        }
        render.set_home_pill_opacity(opacity);
    }

    /// The navigation layer: the home pill, and the reserved left edge the swipe starts
    /// from. Returns whether it consumed the event.
    ///
    /// Runs before everything, including a fullscreen browser, because it is the only way
    /// out of one. The cost is a sliver of the left edge that pages underneath never see
    /// — deliberate, and the reason the strip is thin.
    fn route_navigation(&mut self, event: &input_touch::TouchEvent) -> bool {
        use crate::overlay::Contact;
        let (w, h) = self.size;

        match event.phase {
            TouchPhase::Down => {
                // A tap on a demoted surface puts it back — the one thing on screen that
                // means "give this the panel again". Answered here, above everything,
                // because a demoted corner sits over whatever the shell is showing.
                if self.restore_minimized(event.x, event.y) {
                    return true;
                }
                let contact = Contact::new(event.x, event.y);
                let on_pill = crate::overlay::hit_pill(w.max(1), h.max(1), event.x, event.y)
                    && self
                        .render
                        .as_ref()
                        .is_some_and(RenderLoop::home_pill_present);
                if on_pill {
                    self.go_home();
                    return true;
                }
                let reserved = contact.from_edge;
                self.contacts.insert(event.id, contact);
                // A contact starting in the reserved edge is never forwarded, so the
                // shell never has to steal one mid-drag — which is the case that leaves a
                // page holding a finger forever.
                reserved
            }
            TouchPhase::Move => {
                let Some(contact) = self.contacts.get_mut(&event.id) else {
                    return false;
                };
                let from_edge = contact.from_edge;
                let complete = contact.is_home_swipe(event.x, event.y);
                let travel = event.x - contact.start.0;
                if complete && !self.started_drag {
                    // Only when nothing is being dragged: with a card in hand the swipe
                    // *is* the navigation, and finishing it is the release's job.
                    contact.fired = true;
                    self.go_home();
                    return true;
                }
                // Part-way: the panel follows the finger, so letting go without
                // finishing puts it back. A gesture that only fires at a threshold feels
                // like a switch; one that moves with the hand feels attached to it.
                if from_edge && !self.started_drag {
                    if let Some(render) = self.render.as_mut() {
                        // Somewhere to go: a screen to leave, or something playing to hand
                        // the glass back to. Asking "is the shell in front" instead would
                        // say yes at Home with nothing on, and animate a drag that uncovers
                        // nothing.
                        if render.shell_depth() > 1 || render.can_hand_back() {
                            self.started_drag = true;
                        }
                    }
                }
                if from_edge && self.started_drag {
                    // The card follows the hand: how far across the panel the finger has
                    // come *is* how far through the navigation it is. Not the gesture
                    // threshold — that decides whether a flick counts, not where the card
                    // sits.
                    let shown = 1.0 - travel.clamp(0.0, 1.0);
                    let now = std::time::Instant::now();
                    let velocity =
                        self.drag_sample
                            .map_or(0.0, |(t, x): (std::time::Instant, f32)| {
                                let dt = (now - t).as_secs_f32();
                                if dt > 0.001 {
                                    -(event.x - x) / dt
                                } else {
                                    0.0
                                }
                            });
                    self.drag_sample = Some((now, event.x));
                    if let Some(render) = self.render.as_mut() {
                        render.drive_transition(shown, velocity);
                    }
                }
                from_edge
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                self.contacts.remove(&event.id).is_some_and(|c| c.from_edge)
            }
        }
    }

    /// Offer a press to the shell.
    ///
    /// Returns whether the shell consumed it. Consuming is separate from acting, for the
    /// same reason it is on the transport strip: a press that lands on a screen the shell
    /// owns must not fall through to a browser underneath, whether or not anything
    /// happened.
    fn offer_to_shell(&mut self, x: f32, y: f32) -> bool {
        use crate::shell::{ScreenHit, ShellEvent};
        let Some(render) = self.render.as_mut() else {
            return false;
        };
        let Some(hit) = render.shell_hit(x, y) else {
            return false;
        };
        match hit {
            ScreenHit::Push(screen) => {
                info!(screen = screen.name(), "shell: a finger on the panel");
                render.shell_push(screen);
                render.set_shell_foreground(true);
            }
            ScreenHit::Back => {
                render.shell_back();
            }
            ScreenHit::Event(event) => {
                // Handed over rather than answered: only `app` knows what a host or a
                // file is.
                if let Some(sink) = self.shell_sink.as_ref() {
                    match &event {
                        ShellEvent::Tile(id) | ShellEvent::Item(id) => {
                            info!(%id, "shell: handing a press to the app");
                        }
                    }
                    sink(event);
                }
            }
        }
        true
    }

    /// Offer a press or release to the transport strip.
    ///
    /// Returns whether the strip *consumed* it. Consuming is separate from acting: a
    /// touch on the scrub track of a source that cannot seek produces no transaction and
    /// must still be swallowed, or it falls through to the browser underneath and scrolls
    /// a page nobody was looking at.
    fn offer_to_transport(&mut self, x: f32, y: f32, phase: crate::transport::TouchPhase) -> bool {
        let Some(render) = self.render.as_ref() else {
            return false;
        };
        if !render.transport_owns(x, y) {
            return false;
        }
        if let Some(txn) = render.transport_action(x, y, phase) {
            if let Some(sink) = self.controls.as_ref() {
                info!(?txn, "transport: a finger on the panel");
                sink(txn);
            }
        }
        true
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
                // The strip gets first refusal on the primary button — it is drawn over
                // whatever else is on screen, so it has to receive over it too.
                if button == PointerButton::Left {
                    let phase = if down {
                        crate::transport::TouchPhase::Press
                    } else {
                        crate::transport::TouchPhase::Release
                    };
                    if self.offer_to_transport(x, y, phase) {
                        return;
                    }
                    // A minimized app restores on click, same as on tap.
                    if down && self.restore_minimized(x, y) {
                        return;
                    }
                    // The shell sits under everything, so it only sees a press where
                    // nothing is covering it — `shell_hit` enforces that. Presses only:
                    // a release is the end of an interaction the press already claimed.
                    if down && self.offer_to_shell(x, y) {
                        return;
                    }
                }
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
                // Any touch raises the pill. Someone who does not know the gesture has to
                // be shown the way out without knowing to ask for it.
                self.wake_pill();
                if let Some(render) = self.render.as_mut() {
                    render.note_touch();
                }

                // The navigation layer sees every phase, and sees them first. It is the
                // only thing above a fullscreen cast, so it is the only thing that can
                // offer a way out of one.
                if self.route_navigation(&event) {
                    return;
                }

                let phase = match event.phase {
                    TouchPhase::Down => Some(crate::transport::TouchPhase::Press),
                    TouchPhase::Up => Some(crate::transport::TouchPhase::Release),
                    // A move is neither: the strip acts on the two ends of a contact, and
                    // swallowing moves would break a drag that started on the browser and
                    // happened to pass over the strip.
                    TouchPhase::Move | TouchPhase::Cancel => None,
                };
                if let Some(phase) = phase {
                    if self.offer_to_transport(event.x, event.y, phase) {
                        return;
                    }
                }
                // A drag over a shell screen scrolls it, rather than falling through to
                // a browser that is not even visible there.
                match event.phase {
                    TouchPhase::Down => {
                        // A minimized app restores on tap, before the shell or the
                        // page underneath can claim the press.
                        if self.restore_minimized(event.x, event.y) {
                            return;
                        }
                        if self
                            .render
                            .as_ref()
                            .is_some_and(|r| r.shell_hit(event.x, event.y).is_some())
                            || self
                                .render
                                .as_ref()
                                .is_some_and(|r| r.shell_scrollable(event.x, event.y))
                        {
                            self.drag_last.insert(event.id, event.y);
                        }
                        if self.offer_to_shell(event.x, event.y) {
                            self.drag_last.remove(&event.id);
                            return;
                        }
                    }
                    TouchPhase::Move => {
                        if let Some(last) = self.drag_last.get_mut(&event.id) {
                            let dy = event.y - *last;
                            *last = event.y;
                            if let Some(render) = self.render.as_mut() {
                                if render.shell_scroll(dy) {
                                    return;
                                }
                            }
                        }
                    }
                    TouchPhase::Up | TouchPhase::Cancel => {
                        self.drag_last.remove(&event.id);
                    }
                }
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
            if let Some(scene) = self.attract.take() {
                // Sent as a command rather than installed directly, so the surface is
                // drawn at the size the compositor actually has and follows every later
                // resize — the old path baked it once at a hardcoded 4K (D38).
                render.set_home(scene);
            }
            if let Some(osd) = self.osd.take() {
                render = render.with_osd(osd);
            }
            self.render = Some(render);
        }
        #[cfg(feature = "electron")]
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
            WindowEvent::KeyboardInput { event, .. } => {
                // Escape backs out one level, and is handled *here*, never delegated to
                // a page: the browser receives input only through the touch/pointer
                // protocol, so a key the kiosk consumes cannot leak into a page that
                // might be listening for it. One level means what the back gesture
                // means: a fullscreen cast surface is left first, then a pushed shell
                // screen, and Home absorbs the rest.
                if event.state == winit::event::ElementState::Pressed
                    && !event.repeat
                    && event.logical_key
                        == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape)
                {
                    self.back_one_level();
                }
            }
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.size = (size.width, size.height);
                if let Some(r) = &mut self.render {
                    r.resize(size.width, size.height);
                }
                #[cfg(feature = "electron")]
                if let Some(host) = &mut self.browser {
                    host.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                // Browser first: its pump may deliver a fresh frame, which
                // then lands in this same redraw's present.
                #[cfg(feature = "electron")]
                if let (Some(host), Some(r)) = (&mut self.browser, &mut self.render) {
                    host.pump(r);
                }
                self.tick_pill();
                if let Some(render) = self.render.as_mut() {
                    let now = std::time::Instant::now();
                    let dt = self
                        .last_frame
                        .map_or(std::time::Duration::ZERO, |t: std::time::Instant| now - t);
                    self.last_frame = Some(now);
                    render.tick_transition(dt);
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
    controls: Option<ControlSink>,
    shell_sink: Option<ShellSink>,
) -> Result<(), PipelineError> {
    let mut app = KioskApp {
        rx: Some(rx),
        attract,
        osd,
        window: None,
        render: None,
        controls,
        shell_sink,
        contacts: std::collections::HashMap::new(),
        drag_last: std::collections::HashMap::new(),
        last_frame: None,
        started_drag: false,
        drag_sample: None,
        pill_since: None,
        pill_drawn: false,
        exit,
        cursor: (0.0, 0.0),
        size: (1, 1),
        #[cfg(feature = "electron")]
        browser: None,
    };
    run_app(&mut app)
}

/// [`run`], plus a main-thread [`ElectronHost`](crate::electron_browser::ElectronHost)
/// pumped every frame (the kiosk loop is the browser's external message pump). Shuts the
/// browser down after the event loop exits.
///
/// # Errors
/// [`PipelineError`] if the event loop can't be created or run.
#[cfg(feature = "electron")]
pub fn run_with_browser(
    rx: Receiver<RenderCommand>,
    attract: Option<AttractImage>,
    osd: Option<OsdController>,
    exit: Option<Arc<AtomicBool>>,
    controls: Option<ControlSink>,
    shell_sink: Option<ShellSink>,
    browser: crate::electron_browser::ElectronHost,
) -> Result<(), PipelineError> {
    let mut app = KioskApp {
        rx: Some(rx),
        attract,
        osd,
        window: None,
        render: None,
        controls,
        shell_sink,
        contacts: std::collections::HashMap::new(),
        drag_last: std::collections::HashMap::new(),
        last_frame: None,
        started_drag: false,
        drag_sample: None,
        pill_since: None,
        pill_drawn: false,
        exit,
        cursor: (0.0, 0.0),
        size: (1, 1),
        browser: Some(browser),
    };
    let result = run_app(&mut app);
    // The browser is stopped on this (main) thread after the loop stops driving it,
    // so every borrowed frame is released before the subprocess goes away.
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
