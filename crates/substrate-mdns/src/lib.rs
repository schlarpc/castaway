//! # substrate-mdns
//!
//! One mDNS-SD responder advertising every protocol's service instance — the cleanest
//! shared layer (architecture §1c). AirPlay, RAOP, Cast, and Spotify Connect all
//! register their `_service._tcp` with TXT records through the single [`MdnsResponder`].
//!
//! `mdns-sd` owns the wire framing; this crate is the typed, validated boundary around
//! it. Service-type validation and TXT construction are pure and unit-tested; the
//! daemon itself is I/O.
#![forbid(unsafe_code)]

pub mod browse;
pub mod error;
pub mod service;

pub use browse::{BrowseEvent, Browser, DiscoveredService};
pub use error::MdnsError;
pub use service::{InstanceLabel, MdnsService};

use mdns_sd::ServiceDaemon;
use tracing::{debug, info, warn};

/// The mDNS port. `mdns-sd` binds it internally (UDP, `SO_REUSEADDR`/`SO_REUSEPORT`,
/// joined to [`MDNS_GROUP`]) — the constant exists so the network-surface registry
/// (`crates/app/src/surface.rs`) can name the port from the same crate that owns the
/// socket, rather than repeating the number.
pub const MDNS_PORT: u16 = 5353;

/// The IPv4 multicast group mDNS queries and answers travel on.
pub const MDNS_GROUP: std::net::Ipv4Addr = std::net::Ipv4Addr::new(224, 0, 0, 251);

/// The shared mDNS responder. Construct once, then [`Self::advertise`] each service.
pub struct MdnsResponder {
    daemon: ServiceDaemon,
    /// Fully-qualified names we registered, so we can unregister on drop.
    registered: Vec<String>,
}

impl MdnsResponder {
    /// Create the shared daemon. It owns port 5353; on the kiosk box Avahi/Bonjour
    /// should be disabled so there's no contention (see OPEN-QUESTIONS Q5).
    ///
    /// # Errors
    /// [`MdnsError::Daemon`] if the daemon can't be created.
    pub fn new() -> Result<Self, MdnsError> {
        let daemon = ServiceDaemon::new().map_err(|e| MdnsError::Daemon(e.to_string()))?;
        Ok(Self {
            daemon,
            registered: Vec::new(),
        })
    }

    /// Restrict the responder to the interface holding `addr` — the LAN the receiver
    /// actually serves.
    ///
    /// Without this the daemon advertises and answers on *every* interface, and each
    /// A record carries every address. On the dev box that meant the Tailscale CGNAT
    /// address rode along in every advertisement: senders resolved two addresses,
    /// pickers browsing multiple interfaces listed the device twice, and whichever
    /// client connected over the tunnel found services that bind and verify against
    /// the LAN. A receiver is a LAN appliance; it should advertise like one.
    ///
    /// # Errors
    /// [`MdnsError::Daemon`] if the daemon refuses the interface selection.
    pub fn restrict_to(&mut self, addr: std::net::IpAddr) -> Result<(), MdnsError> {
        self.daemon
            .disable_interface(mdns_sd::IfKind::All)
            .map_err(|e| MdnsError::Daemon(e.to_string()))?;
        self.daemon
            .enable_interface(mdns_sd::IfKind::Addr(addr))
            .map_err(|e| MdnsError::Daemon(e.to_string()))
    }

    /// Advertise a service instance. Addresses are auto-detected across interfaces.
    ///
    /// # Errors
    /// [`MdnsError`] if the service type is malformed or registration fails.
    pub fn advertise(&mut self, service: &MdnsService) -> Result<(), MdnsError> {
        // A rewritten instance label changes the name a picker shows, so say so once
        // rather than letting the device quietly appear under a different name.
        if let Some(requested) = service.instance.rewritten_from() {
            warn!(
                service = %service.service_type,
                %requested,
                advertised = %service.instance,
                "instance name is not encodable as one DNS label; advertising a repaired name"
            );
        }
        let info = service.to_service_info()?;
        let fullname = info.get_fullname().to_string();
        self.daemon
            .register(info)
            .map_err(|e| MdnsError::Register(e.to_string()))?;
        info!(service = %service.service_type, instance = %service.instance, "mDNS advertised");
        self.registered.push(fullname);
        Ok(())
    }

    /// Browse the LAN for instances of a service type (`_nvstream._tcp`). The returned
    /// [`Browser`] yields resolutions and removals until dropped; multiple concurrent
    /// browses on the one daemon are fine.
    ///
    /// # Errors
    /// [`MdnsError::InvalidServiceType`] for a malformed type,
    /// [`MdnsError::Browse`] if the daemon refuses the query.
    pub fn browse(&self, service_type: &str) -> Result<Browser, MdnsError> {
        Browser::start(&self.daemon, service_type)
    }

    /// Stop advertising everything (also happens on drop). Best-effort.
    pub fn shutdown(&mut self) {
        for fullname in self.registered.drain(..) {
            let _ = self.daemon.unregister(&fullname);
            debug!(%fullname, "mDNS unregistered");
        }
    }
}

impl Drop for MdnsResponder {
    fn drop(&mut self) {
        self.shutdown();
    }
}
