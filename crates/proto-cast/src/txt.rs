//! The `_googlecast._tcp` TXT record's vocabulary, as types rather than string literals.
//!
//! Every value here is read by a sender *before it connects to anything*, which makes
//! this the highest-consequence, lowest-visibility surface in the protocol: a wrong or
//! missing key does not produce an error, it produces a device that is absent from a
//! picker with nothing anywhere saying why. Three separate bugs of exactly that shape
//! are on the record — a missing `st` (openscreen refuses the record outright), a missing
//! `nf` (Play Services refuses it), and no sub-types at all (#226).
//!
//! So the keys are modelled. What this buys, concretely: `ca` is built from named
//! capabilities instead of the literal `5`, so a claim this receiver cannot honour has to
//! be *written* rather than mistyped; `st` is an enum, so "0" cannot silently become the
//! wrong state; and the one field whose meaning is genuinely not established — `nf` — says
//! so in its own type rather than in a comment beside a magic number.
//!
//! Sources are named per item. Where the meaning came off a decompiler or a capture
//! rather than a specification, the doc comment says which, because that is the
//! difference between a fact and a working assumption.

use std::fmt;

/// One capability bit in the mDNS `ca` bitmask.
///
/// **Not** the same vocabulary as `GET_DEVICE_INFO`'s `deviceCapabilities` (see
/// [`crate::messages::DEFAULT_DEVICE_CAPABILITIES`]) — two different fields, two
/// different numbering schemes, and conflating them is how a receiver ends up describing
/// itself two ways.
///
/// The five below are the Cast SDK's published `CastDevice.CAPABILITY_*` constants, and
/// Play Services' own bitmask tester was read against them: it reduces to
/// `(ca & bit) == bit` over exactly these plus three higher bits (see
/// [`OBSERVED_UNMODELLED`]). Independently confirmed against real hardware on the
/// development LAN — a Google Home Mini advertises `AUDIO_OUT` and *not* `VIDEO_OUT`,
/// and a Cast Group advertises that same value plus `MULTIZONE_GROUP` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum DeviceCapability {
    /// The device drives a screen.
    VideoOut = 1,
    /// The device accepts video *from* somewhere (a camera input).
    VideoIn = 2,
    /// The device drives speakers.
    AudioOut = 4,
    /// The device accepts audio in (a microphone).
    AudioIn = 8,
    /// A development build, which senders treat differently.
    DevMode = 16,
    /// The instance is a speaker *group* rather than one device.
    MultizoneGroup = 32,
}

impl DeviceCapability {
    /// The bit this capability occupies.
    #[must_use]
    pub const fn bit(self) -> u32 {
        self as u32
    }
}

/// Bits real devices set that this receiver does not model, and why they are left alone.
///
/// Read off a live capture: a Google Home Mini advertises `ca=199172`, which is
/// `AUDIO_OUT | 0x200 | 0x800 | 0x10000 | 0x20000`. Play Services' capability tester
/// branches on `0x40`, `0x80` and `0x10000` (the last cross-checked against a field in
/// the device's own `eureka_info`), and never looks at `0x200`, `0x800` or `0x20000` at
/// all — those are carried by hardware and read by something else, or by nothing.
///
/// They are deliberately **not** given names here. A capability bit is a claim about what
/// this device can do, and naming one whose meaning has not been established would put a
/// guess into a type where it reads as a fact. Recorded so the next person does not have
/// to re-derive that they exist.
pub const OBSERVED_UNMODELLED: &[u32] = &[0x40, 0x80, 0x200, 0x800, 0x1_0000, 0x2_0000];

/// The `ca` value: which capabilities this receiver claims.
///
/// A set rather than a number so the advertisement is built from what the receiver *is*.
/// The panel drives a screen and speakers, which is `VideoOut | AudioOut` — the `5` that
/// used to be a literal in the advertisement, now derived from the two facts that make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceCapabilities(u32);

impl DeviceCapabilities {
    /// No capabilities claimed.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Add a capability.
    #[must_use]
    pub const fn with(self, capability: DeviceCapability) -> Self {
        Self(self.0 | capability.bit())
    }

    /// Whether a capability is claimed.
    #[must_use]
    pub const fn has(self, capability: DeviceCapability) -> bool {
        self.0 & capability.bit() == capability.bit()
    }

    /// The raw bitmask, as the TXT record carries it.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// What this panel is: a screen with speakers.
    #[must_use]
    pub const fn panel() -> Self {
        Self::empty()
            .with(DeviceCapability::VideoOut)
            .with(DeviceCapability::AudioOut)
    }
}

impl fmt::Display for DeviceCapabilities {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The `st` key: whether the receiver is idle or already serving somebody.
///
/// **Mandatory**, and its absence is not a soft failure: openscreen's
/// `ReceiverInfoFromDnsSdInstance` rejects the entire record with "Missing receiver
/// status flag", so a sender that parses strictly drops the device before opening a
/// socket. Play Services' own parser reads the same two values and logs "Invalid receiver
/// status" for anything else (decompiled, `MdnsDeviceScannerEntry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ReceiverState {
    /// Nothing is running; a sender may launch.
    #[default]
    Idle = 0,
    /// An application is already running.
    Busy = 1,
}

impl fmt::Display for ReceiverState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", *self as u8)
    }
}

/// The `nf` key — "remote control notifications", the notification a phone shows so
/// somebody can control a cast that is already playing.
///
/// Play Services stores it as `rcnEnabledStatus` and **validates it**: 1, 2 and 3 are
/// accepted, and anything else — including the key being absent, which parses as 0 — is
/// logged as `Invalid remote control notifications enabled status` and replaced by a
/// server-side default. That validation is why this key exists here at all: a record
/// without it is refused by the scanner, which is one of the three reasons this receiver
/// was in no picker (#226).
///
/// **What 2 and 3 mean is not established.** The value is parsed, validated,
/// round-tripped through the device object and never branched on anywhere in the module
/// that reads it; the consumer is elsewhere. So they are [`Self::Reserved`] rather than
/// invented names — the type says exactly as much as is known and no more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteControlNotifications {
    /// `1` — what every real Cast device on the development LAN advertises, and what this
    /// receiver sends. Consistent with the `controlNotifications: 1` already reported to
    /// a device prober over `GET_DEVICE_INFO`: one claim, two surfaces, one source.
    Enabled,
    /// `2` or `3`: accepted by Play Services, meaning not established. Constructible so a
    /// capture can be modelled faithfully; never produced by this receiver.
    Reserved(u8),
}

impl RemoteControlNotifications {
    /// The wire value.
    #[must_use]
    pub const fn value(self) -> u8 {
        match self {
            Self::Enabled => 1,
            Self::Reserved(raw) => raw,
        }
    }

    /// Parse a wire value, rejecting the ones Play Services rejects.
    ///
    /// `None` for anything outside `1..=3` — including `0`, which is what an absent key
    /// parses to and what this whole type exists to stop us sending.
    #[must_use]
    pub const fn from_value(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Enabled),
            2 | 3 => Some(Self::Reserved(raw)),
            _ => None,
        }
    }
}

impl fmt::Display for RemoteControlNotifications {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value())
    }
}

/// The `ve` key: the CASTv2 generation this receiver speaks.
///
/// Two characters, zero-padded, and the padding is load-bearing in one direction — Play
/// Services parses it with `Integer.parseInt`, which accepts `"05"`, but the value is
/// compared numerically (`>= 4` gates the sub-type matching that decides whether a device
/// is offered to a filtered picker at all). So this is not decoration: a receiver
/// reporting a lower generation is dropped from those pickers before anything else is
/// considered.
pub const PROTOCOL_GENERATION: &str = "05";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_advertises_a_screen_and_speakers_and_says_so_as_five() {
        // The literal this type replaced. Asserted rather than assumed, because the
        // number is what goes on the wire and the names are what we reason with.
        let caps = DeviceCapabilities::panel();
        assert_eq!(caps.bits(), 5);
        assert_eq!(caps.to_string(), "5");
        assert!(caps.has(DeviceCapability::VideoOut));
        assert!(caps.has(DeviceCapability::AudioOut));
        // And claims nothing else — a panel has no camera, no microphone, is not a
        // development build, and is one device rather than a speaker group.
        assert!(!caps.has(DeviceCapability::VideoIn));
        assert!(!caps.has(DeviceCapability::AudioIn));
        assert!(!caps.has(DeviceCapability::DevMode));
        assert!(!caps.has(DeviceCapability::MultizoneGroup));
    }

    /// The two real values off the development LAN, decomposed against the published
    /// constants — which is the evidence that this numbering is the right one.
    #[test]
    fn a_real_speakers_bitmask_decomposes_the_way_the_published_constants_say() {
        // A Google Home Mini: audio out, no video out. If the numbering were wrong this
        // would claim a screen, and the device plainly has none.
        let mini = DeviceCapabilities(199_172);
        assert!(mini.has(DeviceCapability::AudioOut));
        assert!(!mini.has(DeviceCapability::VideoOut));
        assert!(!mini.has(DeviceCapability::MultizoneGroup));

        // The Cast group on the same LAN is that value plus exactly one bit, and it is
        // the group bit. Two devices, one difference, and it lands where it should.
        let group = DeviceCapabilities(199_204);
        assert_eq!(
            group.bits() - mini.bits(),
            DeviceCapability::MultizoneGroup.bit()
        );
        assert!(group.has(DeviceCapability::MultizoneGroup));
    }

    #[test]
    fn the_state_flag_is_the_two_values_a_sender_accepts() {
        assert_eq!(ReceiverState::Idle.to_string(), "0");
        assert_eq!(ReceiverState::Busy.to_string(), "1");
        assert_eq!(ReceiverState::default(), ReceiverState::Idle);
    }

    /// The validation Play Services applies, mirrored — so a value it would refuse
    /// cannot be constructed here and reach the wire.
    #[test]
    fn remote_control_notifications_rejects_what_play_services_rejects() {
        assert_eq!(RemoteControlNotifications::Enabled.value(), 1);
        assert_eq!(
            RemoteControlNotifications::from_value(1),
            Some(RemoteControlNotifications::Enabled)
        );
        for accepted in [2, 3] {
            assert_eq!(
                RemoteControlNotifications::from_value(accepted),
                Some(RemoteControlNotifications::Reserved(accepted))
            );
        }
        // 0 is what an absent key parses to, and it is the failure this type exists to
        // make unrepresentable.
        assert_eq!(RemoteControlNotifications::from_value(0), None);
        assert_eq!(RemoteControlNotifications::from_value(4), None);
    }

    #[test]
    fn the_generation_is_numerically_above_the_sub_type_matching_gate() {
        // Play Services only looks at a device's sub-types when this parses to >= 4,
        // and the sub-types are what get us into a filtered picker (#226).
        assert!(PROTOCOL_GENERATION.parse::<u32>().is_ok_and(|v| v >= 4));
    }
}
