//! The typed, validated mDNS service description and its conversion to an
//! `mdns_sd::ServiceInfo`. Validation lives here so the responder never registers a
//! malformed type.

use std::fmt;

use mdns_sd::ServiceInfo;

use crate::error::MdnsError;

/// The longest a single DNS label may be, in octets (RFC 1035 §2.3.4).
///
/// `mdns-sd` does not clamp: `DnsOutPacket::write_utf8` asserts `len < 64` and *panics*
/// on the daemon thread, which takes the whole responder down without a log line.
const MAX_LABEL_OCTETS: usize = 63;

/// A DNS-SD service instance name, guaranteed to survive encoding as **exactly one DNS
/// label**.
///
/// This type exists because the alternative was invisible. `mdns-sd` composes the
/// record's owner name as the *string* `"{instance}.{type}.{domain}"` and then encodes
/// it by splitting on every `'.'` — it implements no escape (`\.`) at all, in either
/// direction. So an instance name containing a dot silently becomes two labels on the
/// wire, and the resulting PTR rdata (`dma.space/screen#airplay._airplay._tcp.local.`,
/// five labels) is not a DNS-SD service instance name. Bonjour's
/// `DeconstructServiceName()` requires exactly one instance label before `_app._proto`,
/// so mDNSResponder — and Avahi — discard the record entirely. Observed directly: with
/// `friendly_name = "dma.space/screen"` the receiver's own `avahi-browse -r -t
/// _airplay._tcp` on the advertising host listed the LAN's Apple TV and Sony receiver
/// and *not* the receiver itself, while a packet dump showed our PTR/SRV/TXT going out
/// on the wire. Nothing logs; the receiver simply does not exist to any conformant
/// resolver, which is exactly what "not visible in iOS Screen Mirroring" looks like.
///
/// So the label is a parsed type rather than a `String` that happens to be well-formed
/// today (ground rule 1): every path into the responder goes through
/// [`MdnsService::new`], and there is no way to hand it a name that cannot be encoded.
/// The conversion is total on purpose — a fallible constructor would turn a cosmetic
/// name problem into a *dropped advertisement*, which is the same invisibility with a
/// different cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLabel {
    /// The label as it will be encoded: no `'.'`, at most [`MAX_LABEL_OCTETS`] octets.
    label: String,
    /// What was asked for, when that differs — so the rewrite is visible in the log
    /// instead of quietly changing the name shown in a picker.
    rewritten_from: Option<String>,
}

impl InstanceLabel {
    /// Parse an instance name into something encodable.
    ///
    /// Two things are repaired, both of which are otherwise silent failures:
    ///
    /// - `'.'` becomes `'-'`, because a dot is `mdns-sd`'s label separator and there is
    ///   no escape that survives its encoder.
    /// - the label is truncated to [`MAX_LABEL_OCTETS`] on a character boundary. The
    ///   reachable case is RAOP, whose instance is `<DEVICEID>@<name>` — thirteen octets
    ///   on top of a name the app already caps at 63 — and the failure mode is a panic
    ///   inside the mDNS daemon thread, not a rejected registration.
    #[must_use]
    pub fn new(raw: impl AsRef<str>) -> Self {
        let raw = raw.as_ref();
        let mut label: String = raw.replace('.', "-");
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
        }
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
        let ty_domain = self.qualified_type()?;
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

    /// Encode a fullname the way `mdns-sd`'s `DnsOutPacket::write_name` does: split on
    /// every `'.'`, one length-prefixed label each, terminated by a zero octet. No
    /// escape processing, because `mdns-sd` implements none — this *is* its encoder.
    fn wire_name(fullname: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in fullname.trim_end_matches('.').split('.') {
            assert!(
                label.len() < 64,
                "mdns-sd panics on a {}-octet label",
                label.len()
            );
            out.push(u8::try_from(label.len()).unwrap());
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }

    /// Split a wire-format name back into labels, as any DNS-SD resolver does.
    fn wire_labels(name: &[u8]) -> Vec<&[u8]> {
        let mut labels = Vec::new();
        let mut i = 0;
        while name[i] != 0 {
            let len = usize::from(name[i]);
            labels.push(&name[i + 1..i + 1 + len]);
            i += 1 + len;
        }
        labels
    }

    #[test]
    fn a_dotted_instance_name_still_encodes_as_one_label() {
        // The receiver's real configured name. Before `InstanceLabel` this produced the
        // five-label PTR rdata `dma | space/screen#airplay | _airplay | _tcp | local`,
        // which Bonjour's DeconstructServiceName() rejects outright — so neither iOS
        // Screen Mirroring nor `avahi-browse` on the advertising host ever listed us.
        let s = MdnsService::new(
            "_airplay._tcp",
            "dma.space/screen#airplay",
            "castaway",
            7000,
        );
        let fullname = format!("{}.{}", s.instance, s.qualified_type().unwrap());
        let wire = wire_name(&fullname);
        let labels = wire_labels(&wire);

        // Byte-exact, against the shape every real receiver on the LAN puts on the wire
        // (`STR-AZ1000ES | _airplay | _tcp | local`, four labels).
        assert_eq!(
            wire,
            b"\x18dma-space/screen#airplay\x08_airplay\x04_tcp\x05local\x00".to_vec(),
        );
        assert_eq!(
            labels.len(),
            4,
            "instance name must occupy exactly one label"
        );
        assert_eq!(labels[0], b"dma-space/screen#airplay");
        assert_eq!(labels[1], b"_airplay");
    }

    #[test]
    fn the_raop_instance_cannot_overflow_a_label() {
        // RAOP prefixes `<DEVICEID>@`, thirteen octets on top of a friendly name the app
        // already caps at the 63-octet label limit. `mdns-sd`'s encoder asserts
        // `len < 64` and panics on its own daemon thread, so this is not a cosmetic cap:
        // uncapped, the whole responder dies with no log line.
        let name = format!("0E8C2E10CAAA@{}", "x".repeat(63));
        let s = MdnsService::new("_raop._tcp", &name, "castaway", 7000);
        assert_eq!(s.instance.as_str().len(), 63);
        // Encodes without tripping mdns-sd's assertion.
        wire_name(&format!("{}.{}", s.instance, s.qualified_type().unwrap()));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let s = MdnsService::new("_raop._tcp", "\u{1f4fa}".repeat(40), "castaway", 7000);
        assert!(s.instance.as_str().len() <= 63);
        assert_eq!(s.instance.as_str().len() % 4, 0, "cut mid-codepoint");
    }

    #[test]
    fn a_rewrite_is_recorded_so_it_can_be_logged() {
        // Silently renaming the device is how this class of bug hides. A name that
        // needed no repair carries no note; one that did carries what was asked for.
        let ok = MdnsService::new("_airplay._tcp", "Hackerspace TV", "castaway", 7000);
        assert_eq!(ok.instance.rewritten_from(), None);
        let fixed = MdnsService::new("_airplay._tcp", "dma.space/screen", "castaway", 7000);
        assert_eq!(fixed.instance.rewritten_from(), Some("dma.space/screen"));
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
