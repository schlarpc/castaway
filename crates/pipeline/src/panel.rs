//! What the panel is presenting: the whole of it, in one place (D38).
//!
//! This is the model the shell, the session surfaces and the browser all used to answer
//! separately. It exists because they disagreed, repeatedly and in the same way: "what is
//! on the glass" was a product of independent variables in three objects —
//! `RenderLoop::shell_front`, the screen stack's depth, `ElectronHost::role` plus its
//! cached `widget_covered` — and the combinations nobody had decided about were the bugs.
//! A now-playing card demoted into the *Home screen's* widget slot while the shell was two
//! screens deep was drawn over the text somebody was reading, because `shell_front` says
//! whether the shell is above the media layers and nothing whatever about which screen it
//! is on.
//!
//! So the whole answer is derived here, purely, from three things: which screens are
//! stacked, which surfaces exist, and whether the shell has been asked forward. Nothing in
//! this module touches the GPU, the compositor or a socket, which is what makes the
//! interesting assertions ("given this panel, that surface is nowhere") unit tests rather
//! than an offscreen render.
//!
//! What is deliberately *not* here:
//!
//! - **Geometry.** [`Placement`] says which of three states a surface is in; the rect it
//!   turns into is the surface's own business — video demotes to the PiP corner, a card
//!   and a page to the widget slot, and each keeps its own arithmetic
//!   ([`crate::attract::WidgetSlot`], [`crate::compositor::Transform::pip`]).
//! - **Precedence between two surfaces in the same slot.** A card and a minimised page
//!   both want the widget slot; which wins is declared on the layers themselves
//!   ([`crate::compositor::LayerId::yields_to`]) and enforced by the compositor. That is
//!   the right place for it: it is a fact about depth, not about focus.
//! - **Occlusion by things this does not model** — the OSD, the transport strip, the
//!   mascot overlay. The compositor keeps its own veto for those.

use crate::attract::AttractScene;
use crate::shell::{Screen, ScreenHit, ScreenStack};

/// Which corner a demoted video goes to. Bottom-right: the shell's own content runs down
/// the left, and the home pill owns the bottom-left.
pub const PIP_CORNER: u8 = 3;

/// A surface the panel can be showing.
///
/// Four, and the fourth is the one that had been hiding: the idle widget and a minimised
/// cast page are the same browser painting into the same slot, told apart in the old code
/// by comparing the current URL against the configured widget URL. They are not the same
/// kind of thing at all — one is an ornament belonging to the Home screen, the other is a
/// session that has been demoted and can be restored — and every question about them has a
/// different answer. Now that difference is a variant instead of a string comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    /// Decoded video: a cast, or a mirror.
    Video,
    /// The now-playing card for a session that has no pixels of its own.
    Card,
    /// A page that *is* the session: YouTube leanback, a Cast app receiver.
    CastPage,
    /// The idle screen's own web widget — the clock in the reserved card.
    ///
    /// Belongs to Home rather than to a session, and that is the whole difference: it is
    /// never full-panel, a press on it is the page's own and never means "restore me", and
    /// it goes away as soon as the shell leaves Home or a session outranks it.
    IdleWidget,
}

impl Surface {
    /// Every surface, so a caller can place all of them without a list of its own going
    /// stale when a fifth appears.
    pub const ALL: [Self; 4] = [Self::Video, Self::Card, Self::CastPage, Self::IdleWidget];

    /// Whether this surface belongs to a session — something a sender put up, which can
    /// own the whole panel and be handed back the glass.
    ///
    /// [`Self::IdleWidget`] is the one that does not: it is part of the Home screen.
    #[must_use]
    pub const fn is_session(self) -> bool {
        !matches!(self, Self::IdleWidget)
    }
}

/// Which surfaces exist right now.
///
/// Presence is not a state machine — it is whatever the layer lifecycle has put up, and it
/// changes when a first frame lands, a card is published, a page is shown, or a deferred
/// clear expires. It lives here anyway, because it is what makes focus answerable: the
/// panel cannot hand the glass to a session that has no surface, and that used to be
/// representable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Surfaces {
    video: bool,
    card: bool,
    cast_page: bool,
    idle_widget: bool,
}

impl Surfaces {
    /// Whether `surface` is up.
    #[must_use]
    pub const fn has(self, surface: Surface) -> bool {
        match surface {
            Surface::Video => self.video,
            Surface::Card => self.card,
            Surface::CastPage => self.cast_page,
            Surface::IdleWidget => self.idle_widget,
        }
    }

    /// Record that `surface` is up, or gone. Returns whether anything changed.
    pub fn set(&mut self, surface: Surface, present: bool) -> bool {
        let slot = match surface {
            Surface::Video => &mut self.video,
            Surface::Card => &mut self.card,
            Surface::CastPage => &mut self.cast_page,
            Surface::IdleWidget => &mut self.idle_widget,
        };
        let changed = *slot != present;
        *slot = present;
        changed
    }

    /// Whether any *session* surface is up — the question focus depends on.
    #[must_use]
    pub const fn any_session(self) -> bool {
        self.video || self.card || self.cast_page
    }
}

/// Who owns the glass.
///
/// Derived, never stored, which is the point: [`Focus::Session`] with no session surface
/// up was representable before and meant nothing — the shell was "behind" something that
/// was not there, so it looked forward while every hit test believed otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// A session surface fills the panel; the shell is behind it, on whatever screen it
    /// was on.
    Session,
    /// The shell owns the glass. Session surfaces, if any, are demoted or hidden.
    Shell,
}

/// Where a surface goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Fills the panel.
    Panel,
    /// Demoted into the Home screen's furniture — the widget slot for a card or a page,
    /// the PiP corner for video. Tappable: a press here means "give it the panel back".
    Widget,
    /// Not on the glass at all.
    ///
    /// Which is not the same as gone: the decoder keeps decoding, the page keeps playing,
    /// the session keeps its clock. A screen above Home owns its whole surface, and a
    /// demoted corner is a Home-screen affordance, so there is nowhere for one to be.
    Hidden,
}

impl Placement {
    /// Whether a surface placed like this is drawn.
    #[must_use]
    pub const fn visible(self) -> bool {
        !matches!(self, Self::Hidden)
    }

    /// Whether this is the demoted, tappable form.
    #[must_use]
    pub const fn is_widget(self) -> bool {
        matches!(self, Self::Widget)
    }
}

/// What one "out" press — the back gesture, the escape key — spent itself on.
///
/// The ordering this encodes used to be an if-chain in the kiosk's `back_one_level`: try
/// to minimise a fullscreen page, else pop a screen, else bring the shell forward. Three
/// branches over two objects, in the order somebody wrote them. As a verb on the panel it
/// collapses into the two cases focus already distinguishes, and callers match
/// exhaustively rather than reading the chain to work out what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Left {
    /// A session that filled the panel was demoted into the shell. The shell now has the
    /// glass, on whatever screen it was left on.
    Demoted,
    /// One shell screen was popped.
    Screen,
    /// Nothing left to leave: the shell has the glass and is already Home.
    Nothing,
}

/// What a press on the panel means.
#[derive(Debug, Clone, PartialEq)]
pub enum PanelHit {
    /// A demoted session surface. The press means "give this the panel back" — never
    /// "forward my touch into it at 42% scale".
    Restore(Surface),
    /// The shell's current screen answered it.
    Shell(ScreenHit),
    /// Nothing the panel owns is there. The press belongs to whatever is underneath — a
    /// page's own viewport, most often.
    Miss,
}

/// The panel: every screen, every surface, and who has the glass.
#[derive(Debug)]
pub struct Panel {
    /// The shell's screens. `None` for a renderer nothing has given a Home to — the
    /// offscreen test harness, a headless tap — which has no screens to protect and so
    /// lets a session keep the panel.
    shell: Option<ScreenStack>,
    surfaces: Surfaces,
    /// Whether the shell has been *asked* forward. Deliberately not the same question as
    /// [`Self::focus`]: asking for the glass when nothing is playing is already answered
    /// by Home being all there is to show.
    shell_forward: bool,
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

impl Panel {
    /// A panel with no screens and nothing playing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            shell: None,
            surfaces: Surfaces {
                video: false,
                card: false,
                cast_page: false,
                idle_widget: false,
            },
            shell_forward: false,
        }
    }

    // -- what is up ---------------------------------------------------------------

    /// Set or refresh what Home shows, creating the stack if this is the first one.
    ///
    /// Home is rebuilt whenever the receiver's state changes — a protocol going down, a
    /// host appearing — and must not yank anyone out of a screen they are reading.
    pub fn set_home(&mut self, scene: AttractScene) {
        match &mut self.shell {
            Some(stack) => stack.update_home(scene),
            None => self.shell = Some(ScreenStack::new(scene)),
        }
    }

    /// Record that a surface is up, or gone. Returns whether anything changed.
    pub fn set_surface(&mut self, surface: Surface, present: bool) -> bool {
        self.surfaces.set(surface, present)
    }

    /// Which surfaces are up.
    #[must_use]
    pub const fn surfaces(&self) -> Surfaces {
        self.surfaces
    }

    /// Whether there is a session surface to hand the glass to.
    ///
    /// What the edge-drag gesture asks before it starts animating: with nothing playing,
    /// dragging the shell aside at Home would uncover nothing and is not a gesture.
    #[must_use]
    pub const fn can_hand_back(&self) -> bool {
        self.surfaces.any_session()
    }

    // -- focus and placement -----------------------------------------------------

    /// Who owns the glass. Total, and derived — see [`Focus`].
    #[must_use]
    pub const fn focus(&self) -> Focus {
        if !self.shell_forward && self.surfaces.any_session() {
            Focus::Session
        } else {
            Focus::Shell
        }
    }

    /// Where `surface` goes right now.
    ///
    /// Total over (focus, which screen, which surface), which is the whole point: there is
    /// no combination left that nobody decided about.
    #[must_use]
    pub fn placement(&self, surface: Surface) -> Placement {
        if !self.surfaces.has(surface) {
            return Placement::Hidden;
        }
        // The idle widget is Home's own furniture: it never fills the panel, and it yields
        // the moment the shell leaves Home. (It also yields to a session surface in the
        // same slot, which is declared on the layer rather than here — see the module
        // docs.)
        if !surface.is_session() {
            return if self.at_home() {
                Placement::Widget
            } else {
                Placement::Hidden
            };
        }
        match self.focus() {
            Focus::Session => Placement::Panel,
            // A screen above Home owns its whole surface. Demoting into furniture that
            // screen does not have is what drew a card over a service screen's text.
            Focus::Shell if !self.at_home() => Placement::Hidden,
            Focus::Shell => Placement::Widget,
        }
    }

    /// Whether the shell is showing Home itself.
    ///
    /// A renderer with no shell counts as Home: it has no screens to be somewhere else on.
    #[must_use]
    fn at_home(&self) -> bool {
        self.shell.as_ref().is_none_or(|s| s.depth() == 1)
    }

    // -- navigation --------------------------------------------------------------

    /// Go one step out, from wherever the panel is. See [`Left`].
    pub fn back(&mut self) -> Left {
        match self.focus() {
            // Leaving a fullscreen session comes first, and demotes *all* of it together —
            // video to its corner, a card and a page to the slot. That togetherness is the
            // fold: minimising the page used to leave the shell nominally behind a surface
            // that was no longer covering anything, so the pill, the PiP and the card each
            // believed something different about who had the glass.
            Focus::Session => {
                self.shell_forward = true;
                Left::Demoted
            }
            Focus::Shell => {
                if self.pop_screen() {
                    Left::Screen
                } else {
                    Left::Nothing
                }
            }
        }
    }

    /// Pop one shell screen, without touching focus. Returns whether anything moved.
    ///
    /// The shell's own step, for a back affordance *on* a screen — which is only reachable
    /// when the shell already has the glass. One press "out" from anywhere is [`Self::back`],
    /// which leaves a fullscreen session first; going through that here would demote a
    /// playing session as a side effect of a press on a button it is covering.
    pub fn pop_screen(&mut self) -> bool {
        self.shell.as_mut().is_some_and(ScreenStack::pop)
    }

    /// Go all the way Home, and bring the shell forward over whatever is playing.
    ///
    /// What the home gesture and the pill do. Something playing is demoted, never stopped:
    /// pressing Home in the middle of a film is not asking for it to end.
    pub fn go_home(&mut self) {
        self.shell_forward = true;
        if let Some(stack) = &mut self.shell {
            stack.go_home();
        }
    }

    /// Hand the glass back to what is playing, if anything is.
    ///
    /// Returns whether [`Self::focus`] moved — which is what callers want it for, since a
    /// focus that did not move needs no layers re-placed and no log line. `false` therefore
    /// covers both "there was nothing to hand it to" and "it already had it"; neither is an
    /// error, and neither has anything for a caller to do.
    pub fn hand_to_session(&mut self) -> bool {
        if !self.surfaces.any_session() {
            return false;
        }
        let before = self.focus();
        self.shell_forward = false;
        before != self.focus()
    }

    /// Bring the shell forward over whatever is playing. Returns whether focus moved.
    pub fn hand_to_shell(&mut self) -> bool {
        let before = self.focus();
        self.shell_forward = true;
        before != self.focus()
    }

    /// Put the panel back to its resting arrangement: Home, with the glass handed to
    /// whatever is playing. Both ends of a session ask for this.
    pub fn rest(&mut self) {
        self.shell_forward = false;
        if let Some(stack) = &mut self.shell {
            stack.go_home();
        }
    }

    /// Push a shell screen, bringing the shell forward — a screen nobody can see is not a
    /// screen anybody navigated to.
    pub fn push(&mut self, screen: Screen) {
        if let Some(stack) = &mut self.shell {
            stack.push(screen);
            self.shell_forward = true;
        }
    }

    /// Replace the screen on top, or push if at Home. What a picker's own refreshes use,
    /// so `back` stays one step however many times the list updated.
    pub fn replace_top(&mut self, screen: Screen) {
        if let Some(stack) = &mut self.shell {
            stack.replace_top(screen);
            self.shell_forward = true;
        }
    }

    // -- the shell underneath ----------------------------------------------------

    /// The screen stack, for the renderer that has to draw it.
    #[must_use]
    pub const fn stack(&self) -> Option<&ScreenStack> {
        self.shell.as_ref()
    }

    /// The screen stack, mutably — for a screen carrying interaction state of its own,
    /// like a picker's scroll position.
    pub const fn stack_mut(&mut self) -> Option<&mut ScreenStack> {
        self.shell.as_mut()
    }

    /// The current screen, if there is a shell at all.
    #[must_use]
    pub fn current(&self) -> Option<&Screen> {
        self.shell.as_ref().map(ScreenStack::current)
    }

    /// How deep the shell is; `1` is Home, `0` is no shell at all.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.shell.as_ref().map_or(0, ScreenStack::depth)
    }

    /// Everything above Home, for a navigation that may need putting back.
    #[must_use]
    pub fn above_screens(&self) -> Vec<Screen> {
        self.shell
            .as_ref()
            .map(ScreenStack::above_screens)
            .unwrap_or_default()
    }

    /// Put the stack back, for a gesture abandoned half-way.
    pub fn restore_screens(&mut self, screens: Vec<Screen>) {
        if let Some(stack) = &mut self.shell {
            stack.restore(screens);
        }
    }

    // -- input -------------------------------------------------------------------

    /// What a panel-normalized press means, given a `width`×`height` surface.
    ///
    /// One answer where there were four — `hit_minimized_card`, `hit_pip`, the browser's
    /// `hit_minimized`, and the shell's own hit test — each deriving "is this demoted"
    /// from a different variable. Demoted surfaces are offered first and in a fixed order,
    /// because they are drawn over the shell and a press cannot belong to both.
    ///
    /// `covered` is the compositor's veto for surfaces this model does not describe: the
    /// transport strip, the OSD, the mascot's arms. It is consulted only for the shell,
    /// which is the one thing that sits under all of them.
    pub fn hit(
        &self,
        width: u32,
        height: u32,
        x: f32,
        y: f32,
        covered: impl FnOnce(f32, f32) -> bool,
    ) -> PanelHit {
        // The card before the video: the card is the one drawn when both a session and a
        // demoted page exist, because a playing session outranks the page's slot.
        for surface in [Surface::Card, Surface::CastPage, Surface::Video] {
            if self.placement(surface).is_widget()
                && demoted_rect(surface, width, height).is_some_and(|r| r.contains(x, y))
            {
                return PanelHit::Restore(surface);
            }
        }
        let Some(stack) = self.shell.as_ref() else {
            return PanelHit::Miss;
        };
        if covered(x, y) {
            return PanelHit::Miss;
        }
        stack
            .current()
            .hit(width.max(1), height.max(1), x, y)
            .map_or(PanelHit::Miss, PanelHit::Shell)
    }
}

/// A panel-normalized rectangle: where a demoted surface sits, as a fraction of the
/// surface.
///
/// Normalized rather than in pixels because that is what both consumers want — the
/// compositor takes a [`crate::compositor::Transform`] and a hit test takes a fraction —
/// and it keeps this module free of device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormRect {
    /// Left edge, `0.0..=1.0`.
    pub x: f32,
    /// Top edge, `0.0..=1.0`.
    pub y: f32,
    /// Width as a fraction of the surface.
    pub w: f32,
    /// Height as a fraction of the surface.
    pub h: f32,
}

impl NormRect {
    /// Whether a panel-normalized point is inside.
    #[must_use]
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x <= self.x + self.w && y <= self.y + self.h
    }
}

/// Where `surface` sits when it is demoted, on a `width`×`height` surface.
///
/// The two answers differ and always have: video shrinks into the PiP corner
/// (bottom-right, clear of the home pill), while a card or a page takes the Home screen's
/// reserved widget card (top-right). Both are 16:9, so both scales are uniform.
#[must_use]
pub fn demoted_rect(surface: Surface, width: u32, height: u32) -> Option<NormRect> {
    let (w, h) = (width.max(1), height.max(1));
    match surface {
        Surface::Video => {
            let t = crate::compositor::Transform::pip(PIP_CORNER);
            Some(NormRect {
                x: t.offset_x,
                y: t.offset_y,
                w: t.scale_x,
                h: t.scale_y,
            })
        }
        Surface::Card | Surface::CastPage | Surface::IdleWidget => {
            crate::attract::WidgetSlot::RightCard.rect(w, h).map(|r| {
                let t = r.transform(w, h);
                NormRect {
                    x: t.offset_x,
                    y: t.offset_y,
                    w: t.scale_x,
                    h: t.scale_y,
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::picker::Picker;

    const W: u32 = 1920;
    const H: u32 = 1080;

    fn panel() -> Panel {
        let mut p = Panel::new();
        p.set_home(AttractScene::demo());
        p
    }

    fn a_screen() -> Screen {
        Screen::Picker(Box::new(Picker::loading("Moonlight", "…")))
    }

    /// Never covered — the compositor veto these tests are not about.
    fn clear(_x: f32, _y: f32) -> bool {
        false
    }

    #[test]
    fn a_panel_with_nothing_playing_is_the_shells() {
        let p = panel();
        assert_eq!(p.focus(), Focus::Shell);
        for surface in Surface::ALL {
            assert_eq!(p.placement(surface), Placement::Hidden, "{surface:?}");
        }
    }

    #[test]
    fn focus_on_a_session_that_is_not_there_is_not_representable() {
        // The invariant the type exists for. Asking for the glass with nothing to give it
        // to used to leave the shell nominally behind a surface that did not exist, so
        // every hit test believed something different about who had the panel.
        let mut p = panel();
        assert!(!p.hand_to_session(), "nothing to hand it to");
        assert_eq!(p.focus(), Focus::Shell);

        // A surface appearing is enough on its own: focus follows what exists, so there is
        // no window in which the panel believes a session has the glass and none is up.
        p.set_surface(Surface::Video, true);
        assert_eq!(p.focus(), Focus::Session);

        assert!(p.hand_to_shell(), "and it can be taken back");
        assert_eq!(p.focus(), Focus::Shell);
        assert!(p.hand_to_session());
        assert_eq!(p.focus(), Focus::Session);

        // …and it goes back on its own when the surface goes away, rather than leaving a
        // panel focused on a corpse.
        p.set_surface(Surface::Video, false);
        assert_eq!(p.focus(), Focus::Shell);
    }

    #[test]
    fn a_session_with_the_glass_fills_the_panel() {
        let mut p = panel();
        p.set_surface(Surface::Video, true);
        p.set_surface(Surface::Card, true);
        assert_eq!(p.placement(Surface::Video), Placement::Panel);
        assert_eq!(p.placement(Surface::Card), Placement::Panel);
    }

    #[test]
    fn the_shell_at_home_demotes_a_session_rather_than_stopping_it() {
        let mut p = panel();
        p.set_surface(Surface::Video, true);
        p.hand_to_shell();
        assert_eq!(p.focus(), Focus::Shell);
        assert_eq!(p.placement(Surface::Video), Placement::Widget);
    }

    #[test]
    fn a_screen_above_home_leaves_no_room_for_a_demoted_surface() {
        // The reported bug, as one assertion. The demoted slot is Home's own furniture, so
        // a screen pushed above Home has nowhere to put it — and drawing it there put a
        // card over the service screen's text.
        let mut p = panel();
        for surface in [Surface::Video, Surface::Card, Surface::CastPage] {
            p.set_surface(surface, true);
        }
        p.hand_to_shell();
        for surface in [Surface::Video, Surface::Card, Surface::CastPage] {
            assert_eq!(p.placement(surface), Placement::Widget, "{surface:?}");
        }

        p.push(a_screen());
        for surface in [Surface::Video, Surface::Card, Surface::CastPage] {
            assert_eq!(p.placement(surface), Placement::Hidden, "{surface:?}");
        }

        // And coming back restores every one of them, with the session untouched.
        assert_eq!(p.back(), Left::Screen);
        for surface in [Surface::Video, Surface::Card, Surface::CastPage] {
            assert_eq!(p.placement(surface), Placement::Widget, "{surface:?}");
        }
    }

    #[test]
    fn the_idle_widget_is_homes_furniture_and_never_a_session() {
        // It shares the slot and the browser with a minimised cast page, and used to share
        // its *type* too — told apart by comparing URLs. Every question about them differs.
        let mut p = panel();
        p.set_surface(Surface::IdleWidget, true);
        assert_eq!(p.placement(Surface::IdleWidget), Placement::Widget);
        assert!(!p.can_hand_back(), "an ornament is not a session");
        assert_eq!(p.focus(), Focus::Shell);

        // It is never full-panel, whatever focus says.
        p.set_surface(Surface::Video, true);
        p.hand_to_session();
        assert_eq!(p.focus(), Focus::Session);
        assert_eq!(p.placement(Surface::IdleWidget), Placement::Widget);

        // And it leaves with Home.
        p.push(a_screen());
        assert_eq!(p.placement(Surface::IdleWidget), Placement::Hidden);
    }

    #[test]
    fn back_leaves_the_fullscreen_session_before_the_screen_under_it() {
        // The ordering that used to live in an if-chain across two objects. It falls out
        // of focus now: you cannot pop a screen you are not looking at.
        let mut p = panel();
        p.set_surface(Surface::CastPage, true);
        p.push(a_screen());
        p.hand_to_session();
        assert_eq!(p.focus(), Focus::Session);

        assert_eq!(p.back(), Left::Demoted, "the cast goes first");
        assert_eq!(p.depth(), 2, "and leaves the screen where it was");
        assert_eq!(p.back(), Left::Screen);
        assert_eq!(p.depth(), 1);
        assert_eq!(p.back(), Left::Nothing);
    }

    #[test]
    fn popping_a_screen_is_not_the_same_act_as_leaving_a_session() {
        // Two verbs, deliberately. A back affordance drawn *on* a screen is only reachable
        // when the shell already has the glass, so pressing it must pop and nothing else —
        // routing it through `back()` would demote whatever is playing as a side effect of
        // a press on a button that session is not even covering.
        let mut p = panel();
        p.set_surface(Surface::Video, true);
        p.push(a_screen());
        p.hand_to_session();
        assert_eq!(p.focus(), Focus::Session);

        assert!(p.pop_screen(), "the screen goes");
        assert_eq!(p.depth(), 1);
        assert_eq!(p.focus(), Focus::Session, "and the session keeps the glass");
    }

    #[test]
    fn back_always_terminates_however_deep_it_got() {
        // The shell's own invariant, restated over the whole panel: whatever route someone
        // took in, enough presses of back get them out.
        let mut p = panel();
        p.set_surface(Surface::Video, true);
        p.hand_to_session();
        for _ in 0..64 {
            p.push(a_screen());
        }
        let mut guard = 0;
        while p.back() != Left::Nothing {
            guard += 1;
            assert!(guard < 1000, "back did not terminate");
        }
        assert_eq!(p.depth(), 1);
        assert_eq!(p.focus(), Focus::Shell);
    }

    #[test]
    fn a_press_on_a_demoted_surface_restores_it_rather_than_reaching_into_it() {
        let mut p = panel();
        p.set_surface(Surface::Card, true);
        p.hand_to_shell();

        let r = demoted_rect(Surface::Card, W, H).unwrap();
        let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(
            p.hit(W, H, cx, cy, clear),
            PanelHit::Restore(Surface::Card),
            "a tap on the corner means give it the panel back"
        );

        // Pushed away, the same point is the screen's — or nothing, but never a restore.
        p.push(a_screen());
        assert!(!matches!(p.hit(W, H, cx, cy, clear), PanelHit::Restore(_)));
    }

    #[test]
    fn a_full_panel_session_owns_every_point() {
        // Nothing demoted, so no restore anywhere — and the shell is behind it, so its
        // tiles must not answer either.
        let mut p = panel();
        p.set_surface(Surface::Video, true);
        p.hand_to_session();
        for (x, y) in [(0.1, 0.1), (0.5, 0.5), (0.9, 0.9)] {
            // The compositor's veto is what reports the cover; the model's job is not to
            // claim the point for the shell on its own account.
            assert_eq!(p.hit(W, H, x, y, |_, _| true), PanelHit::Miss);
        }
    }

    #[test]
    fn the_idle_widget_is_never_a_restore_target() {
        // Tapping the clock talks to the clock. It is not a minimised session, so there is
        // nothing to bring back — and the press has to fall through to the page.
        let mut p = panel();
        p.set_surface(Surface::IdleWidget, true);
        let r = demoted_rect(Surface::IdleWidget, W, H).unwrap();
        let hit = p.hit(W, H, r.x + r.w / 2.0, r.y + r.h / 2.0, clear);
        assert!(!matches!(hit, PanelHit::Restore(_)), "{hit:?}");
    }

    #[test]
    fn resting_the_panel_hands_the_glass_back_and_returns_home() {
        let mut p = panel();
        p.set_surface(Surface::Card, true);
        p.push(a_screen());
        assert_eq!(p.focus(), Focus::Shell, "pushing brings the shell forward");

        p.rest();
        assert_eq!(p.depth(), 1);
        assert_eq!(p.focus(), Focus::Session);
    }

    #[test]
    fn a_renderer_with_no_shell_lets_the_session_keep_the_panel() {
        // The offscreen harness and the headless tap: no screens to protect, so nothing to
        // demote for.
        let mut p = Panel::new();
        p.set_surface(Surface::Video, true);
        assert_eq!(p.depth(), 0);
        assert_eq!(p.placement(Surface::Video), Placement::Panel);
        p.hand_to_shell();
        assert_eq!(
            p.placement(Surface::Video),
            Placement::Widget,
            "and asking for the shell still demotes, rather than hiding into a screen \
             that does not exist"
        );
    }
}
