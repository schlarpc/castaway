//! The advertised-device model: from one [`SsdpDevice`] we derive the set of
//! `NT`/`USN` [`Target`]s SSDP requires us to announce (root device, the UUID, the
//! device type, and each service type), each with the UPnP-mandated USN format.

/// One advertised `(NT, USN)` pair. SSDP requires a separate announcement per target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The notification/search type (`NT`/`ST`).
    pub nt: String,
    /// The unique service name (`USN`), formatted per UPnP rules.
    pub usn: String,
}

/// A UPnP root device we advertise over SSDP. Its [`Self::targets`] expand to every
/// `NT`/`USN` the spec says a controller may search for.
#[derive(Debug, Clone)]
pub struct SsdpDevice {
    /// The device UUID, *including* the `uuid:` prefix (e.g. `uuid:0f8c...`).
    pub uuid: String,
    /// The device type URN (e.g. `urn:schemas-upnp-org:device:MediaRenderer:1`).
    pub device_type: String,
    /// Service type URNs hosted by this device.
    pub services: Vec<String>,
}

impl SsdpDevice {
    /// Expand into the full set of advertised targets, in the canonical order
    /// (root device, uuid, device type, then services).
    #[must_use]
    pub fn targets(&self) -> Vec<Target> {
        let mut out = Vec::with_capacity(3 + self.services.len());
        // Root device.
        out.push(Target {
            nt: "upnp:rootdevice".to_string(),
            usn: format!("{}::upnp:rootdevice", self.uuid),
        });
        // The bare UUID (USN == NT here).
        out.push(Target {
            nt: self.uuid.clone(),
            usn: self.uuid.clone(),
        });
        // Device type.
        out.push(Target {
            nt: self.device_type.clone(),
            usn: format!("{}::{}", self.uuid, self.device_type),
        });
        // Each service type.
        for svc in &self.services {
            out.push(Target {
                nt: svc.clone(),
                usn: format!("{}::{}", self.uuid, svc),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn renderer() -> SsdpDevice {
        SsdpDevice {
            uuid: "uuid:11111111-2222-3333-4444-555555555555".into(),
            device_type: "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
            services: vec![
                "urn:schemas-upnp-org:service:AVTransport:1".into(),
                "urn:schemas-upnp-org:service:RenderingControl:1".into(),
            ],
        }
    }

    #[test]
    fn expands_all_targets_in_order() {
        let t = renderer().targets();
        assert_eq!(t.len(), 5);
        assert_eq!(t[0].nt, "upnp:rootdevice");
        assert_eq!(
            t[0].usn,
            "uuid:11111111-2222-3333-4444-555555555555::upnp:rootdevice"
        );
        assert_eq!(t[1].nt, t[1].usn); // bare uuid
        assert_eq!(t[2].nt, "urn:schemas-upnp-org:device:MediaRenderer:1");
        assert!(t[3]
            .usn
            .ends_with("::urn:schemas-upnp-org:service:AVTransport:1"));
    }

    #[test]
    fn usn_uses_double_colon_separator() {
        let t = renderer().targets();
        for target in &t[2..] {
            assert!(target.usn.contains("::"), "USN must use :: separator");
        }
    }
}
