//! Finding the phone that just asked to be commissioned, and telling phones we exist.
//!
//! Both directions of DNS-SD, on the project's one responder (D26, D45) rather than the
//! one `rs-matter` ships:
//!
//! - **Advertising** `_matterd._udp` — the commissioner service. This is what a Casting
//!   Client browses for to find a TV worth sending a UDC message to, so its TXT record is
//!   the panel's entire first impression.
//! - **Browsing** `_matterc._udp` — commissionable nodes. Inverted from ordinary Matter:
//!   the *phone* starts advertising itself as commissionable after the user types the
//!   passcode, and the panel goes looking for it.

use std::net::SocketAddr;
use std::time::Duration;

use substrate_mdns::{BrowseEvent, Browser, MdnsResponder, MdnsService};

use crate::error::MatterError;
use crate::udc::InstanceName;

/// The commissioner service a Casting Client browses for.
pub const COMMISSIONER_SERVICE: &str = "_matterd._udp";

/// The commissionable-node service a client advertises once it wants commissioning.
pub const COMMISSIONABLE_SERVICE: &str = "_matterc._udp";

/// The Casting Video Player device type, as the `DT` key wants it (decimal).
const DEVICE_TYPE_CASTING_VIDEO_PLAYER: u16 = 0x0023;

/// Build the `_matterd._udp` advertisement.
///
/// The TXT keys are Core spec §4.3.4's commissioner set. `DT` is what makes a phone show
/// this panel under "TVs" rather than as an unrecognised node, and `VP` is what a client
/// checks before it will cast at all.
#[must_use]
pub fn commissioner_service(
    friendly_name: &str,
    host: &str,
    port: u16,
    vendor_id: u16,
    product_id: u16,
) -> MdnsService {
    MdnsService::new(COMMISSIONER_SERVICE, friendly_name, host, port)
        .with_txt("VP", format!("{vendor_id}+{product_id}"))
        .with_txt("DT", DEVICE_TYPE_CASTING_VIDEO_PLAYER.to_string())
        .with_txt("DN", friendly_name)
        // Session idle/active retransmission intervals, in milliseconds. Present because
        // a client that has to guess assumes the spec's defaults, and the panel is a
        // mains-powered box on Ethernet — it can answer faster than a battery node.
        .with_txt("SII", "500")
        .with_txt("SAI", "300")
}

/// Where a commissionable node was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commissionable {
    /// The address to run PASE against.
    pub addr: SocketAddr,
}

/// Watch for the commissionable node named `instance`, giving up after `timeout`.
///
/// The client advertises itself only *after* the user has typed the passcode, so this is
/// a wait on a person, not on a network: the timeout is generous by design and the
/// failure it reports is "nobody typed it", not "mDNS is broken".
///
/// # Errors
/// [`MatterError::CommissioneeNotFound`] on timeout, or [`MatterError::Mdns`] if the
/// browse itself fails.
pub async fn await_commissionable(
    responder: &MdnsResponder,
    instance: &InstanceName,
    timeout: Duration,
) -> Result<Commissionable, MatterError> {
    let browser = responder.browse(COMMISSIONABLE_SERVICE)?;

    tokio::time::timeout(timeout, watch(browser, instance))
        .await
        .map_err(|_| MatterError::CommissioneeNotFound {
            instance: instance.to_string(),
        })?
}

async fn watch(
    mut browser: Browser,
    instance: &InstanceName,
) -> Result<Commissionable, MatterError> {
    while let Some(event) = browser.next().await {
        let BrowseEvent::Resolved(service) = event else {
            continue;
        };

        if !instance_matches(&service.instance, instance) {
            continue;
        }

        // A record can legitimately resolve with no addresses yet — the SRV arrived and
        // the A/AAAA has not. Ignoring it lets the next refresh carry one, rather than
        // failing the whole commissioning on a partial answer.
        let Some(ip) = service.addresses.first() else {
            tracing::debug!(instance = %service.instance, "matter: commissionable node has no address yet");
            continue;
        };

        return Ok(Commissionable {
            addr: SocketAddr::new(*ip, service.port),
        });
    }

    Err(MatterError::CommissioneeNotFound {
        instance: instance.to_string(),
    })
}

/// Whether an mDNS instance label is the node a UDC message named.
///
/// Case-insensitive: the spec says the instance name is uppercase hex, and it is compared
/// against a string a phone put in a *different* message. A phone that is consistent in
/// lowercase would otherwise be undiscoverable — a failure indistinguishable, from the
/// panel, from the user never typing the passcode.
fn instance_matches(label: &str, instance: &InstanceName) -> bool {
    label.eq_ignore_ascii_case(instance.as_str())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_commissioner_record_says_it_is_a_tv() {
        let service = commissioner_service("hackerspace tv", "castaway", 5550, 0xFFF1, 0x8001);
        assert_eq!(service.service_type, COMMISSIONER_SERVICE);
        assert_eq!(service.port, 5550);

        let txt: std::collections::HashMap<_, _> = service.txt.iter().cloned().collect();
        assert_eq!(txt["DT"], "35", "casting video player, in decimal");
        assert_eq!(txt["VP"], "65521+32769");
        assert_eq!(txt["DN"], "hackerspace tv");
    }

    /// The instance name arrives in one message and is matched against another. A phone
    /// that is consistently lowercase must not be undiscoverable.
    #[test]
    fn instance_matching_ignores_case() {
        let instance = InstanceName::new("BC5C01A61C48892F").unwrap();
        assert!(instance_matches("BC5C01A61C48892F", &instance));
        assert!(instance_matches("bc5c01a61c48892f", &instance));
        assert!(!instance_matches("BC5C01A61C48892E", &instance));
        assert!(!instance_matches("", &instance));
    }
}
