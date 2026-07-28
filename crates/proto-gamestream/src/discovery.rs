//! Typed view of a discovered GameStream host.
//!
//! The mDNS browse itself lives in `substrate-mdns`; this module is the pure mapping
//! from a resolved `_nvstream._tcp` instance to a [`HostCandidate`] the adapter and the
//! chooser page can hold. Sunshine's advertisement is minimal — an instance name, a
//! port (47989, the HTTP half), addresses, and no TXT worth trusting — so everything
//! else about a host (paired? codecs? apps?) comes from asking NVHTTP, not from here.

use std::net::IpAddr;

use substrate_mdns::DiscoveredService;

/// A GameStream host seen on the LAN, keyed by its mDNS fullname.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCandidate {
    /// The mDNS fullname — the identity a removal refers to.
    pub fullname: String,
    /// Human-readable name for the chooser (the mDNS instance label).
    pub name: String,
    /// The address to dial. IPv4 preferred purely because Sunshine binds its UDP
    /// stream sockets dual-stack but GFE never did IPv6; when only IPv6 resolved,
    /// that is what we use.
    pub address: IpAddr,
    /// The NVHTTP HTTP port (TLS port is asked from `/serverinfo`, not assumed).
    pub http_port: u16,
}

impl HostCandidate {
    /// Map a resolved mDNS instance, or `None` when it carried no usable address.
    #[must_use]
    pub fn from_resolved(service: &DiscoveredService) -> Option<Self> {
        let address = service
            .addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| service.addresses.first())
            .copied()?;
        Some(Self {
            fullname: service.fullname.clone(),
            name: service.instance.clone(),
            address,
            http_port: service.port,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn resolved(addresses: Vec<IpAddr>) -> DiscoveredService {
        DiscoveredService {
            fullname: "somepc._nvstream._tcp.local.".into(),
            instance: "somepc".into(),
            hostname: "somepc.local.".into(),
            addresses,
            port: 47989,
            txt: vec![],
        }
    }

    #[test]
    fn prefers_ipv4_when_both_families_resolved() {
        let svc = resolved(vec![
            "fe80::1".parse().unwrap(),
            "10.0.0.7".parse().unwrap(),
        ]);
        let host = HostCandidate::from_resolved(&svc).unwrap();
        assert_eq!(host.address, "10.0.0.7".parse::<IpAddr>().unwrap());
        assert_eq!(host.http_port, 47989);
    }

    #[test]
    fn takes_ipv6_when_it_is_all_there_is() {
        let svc = resolved(vec!["fe80::1".parse().unwrap()]);
        let host = HostCandidate::from_resolved(&svc).unwrap();
        assert!(host.address.is_ipv6());
    }

    #[test]
    fn refuses_an_addressless_advertisement() {
        assert!(HostCandidate::from_resolved(&resolved(vec![])).is_none());
    }
}
