//! The SSDP responder actor: owns the UDP 1900 multicast socket, answers `M-SEARCH`
//! with unicast `200 OK`, emits periodic multicast `NOTIFY ssdp:alive`, and sends
//! `ssdp:byebye` on graceful shutdown. All message *content* comes from the pure
//! [`crate::message`] layer; this file is just sockets and timing.

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

use crate::device::SsdpDevice;
use crate::error::SsdpError;
use crate::message::{SsdpRequest, SsdpResponse};
use crate::{SSDP_MULTICAST_ADDR, SSDP_PORT};

/// Configuration for the responder.
#[derive(Debug, Clone)]
pub struct ResponderConfig {
    /// Local interface IPv4 used for the multicast join and as the `LOCATION` host.
    pub interface: Ipv4Addr,
    /// The HTTP port the description host listens on (goes into `LOCATION`).
    pub http_port: u16,
    /// `SERVER` header value.
    pub server: String,
    /// `CACHE-CONTROL` max-age (seconds); the re-announce interval is half of this.
    pub max_age: u32,
}

impl ResponderConfig {
    /// The interval between `NOTIFY ssdp:alive` bursts — half of `max_age`, a common
    /// convention so a controller sees at least two announcements per cache lifetime.
    #[must_use]
    pub fn notify_interval(&self) -> Duration {
        Duration::from_secs(u64::from(self.max_age.max(2)) / 2)
    }
}

/// How many times each `NOTIFY ssdp:alive` round is sent, [`NOTIFY_REPEAT_GAP`] apart.
///
/// SSDP rides bare UDP with no retry of its own, and UDA §1.2 tells advertisers to
/// send each announcement more than once for exactly that reason — every real stack
/// repeats the burst 2–3 times. One lost datagram per interval used to mean half a
/// cache lifetime of invisibility.
const NOTIFY_REPEATS: u32 = 3;

/// The pause between repeats of an alive burst. Small and fixed: the spec asks only
/// that repeats not be back-to-back, and the responder is single-tasked, so a long gap
/// here would delay M-SEARCH answers.
const NOTIFY_REPEAT_GAP: Duration = Duration::from_millis(120);

/// One device advertised by the responder, with the path its description is served at.
struct Advertised {
    device: SsdpDevice,
    description_path: String,
}

/// The SSDP responder. Register devices, then [`Responder::run`] it to completion.
pub struct Responder {
    config: ResponderConfig,
    devices: Vec<Advertised>,
}

impl Responder {
    /// Create an empty responder.
    #[must_use]
    pub fn new(config: ResponderConfig) -> Self {
        Self {
            config,
            devices: Vec::new(),
        }
    }

    /// Advertise `device`, whose description XML is served at `description_path`
    /// (e.g. `/dlna/description.xml`).
    #[must_use]
    pub fn advertise(mut self, device: SsdpDevice, description_path: impl Into<String>) -> Self {
        self.devices.push(Advertised {
            device,
            description_path: description_path.into(),
        });
        self
    }

    /// The `LOCATION` URL for a device's description.
    fn location_for(&self, adv: &Advertised) -> String {
        format!(
            "http://{}:{}{}",
            self.config.interface, self.config.http_port, adv.description_path
        )
    }

    fn response_for(&self, adv: &Advertised) -> SsdpResponse {
        SsdpResponse {
            location: self.location_for(adv),
            server: self.config.server.clone(),
            max_age: self.config.max_age,
        }
    }

    /// Run the responder until `shutdown` resolves, then send `byebye` and return.
    ///
    /// # Errors
    /// [`SsdpError::Io`] if the socket can't be created/bound.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), SsdpError> {
        let sock = bind_multicast(self.config.interface)?;
        let multicast: SocketAddr = SocketAddrV4::new(SSDP_MULTICAST_ADDR, SSDP_PORT).into();
        let mut buf = vec![0u8; 2048];
        let mut ticker = tokio::time::interval(self.config.notify_interval());

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => {
                    self.send_byebye(&sock, multicast).await;
                    return Ok(());
                }
                _ = ticker.tick() => {
                    self.send_alive(&sock, multicast).await;
                }
                res = sock.recv_from(&mut buf) => {
                    match res {
                        Ok((n, from)) => self.handle_datagram(&sock, &buf[..n], from).await,
                        Err(e) => warn!(error = %e, "SSDP recv failed"),
                    }
                }
            }
        }
    }

    async fn handle_datagram(&self, sock: &UdpSocket, data: &[u8], from: SocketAddr) {
        let req = match SsdpRequest::parse(data) {
            Ok(r) => r,
            Err(_) => return, // not ours / malformed; ignore silently
        };
        let SsdpRequest::Search { st, .. } = req else {
            return; // NOTIFY from others — ignore
        };
        for adv in &self.devices {
            let resp = self.response_for(adv);
            for target in adv.device.targets().iter().filter(|t| st.selects(t)) {
                let msg = resp.search_ok(target);
                if let Err(e) = sock.send_to(msg.as_bytes(), from).await {
                    warn!(error = %e, %from, "SSDP unicast reply failed");
                }
            }
        }
        debug!(%from, "answered M-SEARCH");
    }

    /// Every `ssdp:alive` NOTIFY one announcement round carries: one per target per
    /// device. Pure — the repeat/burst policy lives in [`Self::send_alive`].
    fn alive_messages(&self) -> Vec<String> {
        let mut msgs = Vec::new();
        for adv in &self.devices {
            let resp = self.response_for(adv);
            for target in adv.device.targets() {
                msgs.push(resp.notify_alive(&target));
            }
        }
        msgs
    }

    /// Every `ssdp:byebye` NOTIFY a graceful shutdown announces.
    fn byebye_messages(&self) -> Vec<String> {
        let mut msgs = Vec::new();
        for adv in &self.devices {
            let resp = self.response_for(adv);
            for target in adv.device.targets() {
                msgs.push(resp.notify_byebye(&target));
            }
        }
        msgs
    }

    async fn send_alive(&self, sock: &UdpSocket, multicast: SocketAddr) {
        let msgs = self.alive_messages();
        for round in 0..NOTIFY_REPEATS {
            if round > 0 {
                tokio::time::sleep(NOTIFY_REPEAT_GAP).await;
            }
            for msg in &msgs {
                let _ = sock.send_to(msg.as_bytes(), multicast).await;
            }
        }
    }

    async fn send_byebye(&self, sock: &UdpSocket, multicast: SocketAddr) {
        for msg in self.byebye_messages() {
            let _ = sock.send_to(msg.as_bytes(), multicast).await;
        }
    }
}

/// Bind a UDP socket to `0.0.0.0:1900` with address reuse and join the SSDP multicast
/// group on `interface`. Reuse is required so we coexist with other multicast users.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the ssdp/udp 1900 entry in crates/app/src/surface.rs"
)]
fn bind_multicast(interface: Ipv4Addr) -> Result<UdpSocket, SsdpError> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let bind: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT).into();
    socket.bind(&bind.into())?;
    socket.join_multicast_v4(&SSDP_MULTICAST_ADDR, &interface)?;
    // The join says where we *listen*; where our NOTIFYs *leave* is a separate
    // option, and left unset it is the default route — on a multi-homed box (this
    // one has a Tailscale tunnel beside the LAN) that can be the wrong wire
    // entirely, with every announcement disappearing into the tunnel.
    socket.set_multicast_if_v4(&interface)?;
    // UDA 1.0 specifies TTL 4 for SSDP; the kernel default is 1, which a router
    // configured to forward multicast between segments would decrement to nothing.
    socket.set_multicast_ttl_v4(4)?;
    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;
    let std_sock: std::net::UdpSocket = socket.into();
    Ok(UdpSocket::from_std(std_sock)?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn cfg() -> ResponderConfig {
        ResponderConfig {
            interface: Ipv4Addr::LOCALHOST,
            http_port: 8080,
            server: "castaway/0.1 UPnP/1.0".into(),
            max_age: 1800,
        }
    }

    #[test]
    fn notify_interval_is_half_max_age() {
        assert_eq!(cfg().notify_interval(), Duration::from_secs(900));
    }

    #[test]
    fn location_includes_interface_port_and_path() {
        let r = Responder::new(cfg()).advertise(
            SsdpDevice {
                uuid: "uuid:x".into(),
                device_type: "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                services: vec![],
            },
            "/dlna/description.xml",
        );
        assert_eq!(
            r.location_for(&r.devices[0]),
            "http://127.0.0.1:8080/dlna/description.xml"
        );
    }

    #[test]
    fn one_alive_round_covers_every_target_and_the_burst_repeats_it() {
        let r = Responder::new(cfg()).advertise(
            SsdpDevice {
                uuid: "uuid:x".into(),
                device_type: "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                services: vec!["urn:schemas-upnp-org:service:AVTransport:1".into()],
            },
            "/dlna/description.xml",
        );
        // Root, uuid, device type, one service: four targets, four NOTIFYs a round.
        let msgs = r.alive_messages();
        assert_eq!(msgs.len(), r.devices[0].device.targets().len());
        assert!(msgs.iter().all(|m| m.contains("ssdp:alive")), "{msgs:?}");
        // And the byebye set mirrors it, target for target.
        let byes = r.byebye_messages();
        assert_eq!(byes.len(), msgs.len());
        assert!(byes.iter().all(|m| m.contains("ssdp:byebye")), "{byes:?}");
        // The burst policy: repeated because UDP drops, bounded because it is noise.
        // 2–3 is what real stacks do; 1 is the defect this replaces.
        assert!((2..=3).contains(&NOTIFY_REPEATS), "{NOTIFY_REPEATS}");
        assert!(NOTIFY_REPEAT_GAP < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn the_multicast_socket_names_its_egress_interface_and_upnp_ttl() {
        // Read the options back off the live socket: `set_multicast_if_v4` was simply
        // never called (NOTIFYs left via the default route — the tunnel, on a box
        // with one), and the TTL rode the kernel default of 1 where UPnP says 4.
        let sock = match bind_multicast(Ipv4Addr::LOCALHOST) {
            Ok(s) => s,
            // A sandbox that refuses multicast joins can't run this; that absence
            // is environmental, not a defect in the code under test.
            Err(e) => {
                eprintln!("skipping: multicast bind unavailable here ({e})");
                return;
            }
        };
        let raw = socket2::SockRef::from(&sock);
        assert_eq!(raw.multicast_if_v4().unwrap(), Ipv4Addr::LOCALHOST);
        assert_eq!(raw.multicast_ttl_v4().unwrap(), 4);
    }
}
