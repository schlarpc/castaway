//! The music visualiser (#15): what the panel does while it is only making sound.
//!
//! An audio session has no pixels of its own, so the now-playing card is the whole of what
//! a two-metre screen shows for it — artwork, a title, and otherwise a still image for the
//! length of an album. This is the part that moves.
//!
//! ## Calm on purpose
//!
//! The issue's whole brief was one line: *projectM is too overwhelming*. So this is not a
//! Milkdrop preset. It is one row of soft bars along the bottom of the card, in the
//! panel's own accent colour, at low contrast — something you notice is alive when you
//! glance at it and never something that competes with the artwork above it. Every choice
//! below that looks conservative is that brief being taken literally:
//!
//! - **Slow release.** Bars fall four times slower than they rise ([`ATTACK`],
//!   [`RELEASE`]). A visualiser that tracks the envelope exactly flickers; one that falls
//!   slowly reads as breathing.
//! - **A floor, not a gate.** Below [`SILENCE`] the bars go to nothing rather than
//!   amplifying the noise floor into a light show while a track is between songs.
//! - **Thirty frames a second, not sixty.** [`FRAME_INTERVAL`]. Nothing here moves fast
//!   enough for the difference to be visible, and the panel spends most of its life idle
//!   (#59) — a visualiser that pinned a core at display rate for the length of an album
//!   would be the single most expensive thing on the box.
//!
//! ## Where the work happens, and where it does not
//!
//! [`Analyzer`] is a [`MixTap`], so it is fed by the mixer thread (#111) — the same
//! samples the speakers got, which is what makes the bars agree with what is audible
//! rather than with one source's idea of itself.
//!
//! That thread does **no analysis**. It copies a downmix into a ring and returns; the
//! Goertzel bank runs on the render thread, inside [`Analyzer::bands`], where a frame
//! budget already exists. The audio path is paced against the device and a decode thread
//! stalls behind it (ground rule 4), so it is the wrong place to put ten thousand
//! multiplies, however cheap they look written down.
//!
//! ## Goertzel rather than an FFT
//!
//! Sixteen bands is far fewer than a 1024-point FFT gives, and all sixteen are wanted, so
//! a filter bank is the smaller answer: one Goertzel per band is `N` multiply-adds against
//! an FFT's `N log N` plus a dependency and a scratch buffer. It is also much easier to be
//! sure of — "a 1 kHz tone lights the 1 kHz band and nothing else" is a test, and it is
//! the test below.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::PipelineError;
use crate::mixer::MixTap;
use crate::text::Rgba;

/// How many bars.
///
/// Sixteen across the width of a card reads as a row of light rather than as a chart,
/// which is the distinction the brief was about. It is also enough that a bass line and a
/// hi-hat land in visibly different places.
pub const BANDS: usize = 16;

/// Samples the analysis looks at.
///
/// ~21 ms at the mix rate. Long enough to resolve the lowest band ([`LOW_HZ`] has a period
/// of 25 ms, so this is just under one cycle — the bottom bar is the least trustworthy and
/// is also the one nobody reads a number off), short enough that the bars answer a beat
/// rather than the bar before it.
const WINDOW: usize = 1024;

/// The bottom of the range.
const LOW_HZ: f32 = 40.0;

/// The top. Above this is air and tape hiss, and a bar for it is a bar that never moves.
const HIGH_HZ: f32 = 16_000.0;

/// How long a bar takes to rise most of the way to a new peak.
const ATTACK: Duration = Duration::from_millis(60);

/// …and to fall. Four times the attack: this is the whole difference between "breathing"
/// and "flickering".
const RELEASE: Duration = Duration::from_millis(400);

/// Below this RMS the panel is quiet and the bars are still.
///
/// A floor rather than a noise gate on each band: the failure it exists to prevent is the
/// gap between two tracks turning the noise floor into a full-scale display, which the
/// per-band normalisation would otherwise do very enthusiastically.
const SILENCE: f32 = 1e-4;

/// How often the visualiser wants a frame.
///
/// See the module docs: nothing here moves fast enough for 60 Hz to be visible, and the
/// alternative is the most expensive thing on an otherwise idle panel.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(33);

/// Band magnitudes, each in `0.0..=1.0`.
///
/// A newtype rather than a bare array because the range is the contract: the renderer
/// multiplies by a height and a colour, and a value outside it is a bar off the top of the
/// texture or a negative one. Nothing outside this module can build one that breaks it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bands([f32; BANDS]);

impl Default for Bands {
    fn default() -> Self {
        Self::SILENT
    }
}

impl Bands {
    /// Every bar down.
    pub const SILENT: Self = Self([0.0; BANDS]);

    /// The magnitudes, low to high.
    #[must_use]
    pub const fn levels(&self) -> &[f32; BANDS] {
        &self.0
    }

    /// Whether anything is moving. A card with a silent visualiser draws no layer at all,
    /// rather than an invisible one that still costs an upload every frame.
    #[must_use]
    pub fn is_silent(&self) -> bool {
        self.0.iter().all(|v| *v < 1e-3)
    }
}

/// The centre frequency of each band, log-spaced from [`LOW_HZ`] to [`HIGH_HZ`].
///
/// Log rather than linear because hearing is: linear spacing puts thirteen of sixteen bars
/// above 4 kHz, where music mostly is not, and the display sits almost still while the
/// bass does all the work.
fn centre_hz(band: usize) -> f32 {
    let t = band as f32 / (BANDS - 1) as f32;
    LOW_HZ * (HIGH_HZ / LOW_HZ).powf(t)
}

/// One Goertzel evaluation: the magnitude of `hz` in `window`.
///
/// `window` is expected to be [`WINDOW`] samples of mono at `rate`.
fn goertzel(window: &[f32], rate: f32, hz: f32) -> f32 {
    let n = window.len();
    if n == 0 || rate <= 0.0 {
        return 0.0;
    }
    let k = hz * n as f32 / rate;
    let omega = 2.0 * std::f32::consts::PI * k / n as f32;
    let coeff = 2.0 * omega.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for (i, sample) in window.iter().enumerate() {
        // Hann, applied here rather than in a second pass over the buffer: without a
        // window the band edges leak into each other badly enough that a pure tone lights
        // half the display, which is exactly what the test below would catch.
        let w = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos());
        let s0 = sample.mul_add(w, coeff.mul_add(s1, -s2));
        s2 = s1;
        s1 = s0;
    }
    let power = s2.mul_add(s2, s1.mul_add(s1, -(coeff * s1 * s2)));
    // Scaled by the window length so the answer is an amplitude rather than a number that
    // depends on how much audio was looked at.
    power.max(0.0).sqrt() * 2.0 / n as f32
}

/// One exponential step toward `target`, at the rate `tau` implies over `dt`.
///
/// Framed as a time constant rather than a per-frame coefficient because the render loop's
/// `dt` is not fixed — it sleeps to a deadline (#59) — and a coefficient chosen for 60 Hz
/// makes the bars fall at a different speed whenever anything else on the panel changes
/// what the loop is doing.
fn approach(current: f32, target: f32, dt: Duration, tau: Duration) -> f32 {
    let tau = tau.as_secs_f32().max(1e-6);
    let alpha = 1.0 - (-dt.as_secs_f32() / tau).exp();
    current + (target - current) * alpha.clamp(0.0, 1.0)
}

/// The rolling window the mixer fills and the renderer reads.
#[derive(Debug)]
struct Ring {
    /// Mono, at the mix rate. Always [`WINDOW`] long; oldest first.
    samples: Box<[f32; WINDOW]>,
    /// Where the next sample goes.
    head: usize,
    /// Loudest absolute sample seen since the last read, for the silence floor and the
    /// normalisation. Reset by [`Analyzer::bands`] so it describes *recent* audio rather
    /// than the loudest moment of the session.
    peak: f32,
}

impl Ring {
    fn push(&mut self, sample: f32) {
        self.samples[self.head] = sample;
        self.head = (self.head + 1) % WINDOW;
        self.peak = self.peak.max(sample.abs());
    }

    /// The window in time order, oldest first.
    fn ordered(&self) -> Vec<f32> {
        let (tail, head) = self.samples.split_at(self.head);
        head.iter().chain(tail.iter()).copied().collect()
    }
}

/// The panel's one audio analyser.
///
/// Attach it to the mixer with [`crate::mixer::AudioMixer::add_tap`]; read it from the
/// render thread with [`Analyzer::bands`].
#[derive(Debug)]
pub struct Analyzer {
    ring: Mutex<Ring>,
    /// The smoothed bars, and when they were last stepped. Only the render thread touches
    /// this, but it is behind the same lock discipline as everything else here rather than
    /// relying on that staying true.
    state: Mutex<Smoothed>,
}

#[derive(Debug)]
struct Smoothed {
    bands: Bands,
    last: Option<Instant>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    /// A silent analyser.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ring: Mutex::new(Ring {
                samples: Box::new([0.0; WINDOW]),
                head: 0,
                peak: 0.0,
            }),
            state: Mutex::new(Smoothed {
                bands: Bands::SILENT,
                last: None,
            }),
        }
    }

    /// The bars as of `now`, stepped toward what the audio is doing.
    ///
    /// Call once per frame from the render thread: this is where the analysis actually
    /// happens, and calling it twice for one frame steps the smoothing twice.
    pub fn bands(&self, now: Instant) -> Bands {
        let (window, peak) = {
            let Ok(mut ring) = self.ring.lock() else {
                return Bands::SILENT;
            };
            let peak = std::mem::take(&mut ring.peak);
            (ring.ordered(), peak)
        };

        let target = if peak < SILENCE {
            // Quiet, and deliberately not "normalise whatever is there": between two
            // tracks the loudest thing in the ring is the noise floor, and dividing by it
            // is how a silent panel ends up with a full-scale display.
            [0.0; BANDS]
        } else {
            let rate = crate::mixer::RATE as f32;
            let mut raw = [0.0f32; BANDS];
            for (band, slot) in raw.iter_mut().enumerate() {
                *slot = goertzel(&window, rate, centre_hz(band));
            }
            // Normalised against the window's own peak rather than full scale, so a
            // quietly-played track still moves. The panel's volume is applied before this
            // point (#111), which is the honest behaviour: bars that ignored the volume
            // would keep dancing after somebody muted the room.
            let loudest = raw.iter().copied().fold(0.0f32, f32::max).max(1e-6);
            for slot in &mut raw {
                // Square root, because loudness is not amplitude: a linear bar spends
                // almost all its time near the bottom and lurches.
                *slot = (*slot / loudest).clamp(0.0, 1.0).sqrt();
            }
            raw
        };

        let Ok(mut state) = self.state.lock() else {
            return Bands::SILENT;
        };
        let dt = state
            .last
            .map_or(FRAME_INTERVAL, |last| now.saturating_duration_since(last));
        state.last = Some(now);
        for (current, want) in state.bands.0.iter_mut().zip(target) {
            let tau = if want > *current { ATTACK } else { RELEASE };
            *current = approach(*current, want, dt, tau).clamp(0.0, 1.0);
        }
        state.bands
    }
}

impl MixTap for Analyzer {
    fn mixed(&self, _at: Instant, stereo: &[f32]) {
        let Ok(mut ring) = self.ring.lock() else {
            return;
        };
        // A copy and nothing else. See the module docs: this is the mixer thread, and the
        // decode threads behind it stall on how long it takes.
        for frame in stereo.chunks_exact(usize::from(crate::mixer::CHANNELS)) {
            ring.push((frame[0] + frame[1]) * 0.5);
        }
    }
}

/// The height of the visualiser strip, as a fraction of the panel it sits on.
pub const STRIP_HEIGHT_FRACTION: f32 = 0.14;

/// Where the strip sits on a `width` × `height` surface, in pixels: `(x, y, w, h)`.
///
/// One definition, like [`crate::transport::placement`], and for the same reason: geometry
/// written out at each consumer is the kind of arithmetic that stays wrong by an offset
/// for months because everything still *looks* about right.
///
/// It borrows the transport strip's width so the two line up when both are on the panel,
/// and sits directly on top of it when there is one. `over_transport` rather than reading
/// the card, because the caller already knows and this stays pure.
#[must_use]
pub fn placement(width: u32, height: u32, over_transport: bool) -> (f32, f32, f32, f32) {
    let (w, h) = (width as f32, height as f32);
    let sw = w * crate::transport::STRIP_WIDTH_FRACTION;
    let sh = h * STRIP_HEIGHT_FRACTION;
    let reserved = if over_transport {
        h * crate::transport::STRIP_HEIGHT_FRACTION
    } else {
        0.0
    };
    ((w - sw) / 2.0, h - reserved - sh, sw, sh)
}

/// Draw the bars at `width` × `height`, returning RGBA8 over transparency.
///
/// The texture is the strip, not the panel: the caller places it. Transparent everywhere a
/// bar is not, because it is composited *over* the card rather than into it — the card is
/// re-rendered only when its metadata changes, and drawing this into it would mean
/// decoding the cover art sixteen hundred times a minute.
///
/// # Errors
/// Never, currently. The `Result` matches every other surface in this crate, all of which
/// can fail on a font load; a change here that needs one should not be a signature change
/// at every call site.
pub fn render(bands: &Bands, width: u32, height: u32) -> Result<Vec<u8>, PipelineError> {
    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
    let (w, h) = (width.max(1) as f32, height.max(1) as f32);

    // Bars occupy a little over half their slot, so the gaps read as gaps at a glance from
    // across a room rather than as a solid block with seams in it.
    let slot = w / BANDS as f32;
    let bar = slot * 0.55;
    let radius = (bar / 2.0).min(h * 0.08);

    for (band, level) in bands.levels().iter().enumerate() {
        // Every bar keeps a visible stub at zero. A row that vanishes entirely between
        // tracks looks like a fault; a row of dots looks like it is waiting.
        let filled = (h - radius * 2.0) * level.clamp(0.0, 1.0);
        let bar_h = (radius * 2.0 + filled).min(h);
        let cx = slot.mul_add(band as f32, slot / 2.0);
        let cy = h - bar_h / 2.0;
        // Brighter with height, so the loud bars carry the eye and the quiet ones stay out
        // of the way — the low-contrast half of "calm".
        let alpha = 0.28f32.mul_add(*level, 0.10);
        let colour = fade(crate::theme::ACCENT, alpha);
        crate::shape::fill_sdf(
            &mut buf,
            width,
            height,
            crate::shape::Rect {
                x: cx - bar / 2.0 - 2.0,
                y: cy - bar_h / 2.0 - 2.0,
                w: bar + 4.0,
                h: bar_h + 4.0,
            },
            colour,
            |px, py| crate::shape::sd_round_box(px, py, cx, cy, bar / 2.0, bar_h / 2.0, radius),
        );
    }
    Ok(buf)
}

/// `colour` at `alpha` of its own opacity.
fn fade(colour: Rgba, alpha: f32) -> Rgba {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let a = (f32::from(colour[3]) * alpha.clamp(0.0, 1.0)).round() as u8;
    [colour[0], colour[1], colour[2], a]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// `seconds` of a sine at `hz`, as interleaved stereo at the mix rate.
    fn tone(hz: f32, seconds: f32) -> Vec<f32> {
        let rate = crate::mixer::RATE as f32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let frames = (rate * seconds) as usize;
        (0..frames)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * hz * i as f32 / rate).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    /// Which band's centre is nearest `hz`.
    fn band_of(hz: f32) -> usize {
        (0..BANDS)
            .min_by(|a, b| {
                let da = (centre_hz(*a) - hz).abs();
                let db = (centre_hz(*b) - hz).abs();
                da.total_cmp(&db)
            })
            .unwrap()
    }

    /// Feed the analyser and read it enough times that the smoothing has settled.
    fn settle(analyzer: &Analyzer, samples: &[f32]) -> Bands {
        let mut now = Instant::now();
        let mut bands = Bands::SILENT;
        for _ in 0..40 {
            analyzer.mixed(now, samples);
            now += FRAME_INTERVAL;
            bands = analyzer.bands(now);
        }
        bands
    }

    #[test]
    fn the_bands_are_log_spaced_across_the_audible_range() {
        // Linear spacing would put thirteen of sixteen bars above 4 kHz, where music
        // mostly is not, and the display would sit still while the bass did all the work.
        assert!((centre_hz(0) - LOW_HZ).abs() < 1.0);
        assert!((centre_hz(BANDS - 1) - HIGH_HZ).abs() < 1.0);
        // Each step is the same *ratio*, which is what log-spaced means.
        let first = centre_hz(1) / centre_hz(0);
        let last = centre_hz(BANDS - 1) / centre_hz(BANDS - 2);
        assert!(
            (first - last).abs() < 0.01,
            "steps of {first} and {last} are not the same ratio"
        );
    }

    #[test]
    fn a_pure_tone_lights_its_own_band_and_leaves_the_others_alone() {
        // The whole justification for a Goertzel bank over an FFT is that this is easy to
        // be sure of. It is also what catches a missing window function: without the Hann
        // above, a single tone leaks across half the display.
        let rate = crate::mixer::RATE as f32;
        // Tones at the bands' own centres rather than at round numbers. A round number
        // lands between two centres — the spacing is a ratio, so at the top of the range
        // one band covers a couple of kilohertz — and this would then be measuring how far
        // 4 kHz is from 3862 Hz rather than how well the bank separates.
        for target in [4usize, 8, 12] {
            let hz = centre_hz(target);
            let window: Vec<f32> = (0..WINDOW)
                .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate).sin() * 0.5)
                .collect();
            let magnitudes: Vec<f32> = (0..BANDS)
                .map(|b| goertzel(&window, rate, centre_hz(b)))
                .collect();
            let peak = magnitudes[target];
            assert!(peak > 0.01, "{hz} Hz produced nothing in its own band");
            for (band, magnitude) in magnitudes.iter().enumerate() {
                if band.abs_diff(target) <= 1 {
                    continue;
                }
                assert!(
                    *magnitude < peak * 0.35,
                    "{hz} Hz leaked into band {band} at {magnitude} against a peak of {peak}"
                );
            }
        }
    }

    #[test]
    fn silence_leaves_every_bar_down_rather_than_amplifying_the_noise_floor() {
        // The failure the floor exists for: between two tracks the loudest thing in the
        // ring is the noise floor, and per-band normalisation divides by it. Without the
        // floor a silent panel puts on a light show.
        let analyzer = Analyzer::new();
        let hiss: Vec<f32> = (0..4800)
            .map(|i| if i % 7 == 0 { 1e-6 } else { -1e-6 })
            .collect();
        let bands = settle(&analyzer, &hiss);
        assert!(
            bands.is_silent(),
            "near-silence produced {:?}",
            bands.levels()
        );
    }

    #[test]
    fn music_moves_the_bar_it_belongs_to() {
        let analyzer = Analyzer::new();
        let bands = settle(&analyzer, &tone(1_000.0, 0.05));
        let target = band_of(1_000.0);
        let level = bands.levels()[target];
        assert!(
            level > 0.5,
            "a 1 kHz tone left its own bar at {level}: {:?}",
            bands.levels()
        );
        assert!(!bands.is_silent());
    }

    #[test]
    fn bars_fall_slower_than_they_rise() {
        // The difference between breathing and flickering, and the one property of this
        // module somebody is most likely to "simplify" into a single coefficient.
        let dt = FRAME_INTERVAL;
        let rising = approach(0.0, 1.0, dt, ATTACK);
        let falling = 1.0 - approach(1.0, 0.0, dt, RELEASE);
        assert!(
            rising > falling * 2.0,
            "a bar rises {rising} and falls {falling} per frame; they should not be close"
        );
        // And neither overshoots, whatever the frame took — the loop sleeps to a deadline,
        // so `dt` after an idle sleep is minutes rather than milliseconds.
        let huge = approach(0.0, 1.0, Duration::from_secs(60), ATTACK);
        assert!((0.0..=1.0).contains(&huge), "overshot to {huge}");
    }

    #[test]
    fn the_analysis_is_not_done_on_the_mixer_thread() {
        // Ground rule 4, as an assertion rather than a comment: the tap is fed by the
        // thread that paces every decoder on the box, so what it does per call has to stay
        // a copy. If the Goertzel bank ever moves into `mixed`, this stops holding.
        let analyzer = Analyzer::new();
        let block = tone(1_000.0, 0.01);
        let started = Instant::now();
        for _ in 0..200 {
            analyzer.mixed(started, &block);
        }
        let taken = started.elapsed();
        assert!(
            taken < Duration::from_millis(200),
            "two seconds of audio took {taken:?} to tap; that is analysis, not a copy"
        );
    }

    #[test]
    fn a_silent_strip_still_draws_a_stub_for_every_bar() {
        // A row that vanishes entirely between tracks looks like a fault. It is also the
        // shape of the "is_silent, so draw nothing" decision one layer up: the caller drops
        // the layer, rather than this drawing an empty texture nobody can see.
        let strip = render(&Bands::SILENT, 320, 48).unwrap();
        assert_eq!(strip.len(), 320 * 48 * 4);
        let lit = strip.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(lit > 0, "a silent strip drew absolutely nothing");
        // …and it is a stub rather than a full-height row.
        let tall = render(&Bands([1.0; BANDS]), 320, 48).unwrap();
        let lit_tall = tall.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(
            lit_tall > lit * 3,
            "a full-scale strip ({lit_tall} px) should be much more than a silent one ({lit} px)"
        );
    }

    #[test]
    fn the_strip_sits_on_the_transport_rather_than_under_it() {
        // Both are drawn against the bottom of the panel, so getting this wrong puts the
        // bars behind the buttons — where they are invisible and, worse, where they would
        // be the thing a finger reaching for pause lands on if they ever occluded.
        let (_, y_alone, _, h) = placement(1920, 1080, false);
        let (_, y_over, _, _) = placement(1920, 1080, true);
        assert!(
            (y_alone + h - 1080.0).abs() < 0.5,
            "with no strip the bars sit on the bottom edge"
        );
        let strip = 1080.0 * crate::transport::STRIP_HEIGHT_FRACTION;
        assert!(
            (y_over + h - (1080.0 - strip)).abs() < 0.5,
            "with a strip they sit directly on top of it"
        );
        // …and they share its width, so the two read as one stack rather than two things.
        let (x, _, w, _) = placement(1920, 1080, true);
        let (tx, _, tw, _) = crate::transport::placement(1920, 1080);
        assert!((x - tx).abs() < 0.5 && (w - tw).abs() < 0.5);
    }

    #[test]
    fn a_bar_stays_inside_the_texture_at_full_scale() {
        // The reason `Bands` is a newtype: the renderer multiplies these by a height, and
        // the buffer has no bounds check that would say so.
        let strip = render(&Bands([1.0; BANDS]), 64, 24).unwrap();
        assert_eq!(strip.len(), 64 * 24 * 4);
    }
}
