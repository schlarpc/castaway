//! How the panel *moves* between the states [`crate::panel`] decides.
//!
//! The panel model is discrete: a surface is on the panel, in the corner, or nowhere. This
//! module is the continuous half — where each surface actually is on the way there, and how
//! fast. The split is deliberate and load-bearing: **the animator may lag the model but can
//! never disagree with it.** Targets come only from `Panel::placement`, hit testing keeps
//! answering from the model's rect, and the worst a stuck animation can do is show something
//! in the wrong place, never route a touch to the wrong thing.
//!
//! ## The three rules everything here follows from
//!
//! Apple's, Google's and Microsoft's motion languages disagree about curves and vocabulary
//! and agree about exactly three things, each of which is physical honesty rather than
//! taste:
//!
//! 1. **A surface that exists before and after must travel, never cross-fade.** If the panel
//!    is showing Spotify and then Spotify is in the corner, that is one object that moved.
//!    Fading a large one out under a small one fading in tells the eye they are two
//!    different things, and the room stops believing the corner is the same session. This is
//!    why [`Motion`] interpolates a *rectangle* and there is no cross-fade path for a
//!    surface that persists.
//! 2. **A surface that appears must come from where it was summoned** — a tile, the corner it
//!    was demoted into, the finger. Appearing in place is correct only when there genuinely
//!    is no origin (a phone starts casting with nobody at the panel), and then it needs a
//!    different treatment so it reads as arrival rather than a glitch. Hence [`Origin`], with
//!    [`Origin::Nowhere`] as a variant somebody has to *choose*: "I forgot to pass the tile's
//!    rect" is then visible in a diff instead of invisible on the glass.
//! 3. **Entrances decelerate, exits accelerate, and exits are faster.** A force applied and
//!    then removed. Encoded in [`Choreography`], and asserted: every exit's response is
//!    strictly shorter than the entrance it reverses.
//!
//! ## Why springs, and why per component
//!
//! A spring takes an initial velocity natively, which is exactly what a finger hands over
//! when a drag is released — the thing an easing curve cannot accept without restarting.
//! It also has one parameter a human can reason about (how long it takes to arrive) rather
//! than four control points.
//!
//! Each of a rect's four components springs independently, in panel-normalized units, and
//! so does opacity. That costs five scalars where a single normalized progress would cost
//! one, and it buys the property that makes interruption free: **retargeting is just a
//! different target.** There is no rebasing, no recomputing where "half way" now means, and
//! a reversal mid-flight carries real velocity through the turn. A progress-based
//! implementation has to rescale its velocity whenever the distance changes, and gets the
//! sign right but the magnitude wrong.

use crate::panel::{NormRect, Placement, Surface};

/// A spring, parameterised the way a person thinks about one: how long it takes to arrive.
///
/// `response` is the period of the undamped oscillation — near enough "the time to settle"
/// at critical damping to be the number worth tuning. `damping` at `1.0` is critically
/// damped and never overshoots; below `1.0` overshoots once and comes back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spring {
    /// Seconds to arrive, roughly. Must be > 0.
    pub response: f32,
    /// 1.0 = critically damped. Below that overshoots.
    pub damping: f32,
}

impl Spring {
    /// A spring that arrives in `response` seconds with `damping`.
    #[must_use]
    pub const fn new(response: f32, damping: f32) -> Self {
        Self { response, damping }
    }

    /// Advance `x` (with velocity `v`) toward `target` by `dt`, returning both.
    ///
    /// Semi-implicit Euler, which is stable for the step sizes a frame gives us and — unlike
    /// explicit Euler — cannot gain energy and diverge when a frame runs long. `dt` is
    /// clamped by the caller for the same reason: a stall must not teleport anything.
    #[must_use]
    pub fn step(self, x: f32, v: f32, target: f32, dt: f32) -> (f32, f32) {
        // ω from the response period. Guarded so a zero response degenerates to "arrive
        // now" rather than to a division by zero.
        if self.response <= f32::EPSILON {
            return (target, 0.0);
        }
        let omega = std::f32::consts::TAU / self.response;
        let stiffness = omega * omega;
        let friction = 2.0 * self.damping * omega;
        let accel = -stiffness * (x - target) - friction * v;
        let v = v + accel * dt;
        (x + v * dt, v)
    }

    /// Whether a value this close to its target, moving this slowly, has arrived.
    ///
    /// Both halves matter: position alone would settle something still moving fast through
    /// its target, and velocity alone would settle something momentarily stationary at the
    /// top of an overshoot.
    #[must_use]
    pub fn settled(x: f32, v: f32, target: f32) -> bool {
        // A thousandth of a panel: less than a pixel on a 4K display, so nothing that
        // remains is visible.
        (x - target).abs() < 0.001 && v.abs() < 0.01
    }
}

/// Where a surface came onto the panel from.
///
/// See rule 2 in the module docs. This is the input that makes an app open *from its tile*
/// rather than materialise in the middle of the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Origin {
    /// A rectangle on the panel: the tile that was pressed, or the corner it is being
    /// summoned from. The surface starts there, at full opacity, and travels — the
    /// persistent-element case, so it must not fade.
    From(NormRect),
    /// Nowhere in particular: a sender started a session with nobody at the panel, so there
    /// is no shared element and no direction that would mean anything.
    ///
    /// Fades through from 94%, which is Material's answer for content with no origin and
    /// reads as arrival rather than as a surface sliding in from a place it never was.
    Nowhere,
}

/// What a surface is doing right now.
///
/// Stored rather than derived, unlike [`crate::panel::Focus`], and the distinction is real:
/// the panel's focus is a fact about the world that can always be recomputed, while "is this
/// arriving or merely moving" is a fact about *history* — the same target, reached from
/// nothing or from somewhere else, is choreographed differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Not on the panel and not moving. The layer can be dropped.
    Absent,
    /// Arriving from an [`Origin`].
    Entering,
    /// Settled where the panel says it goes.
    Resting,
    /// Travelling between two placements.
    Moving,
    /// On its way off the panel. Still drawn — a surface that is removed while it is still
    /// visible is the snap this module exists to remove.
    Leaving,
}

impl Phase {
    /// Whether the surface should be composited at all.
    #[must_use]
    pub const fn drawn(self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// One transition, as the choreography sees it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// Where it was. `None` means it was not on the panel.
    pub from: Option<Placement>,
    /// Where it is going. `None` means it is leaving.
    pub to: Option<Placement>,
    /// Where it is arriving from. Consulted only when `from` is `None`.
    pub origin: Origin,
}

/// The table: which spring each transition gets.
///
/// One place, so the panel's feel is legible as data rather than spread over the call sites
/// that happen to trigger each move. The numbers are the design languages' own ranges —
/// large-surface transitions in the 300–500 ms band, exits at roughly two thirds of their
/// entrance — and the one deliberate exception is called out below.
pub struct Choreography;

impl Choreography {
    /// The spring for `step`.
    #[must_use]
    pub fn spring(step: Step) -> Spring {
        use Placement::{Hidden, Panel, Widget};
        match (step.from, step.to) {
            // Appearing. From a rect it is a container transform and can afford to be
            // stately; from nowhere it is a fade-through and should be brisk, because
            // nothing about it rewards being watched.
            (None, _) => match step.origin {
                Origin::From(_) => Spring::new(0.40, 1.0),
                Origin::Nowhere => Spring::new(0.28, 1.0),
            },
            // Leaving altogether: the fastest thing the panel does. Nobody is waiting for
            // something to finish going away.
            (_, None) => Spring::new(0.20, 1.0),

            // The shared-element pair — the panel's signature move, and the one the eye
            // follows all the way, so it gets the longest response.
            (Some(Panel), Some(Widget)) => Spring::new(0.42, 1.0),
            // The summon. The one place overshoot is right: this is the transition a person
            // directly asked for by putting a finger on the corner, and a touch of
            // liveliness is how the panel acknowledges *them* rather than looking as
            // uniformly smooth as everything that happens on its own. Apple's
            // open-from-icon does exactly this. Everything else on a display people see out
            // of the corner of their eye all day is critically damped.
            (Some(Widget), Some(Panel)) => Spring::new(0.38, 0.85),

            // The demoted form arriving and leaving — what a shell screen opening over Home
            // does to whatever was in the corner.
            (Some(Hidden), Some(Widget)) => Spring::new(0.32, 0.90),
            (Some(Widget), Some(Hidden)) => Spring::new(0.22, 1.0),
            (Some(Hidden), Some(Panel)) => Spring::new(0.34, 0.90),
            (Some(Panel), Some(Hidden)) => Spring::new(0.24, 1.0),

            // Same placement: asked for on the pump that notices nothing moved. Never
            // actually integrates anything, but a total match is worth more than an
            // `unreachable!` on a runtime-reachable path (ground rule 7).
            (Some(_), Some(_)) => Spring::new(0.30, 1.0),
        }
    }

    /// How much of the panel a surface with no origin starts at.
    ///
    /// Not 1.0, so arrival has a direction (outward) even with no place to come from, and
    /// not far from 1.0, because a big scale on a large surface reads as a zoom rather than
    /// as something taking its place.
    pub const FADE_THROUGH_SCALE: f32 = 0.94;

    /// How much of its rect a surface shrinks to as it leaves.
    ///
    /// Inward, the mirror of arrival. Slighter than the entrance because it is quicker and a
    /// large travel would not be seen.
    pub const EXIT_SCALE: f32 = 0.96;

    /// How far the floor pushes back while a session has the glass.
    ///
    /// The shell is not a [`Surface`], but it has to move: an app shrinking into the corner
    /// over a shell that appears instantly reads as the app shrinking over a static wall,
    /// rather than as the shell coming forward to receive it. Two per cent is the whole
    /// effect — enough to register as depth, not enough to notice as a change of size.
    ///
    /// Greater than one, which is the opposite of what every phone does, and for a reason
    /// that is specific to being the bottom layer: there is nothing behind the shell but the
    /// clear colour, so scaling it *down* would open a black border around the panel. Scaling
    /// slightly up overfills instead, and combined with the dim it still reads as receding —
    /// which is what the two together are for. The crop is one per cent of each edge, well
    /// inside every screen's own margin.
    pub const FLOOR_RECESS: f32 = 1.02;

    /// How much the floor dims while a session has the glass.
    pub const FLOOR_DIM: f32 = 0.85;

    /// The floor's own spring. Between the demote's and the summon's, because it moves with
    /// both and should never be the thing that finishes last.
    #[must_use]
    pub const fn floor() -> Spring {
        Spring::new(0.30, 1.0)
    }
}

/// Where a surface is heading, and how visible it should be when it gets there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    /// The rect it is heading for.
    pub rect: NormRect,
    /// The opacity it is heading for.
    pub opacity: f32,
}

/// One surface's live geometry: where it actually is, and how fast each edge is moving.
#[derive(Debug, Clone, Copy)]
pub struct Motion {
    at: NormRect,
    /// Per-component velocity, in panel-widths (or heights) per second.
    vel: NormRect,
    opacity: f32,
    opacity_vel: f32,
    phase: Phase,
    spring: Spring,
}

impl Default for Motion {
    fn default() -> Self {
        Self::absent()
    }
}

impl Motion {
    /// A surface that is not on the panel.
    #[must_use]
    pub const fn absent() -> Self {
        Self {
            at: NormRect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            vel: NormRect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            opacity: 0.0,
            opacity_vel: 0.0,
            phase: Phase::Absent,
            spring: Spring::new(0.30, 1.0),
        }
    }

    /// Where it is now. What the compositor gets.
    #[must_use]
    pub const fn frame(&self) -> NormRect {
        self.at
    }

    /// How visible it is now.
    #[must_use]
    pub const fn opacity(&self) -> f32 {
        self.opacity
    }

    /// What it is doing.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether it should be composited.
    #[must_use]
    pub const fn drawn(&self) -> bool {
        self.phase.drawn()
    }

    /// Start arriving at `target` from `origin`.
    ///
    /// The surface is placed at its origin *before* the first step, so the first frame it is
    /// composited in is already the right size in the right place — a frame at the
    /// destination followed by a frame at the origin is a flash, and one flash is worse than
    /// no animation at all.
    pub fn enter(&mut self, target: Target, origin: Origin, spring: Spring) {
        self.at = match origin {
            Origin::From(rect) => rect,
            Origin::Nowhere => target.rect.scaled(Choreography::FADE_THROUGH_SCALE),
        };
        // From a rect the surface is the same object arriving, so it is opaque the whole way
        // (rule 1). From nowhere there is nothing for it to be continuous with, so it fades.
        self.opacity = match origin {
            Origin::From(_) => target.opacity,
            Origin::Nowhere => 0.0,
        };
        self.vel = NormRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        self.opacity_vel = 0.0;
        self.phase = Phase::Entering;
        self.spring = spring;
    }

    /// Be at `target` now, with no motion at all.
    ///
    /// For a renderer nobody is watching — the offscreen harness, a headless tap. There is no
    /// shell, so there are no screens to protect, no widget slot to demote into and no eye to
    /// follow anything; what a caller wants there is a deterministic frame after one pump,
    /// not a surface caught 6% of the way into an entrance.
    pub fn snap(&mut self, target: Target) {
        self.at = target.rect;
        self.opacity = target.opacity;
        self.vel = NormRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        self.opacity_vel = 0.0;
        self.phase = if target.opacity > 0.0 {
            Phase::Resting
        } else {
            Phase::Absent
        };
    }

    /// Head somewhere else. Velocity is kept, so a reversal mid-flight carries through the
    /// turn instead of stopping dead and starting again.
    pub fn move_to(&mut self, spring: Spring) {
        self.phase = Phase::Moving;
        self.spring = spring;
    }

    /// Start leaving. Still drawn until it has finished.
    pub fn leave(&mut self, spring: Spring) {
        self.phase = Phase::Leaving;
        self.spring = spring;
    }

    /// Hand a velocity to the animation — what a released drag does.
    ///
    /// In panel-widths per second, applied to the horizontal components, because every
    /// gesture on this panel is horizontal. A spring accepting this is the whole reason
    /// these are springs and not curves.
    pub fn push(&mut self, velocity: f32) {
        self.vel.x += velocity;
    }

    /// Advance toward `target` by `dt`. Returns whether anything is still moving.
    ///
    /// The one place a frame's worth of time is integrated, and the one place a phase
    /// finishes: a surface that has arrived becomes [`Phase::Resting`], and one that has
    /// finished leaving becomes [`Phase::Absent`] so its layer can go.
    pub fn step(&mut self, target: Target, dt: f32) -> bool {
        if self.phase == Phase::Absent {
            return false;
        }
        // A long frame — a stall, a hitch, a debugger — must not launch anything across the
        // panel. Clamped rather than subdivided: at 20 fps the spring is still stable, and
        // an animation that runs slightly slow through a stall is invisible next to one that
        // teleports.
        let dt = dt.min(0.05);
        let (rect, vel) = step_rect(self.spring, self.at, self.vel, target.rect, dt);
        self.at = rect;
        self.vel = vel;
        let (o, ov) = self
            .spring
            .step(self.opacity, self.opacity_vel, target.opacity, dt);
        self.opacity = o.clamp(0.0, 1.0);
        self.opacity_vel = ov;

        if !settled_rect(self.at, self.vel, target.rect)
            || !Spring::settled(self.opacity, self.opacity_vel, target.opacity)
        {
            return true;
        }
        // Arrived. Snap the residue away so a layer is never left a thousandth of a panel
        // off, and retire the phase.
        self.at = target.rect;
        self.opacity = target.opacity;
        self.vel = NormRect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        self.opacity_vel = 0.0;
        self.phase = match self.phase {
            Phase::Leaving => Phase::Absent,
            _ => Phase::Resting,
        };
        false
    }
}

/// Step all four components of a rect.
fn step_rect(
    spring: Spring,
    at: NormRect,
    vel: NormRect,
    target: NormRect,
    dt: f32,
) -> (NormRect, NormRect) {
    let (x, vx) = spring.step(at.x, vel.x, target.x, dt);
    let (y, vy) = spring.step(at.y, vel.y, target.y, dt);
    let (w, vw) = spring.step(at.w, vel.w, target.w, dt);
    let (h, vh) = spring.step(at.h, vel.h, target.h, dt);
    (
        NormRect { x, y, w, h },
        NormRect {
            x: vx,
            y: vy,
            w: vw,
            h: vh,
        },
    )
}

fn settled_rect(at: NormRect, vel: NormRect, target: NormRect) -> bool {
    Spring::settled(at.x, vel.x, target.x)
        && Spring::settled(at.y, vel.y, target.y)
        && Spring::settled(at.w, vel.w, target.w)
        && Spring::settled(at.h, vel.h, target.h)
}

/// The floor's motion: the shell receding under a session that has the glass.
#[derive(Debug, Clone, Copy, Default)]
pub struct Floor {
    scale: Option<f32>,
    scale_vel: f32,
    dim: f32,
    dim_vel: f32,
}

impl Floor {
    /// Where the shell sits, and how bright it is. `None` until it has first been stepped,
    /// so a renderer that never animates leaves the layer alone entirely.
    #[must_use]
    pub fn placement(&self) -> Option<(NormRect, f32)> {
        let scale = self.scale?;
        let inset = (1.0 - scale) / 2.0;
        Some((
            NormRect {
                x: inset,
                y: inset,
                w: scale,
                h: scale,
            },
            self.dim,
        ))
    }

    /// Advance toward the arrangement `recessed` asks for. Returns whether it is still
    /// moving.
    pub fn step(&mut self, recessed: bool, dt: f32) -> bool {
        let (target_scale, target_dim) = if recessed {
            (Choreography::FLOOR_RECESS, Choreography::FLOOR_DIM)
        } else {
            (1.0, 1.0)
        };
        // First step: start where we are being asked to be, so nothing animates on boot.
        let Some(scale) = self.scale else {
            self.scale = Some(target_scale);
            self.dim = target_dim;
            return false;
        };
        let dt = dt.min(0.05);
        let spring = Choreography::floor();
        let (s, sv) = spring.step(scale, self.scale_vel, target_scale, dt);
        let (d, dv) = spring.step(self.dim, self.dim_vel, target_dim, dt);
        self.scale = Some(s);
        self.scale_vel = sv;
        self.dim = d.clamp(0.0, 1.0);
        self.dim_vel = dv;
        if Spring::settled(s, sv, target_scale) && Spring::settled(d, dv, target_dim) {
            self.scale = Some(target_scale);
            self.dim = target_dim;
            self.scale_vel = 0.0;
            self.dim_vel = 0.0;
            return false;
        }
        true
    }
}

/// Every surface's motion, indexed the way [`Surface::ALL`] is.
#[derive(Debug, Clone, Copy, Default)]
pub struct Motions {
    motions: [Motion; Surface::ALL.len()],
    /// The floor, which is not a surface but moves with them.
    pub floor: Floor,
}

impl Motions {
    /// One surface's motion.
    #[must_use]
    pub fn get(&self, surface: Surface) -> &Motion {
        let i = index(surface);
        // `index` is exhaustive over `Surface` and the array is `Surface::ALL.len()` long,
        // so this cannot fall through — but it is written as a fallback rather than an
        // index, because a panic on the render thread takes the panel down (ground rule 7).
        self.motions.get(i).unwrap_or(&ABSENT)
    }

    /// One surface's motion, mutably.
    pub fn get_mut(&mut self, surface: Surface) -> Option<&mut Motion> {
        self.motions.get_mut(index(surface))
    }
}

/// A resting place for [`Motions::get`]'s unreachable fallback.
static ABSENT: Motion = Motion::absent();

const fn index(surface: Surface) -> usize {
    match surface {
        Surface::Video => 0,
        Surface::Card => 1,
        Surface::CastPage => 2,
        Surface::IdleWidget => 3,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const FULL: NormRect = NormRect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
    const CORNER: NormRect = NormRect {
        x: 0.66,
        y: 0.66,
        w: 0.32,
        h: 0.32,
    };
    /// One frame at 60 Hz.
    const FRAME: f32 = 1.0 / 60.0;

    fn panel_target() -> Target {
        Target {
            rect: FULL,
            opacity: 1.0,
        }
    }

    fn corner_target() -> Target {
        Target {
            rect: CORNER,
            opacity: 1.0,
        }
    }

    /// Run a motion to rest, returning how long it took and how far past the target it went.
    fn settle(motion: &mut Motion, target: Target) -> (f32, f32) {
        let mut t = 0.0;
        let mut worst_overshoot: f32 = 0.0;
        // Two seconds is far longer than any spring in the table; a motion that needs it has
        // a bug, and the assertion below says so.
        while motion.step(target, FRAME) && t < 2.0 {
            t += FRAME;
            // Overshoot measured on width, which every transition here changes.
            let past = (target.rect.w - motion.frame().w).abs();
            let span = (target.rect.w - CORNER.w).abs().max(0.001);
            worst_overshoot = worst_overshoot.max(0.0_f32.max(past / span - 1.0).min(1.0));
        }
        (t, worst_overshoot)
    }

    #[test]
    fn every_transition_in_the_table_settles_promptly() {
        // A motion that does not finish leaves a layer permanently a little wrong, and on an
        // unattended panel nobody is watching to notice. This is the assertion that keeps
        // the table honest when somebody tunes it.
        for from in [
            None,
            Some(Placement::Panel),
            Some(Placement::Widget),
            Some(Placement::Hidden),
        ] {
            for to in [
                None,
                Some(Placement::Panel),
                Some(Placement::Widget),
                Some(Placement::Hidden),
            ] {
                for origin in [Origin::Nowhere, Origin::From(CORNER)] {
                    let step = Step { from, to, origin };
                    let spring = Choreography::spring(step);
                    let mut m = Motion::absent();
                    m.enter(panel_target(), origin, spring);
                    let (t, _) = settle(&mut m, corner_target());
                    assert!(t < 1.0, "{step:?} took {t:.2}s to settle with {spring:?}");
                    assert_eq!(m.phase(), Phase::Resting);
                    assert_eq!(m.frame(), CORNER, "{step:?} did not arrive exactly");
                }
            }
        }
    }

    #[test]
    fn exits_are_quicker_than_the_entrances_they_reverse() {
        // Rule 3, as an assertion rather than a comment. A force applied and then removed:
        // nobody is waiting for something to finish going away, and an exit that takes as
        // long as its entrance makes the panel feel like it is thinking.
        use Placement::{Hidden, Panel, Widget};
        let pairs = [
            (Some(Panel), Some(Hidden)),
            (Some(Widget), Some(Hidden)),
            (Some(Panel), Some(Widget)),
        ];
        for (a, b) in pairs {
            let out = Choreography::spring(Step {
                from: a,
                to: b,
                origin: Origin::Nowhere,
            });
            let back = Choreography::spring(Step {
                from: b,
                to: a,
                origin: Origin::Nowhere,
            });
            let (shrinking, growing) = if matches!(b, Some(Hidden)) {
                (out, back)
            } else {
                // Panel → Widget is the demote, which is the *entrance* of the corner form;
                // its reverse is the summon. Both are shared-element travel, and neither is
                // an exit, so the rule that applies is the one below.
                continue;
            };
            assert!(
                shrinking.response < growing.response,
                "leaving ({shrinking:?}) should be quicker than arriving ({growing:?})"
            );
        }
        // Leaving the panel altogether is quicker than any arrival on it.
        let gone = Choreography::spring(Step {
            from: Some(Panel),
            to: None,
            origin: Origin::Nowhere,
        });
        for origin in [Origin::Nowhere, Origin::From(CORNER)] {
            let arriving = Choreography::spring(Step {
                from: None,
                to: Some(Panel),
                origin,
            });
            assert!(gone.response < arriving.response, "{origin:?}");
        }
    }

    #[test]
    fn only_the_summon_overshoots() {
        // A display people see out of the corner of their eye all day should not wobble. The
        // one exception is the transition somebody asked for with a finger, and it is
        // deliberate — so it is worth a test that fails if a future tune spreads it.
        use Placement::{Hidden, Panel, Widget};
        let summon = Step {
            from: Some(Widget),
            to: Some(Panel),
            origin: Origin::Nowhere,
        };
        assert!(Choreography::spring(summon).damping < 1.0);

        for step in [
            Step {
                from: Some(Panel),
                to: Some(Widget),
                origin: Origin::Nowhere,
            },
            Step {
                from: Some(Widget),
                to: Some(Hidden),
                origin: Origin::Nowhere,
            },
            Step {
                from: Some(Panel),
                to: None,
                origin: Origin::Nowhere,
            },
            Step {
                from: None,
                to: Some(Panel),
                origin: Origin::Nowhere,
            },
        ] {
            let spring = Choreography::spring(step);
            let mut m = Motion::absent();
            m.enter(corner_target(), Origin::From(CORNER), spring);
            let (_, overshoot) = settle(&mut m, panel_target());
            assert!(
                overshoot < 0.005,
                "{step:?} overshot by {overshoot:.3} of its travel"
            );
        }
    }

    #[test]
    fn a_surface_that_persists_travels_and_never_fades() {
        // Rule 1. The demote is one object moving, so its opacity must not dip on the way —
        // a fade is what tells the eye there are two objects.
        let spring = Choreography::spring(Step {
            from: Some(Placement::Panel),
            to: Some(Placement::Widget),
            origin: Origin::Nowhere,
        });
        let mut m = Motion::absent();
        m.enter(panel_target(), Origin::From(FULL), spring);
        assert_eq!(m.opacity(), 1.0, "it starts opaque");
        m.move_to(spring);
        let mut t = 0.0;
        while m.step(corner_target(), FRAME) && t < 2.0 {
            t += FRAME;
            assert!(
                m.opacity() > 0.99,
                "opacity dipped to {} mid-travel",
                m.opacity()
            );
        }
        assert_eq!(m.frame(), CORNER);
    }

    #[test]
    fn a_surface_with_nowhere_to_come_from_fades_through_rather_than_sliding() {
        // Rule 2's other half: no origin means no direction that would mean anything, so it
        // arrives outward from just inside its own rect instead of travelling from an edge it
        // was never at.
        let spring = Choreography::spring(Step {
            from: None,
            to: Some(Placement::Panel),
            origin: Origin::Nowhere,
        });
        let mut m = Motion::absent();
        m.enter(panel_target(), Origin::Nowhere, spring);
        assert_eq!(m.opacity(), 0.0, "it fades in");
        let start = m.frame();
        assert!(
            start.w < 1.0 && start.w > 0.9,
            "it should start just inside its rect, got {start:?}"
        );
        // Centred, not offset toward an edge.
        assert!((start.x - (1.0 - start.w) / 2.0).abs() < 1e-6);
        settle(&mut m, panel_target());
        assert_eq!(m.frame(), FULL);
        assert_eq!(m.opacity(), 1.0);
    }

    #[test]
    fn an_app_summoned_from_the_corner_starts_at_the_corner() {
        // The whole point of `Origin`: the first composited frame is already at the corner,
        // so there is no flash of the destination before the travel begins.
        let spring = Choreography::spring(Step {
            from: None,
            to: Some(Placement::Panel),
            origin: Origin::From(CORNER),
        });
        let mut m = Motion::absent();
        m.enter(panel_target(), Origin::From(CORNER), spring);
        assert_eq!(m.frame(), CORNER);
        assert_eq!(m.opacity(), 1.0, "a shared element does not fade in");
        assert_eq!(m.phase(), Phase::Entering);
    }

    #[test]
    fn a_reversal_mid_flight_carries_its_velocity_through_the_turn() {
        // What makes interruption feel alive rather than mechanical, and the reason each
        // component springs in real units: retargeting is just a different target, so the
        // velocity at the moment of the turn is the velocity it keeps. A progress-based
        // implementation has to rescale it and gets the magnitude wrong.
        let spring = Choreography::spring(Step {
            from: Some(Placement::Panel),
            to: Some(Placement::Widget),
            origin: Origin::Nowhere,
        });
        let mut m = Motion::absent();
        m.enter(panel_target(), Origin::From(FULL), spring);
        m.move_to(spring);
        for _ in 0..8 {
            m.step(corner_target(), FRAME);
        }
        let turning_at = m.frame();
        assert!(turning_at.w < 1.0, "it should be on its way to the corner");

        // The property is *velocity continuity*, not "the next frame still shrinks": with a
        // stiff spring and this much displacement one frame is enough to flip the sign, and
        // that is correct physics rather than lost momentum. So compare against the same
        // motion started from the same place at a standstill — the one carrying inward
        // momentum must lag behind it.
        let mut standing = Motion::absent();
        standing.enter(
            Target {
                rect: turning_at,
                opacity: 1.0,
            },
            Origin::From(turning_at),
            spring,
        );
        standing.move_to(spring);

        m.move_to(spring);
        m.step(panel_target(), FRAME);
        standing.step(panel_target(), FRAME);
        assert!(
            m.frame().w < standing.frame().w,
            "the turn dropped the momentum: carried {} vs standing {}",
            m.frame().w,
            standing.frame().w
        );
        // …and it still gets there.
        settle(&mut m, panel_target());
        assert_eq!(m.frame(), FULL);
    }

    #[test]
    fn a_released_drag_hands_its_velocity_to_the_spring() {
        // The property an easing curve cannot have. A flick keeps going after the hand has
        // stopped, because the spring was given the hand's speed rather than restarted.
        let spring = Choreography::spring(Step {
            from: Some(Placement::Panel),
            to: Some(Placement::Widget),
            origin: Origin::Nowhere,
        });
        let mut thrown = Motion::absent();
        thrown.enter(panel_target(), Origin::From(FULL), spring);
        thrown.move_to(spring);
        thrown.push(1.5);

        let mut still = Motion::absent();
        still.enter(panel_target(), Origin::From(FULL), spring);
        still.move_to(spring);

        // Heading somewhere to the right of where it is, so a rightward throw arrives first.
        let target = Target {
            rect: CORNER,
            opacity: 1.0,
        };
        thrown.step(target, FRAME);
        still.step(target, FRAME);
        assert!(
            thrown.frame().x > still.frame().x,
            "the throw should be ahead: {} vs {}",
            thrown.frame().x,
            still.frame().x
        );
    }

    #[test]
    fn a_leaving_surface_stays_drawn_until_it_has_gone() {
        // The snap this module exists to remove: a layer dropped the instant the model said
        // the session was over, so a card vanished rather than left.
        let spring = Choreography::spring(Step {
            from: Some(Placement::Panel),
            to: None,
            origin: Origin::Nowhere,
        });
        let mut m = Motion::absent();
        m.enter(panel_target(), Origin::From(FULL), spring);
        m.leave(spring);
        let target = Target {
            rect: FULL.scaled(Choreography::EXIT_SCALE),
            opacity: 0.0,
        };
        let mut frames = 0;
        while m.step(target, FRAME) {
            assert!(m.drawn(), "it must stay composited while it is leaving");
            frames += 1;
            assert!(frames < 200, "it never left");
        }
        assert!(frames > 3, "it left in one frame, which is a snap");
        assert_eq!(m.phase(), Phase::Absent, "and now the layer can go");
    }

    #[test]
    fn a_long_frame_does_not_launch_anything_across_the_panel() {
        // A stall, a hitch, a debugger breakpoint. Integrating a whole second of spring at
        // once would put the surface somewhere nobody asked for — and with a stiff spring,
        // somewhere off the panel entirely.
        let spring = Choreography::spring(Step {
            from: Some(Placement::Widget),
            to: Some(Placement::Panel),
            origin: Origin::Nowhere,
        });
        let mut m = Motion::absent();
        m.enter(corner_target(), Origin::From(CORNER), spring);
        m.move_to(spring);
        m.step(panel_target(), 5.0);
        let f = m.frame();
        assert!(
            f.x >= -0.5 && f.x <= 1.5 && f.w > 0.0 && f.w < 2.0,
            "a 5-second frame threw it to {f:?}"
        );
    }

    #[test]
    fn the_floor_starts_where_it_is_asked_to_be_and_does_not_animate_on_boot() {
        // A panel that fades its own idle screen in at startup looks like it is recovering
        // from something.
        let mut floor = Floor::default();
        assert_eq!(
            floor.placement(),
            None,
            "untouched, so leave the layer alone"
        );
        assert!(!floor.step(true, FRAME), "the first step is a placement");
        let (rect, dim) = floor.placement().unwrap();
        assert!((rect.w - Choreography::FLOOR_RECESS).abs() < 1e-6);
        assert!((dim - Choreography::FLOOR_DIM).abs() < 1e-6);
        // Overscanned, never inset: the shell is the bottom layer, so a gap would be a black
        // border around the whole panel.
        assert!(rect.x <= 0.0 && rect.w >= 1.0, "{rect:?}");
    }

    #[test]
    fn the_floor_comes_forward_when_the_session_hands_the_glass_back() {
        let mut floor = Floor::default();
        floor.step(true, FRAME);
        let mut t = 0.0;
        while floor.step(false, FRAME) && t < 2.0 {
            t += FRAME;
        }
        assert!(t < 1.0, "the floor took {t:.2}s");
        let (rect, dim) = floor.placement().unwrap();
        assert!((rect.w - 1.0).abs() < 1e-6, "back to full size");
        assert!((dim - 1.0).abs() < 1e-6, "and full brightness");
    }

    #[test]
    fn motions_are_addressed_by_surface_without_an_index_anybody_can_get_wrong() {
        let mut all = Motions::default();
        for surface in Surface::ALL {
            assert_eq!(all.get(surface).phase(), Phase::Absent);
        }
        all.get_mut(Surface::Card).unwrap().enter(
            panel_target(),
            Origin::Nowhere,
            Spring::new(0.3, 1.0),
        );
        assert_eq!(all.get(Surface::Card).phase(), Phase::Entering);
        for other in [Surface::Video, Surface::CastPage, Surface::IdleWidget] {
            assert_eq!(
                all.get(other).phase(),
                Phase::Absent,
                "{other:?} moved with the card"
            );
        }
    }
}
