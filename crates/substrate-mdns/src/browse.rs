//! Browsing — the *other* direction. Everything else in this crate advertises castaway
//! to senders; GameStream inverts the roles (the panel dials out to a Sunshine host,
//! D37), so it needs to find services rather than be found. Same daemon, same typed
//! boundary: `mdns_sd` types stop here, callers see [`BrowseEvent`]/[`DiscoveredService`].

use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tracing::debug;

use crate::error::MdnsError;
use crate::service::qualify_type;

/// A resolved service instance seen on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredService {
    /// The full instance name (`instance._type._tcp.local.`) — the stable identity a
    /// removal event refers back to.
    pub fullname: String,
    /// The instance label alone, human-readable (`Sunshine on somepc`).
    pub instance: String,
    /// The advertised host name (`somepc.local.`).
    pub hostname: String,
    /// Every address the instance resolved to. May legitimately be empty when a
    /// record advertises a name but no A/AAAA reached us yet.
    pub addresses: Vec<IpAddr>,
    /// The advertised port.
    pub port: u16,
    /// TXT records, in received order.
    pub txt: Vec<(String, String)>,
}

/// What a browse yields over time. Resolutions repeat when records refresh; treat
/// [`BrowseEvent::Resolved`] as upsert-by-`fullname`, not append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseEvent {
    /// An instance is present and resolved.
    Resolved(DiscoveredService),
    /// An instance's records expired or were withdrawn.
    Removed {
        /// The `fullname` a prior [`BrowseEvent::Resolved`] carried.
        fullname: String,
    },
}

/// An active browse for one service type. Dropping it stops the query.
pub struct Browser {
    daemon: ServiceDaemon,
    ty_domain: String,
    rx: mdns_sd::Receiver<ServiceEvent>,
}

impl Browser {
    pub(crate) fn start(daemon: &ServiceDaemon, service_type: &str) -> Result<Self, MdnsError> {
        let ty_domain = qualify_type(service_type)?;
        let rx = daemon
            .browse(&ty_domain)
            .map_err(|e| MdnsError::Browse(e.to_string()))?;
        Ok(Self {
            daemon: daemon.clone(),
            ty_domain,
            rx,
        })
    }

    /// The next resolution or removal. `None` means the daemon shut down, which ends
    /// the browse for good — callers should treat it like a closed channel.
    ///
    /// Cancel-safe: an event is only taken off the channel when it is returned.
    pub async fn next(&mut self) -> Option<BrowseEvent> {
        loop {
            match self.rx.recv_async().await.ok()? {
                ServiceEvent::ServiceResolved(info) => {
                    let mut addresses: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
                    // HashSet order is per-process random; sorted so the same records
                    // always produce the same event (fixtures pin this).
                    addresses.sort_unstable();
                    let instance = info
                        .get_fullname()
                        .split_once("._")
                        .map_or_else(|| info.get_fullname().to_string(), |(i, _)| i.to_string());
                    return Some(BrowseEvent::Resolved(DiscoveredService {
                        fullname: info.get_fullname().to_string(),
                        instance,
                        hostname: info.get_hostname().to_string(),
                        addresses,
                        port: info.get_port(),
                        txt: info
                            .get_properties()
                            .iter()
                            .map(|p| (p.key().to_string(), p.val_str().to_string()))
                            .collect(),
                    }));
                }
                ServiceEvent::ServiceRemoved(_ty, fullname) => {
                    return Some(BrowseEvent::Removed { fullname });
                }
                other => debug!(?other, "mDNS browse event ignored"),
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(&self.ty_domain);
    }
}
