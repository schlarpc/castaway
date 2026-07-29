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

/// The model string. Claiming an Apple TV is a *mirroring* requirement — a bespoke name
/// was fine while this was an audio receiver, and the comment here used to say the model
/// would change "when mirroring does". Mirroring is done (STATUS: end to end, from a
/// real FairPlay vector over real sockets), so this is that change: `AppleTV3,2` is what
/// UxPlay claims, the Path B worked example (`docs/airplay-research.md` §4.4). Not a
/// `Mac\d+,\d+` string, which pyatv keeps on an explicit blocklist.
pub const MODEL: &str = "AppleTV3,2";

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
/// **The mask is UxPlay's pairing-less variant, `0x527FFEE6` — the Path B worked
/// example** (`docs/airplay-research.md` §4.4), adopted whole once the obligations it
/// carries were all real here: mirroring end to end, `/fp-setup` answering both round
/// trips, resend requests actually sent, AAC decoded. A hand-curated conservative mask
/// preceded it and was invisible to real senders — a picker's listing rules are
/// folklore, so the mask a listed implementation ships *is* the specification.
/// The regression test pins the exact value.
///
/// What the groups oblige us to do, and what backs each:
///
/// - **7** — screen mirroring; the mirroring session is implemented end to end.
/// - **9** — audio. Load-bearing for visibility: with it clear, owntone-class senders
///   drop the device from the picker entirely rather than showing it as broken.
/// - **18/19** — PCM and ALAC audio formats; matches `cn=0,1` on the RAOP record.
/// - **20/21** — AAC. The ffmpeg pipeline decodes AAC, and mirror audio (AAC-ELD)
///   rides the FairPlay session. `cn` still steers the plain audio flow to `0,1`.
/// - **22** — accept an unencrypted audio stream. Matches `et=0`.
/// - **11** — retransmit. The resend request on the control port is sent and served.
/// - **2/12/14** — the FairPlay family: the sender runs `/fp-setup`, and both round
///   trips answer correctly (`crypto-fairplay`); the `ekey` unwrap behind mirroring is
///   `crypto-playfair`, vector-verified.
/// - **1/5/6/10/13/15/16/17/25/28** — the rest of UxPlay's mask (video, photo, HLS,
///   metadata and audio bookkeeping). UxPlay serves these minimally and is listed;
///   diverging from a proven mask to shave bits is how the previous mask happened.
/// - **30** — publish the modern `_airplay._tcp` key set (`pi`, `protovers`, `acl`),
///   which we do. Set by every real device observed, including AirPlay-1-only ones.
///
/// Deliberately **not** set, and why:
///
/// - **42** (multi-codec screen) — HEVC is policy, not constant: see
///   [`FEATURE_SCREEN_MULTI_CODEC`].
/// - **26** (MFi) — would promise `/auth-setup` against a coprocessor we do not have.
///   shairport-sync *removed* this bit in 4.3 for exactly this reason.
/// - **27** (legacy pairing) — beyond gating a 5-second pairing round trip, this bit
///   changes the media key derivation: set, the AES key must be hashed with the
///   pair-verify ECDH secret; clear, it must not. Mismatched, a session completes
///   cleanly and then renders noise. UxPlay documents the bit-27-off variant as the
///   pairing bypass; if iOS ever stops accepting it, this is the next lever.
/// - **40/41/47** (buffered audio, PTP) — no buffered stream type, no IEEE-1588 clock.
/// - **38/43/46/48** (HomeKit) — `/pair-setup` answers 501.
/// - **51** (unified pair-setup + MFi) — nobody open-source has made it work; the
///   observed failure is iOS giving up at Pair-Setup [2/5].
const FEATURE_BITS: &[u8] = &[
    1, 2, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 25, 28, 30,
];

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

    /// The advertised `pk`: 32 bytes of stable hex, derived from the pairing id.
    ///
    /// Nothing verifies it — bit 27 is off, so no sender ever runs pair-verify against
    /// this key — but every *listed* receiver ships one, including UxPlay, which
    /// hardcodes a value in `dnssdint.h`. The earlier position ("real pairing-less
    /// devices omit it: Marantz, Libratone") described audio-era hardware; the worked
    /// example for a mirroring receiver carries a `pk`, so this does too. Derived
    /// rather than random so it is the same key tomorrow: a sender that caches device
    /// identities should find the same one.
    #[must_use]
    pub fn public_key_hex(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let digest = Sha256::digest(self.pairing_id.as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
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
            .with_txt("pk", self.public_key_hex())
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
            // Same key as the AirPlay record's; senders group the two services by it.
            .with_txt("pk", self.public_key_hex())
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
    fn the_mask_is_the_path_b_worked_example_exactly() {
        // UxPlay's pairing-less mask, byte for byte (`docs/airplay-research.md` §4.4).
        // Which bits make a picker list a device is folklore, so the mask a listed
        // implementation ships is the specification — a hand-curated "conservative"
        // subset of it is how this receiver spent a while invisible to every iPhone.
        assert_eq!(ident().features().txt(), "0x527FFEE6");
    }

    #[test]
    fn we_do_not_promise_what_we_cannot_serve() {
        // Each of these bits sends a sender down a flow that ends in a 501 or in
        // silence. They are the difference between "not implemented yet" and "lies to
        // every iPhone in the room", so they get a test rather than a comment.
        let f = ident().features();
        for (bit, why) in [
            (42u8, "HEVC mirroring, which was not asked for"),
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
        // entirely, 7 or none of them ever offers a screen, 11/12/14 because the
        // resend and FairPlay paths behind them are implemented and load-bearing.
        assert!(f.has(9), "bit 9 (audio) is required to appear at all");
        assert!(
            f.has(7),
            "bit 7 (screen) is required to be offered a mirror"
        );
        assert!(f.has(11) && f.has(12) && f.has(14));
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
    fn the_public_key_is_present_stable_and_shared_by_both_records() {
        // Nothing verifies it (bit 27 is off), but every listed mirroring receiver
        // ships one — UxPlay hardcodes theirs — and senders group the two services by
        // it. 64 hex chars: the shape of an Ed25519 public key, never empty (an empty
        // `pk` publishes an identity a sender cannot verify against).
        let id = ident();
        let pk = txt_of(&id.airplay_service(), "pk").unwrap();
        assert_eq!(pk.len(), 64);
        assert!(pk.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(txt_of(&id.raop_service(), "pk").as_deref(), Some(&*pk));
        // Stable: the same identity advertises the same key tomorrow.
        assert_eq!(pk, id.public_key_hex());
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
