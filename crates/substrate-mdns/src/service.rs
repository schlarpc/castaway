//! The typed, validated mDNS service description and its conversion to an
//! `mdns_sd::ServiceInfo`. Validation lives here so the responder never registers a
//! malformed type.

use mdns_sd::ServiceInfo;

use crate::error::MdnsError;

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
    /// The instance/friendly name, e.g. `castaway`.
    pub instance: String,
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
        instance: impl Into<String>,
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            service_type: service_type.into(),
            instance: instance.into(),
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
            &self.instance,
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
