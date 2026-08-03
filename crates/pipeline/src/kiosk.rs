//! The kiosk output: a `winit` borderless-fullscreen window whose surface the
//! [`WgpuCompositor`] renders into. This owns the winit event loop and therefore must
//! run on the **main thread** (architecture §6) — the tokio runtime and decode threads
//! live elsewhere and feed frames in over the [`RenderLoop`]'s channel.
//!
//! Presenting is demand-driven (#59). Every producer that queues work for this loop —
//! render commands, video frames, browser paints, OSD banners, the exit flag — wakes it
//! through a [`castaway_core::Waker`] armed with the event loop's proxy, and each frame
//! the loop recomputes what it owes the glass next ([`crate::demand::Demand`]): another
//! frame at display rate while something moves, a timer for the next scheduled change,
//! or nothing at all. Redraws are capped at the display's own refresh interval either
//! way; Mailbox present never blocks, so the pacing comes from `ControlFlow`, which is
//! the only place it can come from. Late frames are still dropped at the bounded
//! channel, so the window always shows the freshest available frame.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use input_touch::{
    ContactId, Input, InputSink, PointerButton, PointerEvent, TouchEvent, TouchPhase,
};
use tracing::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

use crate::error::PipelineError;
use crate::osd::OsdController;
use crate::render_pipeline::RenderLoop;
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
    rx: Option<crate::render_pipeline::RenderRx>,
    attract: Option<AttractImage>,
    osd: Option<OsdController>,
    /// The music visualiser's analyser (#15), installed on the render loop once the
    /// compositor exists. Held here for the same reason the attract scene is: the loop is
    /// not built until there is a surface to build it against.
    #[cfg(feature = "audio")]
    visualizer: Option<Arc<crate::visualizer::Analyzer>>,
    window: Option<Arc<Window>>,
    render: Option<RenderLoop>,
    /// External shutdown request (ctrl-c / service failure): checked whenever the loop
    /// runs, since a borderless-fullscreen kiosk has no chrome to close. The setter
    /// wakes the loop, which is what makes "whenever it runs" mean "now" (#59).
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
    /// Whether the primary mouse button is down — i.e. a synthesized contact is live.
    pointer_contact: bool,
    /// When the pointer was last actually used — moved, clicked or scrolled.
    ///
    /// `None` until it is, which on a wall panel with no mouse attached is forever, and
    /// is the state in which no arrow is drawn at all (#84).
    pointer_used: Option<std::time::Instant>,
    /// What the window was last told about cursor visibility, so a frame that changes
    /// nothing does not call into the window system. Starts `None` — nothing has been
    /// said yet, so the first frame says something whichever way it goes.
    cursor_shown: Option<bool>,
    /// The last position and time of that drag, for the velocity a flick carries.
    drag_sample: Option<(std::time::Instant, f32)>,
    /// Where each contact was last seen, for turning a drag into a scroll.
    drag_last: std::collections::HashMap<ContactId, f32>,
    /// The contact dragging the transport strip's scrub track, if any (#97).
    ///
    /// One, because there is one knob. Held so the strip gets that contact's *moves* and
    /// only that contact's: a drag that started on the browser and happens to pass over
    /// the strip belongs to the page underneath, and swallowing it would steal a scroll.
    scrubbing: Option<ContactId>,
    /// Live touch contacts, for the edge swipe. Nothing else in the tree tracked these:
    /// ids were carried faithfully to the browser and then forgotten (D38).
    contacts: std::collections::HashMap<ContactId, crate::overlay::Contact>,
    /// When the home pill was last raised, or `None` when it is not showing.
    pill_since: Option<std::time::Instant>,
    /// Whether the pill's texture is currently a layer, so the fade only uploads once.
    pill_drawn: bool,
    /// Whether an event arrived since the last redraw, so the panel owes the glass one
    /// more frame regardless of what the standing facts say. Set by input and by wakes,
    /// cleared by the redraw itself (#59).
    dirty: bool,
    /// The earliest the next redraw may present, from the last one plus the display's
    /// refresh interval — the cap that turns "everything wakes the loop" into "at most
    /// display rate". `None` before the first frame.
    next_frame_at: Option<std::time::Instant>,
    /// One display refresh, read off the monitor when the window comes up; the 60 Hz
    /// fallback covers a compositor that will not say.
    frame_interval: std::time::Duration,
    /// Input from off this thread — remote peers driving the panel (#18). Drained
    /// wherever the loop runs, so it obeys the same wake-and-sleep discipline as every
    /// other producer rather than needing a winit user-event type of its own.
    remote_input: Option<Arc<input_touch::RemoteInputQueue>>,
    /// The active session's touch surface — see [`KioskWiring::touch_surface`].
    touch_surface: Option<castaway_core::TouchHandle>,
    /// The surface [`Self::panel_size_known`] has already told the panel size to, so a
    /// resize and a new session both reach it and nothing else does.
    sized_surface: Option<Arc<dyn castaway_core::TouchSurface>>,
    /// The main-thread browser host (this loop is its message pump — architecture §6).
    #[cfg(feature = "electron")]
    browser: Option<crate::electron_browser::ElectronHost>,
}

impl KioskApp {
    /// The active session's touch surface, if one currently holds the glass.
    ///
    /// Checked ahead of [`Self::input_sink`]: a Miracast source with UIBC up is showing
    /// its own screen on the panel, so a finger on that picture belongs to it and not to
    /// the browser layer underneath.
    fn touch_surface(&mut self) -> Option<Arc<dyn castaway_core::TouchSurface>> {
        let surface = self.touch_surface.as_ref()?.get()?;
        // The panel's size is the router's to know, and the surface needs it to undo the
        // compositor's letterboxing. Told on arrival and on every resize.
        let known = self
            .sized_surface
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &surface));
        if !known {
            surface.panel_resized(self.size.0, self.size.1);
            self.sized_surface = Some(Arc::clone(&surface));
        }
        Some(surface)
    }

    /// Hand one contact to the session holding the glass. `false` if none is.
    ///
    /// Named `deliver_` rather than `to_`: it dispatches an event, it does not convert
    /// `self` into anything, and the `to_*` prefix on a `&mut self` method reads as a
    /// conversion to everyone including clippy.
    fn deliver_touch(&mut self, event: TouchEvent) -> bool {
        match self.touch_surface() {
            Some(surface) => {
                surface.touch(event.to_surface());
                true
            }
            None => false,
        }
    }

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

    /// Note a touch for the home pill's brighten-and-fade. Any touch does this: it is the
    /// affordance for someone who does not know the gesture, so it has to brighten for
    /// someone who does not know to ask for it. Whether the pill exists at all is not
    /// decided here — [`Self::tick_pill`] derives that from focus every frame.
    fn wake_pill(&mut self) {
        self.pill_since = Some(std::time::Instant::now());
    }

    /// Note that the pointer was used, which is what puts the cursor back on the glass.
    fn wake_cursor(&mut self) {
        self.pointer_used = Some(std::time::Instant::now());
    }

    /// Note that somebody touched the glass, which takes the cursor off it at once.
    ///
    /// Not a fade and not a timeout: a finger on the panel is a person who is not using
    /// the mouse, and the arrow left over from whoever last did is exactly the parked
    /// arrow this is all about.
    fn sleep_cursor(&mut self) {
        self.pointer_used = None;
    }

    /// Apply the cursor policy to the window. Called every frame; costs nothing when the
    /// answer has not changed.
    fn tick_cursor(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let used = self.pointer_used.map(|at| at.elapsed());
        let shown = crate::cursor::shown(used, self.pointer_contact);
        if self.cursor_shown == Some(shown) {
            return;
        }
        window.set_cursor_visible(shown);
        self.cursor_shown = Some(shown);
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
            // Going Home is somebody using the panel, whether the press came from the
            // glass, the edge swipe or a remote. It has to count as one, or the idle
            // return has no touch to date itself from: a remote pressing Home over a film
            // leaves the shell holding the glass, which is away from rest, and the film
            // would never come back (#23).
            render.note_touch();
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
            crate::panel::PanelHit::Close(surface) => {
                // The panel only *offers* the badge; ending the launch is the app's.
                // Consumed either way — a press on a visible X must not fall through
                // and restore the thing it meant to close.
                info!(?surface, "shell: closing a demoted surface");
                if let Some(sink) = self.shell_sink.as_ref() {
                    sink(crate::shell::ShellEvent::ClosePage);
                }
                true
            }
            crate::panel::PanelHit::Shell(_) | crate::panel::PanelHit::Miss => false,
        }
    }

    /// Update the pill layer for this frame. Cheap: while it is up and unchanging this
    /// writes one 48-byte uniform, and it only rasterizes when it first appears.
    fn tick_pill(&mut self) {
        let Some(render) = self.render.as_mut() else {
            return;
        };
        // The whole visibility policy is `pill_presence`, recomputed from focus every
        // frame: the pill exists only while a session covers the shell (and with it the
        // back button the shell's screens keep in the same corner), and while it exists
        // it never fully leaves — an exit affordance faded to nothing is
        // indistinguishable from there being no way out (TODO 16/19).
        let touched = self.pill_since.map(|since| since.elapsed());
        let opacity = crate::overlay::pill_presence(render.session_fullscreen(), touched);
        if opacity <= 0.0 {
            if self.pill_drawn {
                render.clear_home_pill();
                self.pill_drawn = false;
            }
            self.pill_since = None;
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
        // A finished brighten-and-fade hands back to the floor.
        if touched.is_some_and(|age| crate::overlay::pill_opacity(age) <= 0.0) {
            self.pill_since = None;
        }
    }

    /// What the panel owes the glass next, recomputed from standing facts every time the
    /// loop is about to sleep (#59). Merged across every part that keeps time: the
    /// render loop's motions and deadlines, the pill's fade, the browser host's
    /// scheduled recovery, and the one-more-frame owed to whatever event just arrived.
    fn demand(&self, now: std::time::Instant) -> crate::demand::Demand {
        use crate::demand::Demand;
        let mut demand = if self.dirty || self.pill_since.is_some() {
            // An event needs one frame to land; a raised pill is mid-fade. (The pill's
            // *resting* presence over a fullscreen session clears `pill_since` and costs
            // nothing — it only animates while a touch is fresh.)
            Demand::Frame
        } else {
            Demand::Idle
        };
        if let Some(render) = self.render.as_ref() {
            demand = demand.merge(render.demand(now));
        }
        // The trap #84 names: this loop sleeps on `Demand`, so a hide deadline that is not
        // merged here is simply slept through and the arrow stays up until something
        // unrelated wakes the loop.
        if let Some(until) = crate::cursor::next_change(
            self.pointer_used
                .map(|at| now.saturating_duration_since(at)),
            self.pointer_contact,
        ) {
            demand = demand.merge(Demand::At(now + until));
        }
        #[cfg(feature = "electron")]
        if let Some(host) = self.browser.as_ref() {
            demand = demand.merge(Demand::deadline(host.next_due()));
        }
        demand
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
                let intent = crate::overlay::edge_drag(
                    contact,
                    event.x,
                    event.y,
                    self.started_drag,
                    self.render.as_ref().map_or(0, RenderLoop::shell_depth),
                    self.render.as_ref().is_some_and(RenderLoop::can_hand_back),
                );
                if intent == crate::overlay::EdgeDrag::Home {
                    contact.fired = true;
                }
                match intent {
                    crate::overlay::EdgeDrag::Ignore => {}
                    crate::overlay::EdgeDrag::Home => self.go_home(),
                    crate::overlay::EdgeDrag::Begin => {
                        // Begin the navigation the finger is going to carry. Without this the
                        // drag drove a transition that did not exist, so nothing followed the
                        // hand — the behaviour the comment above described had never happened.
                        // `started_drag` is only set if one really began, so every other case
                        // leaves the completed-swipe branch reachable.
                        if let Some(render) = self.render.as_mut() {
                            if render.shell_back() {
                                self.started_drag = true;
                                self.drag_sample = None;
                            }
                        }
                    }
                    crate::overlay::EdgeDrag::Carry { shown } => {
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
                }
                from_edge
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                let from_edge = self.contacts.remove(&event.id).is_some_and(|c| c.from_edge);
                // Let go. Where the navigation lands is decided from where it was released and
                // how fast it was moving — and the flag has to be cleared here, or the
                // completed-swipe branch stays unreachable for the rest of the process's life.
                if self.started_drag {
                    if let Some(render) = self.render.as_mut() {
                        render.release_transition();
                    }
                    self.started_drag = false;
                    self.drag_sample = None;
                }
                from_edge
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
            ScreenHit::Push { screen, from } => {
                info!(screen = screen.name(), "shell: a finger on the panel");
                // The rect the press landed on travels with it: the screen grows out of the
                // tile somebody is looking at, and `back` shrinks it back into the same
                // place.
                render.shell_push_from(screen, from);
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
                        // Emitted by restore_minimized, never by a screen.
                        ShellEvent::ClosePage => {}
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
        let Some(render) = self.render.as_mut() else {
            return false;
        };
        // A captured drag is exempt from the ownership test, and has to be: the finger is
        // allowed to leave the track — people drag upwards to fine-tune — and asking
        // whether it is still over the strip would drop the scrub mid-gesture.
        let captured = phase == crate::transport::TouchPhase::Drag;
        if !captured && !render.transport_owns(x, y) {
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

    /// Apply everything a remote peer has queued since the last time the loop ran.
    ///
    /// Called from both the wake path and the pre-sleep pass: the queue is empty almost
    /// always, so a second check costs one uncontended lock, and it means no plausible
    /// interleaving of a push against a wake can leave input sitting until the *next*
    /// thing happens to wake the panel.
    fn drain_remote_input(&mut self) {
        let Some(queue) = self.remote_input.clone() else {
            return;
        };
        let batch = queue.drain();
        if batch.is_empty() {
            return;
        }
        // Input earns a frame for the same reason a finger on the glass does.
        self.dirty = true;
        for event in batch {
            match event {
                input_touch::RemoteEvent::Input(input) => self.apply(input),
                input_touch::RemoteEvent::Gone(origin) => self.forget_origin(origin),
                // The same road the pill and the edge swipe take, so a remote's way home
                // and the panel's are one behaviour rather than two that drift.
                input_touch::RemoteEvent::Home => self.go_home(),
            }
        }
    }

    /// A peer has gone away: drop everything the router holds for it, and tell whatever
    /// is underneath to let go too.
    ///
    /// Cancelled rather than released, and only for this origin. A dropped connection did
    /// not *finish* a gesture — synthesising the release would commit whatever it was
    /// over, which on the transport strip means seeking to wherever the finger happened
    /// to be when the phone lost Wi-Fi.
    fn forget_origin(&mut self, origin: input_touch::InputOrigin) {
        // Routed as real cancellations rather than deleted from the maps, because the
        // navigation layer keeps state *outside* them: a contact that had begun an edge
        // swipe left `started_drag` set and a transition being driven by a finger. Just
        // dropping the entry would strand the shell mid-transition with nothing left to
        // release it — the panel would sit at a half-open screen until someone touched
        // the glass. `route_navigation`'s cancel branch already unwinds all of it, so the
        // fix is to use it rather than to reimplement it here.
        //
        // The position is where the contact went down. Nothing reads it on a cancel, and
        // it is the only position this end ever knew.
        let doomed: Vec<(ContactId, (f32, f32))> = self
            .contacts
            .iter()
            .filter(|(id, _)| id.is_from(origin))
            .map(|(id, contact)| (*id, contact.start))
            .collect();
        for (id, (x, y)) in doomed {
            self.route_contact(TouchEvent::new(id, TouchPhase::Cancel, x, y));
        }
        // Whatever the navigation layer did not own — a contact that was scrolling a
        // shell screen, or one the maps still hold because it never reached them.
        self.contacts.retain(|id, _| !id.is_from(origin));
        self.drag_last.retain(|id, _| !id.is_from(origin));
        if let Some(sink) = self.input_sink() {
            sink.cancel_origin(origin);
        }
        info!(?origin, "remote input: peer gone, contacts cancelled");
    }

    /// Turn a winit window event into a normalized [`Input`], or nothing if it is not
    /// input at all.
    ///
    /// The *only* part of the input path that knows what winit is. Everything past here
    /// takes [`Input`], which is what lets a remote peer drive the same routing over a
    /// socket (#18) and lets the routing be tested without opening a window.
    ///
    /// The one piece of state it keeps is the cursor position, because winit reports a
    /// button press without one and the router should not have to know that.
    fn decode_window_event(&mut self, event: &WindowEvent) -> Option<Input> {
        let size = self.size;
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                self.wake_cursor();
                let (x, y) = normalize(position.x, position.y, size);
                Some(Input::Pointer(PointerEvent::Move { x, y }))
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.wake_cursor();
                let button = pointer_button(*button)?;
                let (x, y) = normalize(self.cursor.0, self.cursor.1, size);
                Some(Input::Pointer(PointerEvent::Button {
                    x,
                    y,
                    button,
                    down: state.is_pressed(),
                }))
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.wake_cursor();
                let (dx, dy) = wheel_pixels(*delta);
                let (x, y) = normalize(self.cursor.0, self.cursor.1, size);
                Some(Input::Pointer(PointerEvent::Wheel { x, y, dx, dy }))
            }
            WindowEvent::Touch(touch) => {
                // A finger on the glass is somebody not using the mouse. On Windows the
                // panel's HID digitizer is expected to drag the native cursor along with
                // every contact, so this is also what stops a touch *summoning* an arrow
                // — and it is why the touch path is the one that hides rather than the
                // pointer path being the only one that shows.
                self.sleep_cursor();
                Some(Input::Touch(translate_touch(touch, size)))
            }
            _ => None,
        }
    }

    /// Route one decoded input through the panel's layers and on to whatever is
    /// underneath.
    ///
    /// Sans-window and sans-socket: the same function serves the glass and every remote
    /// peer, which is the whole point of the split (ground rule 3).
    fn apply(&mut self, input: Input) {
        match input {
            Input::Touch(event) => {
                if self.route_contact(event) {
                    return;
                }
                if self.deliver_touch(event) {
                    return;
                }
                if let Some(sink) = self.input_sink() {
                    sink.touch(event);
                }
            }
            Input::Pointer(PointerEvent::Move { x, y }) => {
                // A held primary button is a contact mid-drag: it takes the same road a
                // finger does, or the gesture half of the panel only exists for touch.
                if self.pointer_contact
                    && self.route_contact(TouchEvent::new(
                        ContactId::POINTER,
                        TouchPhase::Move,
                        x,
                        y,
                    ))
                {
                    return;
                }
                // The panel's own mouse is a finger for a driven session too, or a
                // desk-tested mirror has no input at all.
                if self.pointer_contact
                    && self.deliver_touch(TouchEvent::new(
                        ContactId::POINTER,
                        TouchPhase::Move,
                        x,
                        y,
                    ))
                {
                    return;
                }
                if let Some(sink) = self.input_sink() {
                    sink.pointer(PointerEvent::Move { x, y });
                }
            }
            Input::Pointer(PointerEvent::Button { x, y, button, down }) => {
                // The primary button is a finger. It used to get a hand-rolled subset of
                // the touch routing — transport, restore, shell, but never the navigation
                // layer — so on any box whose screen reports as a *pointer* (a dev
                // mouse, and some HID touch stacks under Wayland) the edge swipe and the
                // home pill simply did not exist, while Esc worked. One path now.
                //
                // This is the panel's own mouse, and both `ContactId::POINTER` and
                // `pointer_contact` are singular because there is one of it. A remote
                // peer's clicks arrive as `Input::Touch` under their own origin instead.
                if button == PointerButton::Left {
                    let phase = if down {
                        TouchPhase::Down
                    } else {
                        TouchPhase::Up
                    };
                    self.pointer_contact = down;
                    let contact = TouchEvent::new(ContactId::POINTER, phase, x, y);
                    if self.route_contact(contact) {
                        return;
                    }
                    if self.deliver_touch(contact) {
                        return;
                    }
                }
                if let Some(sink) = self.input_sink() {
                    sink.pointer(PointerEvent::Button { x, y, button, down });
                }
            }
            Input::Pointer(wheel @ PointerEvent::Wheel { .. }) => {
                if let Some(sink) = self.input_sink() {
                    sink.pointer(wheel);
                }
            }
        }
    }

    /// Route one contact — a finger, or the primary mouse button standing in for one —
    /// through the panel's own layers. Returns whether it was consumed; an unconsumed
    /// contact belongs to whatever surface is underneath, in the caller's vocabulary
    /// (a touch to the touch sink, a pointer event to the pointer sink).
    fn route_contact(&mut self, event: TouchEvent) -> bool {
        // Any contact raises the pill. Someone who does not know the gesture has to
        // be shown the way out without knowing to ask for it.
        self.wake_pill();
        if let Some(render) = self.render.as_mut() {
            render.note_touch();
        }

        // The navigation layer sees every phase, and sees them first. It is the
        // only thing above a fullscreen cast, so it is the only thing that can
        // offer a way out of one.
        if self.route_navigation(&event) {
            return true;
        }

        // Which phase the strip is offered is a rule, and it lives in `transport::offer`
        // where it can be tested (#97). What is left here is the bookkeeping: which
        // contact owns the scrubber, and unwinding it when that contact goes away.
        let scrubbing = self.scrubbing == Some(event.id);
        let phase = crate::transport::offer(event.phase, scrubbing);
        if scrubbing && event.phase == TouchPhase::Cancel {
            self.scrubbing = None;
            if let Some(render) = self.render.as_mut() {
                render.clear_scrub_preview();
            }
        }
        if let Some(phase) = phase {
            let answered = self.offer_to_transport(event.x, event.y, phase);
            match event.phase {
                // A press the strip answered with a preview is a scrub beginning.
                TouchPhase::Down if answered => {
                    if self
                        .render
                        .as_ref()
                        .is_some_and(|r| r.scrub_preview().is_some())
                    {
                        self.scrubbing = Some(event.id);
                    }
                }
                // A lift ends it whether the strip answered or not: a finger that wandered
                // off the strip before letting go is still a finger letting go, and the
                // strip declines to answer for a point it does not own.
                TouchPhase::Up if scrubbing => {
                    self.scrubbing = None;
                    if let Some(render) = self.render.as_mut() {
                        render.clear_scrub_preview();
                    }
                }
                _ => {}
            }
            if answered {
                return true;
            }
        }
        // A drag over a shell screen scrolls it, rather than falling through to
        // a browser that is not even visible there.
        match event.phase {
            TouchPhase::Down => {
                // A minimized app restores on tap, before the shell or the
                // page underneath can claim the press.
                if self.restore_minimized(event.x, event.y) {
                    return true;
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
                    return true;
                }
            }
            TouchPhase::Move => {
                if let Some(last) = self.drag_last.get_mut(&event.id) {
                    let dy = event.y - *last;
                    *last = event.y;
                    if let Some(render) = self.render.as_mut() {
                        if render.shell_scroll(dy) {
                            return true;
                        }
                    }
                }
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                self.drag_last.remove(&event.id);
            }
        }
        false
    }
}

/// One frame at 60 Hz, until the monitor says otherwise (see `resumed`).
const FALLBACK_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_nanos(16_666_667);

/// The window's stable identity: Wayland `app_id`, X11 `WM_CLASS`, and the name the
/// desktop entry and hicolor icons are installed under. The three have to agree or
/// the Wayland icon lookup finds nothing, so it is one constant.
const APP_ID: &str = "castaway";

/// The window icon, on the platforms that take one from the window (X11, Windows).
///
/// 64px: X11 taskbars and pagers scale down from `_NET_WM_ICON`, and 64 is the
/// largest anything asks for at 1x without being wasteful to ship in every window.
/// `None` — no icon rather than no window — if the artwork fails to rasterize; the
/// checked-in artwork failing is caught by `icon`'s own tests, not here.
fn window_icon() -> Option<winit::window::Icon> {
    const SIDE: u32 = 64;
    let rgba = crate::icon::rasterize(SIDE)?;
    winit::window::Icon::from_rgba(rgba, SIDE, SIDE).ok()
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

/// Map a winit multi-touch contact to a normalized [`TouchEvent`]. winit ids are u64 but
/// stay small in practice; the truncation into the router's `u32` is only ever compared
/// against other *panel* contacts, since [`ContactId`] keeps the origins apart.
fn translate_touch(touch: &winit::event::Touch, size: (u32, u32)) -> TouchEvent {
    let phase = match touch.phase {
        winit::event::TouchPhase::Started => TouchPhase::Down,
        winit::event::TouchPhase::Moved => TouchPhase::Move,
        winit::event::TouchPhase::Ended => TouchPhase::Up,
        winit::event::TouchPhase::Cancelled => TouchPhase::Cancel,
    };
    let (x, y) = normalize(touch.location.x, touch.location.y, size);
    #[allow(clippy::cast_possible_truncation)]
    TouchEvent::new(ContactId::panel(touch.id as u32), phase, x, y)
}

/// The panel's pointer as a winit cursor, or `None` if it cannot be built.
///
/// 32 px square: the size a desktop cursor is authored at, and what both X11 and Windows
/// expect. A panel at 4K scales it in the compositor's cursor plane like any other.
fn themed_cursor(event_loop: &ActiveEventLoop) -> Option<winit::window::Cursor> {
    let (rgba, (hx, hy)) = crate::cursor::rasterize(CURSOR_SIDE)?;
    let source =
        winit::window::CustomCursor::from_rgba(rgba, CURSOR_SIDE_U16, CURSOR_SIDE_U16, hx, hy)
            .map_err(|e| debug!(error = %e, "the themed cursor would not build"))
            .ok()?;
    Some(event_loop.create_custom_cursor(source).into())
}

/// The side of the cursor bitmap, in pixels.
const CURSOR_SIDE: u32 = 32;
const CURSOR_SIDE_U16: u16 = 32;

impl ApplicationHandler for KioskApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("castaway")
            // X11 and Windows read the icon off the window itself. Wayland has no
            // such property — there the compositor looks up a `.desktop` entry whose
            // name matches the app_id set below, so the icon only appears if
            // `castaway.desktop` (Icon=castaway) and the hicolor PNGs are installed,
            // which nix/linux-kiosk.nix does.
            .with_window_icon(window_icon())
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        // A stable identity for the window: Wayland app_id and X11 WM_CLASS, both
        // "castaway". This is the string desktops key everything on — the Wayland
        // icon lookup above, taskbar grouping, window rules — and winit's default
        // is the generic "winit" otherwise. Called fully qualified because both
        // platform extension traits name their method `with_name`.
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::{wayland, x11};
            let attrs = wayland::WindowAttributesExtWayland::with_name(attrs, APP_ID, APP_ID);
            x11::WindowAttributesExtX11::with_name(attrs, APP_ID, APP_ID)
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                error!(error = %e, "failed to create kiosk window");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        // The panel's own pointer, in place of the OS arrow — the last piece of
        // Windows/Linux chrome that leaks through a surface otherwise entirely ours. It
        // rides the hardware cursor path, so no frame of lag and no compositor work; and
        // it is installed once, because whether it is *drawn* is `tick_cursor`'s and a
        // hidden cursor costs nothing to have themed.
        //
        // A failure here is cosmetic — the OS arrow, hidden on the same schedule — so it
        // is logged and stepped over rather than taken as a reason not to have a window,
        // exactly as the icon is.
        match themed_cursor(event_loop) {
            Some(cursor) => window.set_cursor(cursor),
            None => debug!("no themed cursor; the platform's own will be used"),
        }
        // Hidden from the first frame. Nothing has moved a mouse yet, and on a wall panel
        // with none attached nothing ever will.
        window.set_cursor_visible(false);
        self.cursor_shown = Some(false);

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
            #[cfg(feature = "audio")]
            if let Some(analyzer) = self.visualizer.take() {
                render = render.with_visualizer(analyzer);
            }
            self.render = Some(render);
        }
        #[cfg(feature = "electron")]
        if let Some(host) = &mut self.browser {
            host.resize(size.width, size.height);
        }
        self.size = (size.width, size.height);
        // The redraw cap comes from the panel itself: presenting faster than the display
        // refreshes is discarded by the Mailbox swapchain unseen (#59). Monitors that
        // will not say (some Wayland compositors) keep the 60 Hz default.
        if let Some(mhz) = window
            .current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .filter(|mhz| *mhz > 0)
        {
            self.frame_interval = std::time::Duration::from_secs_f64(1000.0 / f64::from(mhz));
        }
        info!(
            width = size.width,
            height = size.height,
            frame_interval = ?self.frame_interval,
            "kiosk window up"
        );
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Any event but the redraw itself can change what the glass should show — a
        // finger raises the pill, a resize moves every layer — so each one earns the
        // panel exactly one more frame, capped at display rate by `about_to_wait`.
        if !matches!(event, WindowEvent::RedrawRequested) {
            self.dirty = true;
        }
        if let Some(input) = self.decode_window_event(&event) {
            self.apply(input);
        }
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
                // Forget who has been told, so the next contact re-tells at the new size.
                self.sized_surface = None;
                if let Some(r) = &mut self.render {
                    r.resize(size.width, size.height);
                }
                #[cfg(feature = "electron")]
                if let Some(host) = &mut self.browser {
                    host.resize(size.width, size.height);
                }
            }
            WindowEvent::RedrawRequested => {
                let now = std::time::Instant::now();
                self.dirty = false;
                self.next_frame_at = Some(now + self.frame_interval);
                // Browser first: its pump may deliver a fresh frame, which
                // then lands in this same redraw's present.
                #[cfg(feature = "electron")]
                if let (Some(host), Some(r)) = (&mut self.browser, &mut self.render) {
                    host.pump(r);
                }
                self.tick_pill();
                self.tick_cursor();
                if let Some(render) = self.render.as_mut() {
                    let dt = self
                        .last_frame
                        .map_or(std::time::Duration::ZERO, |t: std::time::Instant| now - t);
                    self.last_frame = Some(now);
                    // Commands, animations, transport, OSD, present — and the answer to
                    // "when next" is `demand`'s, read in `about_to_wait`. Nothing here
                    // requests a redraw: whether another frame is owed is a standing
                    // fact, not a habit.
                    render.frame(dt);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, (): ()) {
        // A wake: some producer put something where the next redraw will find it — a
        // render command, a browser paint, a banner, or the exit flag, which
        // `about_to_wait` checks on every pass.
        self.dirty = true;
        self.drain_remote_input();
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Before deciding whether the panel may sleep, take anything a peer queued: a
        // pending input is work owed, and sleeping on it would hold the contact until
        // something unrelated woke the loop.
        self.drain_remote_input();
        if self
            .exit
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            info!("kiosk: exit requested, closing");
            event_loop.exit();
            return;
        }
        use crate::demand::Demand;
        let now = std::time::Instant::now();
        // What the panel owes the glass, and when it is allowed to pay: a demanded
        // frame waits for the display-rate cap; a scheduled change waits for its
        // moment; an idle panel sleeps until a wake arrives. This is the whole
        // replacement for the old unconditional `request_redraw` — the ~970 fps
        // free-run that burned a core drawing an idle shell.
        match self.demand(now) {
            Demand::Frame => {
                let due = self.next_frame_at.filter(|due| *due > now);
                match (due, &self.window) {
                    (None, Some(window)) => window.request_redraw(),
                    (Some(due), _) => event_loop.set_control_flow(ControlFlow::WaitUntil(due)),
                    (None, None) => event_loop.set_control_flow(ControlFlow::Wait),
                }
            }
            Demand::At(at) if at <= now => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                } else {
                    event_loop.set_control_flow(ControlFlow::Wait);
                }
            }
            Demand::At(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            Demand::Idle => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Everything the kiosk loop needs besides its render channel.
///
/// A struct rather than eight positional arguments: every one of them is optional, most
/// are `Option<Arc<dyn Fn…>>`, and at that width a caller swapping two of them typechecks
/// and misbehaves at runtime. `Default` gives the "none of it" case, which is what the
/// tests and the headless build want.
#[derive(Default)]
pub struct KioskWiring {
    /// The idle scene shown before and between casts.
    pub attract: Option<AttractImage>,
    /// The on-screen-display banner controller.
    pub osd: Option<OsdController>,
    /// The music visualiser's analyser (#15). The same one attached to the mixer as a tap;
    /// this end only reads it.
    #[cfg(feature = "audio")]
    pub visualizer: Option<Arc<crate::visualizer::Analyzer>>,
    /// External shutdown request (ctrl-c, or a service that failed).
    pub exit: Option<Arc<AtomicBool>>,
    /// Where a press on the transport strip goes.
    pub controls: Option<ControlSink>,
    /// Where a shell press the panel cannot answer itself goes.
    pub shell_sink: Option<ShellSink>,
    /// Input queued from off the main thread by remote peers (#18).
    pub remote_input: Option<Arc<input_touch::RemoteInputQueue>>,
    /// The active session's touch surface, if it published one.
    ///
    /// Taken from [`castaway_core::SessionManager::touch_handle`] before the manager is
    /// consumed. A session that can be *driven* from the glass — a Miracast source with
    /// UIBC negotiated — takes the panel ahead of the browser layer for the whole time it
    /// is the active source, because the picture on screen is its picture (#125).
    pub touch_surface: Option<castaway_core::TouchHandle>,
}

impl KioskWiring {
    /// Build the loop's state around this wiring.
    fn into_app(self, rx: crate::render_pipeline::RenderRx) -> KioskApp {
        KioskApp {
            rx: Some(rx),
            attract: self.attract,
            osd: self.osd,
            #[cfg(feature = "audio")]
            visualizer: self.visualizer,
            exit: self.exit,
            controls: self.controls,
            shell_sink: self.shell_sink,
            remote_input: self.remote_input,
            touch_surface: self.touch_surface,
            sized_surface: None,
            window: None,
            render: None,
            contacts: std::collections::HashMap::new(),
            drag_last: std::collections::HashMap::new(),
            scrubbing: None,
            last_frame: None,
            started_drag: false,
            pointer_contact: false,
            drag_sample: None,
            pill_since: None,
            pointer_used: None,
            cursor_shown: None,
            pill_drawn: false,
            dirty: true,
            next_frame_at: None,
            frame_interval: FALLBACK_FRAME_INTERVAL,
            cursor: (0.0, 0.0),
            size: (1, 1),
            #[cfg(feature = "electron")]
            browser: None,
        }
    }
}

/// Run the kiosk to completion (blocks the calling — main — thread). Consumes render
/// commands from `rx` and displays them fullscreen until the window is closed or
/// `wiring.exit` is set (ctrl-c).
///
/// # Errors
/// [`PipelineError`] if the event loop can't be created or run.
pub fn run(rx: crate::render_pipeline::RenderRx, wiring: KioskWiring) -> Result<(), PipelineError> {
    run_app(&mut wiring.into_app(rx))
}

/// [`run`], plus a main-thread [`ElectronHost`](crate::electron_browser::ElectronHost)
/// pumped every frame (the kiosk loop is the browser's external message pump). Shuts the
/// browser down after the event loop exits.
///
/// # Errors
/// [`PipelineError`] if the event loop can't be created or run.
#[cfg(feature = "electron")]
pub fn run_with_browser(
    rx: crate::render_pipeline::RenderRx,
    wiring: KioskWiring,
    browser: crate::electron_browser::ElectronHost,
) -> Result<(), PipelineError> {
    let mut app = wiring.into_app(rx);
    app.browser = Some(browser);
    let result = run_app(&mut app);
    // The browser is stopped on this (main) thread after the loop stops driving it,
    // so every borrowed frame is released before the subprocess goes away.
    if let Some(host) = app.browser.take() {
        host.shutdown();
    }
    result
}

fn run_app(app: &mut KioskApp) -> Result<(), PipelineError> {
    let event_loop = EventLoop::<()>::with_user_event()
        .build()
        .map_err(|e| PipelineError::GpuInit(format!("event loop: {e}")))?;
    // The wake path (#59): every producer that queues work for this loop holds a
    // `Waker`, and the wakers are armed with the loop's proxy here — the first moment a
    // proxy exists. Wakes that arrived before this fired are latched and land now.
    let arm = |waker: castaway_core::Waker| {
        let proxy = event_loop.create_proxy();
        // A closed loop means the process is leaving; a wake with nowhere to go is fine.
        waker.arm(move || {
            let _ = proxy.send_event(());
        });
    };
    if let Some(rx) = &app.rx {
        arm(rx.waker());
    }
    if let Some(osd) = &app.osd {
        arm(osd.waker());
    }
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
    #![allow(clippy::unwrap_used)]
    use super::*;
    use input_touch::{InputOrigin, RemoteId};

    /// A router with no window and no compositor behind it.
    ///
    /// This is what the decode/apply split buys: the layers that decide *where* a
    /// contact goes — the reserved edge, the pill, the per-contact bookkeeping — are
    /// reachable without opening a window or standing up a GPU, so the cases that used
    /// to need a person and a panel are ordinary unit tests. Everything that needs the
    /// compositor (the transport strip, shell hits, scrolling) simply declines with
    /// `render: None`, which is the same road a press takes on a panel showing nothing.
    fn router() -> KioskApp {
        KioskApp {
            rx: None,
            attract: None,
            osd: None,
            #[cfg(feature = "audio")]
            visualizer: None,
            window: None,
            render: None,
            controls: None,
            shell_sink: None,
            remote_input: None,
            contacts: std::collections::HashMap::new(),
            drag_last: std::collections::HashMap::new(),
            scrubbing: None,
            // No session holding the glass, which is what these cases are about. These two
            // were added to `KioskApp` without ever reaching this literal, and nothing
            // noticed: no gate has ever built this crate's tests with `kiosk` on, so the
            // whole module below has not compiled for as long as they have existed.
            touch_surface: None,
            sized_surface: None,
            last_frame: None,
            started_drag: false,
            pointer_contact: false,
            drag_sample: None,
            pill_since: None,
            pointer_used: None,
            cursor_shown: None,
            pill_drawn: false,
            dirty: false,
            next_frame_at: None,
            frame_interval: FALLBACK_FRAME_INTERVAL,
            exit: None,
            cursor: (0.0, 0.0),
            size: (3840, 2160),
            #[cfg(feature = "electron")]
            browser: None,
        }
    }

    fn down(id: ContactId, x: f32, y: f32) -> Input {
        Input::Touch(TouchEvent::new(id, TouchPhase::Down, x, y))
    }

    fn up(id: ContactId, x: f32, y: f32) -> Input {
        Input::Touch(TouchEvent::new(id, TouchPhase::Up, x, y))
    }

    fn moved(id: ContactId, x: f32, y: f32) -> Input {
        Input::Touch(TouchEvent::new(id, TouchPhase::Move, x, y))
    }

    const PANEL: (u32, u32) = (1920, 1080);

    /// A router with a real strip on a real (offscreen) panel.
    ///
    /// `router()` above declines everything the compositor owns, which is the right shape
    /// for the contact bookkeeping it was written for and useless for the scrub: the whole
    /// question is what happens between a finger and a laid-out strip. `None` where there
    /// is no GPU.
    fn router_with_strip() -> Option<KioskApp> {
        use castaway_core::{ControlCapabilities, NowPlaying, PlaybackState};
        let (tx, rx) = crate::render_pipeline::render_channel(8);
        let mut render = crate::test_gpu::render_loop(PANEL.0, PANEL.1, rx)?;

        let mut track = NowPlaying::default().with_title("one");
        track.state = PlaybackState::Playing;
        track.position = Some(std::time::Duration::ZERO);
        track.duration = Some(std::time::Duration::from_secs(200));
        tx.send(crate::render_pipeline::RenderCommand::NowPlaying(Box::new(
            crate::nowplaying_card::NowPlayingCard {
                track,
                source: castaway_core::SourceDescription::default(),
                up_next: Vec::new(),
                controls: ControlCapabilities::PLAY
                    | ControlCapabilities::PAUSE
                    | ControlCapabilities::SEEK,
            },
        )));
        render.pump();

        let mut app = router();
        app.size = PANEL;
        app.render = Some(render);
        Some(app)
    }

    /// A point `fraction` along the scrub track, panel-normalized.
    fn on_track(app: &KioskApp, fraction: f32) -> (f32, f32) {
        let model = app
            .render
            .as_ref()
            .unwrap()
            .transport_model()
            .expect("a strip is on screen");
        let (ox, oy, sw, sh) = crate::transport::placement(PANEL.0, PANEL.1);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let layout = crate::transport::layout(&model, sw.round() as u32, sh.round() as u32);
        let track = layout.track_touch.expect("a seekable track has a bar");
        let (_, ty) = track.center();
        (
            (ox + track.x + track.w * fraction) / PANEL.0 as f32,
            (oy + ty) / PANEL.1 as f32,
        )
    }

    #[test]
    fn a_finger_on_the_scrub_track_owns_it_until_it_lifts() {
        let Some(mut app) = router_with_strip() else {
            return;
        };
        let finger = ContactId::panel(1);

        let (x, y) = on_track(&app, 0.2);
        app.apply(down(finger, x, y));
        assert_eq!(
            app.scrubbing,
            Some(finger),
            "a press on the track begins a scrub"
        );
        let first = app.render.as_ref().unwrap().scrub_preview();
        assert!(
            first.is_some_and(|f| (f - 0.2).abs() < 0.02),
            "and the bar goes to the finger: {first:?}"
        );

        // Sliding along it moves the preview — the moves reach the strip *because* this
        // contact owns it, which is the half that took a third TouchPhase to express.
        let (x, y) = on_track(&app, 0.8);
        app.apply(moved(finger, x, y));
        let moved_to = app.render.as_ref().unwrap().scrub_preview();
        assert!(
            moved_to.is_some_and(|f| (f - 0.8).abs() < 0.02),
            "the bar must follow: {moved_to:?}"
        );

        app.apply(up(finger, x, y));
        assert_eq!(app.scrubbing, None, "the lift ends it");
        assert_eq!(
            app.render.as_ref().unwrap().scrub_preview(),
            None,
            "and the picture goes back to being the source's"
        );
    }

    #[test]
    fn a_drag_that_merely_crosses_the_strip_is_left_alone() {
        // The constraint D38 added and #97 had to keep: a contact that went down on the
        // browser and happens to pass over the strip belongs to the page underneath.
        // Taking its moves would steal a scroll from whatever someone was actually
        // reading, and the theft would be invisible — the page just stops following the
        // finger near the bottom of the screen.
        let Some(mut app) = router_with_strip() else {
            return;
        };
        let finger = ContactId::panel(2);

        // Down in the middle of the panel, nowhere near the strip.
        app.apply(down(finger, 0.5, 0.3));
        assert_eq!(app.scrubbing, None, "nothing was scrubbed by that");

        // …and now across the track.
        let (x, y) = on_track(&app, 0.5);
        app.apply(moved(finger, x, y));
        assert_eq!(
            app.scrubbing, None,
            "a passing drag must not capture the scrubber"
        );
        assert_eq!(
            app.render.as_ref().unwrap().scrub_preview(),
            None,
            "and must not move the bar under it"
        );
    }

    #[test]
    fn a_lost_contact_puts_the_bar_back_rather_than_seeking() {
        // A remote peer that drops off Wi-Fi mid-drag. `forget_origin` routes a cancel,
        // and a cancel is not a lift: the gesture did not finish, so the bar returns to
        // the music and the source is asked for nothing.
        let Some(mut app) = router_with_strip() else {
            return;
        };
        let seeks = Arc::new(std::sync::Mutex::new(Vec::new()));
        app.controls = Some(Arc::new({
            let seeks = Arc::clone(&seeks);
            move |txn| seeks.lock().unwrap().push(txn)
        }));

        let remote = ContactId::remote(RemoteId::new(7), 0);
        let (x, y) = on_track(&app, 0.6);
        app.apply(down(remote, x, y));
        assert_eq!(app.scrubbing, Some(remote));

        app.forget_origin(InputOrigin::Remote(RemoteId::new(7)));
        assert_eq!(app.scrubbing, None, "the contact is gone, so the scrub is");
        assert_eq!(
            app.render.as_ref().unwrap().scrub_preview(),
            None,
            "the bar goes back to the music"
        );
        assert!(
            seeks.lock().unwrap().is_empty(),
            "a dropped connection must not seek anywhere: {:?}",
            seeks.lock().unwrap()
        );
    }

    #[test]
    fn two_peers_numbering_their_fingers_the_same_are_two_contacts() {
        // The failure `ContactId` exists to prevent, at the layer that would have
        // suffered it: the router's own contact map. Before the origin was part of the
        // identity these three presses were one entry, and the first release ended all
        // of them.
        let mut app = router();
        let (alice, bob) = (RemoteId::new(1), RemoteId::new(2));
        app.apply(down(ContactId::remote(alice, 0), 0.3, 0.3));
        app.apply(down(ContactId::remote(bob, 0), 0.5, 0.5));
        app.apply(down(ContactId::panel(0), 0.7, 0.7));
        assert_eq!(app.contacts.len(), 3);

        app.apply(up(ContactId::remote(alice, 0), 0.3, 0.3));
        assert_eq!(app.contacts.len(), 2);
        assert!(app.contacts.contains_key(&ContactId::remote(bob, 0)));
        assert!(app.contacts.contains_key(&ContactId::panel(0)));
    }

    #[test]
    fn a_remote_contact_reaches_the_reserved_edge_like_a_finger() {
        // "As if they were actual touches on the screen" is the whole ask, and the edge
        // strip is the sharpest test of it: a contact starting there is swallowed by the
        // navigation layer rather than passed to whatever is underneath.
        let mut app = router();
        let remote = ContactId::remote(RemoteId::new(1), 0);
        app.apply(down(remote, 0.001, 0.5));
        let contact = app.contacts.get(&remote).expect("tracked");
        assert!(
            contact.from_edge,
            "a remote press on the reserved edge is the panel's, exactly as a finger's is"
        );
    }

    #[test]
    fn a_pointer_event_wakes_the_cursor_and_a_touch_puts_it_away() {
        // The two directions of #84, at the level that decides them. A panel nobody has
        // moved a mouse on draws no arrow; moving one draws it; and a finger on the glass
        // takes it away again at once rather than on a timer — the person at the panel is
        // not using the mouse, and the arrow left over from whoever last did is exactly
        // the parked arrow this is about.
        let mut app = router();
        assert!(
            app.pointer_used.is_none(),
            "nothing has used the pointer yet"
        );
        assert!(!crate::cursor::shown(None, false));

        app.decode_window_event(&WindowEvent::CursorMoved {
            device_id: winit::event::DeviceId::dummy(),
            position: winit::dpi::PhysicalPosition::new(100.0, 100.0),
        });
        let used = app.pointer_used.map(|at| at.elapsed());
        assert!(used.is_some(), "a move must wake the cursor");
        assert!(crate::cursor::shown(used, false));

        app.decode_window_event(&WindowEvent::Touch(winit::event::Touch {
            device_id: winit::event::DeviceId::dummy(),
            phase: winit::event::TouchPhase::Started,
            location: winit::dpi::PhysicalPosition::new(200.0, 200.0),
            force: None,
            id: 0,
        }));
        assert!(
            app.pointer_used.is_none(),
            "a finger on the glass takes the arrow off it"
        );
        assert!(!crate::cursor::shown(None, false));
    }

    #[test]
    fn the_cursors_hide_deadline_reaches_the_demand_calculation() {
        // The trap the issue names: this loop sleeps on `Demand`, and `Demand::Idle`
        // becomes `ControlFlow::Wait`. A hide deadline nothing asks a frame for is slept
        // through, and the arrow stays up until something unrelated wakes the loop —
        // which on an idle panel is not soon.
        let mut app = router();
        let now = std::time::Instant::now();
        assert!(
            matches!(app.demand(now), crate::demand::Demand::Idle),
            "an untouched panel asks for nothing"
        );

        app.pointer_used = Some(now);
        match app.demand(now) {
            crate::demand::Demand::At(at) => {
                assert_eq!(at, now + crate::cursor::HOLD, "the hide is what is due");
            }
            other => panic!("a visible cursor owes a frame at its hide time, got {other:?}"),
        }

        // …and once it has hidden, nothing is owed again.
        app.pointer_used = Some(now - crate::cursor::HOLD);
        assert!(
            matches!(app.demand(now), crate::demand::Demand::Idle),
            "a hidden cursor asks for nothing"
        );
    }

    #[test]
    fn a_remote_contact_does_not_touch_the_panel_mouse_state() {
        // `pointer_contact` and `ContactId::POINTER` are singular because the panel has
        // one mouse. A remote's press must not claim them, or two drivers would fight
        // over one flag and a remote release would end the local drag.
        let mut app = router();
        app.apply(down(ContactId::remote(RemoteId::new(1), 0), 0.5, 0.5));
        assert!(!app.pointer_contact);
        assert!(!app.contacts.contains_key(&ContactId::POINTER));
    }

    #[test]
    fn the_local_mouse_still_becomes_a_contact() {
        // The behaviour the split had to preserve: a press is a finger, so the gesture
        // layer sees it. Regressing this would silently remove the edge swipe and the
        // home pill on every box whose screen reports as a pointer.
        let mut app = router();
        app.apply(Input::Pointer(PointerEvent::Button {
            x: 0.5,
            y: 0.5,
            button: PointerButton::Left,
            down: true,
        }));
        assert!(app.pointer_contact);
        assert!(app.contacts.contains_key(&ContactId::POINTER));

        app.apply(Input::Pointer(PointerEvent::Button {
            x: 0.5,
            y: 0.5,
            button: PointerButton::Left,
            down: false,
        }));
        assert!(!app.pointer_contact);
        assert!(app.contacts.is_empty());
    }

    #[test]
    fn a_non_primary_button_is_not_a_contact() {
        let mut app = router();
        app.apply(Input::Pointer(PointerEvent::Button {
            x: 0.5,
            y: 0.5,
            button: PointerButton::Right,
            down: true,
        }));
        assert!(!app.pointer_contact);
        assert!(app.contacts.is_empty());
    }

    #[test]
    fn any_contact_raises_the_pill_whoever_it_belongs_to() {
        // The pill is the affordance for someone who does not know the gesture. Someone
        // driving from a phone needs it for the same reason — more so, since they cannot
        // see the bezel.
        let mut app = router();
        assert!(app.pill_since.is_none());
        app.apply(down(ContactId::remote(RemoteId::new(1), 0), 0.5, 0.5));
        assert!(app.pill_since.is_some());
    }

    #[test]
    fn a_cancel_ends_a_contact_as_thoroughly_as_a_release() {
        // What a dropped peer's contacts become. If `Cancel` left the entry behind, the
        // router would believe a finger was down for the rest of the process's life.
        let mut app = router();
        let id = ContactId::remote(RemoteId::new(1), 0);
        app.apply(down(id, 0.5, 0.5));
        app.apply(Input::Touch(TouchEvent::new(
            id,
            TouchPhase::Cancel,
            0.5,
            0.5,
        )));
        assert!(app.contacts.is_empty());
    }

    #[test]
    fn a_drained_queue_reaches_the_router() {
        // The whole path a remote contact takes, minus the socket: queued off-thread,
        // drained where the loop runs, routed exactly as a finger would be.
        let mut app = router();
        let queue = Arc::new(input_touch::RemoteInputQueue::new(
            castaway_core::Waker::new(),
        ));
        app.remote_input = Some(Arc::clone(&queue));
        let id = ContactId::remote(RemoteId::new(1), 0);

        queue.push_input(down(id, 0.5, 0.5));
        assert!(app.contacts.is_empty(), "nothing lands before a drain");

        app.drain_remote_input();
        assert!(app.contacts.contains_key(&id));
        assert!(app.dirty, "input owes the glass a frame");
    }

    #[test]
    fn a_departed_peer_leaves_nothing_behind_in_the_router() {
        // `Gone` has to clear the router's own maps, not just tell the sink. A contact
        // left in `contacts` keeps its origin's edge-swipe state alive forever, and one
        // left in `drag_last` keeps scrolling a screen with a finger that went home.
        let mut app = router();
        let queue = Arc::new(input_touch::RemoteInputQueue::new(
            castaway_core::Waker::new(),
        ));
        app.remote_input = Some(Arc::clone(&queue));
        let peer = RemoteId::new(1);
        let gone = ContactId::remote(peer, 0);
        let staying = ContactId::panel(0);

        queue.push_input(down(gone, 0.5, 0.5));
        queue.push_input(down(staying, 0.7, 0.7));
        queue.push_gone(InputOrigin::Remote(peer));
        app.drain_remote_input();

        assert!(
            !app.contacts.contains_key(&gone),
            "the peer's contact is gone"
        );
        assert!(
            app.contacts.contains_key(&staying),
            "and nobody else's went with it"
        );
    }

    #[test]
    fn a_peer_that_presses_and_immediately_drops_strands_nothing() {
        // The ordering the queue exists to preserve. If the cancellation were tracked
        // beside the input rather than in it, this could apply the press *after* the
        // cancel and leave the contact down for the life of the process.
        let mut app = router();
        let queue = Arc::new(input_touch::RemoteInputQueue::new(
            castaway_core::Waker::new(),
        ));
        app.remote_input = Some(Arc::clone(&queue));
        let peer = RemoteId::new(1);

        queue.push_input(down(ContactId::remote(peer, 0), 0.5, 0.5));
        queue.push_gone(InputOrigin::Remote(peer));
        app.drain_remote_input();

        assert!(app.contacts.is_empty());
    }

    #[test]
    fn a_peer_that_drops_mid_swipe_does_not_strand_the_navigation() {
        // The navigation layer keeps state outside the contact map: a contact that began
        // an edge swipe sets `started_drag` and leaves a transition being driven by a
        // finger. Dropping the map entry alone would leave the shell at a half-open
        // screen with nothing left to release it.
        let mut app = router();
        let peer = RemoteId::new(1);
        let id = ContactId::remote(peer, 0);
        app.apply(down(id, 0.001, 0.5));
        app.started_drag = true;

        app.forget_origin(InputOrigin::Remote(peer));

        assert!(!app.started_drag, "the drag was never released");
        assert!(app.drag_sample.is_none());
        assert!(app.contacts.is_empty());
    }

    #[test]
    fn forgetting_an_origin_leaves_every_other_origins_contacts_alone() {
        let mut app = router();
        let peer = RemoteId::new(1);
        app.apply(down(ContactId::remote(peer, 0), 0.3, 0.3));
        app.apply(down(ContactId::panel(0), 0.6, 0.6));

        app.forget_origin(InputOrigin::Remote(peer));

        assert_eq!(app.contacts.len(), 1);
        assert!(app.contacts.contains_key(&ContactId::panel(0)));
    }

    #[test]
    fn draining_without_a_queue_is_nothing() {
        // The headless build, and every build before a peer has ever connected.
        let mut app = router();
        app.drain_remote_input();
        assert!(!app.dirty);
    }

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
