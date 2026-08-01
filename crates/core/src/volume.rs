//! Volume, held as the thing the pipeline actually multiplies by.
//!
//! Two scales meet here and they look identical on the wire — both arrive as a number
//! between zero and one — while differing by about 10 dB across the middle of their
//! range. A sender sends a **position**: where a finger is on a slider, or a dBFS figure
//! read off one. The mixer needs an **amplitude**: what to multiply each sample by. For a
//! long time the position was handed straight to the multiply, which made a control whose
//! top half of travel did almost nothing and whose bottom collapsed (#85).
//!
//! So the conversion lives here, once, at the boundary: every `proto-*` crate parses its
//! own native scale into a [`Volume`] and nothing inward ever sees a position again. The
//! type is the enforcement — there is no way to build one from a bare `f32` without
//! saying which scale that `f32` is on.

/// How much of a slider's travel is spent getting from silence to full scale.
///
/// The conventional range for a media slider, and wider than the −30 dB AirPlay's own
/// protocol uses — chosen because the panel sits in a room with people in it, where the
/// bottom of the travel wants to be quiet rather than nearly gone.
///
/// Any value here is a taste decision; what matters is that it is made *once*, and that
/// every protocol inherits it rather than inventing its own.
const SLIDER_RANGE_DB: f32 = 60.0;

/// The dBFS figure at or below which a sender means silence rather than "very quiet".
///
/// AirPlay's own sentinel is −144, which is what this was picked to swallow.
const SILENCE_DBFS: f32 = -144.0;

/// An output volume.
///
/// Stored as amplitude — the number the mixer multiplies samples by — because that is the
/// one representation with a single unambiguous meaning. Build it with
/// [`Volume::from_position`] or [`Volume::from_dbfs`]; read it with [`Volume::amplitude`]
/// or, when a sender needs its slider told where it ended up, [`Volume::position`].
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Volume(f32);

impl Default for Volume {
    /// Full scale — what a session plays at until someone says otherwise.
    fn default() -> Self {
        Self::FULL
    }
}

impl Volume {
    /// Silence.
    pub const SILENT: Self = Self(0.0);

    /// Unity gain: samples pass through untouched.
    pub const FULL: Self = Self(1.0);

    /// From a slider position in `0.0..=1.0`.
    ///
    /// This is the scale almost every sender speaks — Cast's `SET_VOLUME` level, DLNA's
    /// `DesiredVolume` percent, AVRCP's seven-bit absolute volume, the Lounge
    /// `setVolume` — all of them a number describing where a finger is, none of them an
    /// amplitude.
    ///
    /// Zero is a hard silence rather than the −60 dB the curve would otherwise give, so
    /// that dragging a slider to the bottom means off and not "almost off".
    ///
    /// A non-finite input is silence: it is the only reading that cannot make things
    /// worse, and a NaN reaching the multiply would turn every sample into a
    /// silence-that-is-not-silence.
    #[must_use]
    pub fn from_position(position: f32) -> Self {
        if !position.is_finite() || position <= 0.0 {
            return Self::SILENT;
        }
        if position >= 1.0 {
            return Self::FULL;
        }
        Self(10.0f32.powf((position - 1.0) * SLIDER_RANGE_DB / 20.0))
    }

    /// From an exact dBFS figure.
    ///
    /// No curve is invented here — the sender did the perceptual part already and handed
    /// over the answer, so this is the textbook conversion and nothing else. AirPlay is
    /// the one protocol in this project that gets to use it.
    #[must_use]
    pub fn from_dbfs(db: f32) -> Self {
        if !db.is_finite() || db <= SILENCE_DBFS {
            return Self::SILENT;
        }
        if db >= 0.0 {
            return Self::FULL;
        }
        Self(10.0f32.powf(db / 20.0))
    }

    /// What to multiply each sample by.
    #[must_use]
    pub fn amplitude(self) -> f32 {
        self.0
    }

    /// Back to a slider position, for telling a sender where its control ended up.
    ///
    /// The exact inverse of [`Self::from_position`] over the whole range, which is what
    /// makes it safe to echo in a `RECEIVER_STATUS` or a `GetVolume`: a control point
    /// that reads back what it just set must not see its own slider move.
    #[must_use]
    pub fn position(self) -> f32 {
        if self.0 <= 0.0 {
            return 0.0;
        }
        (1.0 + self.0.log10() * 20.0 / SLIDER_RANGE_DB).clamp(0.0, 1.0)
    }

    /// As a dBFS figure, with [`Self::SILENT`] reported as the silence sentinel.
    #[must_use]
    pub fn dbfs(self) -> f32 {
        if self.0 <= 0.0 {
            return SILENCE_DBFS;
        }
        self.0.log10() * 20.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect #85 is about, stated as an assertion: the middle of a slider's travel
    /// must not be ~10 dB hot.
    #[test]
    fn a_half_travel_slider_is_not_half_amplitude() {
        let half = Volume::from_position(0.5);
        assert!(
            half.amplitude() < 0.1,
            "half travel produced {} — that is the linear multiply this issue is about",
            half.amplitude()
        );
        // −30 dB, which is half of a 60 dB range.
        assert!((half.dbfs() - -30.0).abs() < 1e-3, "got {} dB", half.dbfs());
    }

    #[test]
    fn the_ends_of_the_travel_are_exact() {
        assert_eq!(Volume::from_position(1.0), Volume::FULL);
        assert_eq!(Volume::from_position(0.0), Volume::SILENT);
        assert_eq!(Volume::from_position(0.0).amplitude(), 0.0);
        // A slider dragged to the bottom means off, not −60 dB.
        assert_eq!(Volume::from_position(-0.0).amplitude(), 0.0);
    }

    /// A control point that reads back what it just set must not see its slider move.
    #[test]
    fn position_round_trips_through_amplitude() {
        for step in 0..=100 {
            #[allow(clippy::cast_precision_loss)]
            let p = step as f32 / 100.0;
            let back = Volume::from_position(p).position();
            assert!((back - p).abs() < 1e-4, "{p} came back as {back}");
        }
    }

    /// AirPlay hands over a real dB figure, so nothing is guessed on that path.
    #[test]
    fn dbfs_is_the_textbook_conversion() {
        assert_eq!(Volume::from_dbfs(0.0), Volume::FULL);
        assert!((Volume::from_dbfs(-6.0).amplitude() - 0.501_187).abs() < 1e-5);
        assert!((Volume::from_dbfs(-20.0).amplitude() - 0.1).abs() < 1e-6);
        // The mute sentinel, and anything past it.
        assert_eq!(Volume::from_dbfs(-144.0), Volume::SILENT);
        assert_eq!(Volume::from_dbfs(-200.0), Volume::SILENT);
    }

    /// The iOS slider table from #85: what the sender means, and what we now produce.
    #[test]
    fn airplay_reproduces_the_senders_own_scale() {
        // −30 dBFS is the bottom of AirPlay's travel; the sender's slider is linear in
        // dB across it, so the amplitude we produce must match 10^(db/20) exactly.
        for (db, expected) in [
            (0.0f32, 1.0f32),
            (-7.5, 0.421_697),
            (-15.0, 0.177_828),
            (-22.5, 0.074_989),
            (-30.0, 0.031_623),
        ] {
            let got = Volume::from_dbfs(db).amplitude();
            assert!((got - expected).abs() < 1e-5, "{db} dB gave {got}");
        }
    }

    #[test]
    fn a_nonsense_reading_is_silence_not_a_nan_in_the_mixer() {
        assert_eq!(Volume::from_position(f32::NAN), Volume::SILENT);
        assert_eq!(Volume::from_dbfs(f32::NAN), Volume::SILENT);
        assert_eq!(Volume::from_position(f32::INFINITY), Volume::SILENT);
        assert!(Volume::from_position(2.0) == Volume::FULL);
    }

    #[test]
    fn the_curve_is_monotonic() {
        let mut last = -1.0;
        for step in 0..=1000 {
            #[allow(clippy::cast_precision_loss)]
            let p = step as f32 / 1000.0;
            let a = Volume::from_position(p).amplitude();
            assert!(a >= last, "position {p} went backwards: {a} after {last}");
            last = a;
        }
    }
}
