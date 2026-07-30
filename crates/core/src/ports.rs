//! Where per-session media sockets bind: a declared port range instead of "whatever
//! the OS hands out".
//!
//! AirPlay and Cast negotiate their media planes per session — the `SETUP`/ANSWER
//! names ports the receiver already holds. Bound at port 0 those sockets land on
//! OS-assigned ephemeral ports, which no firewall rule can name in advance: on a
//! firewalled box the control plane looks perfect and the media never arrives, which
//! from the room reads as "mirroring doesn't work". Binding them from a range declared
//! here lets a deployment open exactly the ports this process may listen on, ahead of
//! time — the contract the network-surface registry (`crates/app/src/surface.rs`)
//! documents and the NixOS module's firewall consumes.

use std::fmt;
use std::num::NonZeroU16;
use std::ops::RangeInclusive;

/// An inclusive port range `first..=last`, with `first` nonzero.
///
/// Zero is excluded by construction: port 0 means "let the OS pick", which is exactly
/// the behaviour a declared range exists to replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    first: NonZeroU16,
    last: NonZeroU16,
}

/// Why a pair of numbers is not a [`PortRange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PortRangeError {
    /// The range starts at 0.
    #[error(
        "a port range cannot start at 0: port 0 means \"let the OS pick\", which is \
         the un-firewallable behaviour a declared range exists to replace"
    )]
    ZeroStart,
    /// `first` is greater than `last`.
    #[error("port range {first}..={last} is backwards")]
    Backwards {
        /// The claimed first port.
        first: u16,
        /// The claimed last port.
        last: u16,
    },
}

impl PortRange {
    /// The inclusive range `first..=last`.
    ///
    /// # Errors
    /// [`PortRangeError`] if `first` is 0 or the bounds are backwards.
    pub const fn new(first: u16, last: u16) -> Result<Self, PortRangeError> {
        let (Some(first_nz), Some(last_nz)) = (NonZeroU16::new(first), NonZeroU16::new(last))
        else {
            return Err(PortRangeError::ZeroStart);
        };
        if first > last {
            return Err(PortRangeError::Backwards { first, last });
        }
        Ok(Self {
            first: first_nz,
            last: last_nz,
        })
    }

    /// The lowest port in the range.
    #[must_use]
    pub const fn first(self) -> u16 {
        self.first.get()
    }

    /// The highest port in the range.
    #[must_use]
    pub const fn last(self) -> u16 {
        self.last.get()
    }

    /// How many ports the range holds (at least 1 by construction).
    #[must_use]
    pub const fn count(self) -> u16 {
        self.last.get() - self.first.get() + 1
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

/// Where a protocol's per-session media sockets bind.
///
/// Threaded into the adapters as a required constructor argument rather than a
/// defaulted builder, so a new call site has to *say* which policy it wants — the
/// silent way back to unfirewallable ephemeral ports should not typecheck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPorts {
    /// OS-assigned ephemeral ports. No firewall rule can cover these ahead of time;
    /// for tests and deliberately unfirewalled runs only.
    Ephemeral,
    /// Bind each media socket to a free port inside this range, lowest first.
    Range(PortRange),
}

impl MediaPorts {
    /// The ports a binder should try, in order, taking the first that binds.
    ///
    /// `Ephemeral` yields the single port 0 (the OS picks); a range yields each of its
    /// ports. Both shapes are one `RangeInclusive`, so binders need exactly one loop.
    #[must_use]
    pub const fn candidates(self) -> RangeInclusive<u16> {
        match self {
            Self::Ephemeral => 0..=0,
            Self::Range(range) => range.first()..=range.last(),
        }
    }
}

impl fmt::Display for MediaPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ephemeral => f.write_str("ephemeral"),
            Self::Range(range) => range.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_range_knows_its_bounds_and_size() {
        let range = PortRange::new(41000, 41031).unwrap();
        assert_eq!(range.first(), 41000);
        assert_eq!(range.last(), 41031);
        assert_eq!(range.count(), 32);
        assert_eq!(range.to_string(), "41000-41031");
    }

    #[test]
    fn a_single_port_is_a_valid_range() {
        let range = PortRange::new(7011, 7011).unwrap();
        assert_eq!(range.count(), 1);
        assert_eq!(range.candidates_len_via_media(), 1);
    }

    #[test]
    fn zero_and_backwards_are_rejected() {
        assert_eq!(PortRange::new(0, 10), Err(PortRangeError::ZeroStart));
        // A range *ending* at 0 can only also start at 0.
        assert_eq!(PortRange::new(0, 0), Err(PortRangeError::ZeroStart));
        assert_eq!(
            PortRange::new(20, 10),
            Err(PortRangeError::Backwards {
                first: 20,
                last: 10
            })
        );
    }

    #[test]
    fn ephemeral_candidates_is_exactly_port_zero() {
        assert_eq!(MediaPorts::Ephemeral.candidates(), 0..=0);
    }

    #[test]
    fn range_candidates_walk_the_whole_range() {
        let policy = MediaPorts::Range(PortRange::new(5, 8).unwrap());
        let all: Vec<u16> = policy.candidates().collect();
        assert_eq!(all, vec![5, 6, 7, 8]);
    }

    impl PortRange {
        /// Count via the same path binders use, so the two cannot disagree.
        fn candidates_len_via_media(self) -> usize {
            MediaPorts::Range(self).candidates().count()
        }
    }
}
