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

    async fn send_alive(&self, sock: &UdpSocket, multicast: SocketAddr) {
        for adv in &self.devices {
            let resp = self.response_for(adv);
            for target in adv.device.targets() {
                let msg = resp.notify_alive(&target);
                let _ = sock.send_to(msg.as_bytes(), multicast).await;
            }
        }
    }

    async fn send_byebye(&self, sock: &UdpSocket, multicast: SocketAddr) {
        for adv in &self.devices {
            let resp = self.response_for(adv);
            for target in adv.device.targets() {
                let msg = resp.notify_byebye(&target);
                let _ = sock.send_to(msg.as_bytes(), multicast).await;
            }
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
}
