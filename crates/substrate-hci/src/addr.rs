//! Bluetooth device addresses, and the endianness trap that comes with them.

use std::fmt;
use std::str::FromStr;

use crate::error::HciError;

/// A 48-bit Bluetooth device address.
///
/// **The wire is little-endian and the human spelling is big-endian.** `AA:BB:CC:DD:EE:FF`
/// travels as `FF EE DD CC BB AA`. Getting this backwards produces an address that looks
/// plausible, pages nothing, and costs an afternoon — so the byte order lives in
/// [`BdAddr::from_wire`]/[`BdAddr::to_wire`] and nowhere else, and the internal
/// representation is fixed as big-endian (display order) so there is exactly one place to
/// be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct BdAddr([u8; 6]);

impl BdAddr {
    /// The all-zero address, which the spec treats as "no device".
    pub const ZERO: Self = Self([0; 6]);

    /// Build from bytes in display order (most significant first).
    #[must_use]
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    /// Parse from HCI wire order (least significant octet first).
    #[must_use]
    pub fn from_wire(mut wire: [u8; 6]) -> Self {
        wire.reverse();
        Self(wire)
    }

    /// Serialise to HCI wire order (least significant octet first).
    #[must_use]
    pub fn to_wire(self) -> [u8; 6] {
        let mut out = self.0;
        out.reverse();
        out
    }

    /// The octets in display order.
    #[must_use]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    /// Whether this is the all-zero "no device" address.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.0 == [0; 6]
    }
}

impl fmt::Display for BdAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02X}:{b:02X}:{c:02X}:{d:02X}:{e:02X}:{g:02X}")
    }
}

impl FromStr for BdAddr {
    type Err = HciError;

    /// Parse the conventional `AA:BB:CC:DD:EE:FF` spelling (display order).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut octets = [0u8; 6];
        let mut seen = 0usize;
        for (slot, part) in octets.iter_mut().zip(s.split(':')) {
            *slot = u8::from_str_radix(part, 16).map_err(|_| HciError::InvalidField {
                field: "bd_addr octet",
                value: 0,
            })?;
            seen += 1;
        }
        if seen != 6 || s.split(':').count() != 6 {
            return Err(HciError::InvalidField {
                field: "bd_addr",
                value: 0,
            });
        }
        Ok(Self(octets))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_wire_is_reversed_from_the_display_spelling() {
        // The single fact this newtype exists to protect. A controller reporting a
        // connection from FF:EE:DD:CC:BB:AA puts AA first on the wire.
        let addr: BdAddr = "AA:BB:CC:DD:EE:FF".parse().unwrap();
        assert_eq!(addr.octets(), [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(addr.to_wire(), [0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA]);
        assert_eq!(BdAddr::from_wire(addr.to_wire()), addr);
    }

    #[test]
    fn display_round_trips_through_parse() {
        let addr = BdAddr::new([0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13]);
        assert_eq!(addr.to_string(), "00:1A:7D:DA:71:13");
        assert_eq!(addr.to_string().parse::<BdAddr>().unwrap(), addr);
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        assert!("not an address".parse::<BdAddr>().is_err());
        assert!("AA:BB:CC:DD:EE".parse::<BdAddr>().is_err());
        assert!("AA:BB:CC:DD:EE:FF:00".parse::<BdAddr>().is_err());
        assert!("ZZ:BB:CC:DD:EE:FF".parse::<BdAddr>().is_err());
    }

    #[test]
    fn zero_is_no_device() {
        assert!(BdAddr::ZERO.is_zero());
        assert!(!BdAddr::new([0, 0, 0, 0, 0, 1]).is_zero());
    }
}
