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

/// The operational service a *commissioned* client resolves the panel by (#173).
///
/// `_tcp` in the label whatever the transport actually is — the Matter spec's own
/// quirk, and what `rs-matter`'s resolver queries.
pub const OPERATIONAL_SERVICE: &str = "_matter._tcp";

/// The Casting Video Player device type, as the `DT` key wants it (decimal).
const DEVICE_TYPE_CASTING_VIDEO_PLAYER: u16 = 0x0023;

/// Session idle retransmission interval, milliseconds — the `SII` TXT key. Faster than
/// the spec default because the panel is a mains-powered box on Ethernet, not a battery
/// node; a client that has to guess assumes the slower defaults.
const SESSION_IDLE_INTERVAL_MS: u32 = 500;

/// Session active retransmission interval, milliseconds — the `SAI` TXT key.
const SESSION_ACTIVE_INTERVAL_MS: u32 = 300;

/// Session active threshold, milliseconds — the `SAT` TXT key; the spec's default,
/// stated rather than left to be assumed because `rs-matter` seeds a fresh CASE
/// session's MRP parameters from the resolve TXT (it does not yet exchange them in
/// Sigma1/2).
const SESSION_ACTIVE_THRESHOLD_MS: u16 = 4000;

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
        .with_txt("SII", SESSION_IDLE_INTERVAL_MS.to_string())
        .with_txt("SAI", SESSION_ACTIVE_INTERVAL_MS.to_string())
}

/// The instance label of the operational record: `<compressed-fabric-id>-<node-id>`,
/// each sixteen uppercase hex digits (Core spec §4.3.1; also exactly what
/// `rs_matter::transport::network::MatterRemoteService::instance_name` queries for).
#[must_use]
pub fn operational_instance(compressed_fabric_id: u64, node_id: u64) -> String {
    format!("{compressed_fabric_id:016X}-{node_id:016X}")
}

/// Build the `_matter._tcp` operational advertisement (#173).
///
/// This record is how a *returning* phone finds the panel: `rs-matter`'s
/// `Transport::initiate` reuses a live CASE session when it has one and otherwise
/// resolves this instance name to establish a fresh one — an idle timeout, a panel
/// restart or a phone that slept overnight all land on the second branch, and without
/// this record that branch has nothing to resolve.
///
/// Not part of [`crate::adapter::MatterAdapter`]'s startup advertisement list, because
/// it depends on fabric state: the instance name *is* the compressed fabric id, and
/// there is nobody entitled to resolve it until the fabric has a member. The adapter
/// publishes it through the shared responder's late handle once that is true.
///
/// The `SII`/`SAI`/`SAT` TXT keys seed the new CASE session's MRP parameters on the
/// client (`rs-matter` reads them back off the resolve because it does not yet exchange
/// them in CASE Sigma1/2). The `_I<compressed-fabric-id>` sub-type is Core spec §4.3.1's
/// per-fabric browse narrowing, published because real controllers use it.
#[must_use]
pub fn operational_service(
    compressed_fabric_id: u64,
    node_id: u64,
    host: &str,
    port: u16,
) -> MdnsService {
    MdnsService::new(
        OPERATIONAL_SERVICE,
        operational_instance(compressed_fabric_id, node_id),
        host,
        port,
    )
    .with_txt("SII", SESSION_IDLE_INTERVAL_MS.to_string())
    .with_txt("SAI", SESSION_ACTIVE_INTERVAL_MS.to_string())
    .with_txt("SAT", SESSION_ACTIVE_THRESHOLD_MS.to_string())
    .with_subtype(format!("I{compressed_fabric_id:016X}"))
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

    /// The operational instance name is the resolve key `rs-matter` queries for —
    /// `{:016X}-{:016X}` of the compressed fabric id and node id — so its shape is the
    /// difference between a returning phone reconnecting and it being stranded (#173).
    #[test]
    fn the_operational_instance_name_is_the_spec_shape() {
        // Zero-padded to sixteen digits each, uppercase, dash-joined.
        assert_eq!(
            operational_instance(0xBC5C_01A6_1C48_892F, 1),
            "BC5C01A61C48892F-0000000000000001"
        );
        assert_eq!(
            operational_instance(0x1, 0xFFFF_FFFF_FFFF_FFFF),
            "0000000000000001-FFFFFFFFFFFFFFFF"
        );
    }

    /// The operational record: `_matter._tcp`, the MRP-seeding TXT keys, and the
    /// per-fabric sub-type — what `Transport::initiate`'s resolve branch reads.
    #[test]
    fn the_operational_record_carries_the_session_parameters() {
        let service = operational_service(0xBC5C_01A6_1C48_892F, 1, "castaway", 5540);
        assert_eq!(service.service_type, OPERATIONAL_SERVICE);
        assert_eq!(
            service.instance.as_str(),
            "BC5C01A61C48892F-0000000000000001"
        );
        assert_eq!(service.port, 5540);

        let txt: std::collections::HashMap<_, _> = service.txt.iter().cloned().collect();
        assert_eq!(txt["SII"], "500");
        assert_eq!(txt["SAI"], "300");
        assert_eq!(txt["SAT"], "4000");

        // Core §4.3.1's per-fabric browse narrowing, as one encodable sub-type.
        assert_eq!(
            service.qualified_subtype(&service.subtypes[0]).unwrap(),
            "_IBC5C01A61C48892F._sub._matter._tcp.local."
        );
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
