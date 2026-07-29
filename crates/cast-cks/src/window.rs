//! The 2-day validity window a CKS credential is scoped to.

use crate::CksError;

/// Seconds in one CKS window. The whole scheme is built on a fixed 2-day
/// schedule: the peer certificate is re-issued on it, and one precomputed
/// signature covers exactly one window.
pub const WINDOW_SECS: i64 = 2 * 86_400;

/// A half-open validity window `[start, end)`, in Unix seconds.
///
/// Both bounds always land on `00:00:00Z`, because a window is a whole number of
/// days from a fixed epoch. That matters: the bounds are written into the peer
/// certificate as `UTCTime`, and the precomputed signature covers those bytes, so
/// a window that is off by a second produces a certificate no signature matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    start_unix: i64,
    end_unix: i64,
}

impl Window {
    /// Build a window from explicit bounds.
    ///
    /// # Errors
    /// [`CksError::Window`] if the bounds are not ordered.
    pub fn new(start_unix: i64, end_unix: i64) -> Result<Self, CksError> {
        if end_unix <= start_unix {
            return Err(CksError::Window(format!(
                "window bounds are not ordered: {start_unix} .. {end_unix}"
            )));
        }
        Ok(Self {
            start_unix,
            end_unix,
        })
    }

    /// Window start, Unix seconds.
    #[must_use]
    pub const fn start_unix(self) -> i64 {
        self.start_unix
    }

    /// Window end, Unix seconds (exclusive).
    #[must_use]
    pub const fn end_unix(self) -> i64 {
        self.end_unix
    }

    /// Whether `unix` falls inside the window.
    ///
    /// Half-open on purpose: window *n*'s `end` is window *n+1*'s `start`, and an
    /// instant on the boundary belongs to the later one — the same way the
    /// reference implementation's table lookup partitions the timeline.
    #[must_use]
    pub const fn contains(self, unix: i64) -> bool {
        unix >= self.start_unix && unix < self.end_unix
    }

    /// The window's bounds rendered as the two `UTCTime` values that go into the
    /// peer certificate: `YYMMDDhhmmssZ`.
    ///
    /// # Errors
    /// [`CksError::Window`] if a bound falls outside the years `UTCTime` can
    /// represent (1950–2049), which no window in any shipped table does.
    pub fn utc_times(self) -> Result<(UtcTime, UtcTime), CksError> {
        Ok((
            UtcTime::from_unix(self.start_unix)?,
            UtcTime::from_unix(self.end_unix)?,
        ))
    }
}

/// The 13-byte ASN.1 `UTCTime` body `YYMMDDhhmmssZ`, as X.509 spells it.
///
/// A newtype rather than a `[u8; 13]` so it cannot be confused with any other
/// fixed-size field while it is being spliced into certificate DER.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcTime([u8; Self::LEN]);

impl UtcTime {
    /// Length of a `UTCTime` body in the form X.509 requires (seconds always
    /// present, `Z` suffix).
    pub const LEN: usize = 13;

    /// Render a Unix timestamp as `UTCTime`.
    ///
    /// # Errors
    /// [`CksError::Window`] if the timestamp is outside 1950–2049, which
    /// `UTCTime`'s two-digit year cannot represent unambiguously.
    pub fn from_unix(unix: i64) -> Result<Self, CksError> {
        let dt = time::OffsetDateTime::from_unix_timestamp(unix)
            .map_err(|e| CksError::Window(format!("timestamp {unix} is not a valid time: {e}")))?;
        let year = dt.year();
        if !(1950..=2049).contains(&year) {
            return Err(CksError::Window(format!(
                "year {year} cannot be encoded as UTCTime"
            )));
        }
        let mut out = [0u8; Self::LEN];
        let fields = [
            year.rem_euclid(100),
            i32::from(u8::from(dt.month())),
            i32::from(dt.day()),
            i32::from(dt.hour()),
            i32::from(dt.minute()),
            i32::from(dt.second()),
        ];
        for (i, value) in fields.into_iter().enumerate() {
            // Each field is two digits by construction of the ranges above.
            out[i * 2] = b'0' + u8::try_from(value / 10).unwrap_or(0);
            out[i * 2 + 1] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        }
        out[12] = b'Z';
        Ok(Self(out))
    }

    /// The encoded bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn utc_time_matches_the_reference_rendering() {
        // Window 0 of the shipped table: 2023-01-01 .. 2023-01-03. These are the
        // exact byte strings that appear in the checked-in peer template, so if
        // this rendering drifts the reissued certificate stops matching.
        assert_eq!(
            UtcTime::from_unix(1_672_531_200).unwrap().as_bytes(),
            b"230101000000Z"
        );
        assert_eq!(
            UtcTime::from_unix(1_672_704_000).unwrap().as_bytes(),
            b"230103000000Z"
        );
    }

    #[test]
    fn utc_time_pads_single_digit_fields() {
        // 2026-07-28T00:00:00Z — the window live at the time this was written.
        assert_eq!(
            UtcTime::from_unix(1_785_196_800).unwrap().as_bytes(),
            b"260728000000Z"
        );
    }

    #[test]
    fn utc_time_rejects_years_it_cannot_encode() {
        // 2050-01-01: the first instant UTCTime's two-digit year makes ambiguous.
        assert!(UtcTime::from_unix(2_524_608_000).is_err());
    }

    #[test]
    fn windows_are_half_open() {
        let w = Window::new(100, 200).unwrap();
        assert!(w.contains(100));
        assert!(w.contains(199));
        assert!(
            !w.contains(200),
            "the upper bound belongs to the next window"
        );
        assert!(!w.contains(99));
    }

    #[test]
    fn unordered_bounds_are_rejected() {
        assert!(Window::new(200, 100).is_err());
        assert!(Window::new(100, 100).is_err());
    }
}
