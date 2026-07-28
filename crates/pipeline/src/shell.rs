//! The app shell: what screen the panel is on, and how you get back (D38).
//!
//! This module is the *model* — pure, no GPU, no channels. It says which screen is
//! current and enforces the one rule that keeps the panel usable: **every screen can be
//! left, and leaving repeatedly always arrives home.** A screen you can enter and not
//! leave is the failure this exists to make unrepresentable (ground rule 1).
//!
//! Screens are sent to the render thread as models, never as pixels, and rasterised
//! there at the true surface size — the same shape `RenderCommand::NowPlaying` already
//! uses, and the reason the idle screen stops being a bitmap baked once at startup.
//!
//! What lives here is only the shell's *own* surfaces. A cast filling the panel is not a
//! screen in this sense: it is a video or browser layer composited above the shell, and
//! the shell is still underneath it holding whatever screen it was on.

use crate::attract::AttractScene;

/// A screen the shell can draw.
///
/// `#[non_exhaustive]` because this list is expected to grow — the picker lands next, and
/// settings and a media library after it — and every `match` on it should be forced to
/// say what it does with a new one.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Screen {
    /// The idle screen: who this panel is, and a tile per thing it can be or do.
    Home(AttractScene),
    /// One service's instructions, opened by pressing its tile.
    Service(Box<crate::service::ServiceScreen>),
    /// A list to choose from — GameStream hosts, then that host's apps.
    Picker(Box<crate::picker::Picker>),
}

impl Screen {
    /// A short, stable name for logs. Not shown to anyone.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Home(_) => "home",
            Self::Service(_) => "service",
            Self::Picker(_) => "picker",
        }
    }
}

/// What a press on a shell screen means.
///
/// The shell answers what it can locally — pressing a service tile opens that service's
/// screen, pressing back goes back — and hands `app` the rest. Same division as the
/// transport strip's: the renderer knows *where* things are, the app knows what they
/// mean (D33).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellEvent {
    /// A tile with no local screen behind it. `app` decides: GameStream opens a host
    /// picker, media opens a library.
    Tile(String),
    /// A row in a picker.
    Item(String),
}

impl Screen {
    /// What a panel-normalized touch hits on this screen, if anything.
    #[must_use]
    pub fn hit(&self, width: u32, height: u32, x: f32, y: f32) -> Option<ScreenHit> {
        match self {
            Self::Home(scene) => crate::attract::tile_hit(scene, width, height, x, y).map(|id| {
                // A tile carrying its own instructions is answered here; one without
                // is the app's to interpret.
                scene
                    .tiles
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.detail.clone().map(|d| (t.clone(), d)))
                    .map_or(ScreenHit::Event(ShellEvent::Tile(id)), |(tile, detail)| {
                        ScreenHit::Push(Screen::Service(Box::new(crate::service::ServiceScreen {
                            tile,
                            detail,
                        })))
                    })
            }),
            Self::Service(_) => {
                crate::service::hit_back(width, height, x, y).then_some(ScreenHit::Back)
            }
            Self::Picker(p) => crate::picker::hit(p, width, height, x, y).map(|h| match h {
                crate::picker::PickerHit::Back => ScreenHit::Back,
                crate::picker::PickerHit::Item(id) => ScreenHit::Event(ShellEvent::Item(id)),
            }),
        }
    }
}

/// What the shell should do about a press.
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenHit {
    /// Go one screen deeper, with this.
    Push(Screen),
    /// Go back one.
    Back,
    /// Not the shell's to decide — tell `app`.
    Event(ShellEvent),
}

/// The screen stack. Home is the floor and cannot be popped.
///
/// A stack rather than a single current screen, because "back" has to mean something
/// once a picker leads to another picker: home → hosts → apps, and back unwinds it.
///
/// Home is a field rather than element zero of a `Vec`, so "the stack is never empty" is
/// structural instead of an invariant somebody has to keep. There is no empty state to
/// handle, so there is no `unwrap` and no arbitrary fallback screen (ground rules 1, 7).
#[derive(Debug, Clone)]
pub struct ScreenStack {
    home: Screen,
    /// Screens above Home, innermost last.
    above: Vec<Screen>,
}

impl ScreenStack {
    /// A stack showing `home`.
    #[must_use]
    pub fn new(home: AttractScene) -> Self {
        Self {
            home: Screen::Home(home),
            above: Vec::new(),
        }
    }

    /// What is on screen now. Total: there is always a screen.
    #[must_use]
    pub fn current(&self) -> &Screen {
        self.above.last().unwrap_or(&self.home)
    }

    /// What is on screen now, mutably — for a screen that carries interaction state of
    /// its own, like a picker's scroll position.
    pub fn current_mut(&mut self) -> &mut Screen {
        self.above.last_mut().unwrap_or(&mut self.home)
    }

    /// How deep we are. `1` is Home.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.above.len() + 1
    }

    /// Whether there is anywhere to go back to.
    #[must_use]
    pub fn can_pop(&self) -> bool {
        !self.above.is_empty()
    }

    /// Push a screen on top of the current one.
    pub fn push(&mut self, screen: Screen) {
        self.above.push(screen);
    }

    /// Go back one screen. Returns whether anything moved — `false` means we were
    /// already home, which is not an error, just nowhere left to go.
    pub fn pop(&mut self) -> bool {
        self.above.pop().is_some()
    }

    /// Go all the way back to Home. Returns whether anything moved.
    ///
    /// This is what the home gesture does, and it is deliberately one operation rather
    /// than "pop until you cannot" at the call site: one action, one state change, no
    /// chance of a partial unwind leaving the panel somewhere nobody asked for.
    pub fn go_home(&mut self) -> bool {
        let moved = self.can_pop();
        self.above.clear();
        moved
    }

    /// Replace the screen on top, or push if we are at Home.
    ///
    /// What "answer the press now, fill it in when the network does" needs: the first
    /// update pushes a "looking…" screen, and the ones after it replace that rather than
    /// burying it, so `back` still means one step rather than one step per refresh.
    pub fn replace_top(&mut self, screen: Screen) {
        match self.above.last_mut() {
            Some(top) => *top = screen,
            None => self.above.push(screen),
        }
    }

    /// Everything stacked above Home, for a navigation that may need putting back.
    #[must_use]
    pub fn above_screens(&self) -> Vec<Screen> {
        self.above.clone()
    }

    /// Put the stack back to `screens` above Home. The other half of
    /// [`Self::above_screens`], for a gesture abandoned half-way.
    pub fn restore(&mut self, screens: Vec<Screen>) {
        self.above = screens;
    }

    /// Replace what Home shows, wherever we currently are.
    ///
    /// Home is rebuilt when the receiver's state changes — a protocol going down, a host
    /// appearing — and that must not yank someone out of a picker they are reading.
    pub fn update_home(&mut self, home: AttractScene) {
        self.home = Screen::Home(home);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn stack() -> ScreenStack {
        ScreenStack::new(AttractScene::demo())
    }

    #[test]
    fn a_new_stack_is_home_and_cannot_go_back_further() {
        let mut s = stack();
        assert_eq!(s.depth(), 1);
        assert!(!s.can_pop());
        assert!(
            !s.pop(),
            "popping at home moves nothing and is not an error"
        );
        assert_eq!(s.depth(), 1);
        assert!(matches!(s.current(), Screen::Home(_)));
    }

    #[test]
    fn back_always_terminates_at_home() {
        // The invariant the whole shell rests on: whatever route someone took in, enough
        // presses of back get them out. A screen you can enter and not leave is the bug
        // this type exists to prevent.
        let mut s = stack();
        for _ in 0..64 {
            s.push(Screen::Home(AttractScene::demo()));
        }
        let mut guard = 0;
        while s.pop() {
            guard += 1;
            assert!(guard < 1000, "back did not terminate");
        }
        assert_eq!(s.depth(), 1);
        assert!(matches!(s.current(), Screen::Home(_)));
    }

    #[test]
    fn going_home_unwinds_everything_at_once() {
        let mut s = stack();
        s.push(Screen::Home(AttractScene::demo()));
        s.push(Screen::Home(AttractScene::demo()));
        assert!(s.go_home());
        assert_eq!(s.depth(), 1);
        // Already home: nothing moved, still not an error.
        assert!(!s.go_home());
    }

    #[test]
    fn replacing_the_top_does_not_deepen_the_stack() {
        // The bug this prevents: a picker that refreshes by pushing needs one `back` per
        // refresh to escape, which on a busy network is unbounded.
        let mut s = stack();
        s.replace_top(Screen::Home(AttractScene::demo()));
        assert_eq!(s.depth(), 2, "the first replace pushes");
        for _ in 0..10 {
            s.replace_top(Screen::Home(AttractScene::demo()));
        }
        assert_eq!(s.depth(), 2, "the rest replace");
        assert!(s.pop());
        assert_eq!(s.depth(), 1, "one back is enough");
    }

    #[test]
    fn rebuilding_home_does_not_disturb_where_someone_is() {
        // Home is rebuilt whenever the receiver's state changes. If that yanked the
        // stack back, a host appearing on the LAN would close a picker mid-read.
        let mut s = stack();
        s.push(Screen::Home(AttractScene::demo()));
        let deep = s.depth();
        s.update_home(AttractScene::demo());
        assert_eq!(s.depth(), deep);
        assert!(s.can_pop());
    }
}
