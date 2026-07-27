//! The WFD information element: what a sink says about itself before any connection
//! exists.
//!
//! This rides in 802.11 beacons and probe responses, so it is the *only* thing a source
//! knows about us when it decides whether to show us in its picker. Android's
//! `isWifiDisplay()` checks exactly three things — the IE is present, session availability
//! is set, and the device type is a sink — and consults nothing else, not even the WPS
//! device type. So this small structure decides discoverability outright.
//!
//! > **Two vendor IEs, two endiannesses.** Wi-Fi Direct's IE and Wi-Fi Display's IE share
//! > element id `0xDD` and OUI `50-6F-9A`, differing only in the OUI *type* — `0x09` vs
//! > `0x0A` — and in that P2P attribute lengths are **little**-endian while WFD subelement
//! > lengths are **big**-endian. Getting that backwards produces an IE that parses to
//! > plausible nonsense rather than failing. Hence [`SubelementId`] and the deliberate
//! > absence of any `u16` in this module that is not immediately consumed.
//!
//! See `docs/miracast-protocol-notes.md` §1.2–§1.6.

use crate::error::IeError;

/// The vendor-specific element id, shared with every other vendor IE.
pub const ELEMENT_ID: u8 = 0xDD;

/// The Wi-Fi Alliance OUI.
pub const OUI_WFA: [u8; 3] = [0x50, 0x6F, 0x9A];

/// The OUI type that distinguishes Wi-Fi *Display* from Wi-Fi Direct (`0x09`).
pub const OUI_TYPE_WFD: u8 = 0x0A;

/// The most subelement bytes one IE body can carry: 255 minus the OUI and OUI type.
///
/// A longer subelement set is split across several WFD IEs in the same frame, and the
/// *concatenation* is what parses — see [`WfdInformationElement::parse_frame`].
pub const MAX_SUBELEMENTS_PER_IE: usize = 251;

/// A WFD subelement id.
///
/// Ids 2–5 were the audio/video/3D/content-protection capability subelements in Miracast
/// v1.0 and are **reserved** as of v2.3 — their content moved into the RTSP parameters
/// entirely. They are named here so a legacy R1 peer's IE still parses, and are never
/// emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SubelementId {
    /// Device Information. Mandatory in every frame that carries the IE.
    DeviceInformation,
    /// The BSSID of the AP or GO we are associated with.
    AssociatedBssid,
    /// Coupled Sink Information.
    CoupledSink,
    /// Extended Capability — where UIBC support is announced.
    ExtendedCapability,
    /// Local IP Address. TDLS only, which we do not implement.
    LocalIpAddress,
    /// Session Information. Group-owner only.
    SessionInformation,
    /// Alternative MAC Address.
    AlternativeMacAddress,
    /// R2 Device Information.
    R2DeviceInformation,
    /// Anything else, including the four v1.0 subelements v2.3 deleted.
    Other(u8),
}

impl SubelementId {
    /// The wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::DeviceInformation => 0,
            Self::AssociatedBssid => 1,
            Self::CoupledSink => 6,
            Self::ExtendedCapability => 7,
            Self::LocalIpAddress => 8,
            Self::SessionInformation => 9,
            Self::AlternativeMacAddress => 10,
            Self::R2DeviceInformation => 11,
            Self::Other(v) => v,
        }
    }

    /// Read a subelement id.
    #[must_use]
    pub const fn from_wire(raw: u8) -> Self {
        match raw {
            0 => Self::DeviceInformation,
            1 => Self::AssociatedBssid,
            6 => Self::CoupledSink,
            7 => Self::ExtendedCapability,
            8 => Self::LocalIpAddress,
            9 => Self::SessionInformation,
            10 => Self::AlternativeMacAddress,
            11 => Self::R2DeviceInformation,
            other => Self::Other(other),
        }
    }
}

/// What kind of WFD device this is. Bits 1:0 of the Device Information bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    /// A sender.
    Source,
    /// A sink that renders both planes. What we are.
    PrimarySink,
    /// A sink that renders only one plane of a coupled pair.
    SecondarySink,
    /// Both roles.
    SourceAndPrimarySink,
}

impl DeviceType {
    const fn wire(self) -> u16 {
        match self {
            Self::Source => 0b00,
            Self::PrimarySink => 0b01,
            Self::SecondarySink => 0b10,
            Self::SourceAndPrimarySink => 0b11,
        }
    }

    const fn from_wire(raw: u16) -> Self {
        match raw & 0b11 {
            0b00 => Self::Source,
            0b01 => Self::PrimarySink,
            0b10 => Self::SecondarySink,
            _ => Self::SourceAndPrimarySink,
        }
    }

    /// Whether a source will consider this device a display it can project to.
    ///
    /// The exact test Android's `isWifiDisplay()` applies.
    #[must_use]
    pub const fn is_sink(self) -> bool {
        matches!(
            self,
            Self::PrimarySink | Self::SecondarySink | Self::SourceAndPrimarySink
        )
    }
}

/// Whether this device will accept a new WFD session right now. Bits 5:4.
///
/// > **This is a hard gate, not a hint.** Miracast v2.3 §4.5: a device advertising
/// > `NotAvailable` *shall not* be connected to. Which makes it the intended mechanism for
/// > "busy with someone else" — a sink already mirroring flips this and disappears from
/// > every picker in the room, rather than accepting a second source and dropping the
/// > first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAvailability {
    /// Not accepting sessions.
    NotAvailable,
    /// Accepting sessions.
    Available,
}

/// Which link a device would rather use. Bit 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferredConnectivity {
    /// Wi-Fi Direct. What we always say — TDLS is R1-only and not implemented.
    WifiDirect,
    /// TDLS.
    Tdls,
}

/// The Device Information subelement (id 0): six octets that decide discoverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInformation {
    /// Source, sink, or both.
    pub device_type: DeviceType,
    /// Whether a session can be started right now.
    pub availability: SessionAvailability,
    /// Preferred link type.
    pub preferred_connectivity: PreferredConnectivity,
    /// Whether the device supports HDCP 2.x.
    pub content_protection: bool,
    /// Whether it answers WFD-typed P2P service discovery.
    ///
    /// Deprecated and removed in v2.3, never issued by Android or Windows, and clear in
    /// both open-source sinks. Modelled so a legacy peer's IE round-trips; always `false`
    /// in what we emit.
    pub service_discovery: bool,
    /// Whether the device does 802.1AS time synchronisation.
    pub time_sync: bool,
    /// Set by a primary sink that has no audio rendering at all — an office projector.
    pub audio_unsupported_at_primary_sink: bool,
    /// Set by a source that can send audio with no video.
    pub audio_only_source: bool,
    /// Whether the device supports coupled-sink operation as a source.
    pub coupled_sink_at_source: bool,
    /// Whether it supports coupled-sink operation as a sink.
    pub coupled_sink_at_sink: bool,
    /// The RTSP port the *source* should connect to… which is nothing of the kind: the
    /// sink is the TCP client (see [`crate::session`]). A sink with no RTSP function at
    /// all sets this to zero.
    pub control_port: u16,
    /// Maximum throughput, in Mbps.
    pub max_throughput_mbps: u16,
}

impl DeviceInformation {
    /// The six-octet body's fixed length.
    pub const BODY_LEN: usize = 6;

    /// What this sink advertises.
    ///
    /// Every bit that is off is off for a documented reason: service discovery is
    /// deprecated, TDLS is R1-only, and content protection is deliberately not
    /// implemented (notes §6) — a sink that claimed HDCP and could not do it would fail
    /// the handshake rather than fall back.
    #[must_use]
    pub const fn sink(control_port: u16, max_throughput_mbps: u16) -> Self {
        Self {
            device_type: DeviceType::PrimarySink,
            availability: SessionAvailability::Available,
            preferred_connectivity: PreferredConnectivity::WifiDirect,
            content_protection: false,
            service_discovery: false,
            time_sync: false,
            audio_unsupported_at_primary_sink: false,
            audio_only_source: false,
            coupled_sink_at_source: false,
            coupled_sink_at_sink: false,
            control_port,
            max_throughput_mbps,
        }
    }

    /// The same advertisement with availability flipped off — how a busy sink withdraws
    /// itself from every picker in the room.
    #[must_use]
    pub const fn busy(self) -> Self {
        Self {
            availability: SessionAvailability::NotAvailable,
            ..self
        }
    }

    /// The 16-bit device information bitmap.
    #[must_use]
    pub const fn bitmap(self) -> u16 {
        let mut bits = self.device_type.wire();
        if self.coupled_sink_at_source {
            bits |= 0x0004;
        }
        if self.coupled_sink_at_sink {
            bits |= 0x0008;
        }
        if matches!(self.availability, SessionAvailability::Available) {
            bits |= 0x0010;
        }
        if self.service_discovery {
            bits |= 0x0040;
        }
        if matches!(self.preferred_connectivity, PreferredConnectivity::Tdls) {
            bits |= 0x0080;
        }
        if self.content_protection {
            bits |= 0x0100;
        }
        if self.time_sync {
            bits |= 0x0200;
        }
        if self.audio_unsupported_at_primary_sink {
            bits |= 0x0400;
        }
        if self.audio_only_source {
            bits |= 0x0800;
        }
        bits
    }

    /// Read the six-octet body.
    ///
    /// # Errors
    /// [`IeError::Truncated`] if fewer than six octets are present.
    pub fn parse(body: &[u8]) -> Result<Self, IeError> {
        let b: &[u8; Self::BODY_LEN] = body
            .get(..Self::BODY_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(IeError::Truncated)?;
        // Big-endian, unlike every length field in the *P2P* IE next door.
        let bits = u16::from_be_bytes([b[0], b[1]]);
        Ok(Self {
            device_type: DeviceType::from_wire(bits),
            // 0b10 and 0b11 are reserved; anything not exactly 0b01 is "do not connect".
            availability: if (bits >> 4) & 0b11 == 0b01 {
                SessionAvailability::Available
            } else {
                SessionAvailability::NotAvailable
            },
            preferred_connectivity: if bits & 0x0080 == 0 {
                PreferredConnectivity::WifiDirect
            } else {
                PreferredConnectivity::Tdls
            },
            content_protection: bits & 0x0100 != 0,
            service_discovery: bits & 0x0040 != 0,
            time_sync: bits & 0x0200 != 0,
            audio_unsupported_at_primary_sink: bits & 0x0400 != 0,
            audio_only_source: bits & 0x0800 != 0,
            coupled_sink_at_source: bits & 0x0004 != 0,
            coupled_sink_at_sink: bits & 0x0008 != 0,
            control_port: u16::from_be_bytes([b[2], b[3]]),
            max_throughput_mbps: u16::from_be_bytes([b[4], b[5]]),
        })
    }

    /// The six-octet body.
    #[must_use]
    pub fn to_body(self) -> [u8; Self::BODY_LEN] {
        let mut out = [0u8; Self::BODY_LEN];
        out[0..2].copy_from_slice(&self.bitmap().to_be_bytes());
        out[2..4].copy_from_slice(&self.control_port.to_be_bytes());
        out[4..6].copy_from_slice(&self.max_throughput_mbps.to_be_bytes());
        out
    }
}

/// The Extended Capability bitmap (subelement 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtendedCapability {
    /// UIBC — the touch back-channel. The one bit that matters to us, and the out-of-band
    /// half of the UIBC negotiation: a source learns from here, before RTSP starts, that
    /// asking about UIBC is worthwhile.
    pub uibc: bool,
    /// I2C read/write pass-through.
    pub i2c: bool,
    /// Preferred display mode.
    pub preferred_display_mode: bool,
    /// Standby and resume control.
    pub standby_resume: bool,
    /// TDLS persistent group.
    pub tdls_persistent: bool,
    /// TDLS persistent BSSID.
    pub tdls_persistent_bssid: bool,
}

impl ExtendedCapability {
    /// The bitmap.
    #[must_use]
    pub const fn bits(self) -> u16 {
        (self.uibc as u16)
            | ((self.i2c as u16) << 1)
            | ((self.preferred_display_mode as u16) << 2)
            | ((self.standby_resume as u16) << 3)
            | ((self.tdls_persistent as u16) << 4)
            | ((self.tdls_persistent_bssid as u16) << 5)
    }

    /// Read the bitmap.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self {
            uibc: bits & 0x0001 != 0,
            i2c: bits & 0x0002 != 0,
            preferred_display_mode: bits & 0x0004 != 0,
            standby_resume: bits & 0x0008 != 0,
            tdls_persistent: bits & 0x0010 != 0,
            tdls_persistent_bssid: bits & 0x0020 != 0,
        }
    }
}

/// One subelement: an id and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subelement {
    /// Which subelement.
    pub id: SubelementId,
    /// Its body, without the id or the length.
    pub body: Vec<u8>,
}

/// A WFD information element, or the concatenation of several from one frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WfdInformationElement {
    /// The subelements, in wire order.
    pub subelements: Vec<Subelement>,
}

impl WfdInformationElement {
    /// The advertisement a sink beacons.
    ///
    /// Device Information is mandatory in every frame that carries the IE; Extended
    /// Capability is what makes a source ask about UIBC at all. The four v1.0 A/V
    /// capability subelements are deliberately absent — v2.3 deleted them, and their
    /// content lives in the RTSP parameters now.
    #[must_use]
    pub fn sink(device: DeviceInformation, extended: ExtendedCapability) -> Self {
        Self {
            subelements: vec![
                Subelement {
                    id: SubelementId::DeviceInformation,
                    body: device.to_body().to_vec(),
                },
                Subelement {
                    id: SubelementId::ExtendedCapability,
                    body: extended.bits().to_be_bytes().to_vec(),
                },
            ],
        }
    }

    /// The Device Information subelement, if present.
    ///
    /// # Errors
    /// [`IeError::Truncated`] if it is present but short.
    pub fn device_information(&self) -> Result<Option<DeviceInformation>, IeError> {
        self.subelements
            .iter()
            .find(|s| s.id == SubelementId::DeviceInformation)
            .map(|s| DeviceInformation::parse(&s.body))
            .transpose()
    }

    /// The Extended Capability subelement, if present.
    #[must_use]
    pub fn extended_capability(&self) -> Option<ExtendedCapability> {
        self.subelements
            .iter()
            .find(|s| s.id == SubelementId::ExtendedCapability)
            .and_then(|s| s.body.get(..2))
            .and_then(|b| b.try_into().ok())
            .map(|b: [u8; 2]| ExtendedCapability::from_bits(u16::from_be_bytes(b)))
    }

    /// Parse a subelement stream — the concatenated bodies of every WFD IE in one frame.
    ///
    /// An unknown or reserved id is kept rather than rejected: v2.3 requires a device to
    /// *"ignore that WFD subelement and parse any remaining fields"*, and reserved ids are
    /// exactly what a future revision will start sending.
    ///
    /// # Errors
    /// [`IeError`] if a subelement declares a length its body does not have.
    pub fn parse_subelements(mut bytes: &[u8]) -> Result<Self, IeError> {
        let mut subelements = Vec::new();
        while !bytes.is_empty() {
            let header = bytes.get(..3).ok_or(IeError::Truncated)?;
            // Big-endian, unlike the P2P IE's little-endian attribute lengths.
            let declared = usize::from(u16::from_be_bytes([header[1], header[2]]));
            let body = bytes
                .get(3..3 + declared)
                .ok_or(IeError::BadSubelementLength {
                    id: header[0],
                    declared,
                    actual: bytes.len().saturating_sub(3),
                })?;
            subelements.push(Subelement {
                id: SubelementId::from_wire(header[0]),
                body: body.to_vec(),
            });
            bytes = bytes.get(3 + declared..).unwrap_or_default();
        }
        Ok(Self { subelements })
    }

    /// Parse every WFD IE in a frame's element stream, concatenating their bodies first.
    ///
    /// The concatenation is not an optimisation. An IE body caps at 255 octets, so a long
    /// subelement set is split across several IEs — and *"if a WFD subelement is not
    /// contained entirely within a single WFD IE, the WFD subelement ID field and Length
    /// field for that subelement occur only once at the start"*. Parsing each IE
    /// independently therefore corrupts any subelement that straddles a boundary, and does
    /// so silently, because the second fragment's first bytes look like a subelement
    /// header.
    ///
    /// Non-WFD elements (including the Wi-Fi Direct IE, whose only difference is one OUI
    /// type byte) are skipped.
    ///
    /// # Errors
    /// [`IeError`] if an element runs past the end of the frame, or a subelement's
    /// declared length does not fit.
    pub fn parse_frame(mut elements: &[u8]) -> Result<Self, IeError> {
        let mut joined = Vec::new();
        while !elements.is_empty() {
            let header = elements.get(..2).ok_or(IeError::Truncated)?;
            let len = usize::from(header[1]);
            let body = elements.get(2..2 + len).ok_or(IeError::Truncated)?;
            // OUI type 0x09 is the Wi-Fi Direct IE: same element id, same OUI, and
            // little-endian lengths — folding it in here would parse to nonsense, so the
            // type byte is part of the test rather than checked afterwards.
            if header[0] == ELEMENT_ID
                && body.get(..3) == Some(&OUI_WFA)
                && body.get(3) == Some(&OUI_TYPE_WFD)
            {
                joined.extend_from_slice(body.get(4..).unwrap_or_default());
            }
            elements = elements.get(2 + len..).unwrap_or_default();
        }
        if joined.is_empty() {
            return Err(IeError::NotWfd);
        }
        Self::parse_subelements(&joined)
    }

    /// Encode the subelement stream, without the element header.
    ///
    /// This is the form wpa_supplicant's control interface wants, one subelement at a
    /// time — see [`WfdInformationElement::subelem_set_hex`].
    #[must_use]
    pub fn to_subelements(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for sub in &self.subelements {
            out.push(sub.id.wire());
            let len = u16::try_from(sub.body.len()).unwrap_or(u16::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&sub.body);
        }
        out
    }

    /// Encode as one or more complete information elements, splitting at the 251-octet
    /// body limit the way hostap does.
    #[must_use]
    pub fn to_elements(&self) -> Vec<u8> {
        let payload = self.to_subelements();
        let mut out = Vec::new();
        for chunk in payload.chunks(MAX_SUBELEMENTS_PER_IE) {
            out.push(ELEMENT_ID);
            // OUI (3) + OUI type (1) + this chunk. The spec words the length field as
            // "4 plus the total length of WFD subelements", which is the same number.
            out.push(u8::try_from(chunk.len() + 4).unwrap_or(u8::MAX));
            out.extend_from_slice(&OUI_WFA);
            out.push(OUI_TYPE_WFD);
            out.extend_from_slice(chunk);
        }
        out
    }

    /// The hex payload for one `WFD_SUBELEM_SET` control command.
    ///
    /// wpa_supplicant prepends the subelement id itself and expects the rest — the
    /// two-byte big-endian length and the body — as hex. So this is *not* the same bytes
    /// as [`WfdInformationElement::to_subelements`] for a single subelement, and the
    /// difference is exactly one leading octet.
    #[must_use]
    pub fn subelem_set_hex(sub: &Subelement) -> String {
        let mut hex = String::new();
        let len = u16::try_from(sub.body.len()).unwrap_or(u16::MAX);
        for byte in len.to_be_bytes().iter().chain(sub.body.iter()) {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
            .collect()
    }

    /// The three field-proven blobs from the notes' §1.4, in the form they are quoted:
    /// a `WFD_SUBELEM_SET` payload, which is the two-byte length and the body **without**
    /// the subelement id — wpa_supplicant prepends that itself. The first is what
    /// MiracleCast and lazycast put on the air; the other two come from sigma-dut, the
    /// Wi-Fi Alliance's own certification harness.
    const FIELD_PROVEN: &[(&str, &str)] = &[
        ("MiracleCast / lazycast", "000600111c4400c8"),
        ("sigma-dut sink", "000601511c440036"),
        ("sigma-dut source", "000601101c440036"),
    ];

    #[test]
    fn the_field_proven_device_information_blobs_round_trip() {
        for (who, blob) in FIELD_PROVEN {
            let bytes = unhex(blob);
            // Two bytes of big-endian length, then exactly that many of body.
            assert_eq!(
                usize::from(u16::from_be_bytes([bytes[0], bytes[1]])),
                bytes.len() - 2,
                "{who}"
            );
            let dev = DeviceInformation::parse(&bytes[2..]).unwrap();
            assert_eq!(dev.control_port, 7236, "{who}");
            let sub = Subelement {
                id: SubelementId::DeviceInformation,
                body: dev.to_body().to_vec(),
            };
            assert_eq!(
                WfdInformationElement::subelem_set_hex(&sub),
                *blob,
                "{who} must re-emit exactly"
            );
        }
    }

    #[test]
    fn the_miraclecast_blob_decodes_to_what_the_notes_annotate() {
        let dev = DeviceInformation::parse(&unhex("000600111c4400c8")[2..]).unwrap();
        assert_eq!(dev.device_type, DeviceType::PrimarySink);
        assert_eq!(dev.availability, SessionAvailability::Available);
        assert_eq!(dev.control_port, 7236);
        assert_eq!(dev.max_throughput_mbps, 200);
        assert!(!dev.content_protection);
        assert!(!dev.service_discovery);
        assert_eq!(
            dev.preferred_connectivity,
            PreferredConnectivity::WifiDirect
        );
    }

    #[test]
    fn our_own_advertisement_matches_the_known_working_bytes() {
        // Byte-identical to what MiracleCast and lazycast put on the air, which is the
        // strongest evidence available without a radio that a sender will accept it.
        let dev = DeviceInformation::sink(7236, 200);
        assert_eq!(hex(&dev.to_body()), "00111c4400c8");
    }

    #[test]
    fn a_busy_sink_withdraws_itself_from_every_picker() {
        // v2.3 §4.5 makes this a hard gate: a source *shall not* connect to a device
        // advertising 0b00. It is the mechanism, not a hack.
        let dev = DeviceInformation::sink(7236, 200).busy();
        assert_eq!(dev.availability, SessionAvailability::NotAvailable);
        assert_eq!(dev.bitmap() & 0x0030, 0);
        let back = DeviceInformation::parse(&dev.to_body()).unwrap();
        assert_eq!(back.availability, SessionAvailability::NotAvailable);
        // Still a sink, still on the same port — only availability changed.
        assert_eq!(back.device_type, DeviceType::PrimarySink);
        assert_eq!(back.control_port, 7236);
    }

    #[test]
    fn a_reserved_availability_value_reads_as_unavailable() {
        // 0b10 and 0b11 are reserved. Treating anything but 0b01 as "do not connect" is
        // the conservative reading, and the only one that cannot invent an invitation.
        let bits: u16 = 0x0001 | (0b11 << 4);
        let mut body = [0u8; 6];
        body[0..2].copy_from_slice(&bits.to_be_bytes());
        assert_eq!(
            DeviceInformation::parse(&body).unwrap().availability,
            SessionAvailability::NotAvailable
        );
    }

    #[test]
    fn every_bit_of_the_bitmap_round_trips() {
        let dev = DeviceInformation {
            device_type: DeviceType::SourceAndPrimarySink,
            availability: SessionAvailability::Available,
            preferred_connectivity: PreferredConnectivity::Tdls,
            content_protection: true,
            service_discovery: true,
            time_sync: true,
            audio_unsupported_at_primary_sink: true,
            audio_only_source: true,
            coupled_sink_at_source: true,
            coupled_sink_at_sink: true,
            control_port: 49_152,
            max_throughput_mbps: 300,
        };
        assert_eq!(DeviceInformation::parse(&dev.to_body()).unwrap(), dev);
    }

    #[test]
    fn the_device_type_test_is_the_one_android_applies() {
        assert!(DeviceType::PrimarySink.is_sink());
        assert!(DeviceType::SecondarySink.is_sink());
        assert!(DeviceType::SourceAndPrimarySink.is_sink());
        assert!(!DeviceType::Source.is_sink());
    }

    #[test]
    fn the_extended_capability_bit_that_matters_is_uibc() {
        let ext = ExtendedCapability {
            uibc: true,
            ..ExtendedCapability::default()
        };
        assert_eq!(ext.bits(), 0x0001);
        let ie = WfdInformationElement::sink(DeviceInformation::sink(7236, 200), ext);
        assert!(ie.extended_capability().unwrap().uibc);
        // The notes' §1.12 profile, written as the two control commands that install it:
        // `WFD_SUBELEM_SET 0 …` and `WFD_SUBELEM_SET 7 …`.
        let payloads: Vec<String> = ie
            .subelements
            .iter()
            .map(WfdInformationElement::subelem_set_hex)
            .collect();
        assert_eq!(payloads, vec!["000600111c4400c8", "00020001"]);
        // The same thing as a subelement stream, where each id *is* present.
        assert_eq!(hex(&ie.to_subelements()), "00000600111c4400c80700020001");
    }

    #[test]
    fn a_whole_element_carries_the_oui_and_the_length_the_spec_words() {
        let ie = WfdInformationElement::sink(
            DeviceInformation::sink(7236, 200),
            ExtendedCapability::default(),
        );
        let bytes = ie.to_elements();
        assert_eq!(bytes[0], ELEMENT_ID);
        // "4 plus the total length of WFD subelements": 4 + 9 + 5.
        assert_eq!(usize::from(bytes[1]), bytes.len() - 2);
        assert_eq!(&bytes[2..5], &OUI_WFA);
        assert_eq!(bytes[5], OUI_TYPE_WFD);
        assert_eq!(WfdInformationElement::parse_frame(&bytes).unwrap(), ie);
    }

    #[test]
    fn the_wifi_direct_ie_next_door_is_skipped_rather_than_parsed() {
        // Same element id, same OUI, one different type byte — and little-endian lengths.
        // Folding it in would parse to plausible nonsense instead of failing.
        let mut frame = vec![ELEMENT_ID, 6, 0x50, 0x6F, 0x9A, 0x09, 0x00, 0x02];
        frame.extend_from_slice(
            &WfdInformationElement::sink(
                DeviceInformation::sink(7236, 200),
                ExtendedCapability::default(),
            )
            .to_elements(),
        );
        let ie = WfdInformationElement::parse_frame(&frame).unwrap();
        assert_eq!(ie.subelements.len(), 2);
        assert_eq!(ie.device_information().unwrap().unwrap().control_port, 7236);
    }

    #[test]
    fn a_subelement_straddling_two_elements_is_parsed_from_the_concatenation() {
        // The failure this prevents is silent: parsing each IE independently makes the
        // second fragment's first three bytes look like a subelement header.
        let big = Subelement {
            id: SubelementId::SessionInformation,
            // Ten 23-octet session descriptors: 230 bytes, plus the 8 already there,
            // pushes past the 251-octet body limit.
            body: vec![0x5A; 260],
        };
        let ie = WfdInformationElement {
            subelements: vec![
                Subelement {
                    id: SubelementId::DeviceInformation,
                    body: DeviceInformation::sink(7236, 200).to_body().to_vec(),
                },
                big.clone(),
            ],
        };
        let frame = ie.to_elements();
        assert!(
            frame.iter().filter(|b| **b == ELEMENT_ID).count() >= 2,
            "the set must actually have been split"
        );
        let back = WfdInformationElement::parse_frame(&frame).unwrap();
        assert_eq!(back, ie);
        assert_eq!(back.subelements[1].body.len(), 260);
    }

    #[test]
    fn an_unknown_subelement_is_kept_rather_than_rejected() {
        // v2.3 requires ignoring it and parsing on; a reserved id is what a future
        // revision starts with. Ids 2-5 are exactly this case — deleted in v2.3, still
        // emitted by legacy R1 peers.
        let mut bytes = unhex("00000600111c4400c8");
        // Subelement 3 was "WFD Video Formats" in v1.0 and is reserved now.
        bytes.extend_from_slice(&unhex("0300020000"));
        let ie = WfdInformationElement::parse_subelements(&bytes).unwrap();
        assert_eq!(ie.subelements.len(), 2);
        assert_eq!(ie.subelements[1].id, SubelementId::Other(3));
        assert!(ie.device_information().unwrap().is_some());
    }

    #[test]
    fn a_subelement_that_overruns_names_itself() {
        let err = WfdInformationElement::parse_subelements(&unhex("0000ff0011")).unwrap_err();
        match err {
            IeError::BadSubelementLength { id, declared, .. } => {
                assert_eq!(id, 0);
                assert_eq!(declared, 255);
            }
            other => panic!("expected BadSubelementLength, got {other}"),
        }
    }

    #[test]
    fn a_frame_with_no_wfd_ie_is_not_a_wfd_device() {
        let frame = vec![ELEMENT_ID, 4, 0x50, 0x6F, 0x9A, 0x09];
        assert_eq!(
            WfdInformationElement::parse_frame(&frame).unwrap_err(),
            IeError::NotWfd
        );
    }

    #[test]
    fn the_control_command_payload_omits_the_id_wpa_supplicant_prepends() {
        // `WFD_SUBELEM_SET 0 000600111c4400c8` is what MiracleCast issues, and the id is
        // the one octet the daemon adds itself.
        let sub = Subelement {
            id: SubelementId::DeviceInformation,
            body: DeviceInformation::sink(7236, 200).to_body().to_vec(),
        };
        assert_eq!(
            WfdInformationElement::subelem_set_hex(&sub),
            "000600111c4400c8"
        );
    }

    #[test]
    fn a_truncated_device_information_body_is_an_error_not_a_default() {
        assert_eq!(
            DeviceInformation::parse(&[0x00, 0x11, 0x1c]).unwrap_err(),
            IeError::Truncated
        );
    }
}
