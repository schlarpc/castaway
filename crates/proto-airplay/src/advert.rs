//! AirPlay mDNS advertisements.
//!
//! Two services light up Apple senders: `_airplay._tcp` (the modern advertisement, which
//! carries the feature bitmask) and `_raop._tcp` (Remote Audio Output — the AirPlay 1
//! audio flow this receiver actually serves).
//!
//! **Everything advertised here is a promise.** A sender reads these records and picks a
//! flow; if it picks one we answer `501` to, the session dies after the handshake with
//! nothing on screen to say why. So the rule for this module is that a bit or a key goes
//! in only when the code behind it works — see `docs/airplay-research.md` §4 for the
//! catalogue of what each one obliges us to implement.

use substrate_mdns::MdnsService;

/// AirPlay's default control port. Both services are advertised here: no reference
/// implementation gives RAOP a port of its own, and 7011 — which this used to
/// advertise — is the AirPlay 1 **UDP timing** port, not a TCP control port.
pub const AIRPLAY_PORT: u16 = 7000;

/// The `_airplay._tcp` service type.
pub const AIRPLAY_SERVICE: &str = "_airplay._tcp";
/// The `_raop._tcp` service type.
pub const RAOP_SERVICE: &str = "_raop._tcp";

/// What we tell senders our source version is.
///
/// This is **a behaviour switch, not a version string**. Senders gate flows on it:
/// `>= 354.54.6` turns on buffered audio and `>= 366` turns on PTP, neither of which
/// exists here. `220.68` is UxPlay's, and sits below every such gate — which is the
/// reason to use it. Raising it silently changes which media plane a sender chooses.
pub const SOURCE_VERSION: &str = "220.68";

/// The model string. A bespoke name is fine for an audio receiver — real ones ship
/// `AVR-X3500H`, `ShairportSync` — but it must not look like `^Mac\d+,\d+$`, which
/// pyatv keeps on an explicit blocklist. Claiming an Apple TV is a *mirroring*
/// requirement, so that changes when mirroring does.
pub const MODEL: &str = "castaway1,1";

/// A 64-bit AirPlay feature bitmask.
///
/// Exists as a type for one reason: the TXT record and the `/info` plist must encode
/// **the same value**, and they encode it differently — TXT as `0x<low32>,0x<high32>`
/// with the *low* word first, `/info` as a single integer. Advertising a 32-bit
/// truncation in one and the full value in the other is a contradiction a sender can
/// see, and it is what this receiver used to do. Deriving both from one `Features`
/// makes that unrepresentable rather than merely fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features(u64);

impl Features {
    /// Build a mask from the bit numbers to set.
    #[must_use]
    pub fn from_bits(bits: &[u8]) -> Self {
        let mut mask = 0u64;
        let mut i = 0;
        while i < bits.len() {
            mask |= 1u64 << bits[i];
            i += 1;
        }
        Self(mask)
    }

    /// The whole 64-bit value, for the `/info` plist.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Whether a given bit is set.
    #[must_use]
    pub const fn has(self, bit: u8) -> bool {
        self.0 & (1u64 << bit) != 0
    }

    /// The mDNS TXT encoding: `0x<low32>,0x<high32>`, **low word first**.
    ///
    /// The comma form is the opposite order to how the number is written, which is the
    /// single most commonly inverted detail in this protocol. The high word is omitted
    /// when zero, as the AirPlay-1-era devices do.
    #[must_use]
    pub fn txt(self) -> String {
        let lo = self.0 & 0xFFFF_FFFF;
        let hi = self.0 >> 32;
        if hi == 0 {
            format!("0x{lo:X}")
        } else {
            format!("0x{lo:X},0x{hi:X}")
        }
    }
}

/// The bits this receiver sets, **by number**.
///
/// Numbers, not names, on purpose: the two published bit tables (openairplay's
/// `features.md` and the pyatv/owntone/airplay2-receiver family) disagree on bits 26,
/// 30, 38 and 48, and pyatv's own source comments that the discrepancy is unresolved.
/// Positions are evidence; names are folklore. Naming them in a Rust type would bake a
/// guess into the code, so each is documented with what setting it *obliges us to do*.
///
/// - **7** — screen mirroring. Without it no sender ever offers a screen, however much
///   of the mirroring stack exists behind it.
/// - **9** — audio. Load-bearing for visibility: with it clear, owntone-class senders
///   drop the device from the picker entirely rather than showing it as broken.
/// - **18** — PCM audio format. Matches `cn=0`.
/// - **19** — ALAC audio format. Matches `cn=1`.
/// - **22** — accept an unencrypted audio stream. Matches `et=0`.
/// - **30** — publish the modern `_airplay._tcp` key set (`pi`, `protovers`, `acl`),
///   which we do. Set by every real device observed, including AirPlay-1-only ones.
///
/// Deliberately **not** set, and why:
///
/// - **11** (retransmit) — the resend request on the control port is not implemented;
///   we drop instead. Every real device sets this, and we still should not until we do.
/// - **42** (multi-codec screen) — HEVC is not decoded here. With it clear a sender
///   sends H.264; with it set and no HEVC path, the sender emits an empty codec-config
///   packet and stalls, which is why `MirrorError::CodecRefused` exists to name it.
/// - **20** (AAC-LC) — not offered in `cn`.
/// - **26** (MFi) — would promise `/auth-setup` against a coprocessor we do not have.
///   shairport-sync *removed* this bit in 4.3 for exactly this reason.
/// - **27** (legacy pairing) — beyond gating a 5-second pairing round trip, this bit
///   changes the media key derivation: set, the AES key must be hashed with the
///   pair-verify ECDH secret; clear, it must not. Mismatched, a session completes
///   cleanly and then renders noise.
/// - **40/41/47** (buffered audio, PTP) — no buffered stream type, no IEEE-1588 clock.
/// - **38/43/46/48** (HomeKit) — `/pair-setup` answers 501.
/// - **51** (unified pair-setup + MFi) — nobody open-source has made it work; the
///   observed failure is iOS giving up at Pair-Setup [2/5].
/// - **12/14/2** (FairPlay-in-`et`) — the *audio* path still advertises `et=0,1`, and
///   mirroring's FairPlay is negotiated through `/fp-setup` rather than through these.
const FEATURE_BITS: &[u8] = &[7, 9, 18, 19, 22, 30];

/// Feature bit 42: the sender may encode HEVC as well as H.264.
///
/// Conditional rather than constant, because it is a *policy* and the failure it causes
/// is silent: set it with no HEVC decoder and a Mac sends an empty codec-config packet
/// and stalls, which looks exactly like a mirror that never started.
const FEATURE_SCREEN_MULTI_CODEC: u8 = 42;

/// Identity used to build the AirPlay/RAOP advertisements.
#[derive(Debug, Clone)]
pub struct AirPlayIdentity {
    /// Friendly name shown in the AirPlay picker.
    pub name: String,
    /// The device id as a MAC-style string, e.g. `AA:BB:CC:DD:EE:FF`. Must be a
    /// syntactically valid unicast MAC — see `derive_mac` in the app crate.
    pub device_id: String,
    /// mDNS host label (becomes `<host>.local.`).
    pub host: String,
    /// The `pi` / `PublicCUAirPlayPairingIdentifier`: a **stable UUID**, not the MAC.
    /// Every real Apple and third-party device advertises a UUID here; only Roku and
    /// Samsung put a MAC in it, and they are the outliers.
    pub pairing_id: String,
    /// Whether to offer HEVC mirroring as well as H.264.
    ///
    /// A knob rather than a constant so both can be exercised against one device in one
    /// sitting: what a sender encodes is decided entirely by what we advertise, so this
    /// is the only way to see the other path at all.
    pub offer_hevc: bool,
    /// The mirroring height to advertise, in pixels.
    ///
    /// The sender treats *height* as the controlling dimension and adjusts width to
    /// however the device is being held, so this is a budget rather than a geometry.
    /// 1080 keeps senders on H.264; 2160 is what makes a Mac reach for HEVC.
    pub mirror_height: u32,
}

impl AirPlayIdentity {
    /// The feature mask this receiver advertises.
    #[must_use]
    pub fn features(&self) -> Features {
        let mut bits = FEATURE_BITS.to_vec();
        if self.offer_hevc {
            bits.push(FEATURE_SCREEN_MULTI_CODEC);
        }
        Features::from_bits(&bits)
    }

    /// The `_airplay._tcp` advertisement.
    #[must_use]
    pub fn airplay_service(&self) -> MdnsService {
        MdnsService::new(AIRPLAY_SERVICE, &self.name, &self.host, AIRPLAY_PORT)
            .with_txt("deviceid", &self.device_id)
            .with_txt("features", self.features().txt())
            .with_txt("srcvers", SOURCE_VERSION)
            // Status flag bit 2, "audio cable attached" — the universal idle value on
            // every captured receiver. It looks like a placeholder and is correct.
            // Bits 3 (0x8) and 9 (0x200) would declare pairing *mandatory*.
            .with_txt("flags", "0x4")
            .with_txt("model", MODEL)
            .with_txt("pi", &self.pairing_id)
            .with_txt("protovers", "1.1")
            // Access control: 0 = anyone may cast. `acl=1` makes pyatv mark the device
            // unpairable outright, so this is not a field to leave to a default.
            .with_txt("acl", "0")
            .with_txt("vv", "2")
            .with_txt("pw", "false")
        // No `pk`. It is the receiver's Ed25519 long-term public key, and we have no
        // pairing to use one with. Advertising it empty — which this used to do —
        // publishes a key a sender may adopt as our identity and then cannot verify
        // against. Real pairing-less devices (Marantz NR1607, Libratone Loop) omit it
        // entirely, and are listed and usable.
    }

    /// The `_raop._tcp` (audio) advertisement, in the AirPlay 1 "classic" dialect.
    ///
    /// The instance name is prefixed with the device id per RAOP convention
    /// (`<DEVICEID>@<name>`, uppercase hex, no separators) — senders rely on it, and
    /// pyatv splits on the first `@` to derive the device's unique identity.
    ///
    /// Note there is deliberately no `ft` key here. `ft` is the AirPlay **2** way to
    /// carry the feature mask on a RAOP record; classic AirPlay 1 records carry none,
    /// and signal capability through `cn`/`et`/`md`/`ch`/`sr`/`ss` instead.
    #[must_use]
    pub fn raop_service(&self) -> MdnsService {
        let instance = format!("{}@{}", self.device_id.replace(':', ""), self.name);
        MdnsService::new(RAOP_SERVICE, instance, &self.host, AIRPLAY_PORT)
            .with_txt("txtvers", "1")
            .with_txt("ch", "2")
            // Codecs: 0 = PCM, 1 = ALAC. Not 2/3 (AAC, AAC-ELD) — ffmpeg can decode
            // them, but nothing here negotiates them, and `cn` is what a sender picks
            // its encoder from.
            .with_txt("cn", "0,1")
            .with_txt("da", "true")
            // Encryption: 0 = none, 1 = RSA. Not 3 or 5 (FairPlay), which is what this
            // used to claim and which sent every iPhone into `/fp-setup` and a 501 —
            // `crypto-fairplay` still cannot derive that key. RSA it can: the session
            // key arrives in `a=rsaaeskey:` and `crypto-raop` unwraps it.
            .with_txt("et", "0,1")
            // Encryption key present. Travels with `et=1`.
            .with_txt("ek", "1")
            // Metadata: text, artwork, progress. Unlike `et`, this is safe to advertise
            // ahead of handling it — `md` makes the sender *push* extra SET_PARAMETER
            // bodies we may ignore, where `et` would make it *encrypt* with something
            // we cannot decrypt. One is harmless, the other is fatal.
            .with_txt("md", "0,1,2")
            .with_txt("sr", "44100")
            .with_txt("ss", "16")
            .with_txt("sv", "false")
            .with_txt("tp", "UDP")
            // AirTunes protocol version 1.1 as a packed u32 (0x00010001).
            .with_txt("vn", "65537")
            .with_txt("vs", SOURCE_VERSION)
            .with_txt("am", MODEL)
            // `sf` on RAOP and `flags` on AirPlay are the same field; every
            // implementation derives both from one variable, so they must not drift.
            .with_txt("sf", "0x4")
            .with_txt("pw", "false")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ident() -> AirPlayIdentity {
        AirPlayIdentity {
            name: "Hackerspace TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
            pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
            offer_hevc: false,
            mirror_height: 1080,
        }
    }

    fn txt_of(svc: &MdnsService, key: &str) -> Option<String> {
        svc.txt
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn airplay_service_has_deviceid_and_features() {
        let s = ident().airplay_service();
        assert_eq!(s.service_type, AIRPLAY_SERVICE);
        assert_eq!(s.port, AIRPLAY_PORT);
        assert_eq!(txt_of(&s, "deviceid").as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert!(txt_of(&s, "features").is_some());
    }

    #[test]
    fn raop_instance_is_prefixed_with_deviceid() {
        let s = ident().raop_service();
        assert!(s.instance.starts_with("AABBCCDDEEFF@"));
        assert_eq!(txt_of(&s, "sr").as_deref(), Some("44100"));
    }

    #[test]
    fn both_services_share_one_port() {
        // 7011, which this used to advertise for RAOP, is the AirPlay 1 UDP *timing*
        // port. No reference implementation gives RAOP a TCP port of its own.
        let id = ident();
        assert_eq!(id.airplay_service().port, id.raop_service().port);
    }

    #[test]
    fn features_txt_puts_the_low_word_first() {
        // The detail this protocol gets wrong most often. A mask with bit 32 set and
        // nothing else must read `0x0,0x1` — not `0x1,0x0`.
        let f = Features::from_bits(&[32]);
        assert_eq!(f.txt(), "0x0,0x1");
        assert_eq!(f.as_u64(), 1u64 << 32);
    }

    #[test]
    fn a_low_only_mask_omits_the_high_word() {
        assert_eq!(Features::from_bits(&[9]).txt(), "0x200");
    }

    #[test]
    fn we_do_not_promise_what_we_cannot_serve() {
        // Each of these bits sends a sender down a flow that ends in a 501 or in
        // silence. They are the difference between "not implemented yet" and "lies to
        // every iPhone in the room", so they get a test rather than a comment.
        let f = ident().features();
        for (bit, why) in [
            (11u8, "retransmit"),
            (12, "FairPlay SAP v2.5"),
            (14, "FairPlay"),
            (20, "AAC-LC, which `cn` does not offer"),
            (42, "HEVC mirroring, which has no decoder here"),
            (26, "MFi auth"),
            (
                27,
                "legacy pairing, which also changes the media key derivation",
            ),
            (40, "buffered audio"),
            (41, "PTP"),
            (46, "HomeKit pairing"),
            (48, "transient pairing"),
            (51, "unified pair-setup + MFi"),
        ] {
            assert!(
                !f.has(bit),
                "bit {bit} promises {why}, which is not implemented"
            );
        }
        // And the ones that must be set: 9 or senders drop us from the picker
        // entirely, 7 or none of them ever offers a screen.
        assert!(f.has(9), "bit 9 (audio) is required to appear at all");
        assert!(
            f.has(7),
            "bit 7 (screen) is required to be offered a mirror"
        );
    }

    #[test]
    fn hevc_is_offered_only_when_it_is_asked_for() {
        // The failure this guards is silent: a sender that picks HEVC against a build
        // with no HEVC path sends an empty codec-config packet and simply stops.
        let mut id = ident();
        assert!(!id.features().has(42), "HEVC is off unless asked for");
        id.offer_hevc = true;
        assert!(id.features().has(42));
        // …and nothing else moved with it.
        assert!(id.features().has(7) && id.features().has(9));
    }

    #[test]
    fn the_advertised_height_is_what_was_configured() {
        // The sender treats height as the controlling dimension, so this is the knob
        // that decides whether a Mac reaches for HEVC at all.
        let mut id = ident();
        id.mirror_height = 2160;
        assert_eq!(id.mirror_height, 2160);
    }

    #[test]
    fn raop_advertises_no_encryption_it_cannot_perform() {
        // `et=0,3,5` is what shipped before: it advertised FairPlay, so an iPhone
        // picked FairPlay, and the session died at `/fp-setup`. RSA (1) we can do;
        // FairPlay (3, 5) still needs a key derivation that does not exist.
        let s = ident().raop_service();
        assert_eq!(txt_of(&s, "et").as_deref(), Some("0,1"));
    }

    #[test]
    fn raop_carries_no_ft_because_this_is_the_airplay_1_dialect() {
        // `ft` is the AirPlay 2 way to put the feature mask on a RAOP record, and pyatv
        // reads it *before* `features`. Advertising one here would claim AirPlay 2.
        assert!(txt_of(&ident().raop_service(), "ft").is_none());
    }

    #[test]
    fn no_public_key_is_advertised_while_there_is_no_pairing() {
        // Not an empty `pk` — no `pk` at all. An empty one publishes an identity a
        // sender cannot verify against.
        assert!(txt_of(&ident().airplay_service(), "pk").is_none());
    }

    #[test]
    fn the_pairing_identifier_is_a_uuid_not_the_mac() {
        let s = ident().airplay_service();
        let pi = txt_of(&s, "pi").unwrap();
        assert!(
            pi.contains('-') && !pi.contains(':'),
            "pi should be a UUID: {pi}"
        );
    }

    #[test]
    fn status_flags_agree_across_both_services() {
        // `flags` and `sf` are the same field under two names; every implementation
        // derives them from one variable and a drift between them is a real defect.
        let id = ident();
        assert_eq!(
            txt_of(&id.airplay_service(), "flags"),
            txt_of(&id.raop_service(), "sf")
        );
    }
}
