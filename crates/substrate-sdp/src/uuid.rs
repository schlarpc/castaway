//! Bluetooth UUIDs and the base-UUID expansion that makes short ones comparable.

use std::fmt;

/// The Bluetooth Base UUID: `00000000-0000-1000-8000-00805F9B34FB`.
///
/// Short UUIDs are shorthand for `xxxxxxxx` substituted into its first four bytes. Two
/// UUIDs that *look* different — `0x110B` and the full 128-bit form — are the same
/// identifier, so equality has to compare expanded forms or a peer that sends the long
/// spelling silently fails to match our records.
const BASE: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B, 0x34, 0xFB,
];

/// A Bluetooth UUID, stored expanded so comparison is always well defined.
///
/// The *encoded* width is remembered separately: records round-trip more compactly, and
/// some stacks are fussy about being answered in the width they asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid {
    bytes: [u8; 16],
    width: UuidWidth,
}

/// How wide a UUID was on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum UuidWidth {
    /// 16-bit short form.
    Short,
    /// 32-bit form.
    Medium,
    /// Full 128-bit form.
    Long,
}

impl Uuid {
    /// Build from a 16-bit short UUID.
    // Truncation is the point: these casts *are* the big-endian byte split.
    #[allow(clippy::cast_possible_truncation)]
    #[must_use]
    pub const fn short(value: u16) -> Self {
        let mut bytes = BASE;
        bytes[2] = (value >> 8) as u8;
        bytes[3] = value as u8;
        Self {
            bytes,
            width: UuidWidth::Short,
        }
    }

    /// Build from a 32-bit UUID.
    #[allow(clippy::cast_possible_truncation)]
    #[must_use]
    pub const fn medium(value: u32) -> Self {
        let mut bytes = BASE;
        bytes[0] = (value >> 24) as u8;
        bytes[1] = (value >> 16) as u8;
        bytes[2] = (value >> 8) as u8;
        bytes[3] = value as u8;
        Self {
            bytes,
            width: UuidWidth::Medium,
        }
    }

    /// Build from a full 128-bit UUID.
    #[must_use]
    pub const fn long(bytes: [u8; 16]) -> Self {
        Self {
            bytes,
            width: UuidWidth::Long,
        }
    }

    /// The expanded 128-bit form.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.bytes
    }

    /// The width this UUID was written in.
    #[must_use]
    pub const fn width(self) -> UuidWidth {
        self.width
    }

    /// The 16-bit short value, if this UUID is inside the base range.
    #[must_use]
    pub fn as_short(self) -> Option<u16> {
        let mut probe = self.bytes;
        probe[2] = 0;
        probe[3] = 0;
        (probe == BASE && self.bytes[0] == 0 && self.bytes[1] == 0)
            .then(|| u16::from_be_bytes([self.bytes[2], self.bytes[3]]))
    }

    // --- Protocol identifiers ---
    /// L2CAP, as it appears in a protocol descriptor list.
    pub const L2CAP: Self = Self::short(0x0100);
    /// AVDTP.
    pub const AVDTP: Self = Self::short(0x0019);
    /// AVCTP.
    pub const AVCTP: Self = Self::short(0x0017);
    /// OBEX — the transport cover art rides on.
    pub const OBEX: Self = Self::short(0x0008);

    // --- Service classes ---
    /// A2DP sink (the role we play).
    pub const AUDIO_SINK: Self = Self::short(0x110B);
    /// A2DP source (the role the phone plays).
    pub const AUDIO_SOURCE: Self = Self::short(0x110A);
    /// Advanced Audio Distribution profile.
    pub const ADVANCED_AUDIO_DISTRIBUTION: Self = Self::short(0x110D);
    /// AVRCP target — the end that owns the player. The phone is this.
    pub const AV_REMOTE_CONTROL_TARGET: Self = Self::short(0x110C);
    /// AVRCP, the profile identifier itself.
    pub const AV_REMOTE_CONTROL: Self = Self::short(0x110E);
    /// AVRCP controller — the end that drives the player. We are this.
    pub const AV_REMOTE_CONTROL_CONTROLLER: Self = Self::short(0x110F);
    /// The public browse root every published record belongs to.
    pub const PUBLIC_BROWSE_ROOT: Self = Self::short(0x1002);
    /// Cover art imaging responder — where the album art actually lives.
    pub const IMAGING_RESPONDER: Self = Self::short(0x111B);
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(short) = self.as_short() {
            return write!(f, "{short:#06x}");
        }
        let b = &self.bytes;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
            b[14], b[15]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_uuid_equals_its_expanded_long_form() {
        // The whole reason UUIDs are stored expanded. A phone that spells AudioSink out
        // in 128 bits must still match the record we published in 16.
        let short = Uuid::AUDIO_SINK;
        let long = Uuid::long(*short.as_bytes());
        assert_eq!(short.as_bytes(), long.as_bytes());
        assert_eq!(short.as_short(), Some(0x110B));
        assert_eq!(long.as_short(), Some(0x110B));
        // …but the width each was written in is remembered separately.
        assert_eq!(short.width(), UuidWidth::Short);
        assert_eq!(long.width(), UuidWidth::Long);
    }

    #[test]
    fn a_uuid_outside_the_base_range_has_no_short_form() {
        let custom = Uuid::long([0xAB; 16]);
        assert_eq!(custom.as_short(), None);
        assert_eq!(custom.to_string().len(), 36);
    }

    #[test]
    fn the_base_uuid_substitution_lands_in_the_first_four_bytes() {
        assert_eq!(
            Uuid::short(0x110B).as_bytes(),
            &[
                0x00, 0x00, 0x11, 0x0B, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5F, 0x9B,
                0x34, 0xFB
            ]
        );
        assert_eq!(
            Uuid::medium(0x0001_110B).as_bytes()[0..4],
            [0, 1, 0x11, 0x0B]
        );
    }

    #[test]
    fn short_uuids_render_as_hex() {
        assert_eq!(Uuid::AUDIO_SINK.to_string(), "0x110b");
    }
}
