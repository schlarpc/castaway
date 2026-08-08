//! The typed, validated mDNS service description and its conversion to an
//! `mdns_sd::ServiceInfo`. Validation lives here so the responder never registers a
//! malformed type.

use std::fmt;

use mdns_sd::ServiceInfo;

use crate::error::MdnsError;

/// The longest a single DNS label may be, in octets (RFC 1035 §2.3.4).
///
/// `mdns-sd` does not clamp to it: `DnsOutPacket::write_utf8` refuses a longer label
/// with `WriteError::NameTooLong`, so the record is silently never written. (Before
/// 0.20 it asserted instead and panicked on the daemon thread, taking the whole
/// responder down with it.) Either way nothing reaches a log, so the clamp is ours.
const MAX_LABEL_OCTETS: usize = 63;

/// A DNS-SD service instance name, guaranteed to survive encoding as **exactly one DNS
/// label**.
///
/// ## What may be in one
///
/// Nearly anything. RFC 6763 §4.1.1 makes the instance portion "arbitrary Net-Unicode
/// text", barring only ASCII control characters, and §4.3 says a literal `'.'` is
/// carried by escaping it as `\.` when the three portions are concatenated into
/// presentation format. `/` and `#` need no special handling at all, and neither does a
/// dot once the encoder escapes it — the LDH (letters/digits/hyphen) rule people reach
/// for here governs *host* names, which is the SRV target, not this. The Apple TV on the
/// test LAN advertises `Living Room (2)`, spaces and parentheses included.
///
/// The one hard limit is length: 63 octets, measured on the *unescaped* label, because
/// that is what is length-prefixed on the wire.
///
/// ## Why it is still a type
///
/// A too-long label is a silent failure. `mdns-sd` refuses to encode one
/// (`WriteError::NameTooLong`), so the record simply does not go out; before 0.20 it
/// asserted instead, taking down its own daemon thread and every other advertisement
/// with it. The reachable case is RAOP, whose instance is `<DEVICEID>@<name>` — thirteen
/// octets on top of a name the app already caps at 63.
///
/// Either way nothing that looks like an error reaches a log, and a receiver missing
/// from the wire is indistinguishable from one that is switched off. So the bound is
/// enforced by parsing at the boundary (ground rule 1): every path into the responder
/// goes through [`MdnsService::new`], and there is no way to hand it a name that cannot
/// be encoded. The conversion is total on purpose — a fallible constructor would turn a
/// cosmetic name problem into a dropped advertisement, which is the same invisibility
/// with a different cause.
///
/// ## History
///
/// This type first shipped substituting `'-'` for every `'.'`, because `mdns-sd` 0.13
/// composed the owner name as the string `"{instance}.{type}.{domain}"` and split it on
/// every dot with no escape implemented in either direction. `dma.space/screen` went out
/// as five labels where DNS-SD permits one instance label, and mDNSResponder and Avahi
/// both discard such a record — which is precisely what "not visible in iOS Screen
/// Mirroring" looked like, with the RTSP listener bound and reporting ready and not one
/// inbound connection ever. 0.20 implements RFC 6763 §4.3 properly, so the substitution
/// is gone and the panel keeps the name it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLabel {
    /// The label as it will be encoded: at most [`MAX_LABEL_OCTETS`] octets. Dots are
    /// kept — escaping them is the encoder's job.
    label: String,
    /// What was asked for, when that differs — so the rewrite is visible in the log
    /// instead of quietly changing the name shown in a picker.
    rewritten_from: Option<String>,
}

impl InstanceLabel {
    /// Parse an instance name into something encodable.
    ///
    /// The label is truncated to [`MAX_LABEL_OCTETS`] on a character boundary, and
    /// nothing else is touched: punctuation, spaces and dots are all legal in an
    /// instance name and are the caller's to choose.
    #[must_use]
    pub fn new(raw: impl AsRef<str>) -> Self {
        let raw = raw.as_ref();
        let mut label: String = raw.to_owned();
        if label.len() > MAX_LABEL_OCTETS {
            let mut end = MAX_LABEL_OCTETS;
            while end > 0 && !label.is_char_boundary(end) {
                end -= 1;
            }
            label.truncate(end);
        }
        let rewritten_from = (label != raw).then(|| raw.to_owned());
        Self {
            label,
            rewritten_from,
        }
    }

    /// The encodable label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.label
    }

    /// The name originally asked for, when it had to be rewritten to be encodable.
    #[must_use]
    pub fn rewritten_from(&self) -> Option<&str> {
        self.rewritten_from.as_deref()
    }

    /// Consume the label, yielding the encodable string.
    #[must_use]
    pub fn into_string(self) -> String {
        self.label
    }
}

impl fmt::Display for InstanceLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

impl AsRef<str> for InstanceLabel {
    fn as_ref(&self) -> &str {
        &self.label
    }
}

impl std::ops::Deref for InstanceLabel {
    type Target = str;

    fn deref(&self) -> &str {
        &self.label
    }
}

/// Fully qualify and validate a service type (`_x._tcp` → `_x._tcp.local.`).
/// Shared by advertising and browsing so both reject the same malformed types.
///
/// # Errors
/// [`MdnsError::InvalidServiceType`] if it isn't a `_name._tcp`/`_udp` type.
pub fn qualify_type(service_type: &str) -> Result<String, MdnsError> {
    let ty = service_type.trim_end_matches('.');
    let looks_valid = ty.starts_with('_') && (ty.contains("._tcp") || ty.contains("._udp"));
    if !looks_valid {
        return Err(MdnsError::InvalidServiceType(service_type.to_string()));
    }
    Ok(if ty.ends_with(".local") {
        format!("{ty}.")
    } else if ty.ends_with("._tcp") || ty.ends_with("._udp") {
        format!("{ty}.local.")
    } else {
        // e.g. already has a subdomain; normalize to end with a dot.
        format!("{ty}.")
    })
}

/// A service instance to advertise, e.g. `_googlecast._tcp` named "castaway" on 8009.
#[derive(Debug, Clone)]
pub struct MdnsService {
    /// The service type, e.g. `_googlecast._tcp` (the `.local.` suffix is optional and
    /// added if missing).
    pub service_type: String,
    /// The instance/friendly name, e.g. `castaway`. Parsed into a single encodable DNS
    /// label at construction — see [`InstanceLabel`] for why that is a type and not a
    /// `String`.
    pub instance: InstanceLabel,
    /// The advertised port.
    pub port: u16,
    /// The mDNS host name label (without domain), e.g. `castaway`. Becomes
    /// `<host>.local.`.
    pub host: String,
    /// TXT record key/value pairs.
    pub txt: Vec<(String, String)>,
    /// DNS-SD *sub-types* to publish this instance under (RFC 6763 §7.1), without the
    /// leading underscore — `674A0243` becomes
    /// `_674A0243._sub._googlecast._tcp.local.`
    ///
    /// Not decoration, and not a filter *we* apply: a sub-type is how a browsing sender
    /// narrows discovery to devices that can run a particular application, **before it
    /// connects to anything**. Play Services does exactly this — it browses
    /// `_<appid>._sub._googlecast._tcp` for the apps it wants and matches each answer
    /// against its filter criteria; a device that answers only the base type is never
    /// associated with any criterion and so never becomes a route, however correctly it
    /// answers everything else afterwards (#226). Real Chromecasts answer these queries;
    /// measured on the development LAN.
    pub subtypes: Vec<String>,
}

impl MdnsService {
    /// Build a service with no TXT records.
    #[must_use]
    pub fn new(
        service_type: impl Into<String>,
        instance: impl AsRef<str>,
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            service_type: service_type.into(),
            instance: InstanceLabel::new(instance),
            host: host.into(),
            port,
            txt: Vec::new(),
            subtypes: Vec::new(),
        }
    }

    /// Publish this instance under a DNS-SD sub-type as well as its base type.
    ///
    /// Repeatable; the leading underscore is added here, so callers pass the bare label.
    #[must_use]
    pub fn with_subtype(mut self, subtype: impl Into<String>) -> Self {
        self.subtypes.push(subtype.into());
        self
    }

    /// The fully-qualified type for one sub-type registration
    /// (`_674A0243._sub._googlecast._tcp.local.`).
    ///
    /// # Errors
    /// [`MdnsError::InvalidServiceType`] if the base type is malformed.
    pub fn qualified_subtype(&self, subtype: &str) -> Result<String, MdnsError> {
        let base = self.qualified_type()?;
        let label = subtype.trim_start_matches('_');
        Ok(format!("_{label}._sub.{base}"))
    }

    /// Add a TXT record (builder style).
    #[must_use]
    pub fn with_txt(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.txt.push((key.into(), value.into()));
        self
    }

    /// The fully-qualified service type (`_x._tcp.local.`), validating the shape.
    ///
    /// # Errors
    /// [`MdnsError::InvalidServiceType`] if it isn't a `_name._tcp`/`_udp` type.
    pub fn qualified_type(&self) -> Result<String, MdnsError> {
        qualify_type(&self.service_type)
    }

    /// Convert to an `mdns_sd::ServiceInfo` with address auto-detection enabled.
    ///
    /// # Errors
    /// [`MdnsError`] on a bad type or if `ServiceInfo` construction fails.
    pub fn to_service_info(&self) -> Result<ServiceInfo, MdnsError> {
        let info = self.to_service_info_for(None)?;
        // Every sub-type on the one instance. `with_subtypes` is the patched crate's
        // (#227); upstream holds a single `Option<String>` and would keep only the last
        // of several registrations, silently.
        let qualified = self
            .subtypes
            .iter()
            .map(|subtype| self.qualified_subtype(subtype))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(info.with_subtypes(&qualified))
    }

    /// The `ServiceInfo` for this instance, optionally registered under one sub-type.
    ///
    /// One sub-type per `ServiceInfo` because that is `mdns-sd`'s shape — its
    /// `ServiceInfo` holds a single `sub_domain`, so publishing several means several
    /// registrations of the same instance.
    ///
    /// # Errors
    /// [`MdnsError`] on a bad type or if `ServiceInfo` construction fails.
    pub fn to_service_info_for(&self, subtype: Option<&str>) -> Result<ServiceInfo, MdnsError> {
        let ty_domain = match subtype {
            Some(subtype) => self.qualified_subtype(subtype)?,
            None => self.qualified_type()?,
        };
        let host_name = format!("{}.local.", self.host.trim_end_matches('.'));
        // A slice, not a `HashMap`: the map's iteration order is randomised per process,
        // so the emitted TXT record came out in a different order on every run. DNS-SD
        // does not care, but it makes the record non-reproducible — and AirPlay's
        // `/info` can be asked to return the raw TXT bytes back to the sender, which is
        // something worth being able to pin with a byte-exact fixture (ground rule 6).
        let info = ServiceInfo::new(
            &ty_domain,
            self.instance.as_str(),
            &host_name,
            "", // addresses filled by enable_addr_auto()
            self.port,
            &self.txt[..],
        )
        .map_err(|e| MdnsError::ServiceInfo(e.to_string()))?
        .enable_addr_auto();
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn qualifies_tcp_type() {
        let s = MdnsService::new("_googlecast._tcp", "castaway", "castaway", 8009);
        assert_eq!(s.qualified_type().unwrap(), "_googlecast._tcp.local.");
    }

    #[test]
    fn accepts_already_qualified() {
        let s = MdnsService::new("_airplay._tcp.local.", "castaway", "castaway", 7000);
        assert_eq!(s.qualified_type().unwrap(), "_airplay._tcp.local.");
    }

    #[test]
    fn rejects_bogus_type() {
        let s = MdnsService::new("googlecast", "x", "h", 1);
        assert!(s.qualified_type().is_err());
    }

    /// Split a presentation-format name into labels the way a resolver does, honouring
    /// RFC 6763 §4.3: `\.` is a literal dot inside a label, `\\` a literal backslash,
    /// and only an *unescaped* dot ends a label.
    ///
    /// Deliberately written against the RFC rather than mirrored from `mdns-sd`, so the
    /// assertions below say what a conformant peer sees and not what our dependency
    /// happens to do.
    fn labels_of(fullname: &str) -> Vec<String> {
        let mut labels = Vec::new();
        let mut current = String::new();
        let mut chars = fullname.trim_end_matches('.').chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(escaped) => current.push(escaped),
                    None => current.push('\\'),
                },
                '.' => labels.push(std::mem::take(&mut current)),
                other => current.push(other),
            }
        }
        labels.push(current);
        labels
    }

    /// The wire form: each label length-prefixed, terminated by a zero octet. The
    /// length is measured on the *unescaped* label, which is what is actually written.
    fn wire_name(fullname: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in labels_of(fullname) {
            assert!(
                label.len() < 64,
                "a {}-octet label cannot encode",
                label.len()
            );
            out.push(u8::try_from(label.len()).unwrap());
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    #[test]
    fn a_dotted_instance_name_occupies_exactly_one_label() {
        // The receiver's real configured name, carried through the actual `ServiceInfo`
        // rather than a hand-built string — the escaping is `mdns-sd`'s to do, and the
        // point of the test is that we hand it something it can do that to.
        //
        // Under 0.13 this produced the five-label PTR rdata
        // `dma | space/screen#airplay | _airplay | _tcp | local`, which Bonjour's
        // DeconstructServiceName() rejects outright, so neither iOS Screen Mirroring nor
        // `avahi-browse` on the advertising host ever listed us.
        let s = MdnsService::new(
            "_airplay._tcp",
            "dma.space/screen#airplay",
            "castaway",
            7000,
        );
        let fullname = s.to_service_info().unwrap().get_fullname().to_string();

        assert!(
            fullname.contains("dma\\.space"),
            "the literal dot must be escaped in presentation format, got {fullname}"
        );

        let labels = labels_of(&fullname);
        assert_eq!(
            labels,
            vec!["dma.space/screen#airplay", "_airplay", "_tcp", "local"],
            "instance name must occupy exactly one label"
        );

        // Byte-exact, against the shape every real receiver on the LAN puts on the wire
        // (`STR-AZ1000ES | _airplay | _tcp | local`, four labels). `/` and `#` need no
        // escape and no repair — RFC 6763 §4.1.1 allows any punctuation here.
        assert_eq!(
            wire_name(&fullname),
            b"\x18dma.space/screen#airplay\x08_airplay\x04_tcp\x05local\x00".to_vec(),
        );
    }

    #[test]
    fn the_raop_instance_cannot_overflow_a_label() {
        // RAOP prefixes `<DEVICEID>@`, thirteen octets on top of a friendly name the app
        // already caps at the 63-octet label limit. Over that, `mdns-sd` returns
        // `WriteError::NameTooLong` and the record is simply never written — and before
        // 0.20 it asserted instead, killing its own daemon thread and taking every other
        // advertisement with it. Neither produces anything that looks like an error.
        let name = format!("0E8C2E10CAAA@{}", "x".repeat(63));
        let s = MdnsService::new("_raop._tcp", &name, "castaway", 7000);
        assert_eq!(s.instance.as_str().len(), 63);
        wire_name(s.to_service_info().unwrap().get_fullname());
    }

    #[test]
    fn the_limit_is_measured_in_octets_not_characters() {
        // A 40-emoji name is 40 characters and 160 octets; the label limit is octets.
        let s = MdnsService::new("_raop._tcp", "\u{1f4fa}".repeat(40), "castaway", 7000);
        assert!(s.instance.as_str().len() <= 63);
        assert_eq!(s.instance.as_str().len() % 4, 0, "cut mid-codepoint");
    }

    #[test]
    fn only_an_unencodable_name_is_rewritten() {
        // Silently renaming the device is how this class of bug hides, so a repair is
        // recorded. A dotted name is *not* a repair any more — it encodes as-is.
        let dotted = MdnsService::new("_airplay._tcp", "dma.space/screen", "castaway", 7000);
        assert_eq!(dotted.instance.rewritten_from(), None);
        assert_eq!(dotted.instance.as_str(), "dma.space/screen");

        let too_long = MdnsService::new("_raop._tcp", "y".repeat(90), "castaway", 7000);
        assert_eq!(
            too_long.instance.rewritten_from(),
            Some("y".repeat(90)).as_deref()
        );
    }

    #[test]
    fn builds_service_info_with_txt() {
        let s = MdnsService::new("_googlecast._tcp", "castaway", "castaway", 8009)
            .with_txt("id", "abcd")
            .with_txt("md", "castaway");
        let info = s.to_service_info().unwrap();
        assert_eq!(info.get_port(), 8009);
        assert!(info.get_property_val_str("id").is_some());
    }
}
