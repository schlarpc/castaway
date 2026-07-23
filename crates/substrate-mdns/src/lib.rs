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

pub mod error;
pub mod service;

pub use error::MdnsError;
pub use service::MdnsService;

use mdns_sd::ServiceDaemon;
use tracing::{debug, info};

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

    /// Advertise a service instance. Addresses are auto-detected across interfaces.
    ///
    /// # Errors
    /// [`MdnsError`] if the service type is malformed or registration fails.
    pub fn advertise(&mut self, service: &MdnsService) -> Result<(), MdnsError> {
        let info = service.to_service_info()?;
        let fullname = info.get_fullname().to_string();
        self.daemon
            .register(info)
            .map_err(|e| MdnsError::Register(e.to_string()))?;
        info!(service = %service.service_type, instance = %service.instance, "mDNS advertised");
        self.registered.push(fullname);
        Ok(())
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
