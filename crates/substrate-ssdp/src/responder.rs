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
use crate::message::{SearchTarget, SsdpRequest, SsdpResponse};
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
        self.run_on(sock, shutdown).await
    }

    /// The same loop, on a socket the caller has already bound and joined.
    ///
    /// Exists so the announcement behaviour can be observed. [`bind_multicast`] sets
    /// `IP_MULTICAST_LOOP` off — correct on the panel, and it means no socket on this host
    /// can see our `NOTIFY`s, so a test that binds a listener beside the responder watches
    /// silence and cannot tell it from a responder that built its messages and never sent
    /// them. That is exactly the gap (#202): `alive_messages` and `byebye_messages` are
    /// tested as string sets, and until now nothing had executed the ticker or the
    /// shutdown branch, or seen a datagram leave.
    ///
    /// What this does *not* cover, and cannot: the socket options themselves — the group
    /// join, `IP_MULTICAST_IF` on a multi-homed box, TTL 4. Those are `bind_multicast`'s
    /// and are a real LAN's to check.
    ///
    /// # Errors
    /// [`SsdpError::Io`] if the socket fails.
    pub async fn run_on(
        self,
        sock: UdpSocket,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), SsdpError> {
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
        for msg in self.search_replies(&st) {
            if let Err(e) = sock.send_to(msg.as_bytes(), from).await {
                warn!(error = %e, %from, "SSDP unicast reply failed");
            }
        }
        debug!(%from, "answered M-SEARCH");
    }

    /// Every `200 OK` an `M-SEARCH` for `st` draws: one per selected target per device.
    /// Pure — [`Self::handle_datagram`] sends each back to the searcher.
    ///
    /// Separated out because the selection is where two root devices on one responder
    /// can go wrong (#202): DLNA and DIAL both advertise `upnp:rootdevice` and the bare
    /// `uuid:` target, so if the two devices shared a UUID, a root search would draw two
    /// replies carrying *one* USN with two LOCATIONs — and a control point that dedupes
    /// on USN (most do) keeps an arbitrary one. Whether that happens is decided entirely
    /// by what this function emits, so it is the thing to pin.
    fn search_replies(&self, st: &SearchTarget) -> Vec<String> {
        let mut msgs = Vec::new();
        for adv in &self.devices {
            let resp = self.response_for(adv);
            for target in adv.device.targets().iter().filter(|t| st.selects(t)) {
                msgs.push(resp.search_ok(target));
            }
        }
        msgs
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

    /// The uuids the two-root-device tests advertise. Distinctive on purpose: the wire
    /// test below shares a real multicast group with every other test process on this
    /// host, so its assertions filter replies down to the ones carrying these.
    const DLNA_UUID: &str = "uuid:5d1a0000-aaaa-4bbb-8ccc-0000000d10a1";
    const DIAL_UUID: &str = "uuid:5d1a0000-aaaa-4bbb-8ccc-0000000d1a12";
    const DIAL_ST: &str = "urn:dial-multiscreen-org:service:dial:1";

    /// The shape the app actually runs (crates/app/src/main.rs `serve`): one responder,
    /// two root devices — the DLNA MediaRenderer and DIAL's tvdevice — each with its own
    /// UUID and its own description document.
    fn two_root_devices(config: ResponderConfig) -> Responder {
        Responder::new(config)
            .advertise(
                SsdpDevice {
                    uuid: DLNA_UUID.into(),
                    device_type: "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                    services: vec!["urn:schemas-upnp-org:service:AVTransport:1".into()],
                },
                "/dlna/description.xml",
            )
            .advertise(
                SsdpDevice {
                    uuid: DIAL_UUID.into(),
                    device_type: "urn:schemas-upnp-org:device:tvdevice:1".into(),
                    services: vec![DIAL_ST.into()],
                },
                "/dial/dd.xml",
            )
    }

    /// The header value of `name` in an SSDP reply, or None.
    fn header<'a>(reply: &'a str, name: &str) -> Option<&'a str> {
        reply.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    /// A targeted DIAL search draws exactly DIAL's answer: ST echoed, the USN composed
    /// from DIAL's own UUID, and the LOCATION pointing at the DIAL device description
    /// rather than the DLNA one. This is the selection a sender's cast button performs
    /// (#202's item 4) and nothing had ever executed it — `handle_datagram` had no test
    /// at any tier, so a responder that answered a DIAL search with the DLNA device (or
    /// with nothing) passed every gate in the tree.
    #[test]
    fn a_dial_search_draws_dials_reply_with_its_own_location_and_usn() {
        let replies = two_root_devices(cfg()).search_replies(&SearchTarget::parse(DIAL_ST));
        assert_eq!(
            replies.len(),
            1,
            "one device advertises the DIAL service, so one reply: {replies:?}"
        );
        let reply = &replies[0];
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        assert_eq!(header(reply, "ST"), Some(DIAL_ST), "ST must be echoed");
        assert_eq!(
            header(reply, "USN"),
            Some(format!("{DIAL_UUID}::{DIAL_ST}").as_str()),
            "the USN is DIAL's uuid composed with the searched target"
        );
        assert_eq!(
            header(reply, "LOCATION"),
            Some("http://127.0.0.1:8080/dial/dd.xml"),
            "the LOCATION must be the DIAL description — a sender fetches it for \
             Application-URL, which the DLNA description does not carry"
        );
    }

    /// Two root devices on one responder never share a USN — the collision `device_uuid`
    /// in crates/app exists to prevent. Both devices answer `upnp:rootdevice` (and
    /// `ssdp:all`); with one UUID between them those answers carry an identical
    /// `uuid:…::upnp:rootdevice` USN and different LOCATIONs, and a control point that
    /// dedupes on USN keeps one arbitrarily — when DLNA's won, DIAL was invisible.
    #[test]
    fn two_root_devices_never_answer_with_one_usn() {
        let r = two_root_devices(cfg());
        let roots = r.search_replies(&SearchTarget::RootDevice);
        assert_eq!(
            roots.len(),
            2,
            "both devices answer a root search: {roots:?}"
        );
        let usns: Vec<&str> = roots.iter().filter_map(|m| header(m, "USN")).collect();
        assert_eq!(usns.len(), 2, "{roots:?}");
        assert_ne!(
            usns[0], usns[1],
            "an identical root USN is the DIAL/DLNA collision — a control point \
             deduping on USN would drop one device"
        );

        // The property behind it, over the whole advertised surface: one USN, one
        // LOCATION. `ssdp:all` selects every target of every device.
        let mut locations: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for reply in &r.search_replies(&SearchTarget::All) {
            let usn = header(reply, "USN").unwrap();
            let location = header(reply, "LOCATION").unwrap();
            if let Some(previous) = locations.insert(usn, location) {
                assert_eq!(
                    previous, location,
                    "USN {usn} answered with two LOCATIONs — the collision a control \
                     point resolves by dropping one of them"
                );
            }
        }
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

    /// Bind a socket on the loopback interface, joined to the SSDP group.
    ///
    /// `loop_back` is the whole reason this is here rather than [`bind_multicast`]: the
    /// production socket sets `IP_MULTICAST_LOOP` **off**, which is right on the panel and
    /// means nothing on this host can observe our announcements. The listener needs the
    /// join; the responder needs loopback on so the listener sees anything at all.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test socket on the loopback interface; the registry governs the \
                  panel's binds"
    )]
    fn test_socket(loop_back: bool) -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket
            .bind(&SocketAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT)).into())?;
        socket.join_multicast_v4(&SSDP_MULTICAST_ADDR, &Ipv4Addr::LOCALHOST)?;
        socket.set_multicast_if_v4(&Ipv4Addr::LOCALHOST)?;
        socket.set_multicast_loop_v4(loop_back)?;
        socket.set_nonblocking(true)?;
        UdpSocket::from_std(socket.into())
    }

    /// The announcements actually leave the socket, repeatedly, and stop with a byebye.
    ///
    /// `alive_messages` and `byebye_messages` were tested as string sets, and nothing had
    /// ever executed `run`'s ticker or its shutdown branch — so a responder that built
    /// every message correctly and sent none of them, or that announced once at startup
    /// and then went quiet, passed every gate in the tree. The symptom in the room is a
    /// device that appears, works, and vanishes from every picker one `max-age` later
    /// (#202).
    ///
    /// `max_age = 4` makes the interval two seconds, so two bursts fit in a test.
    #[tokio::test(flavor = "multi_thread")]
    async fn announcements_reach_the_wire_and_stop_with_a_byebye() {
        let Ok(listener) = test_socket(false) else {
            eprintln!("skipping: multicast unavailable here");
            return;
        };
        let Ok(sender) = test_socket(true) else {
            eprintln!("skipping: multicast unavailable here");
            return;
        };

        let responder = Responder::new(ResponderConfig {
            max_age: 4,
            ..cfg()
        })
        .advertise(
            SsdpDevice {
                uuid: "uuid:abcd".into(),
                device_type: "urn:schemas-upnp-org:device:MediaRenderer:1".into(),
                services: vec!["urn:schemas-upnp-org:service:AVTransport:1".into()],
            },
            "/dlna/description.xml",
        );
        let targets = 4; // root + uuid + device type + one service
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            let _ = responder
                .run_on(sender, async {
                    let _ = stop_rx.await;
                })
                .await;
        });

        // Collect for long enough that a once-at-startup announcer is distinguishable
        // from one that keeps its promise: two intervals plus the burst gaps.
        let mut alive = 0usize;
        let mut buf = vec![0u8; 2048];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5) + NOTIFY_REPEAT_GAP * 4;
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok((n, _))) =
                tokio::time::timeout(Duration::from_millis(500), listener.recv_from(&mut buf))
                    .await
            else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]);
            if text.starts_with("NOTIFY") && text.contains("ssdp:alive") {
                alive += 1;
            }
        }

        // Two bursts, each repeating at least twice, one message per target.
        //
        // The floor is a literal `2` and **not** `NOTIFY_REPEATS`, which is the mistake
        // this originally made: an expectation computed from the constant moves with it,
        // so dropping the repeats to 1 passed. The exact value is pinned next door by
        // `one_alive_round_covers_every_target_and_the_burst_repeats_it`; what this
        // asserts is the property that survives a change to it: announcements keep
        // arriving, in bursts, after startup.
        const MIN_REPEATS: usize = 2;
        let due = targets * MIN_REPEATS * 2;
        assert!(
            alive >= due,
            "saw {alive} alive NOTIFYs; at least {due} were due over two intervals. A \
             responder that announces once at startup and never again is a device that \
             vanishes from every picker one max-age later"
        );

        // …and shutting down says goodbye, once per target, promptly. Without this a
        // control point holds a stale entry for the whole cache lifetime and offers the
        // user a device that is not there.
        let _ = stop_tx.send(());
        let mut byebye = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && byebye < targets {
            let Ok(Ok((n, _))) =
                tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buf))
                    .await
            else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]);
            if text.starts_with("NOTIFY") && text.contains("ssdp:byebye") {
                byebye += 1;
            }
        }
        assert_eq!(
            byebye, targets,
            "every advertised target must be withdrawn, not just the root device"
        );

        running.await.unwrap();
    }

    /// A control point's search socket: an ephemeral loopback port that sends to the
    /// SSDP group and collects the unicast replies. Loopback is on so the responder's
    /// group-joined socket sees our datagram at all; the replies come back unicast to
    /// the ephemeral port, so no other test's socket can steal them.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test searcher on an ephemeral loopback port; the registry governs \
                  the panel's binds"
    )]
    fn searcher_socket() -> std::io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.bind(&SocketAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).into())?;
        socket.set_multicast_if_v4(&Ipv4Addr::LOCALHOST)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_nonblocking(true)?;
        UdpSocket::from_std(socket.into())
    }

    /// Search for `st` against the live group and collect replies until `enough` of the
    /// ones carrying our test uuids have arrived (or a deadline expires). The group is
    /// real and shared — another test process's responder may answer too — so the filter
    /// is what keeps this deterministic. The search is re-sent on each poll timeout
    /// because SSDP is bare UDP: a lost datagram is a slow test, not a failed one.
    async fn search_ours(sock: &UdpSocket, st: &str, enough: usize) -> Vec<String> {
        let request = format!(
            "M-SEARCH * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             MAN: \"ssdp:discover\"\r\n\
             MX: 1\r\n\
             ST: {st}\r\n\
             \r\n"
        );
        let group: SocketAddr = SocketAddrV4::new(SSDP_MULTICAST_ADDR, SSDP_PORT).into();
        let mut ours = Vec::new();
        let mut buf = vec![0u8; 2048];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline && ours.len() < enough {
            sock.send_to(request.as_bytes(), group).await.unwrap();
            let poll_until = tokio::time::Instant::now() + Duration::from_millis(500);
            while tokio::time::Instant::now() < poll_until && ours.len() < enough {
                let Ok(Ok((n, _))) =
                    tokio::time::timeout(Duration::from_millis(200), sock.recv_from(&mut buf))
                        .await
                else {
                    continue;
                };
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let usn = header(&text, "USN").unwrap_or_default();
                if (usn.contains(DLNA_UUID) || usn.contains(DIAL_UUID))
                    && !ours
                        .iter()
                        .any(|seen: &String| header(seen, "USN") == header(&text, "USN"))
                {
                    ours.push(text);
                }
            }
        }
        ours
    }

    /// The positive DIAL discovery path on a real wire (#202's item 4): an `M-SEARCH`
    /// datagram for `ST: urn:dial-multiscreen-org:service:dial:1` reaches the running
    /// responder over the multicast group, and the unicast `200 OK` that comes back
    /// carries DIAL's own USN and LOCATION — while a root search draws two answers with
    /// *distinct* USNs, which is the collision property `device_uuid` protects. The pure
    /// tests above pin the same selection; this one proves the datagrams cross a socket,
    /// through the same `run_on` seam the NOTIFY test uses.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dial_msearch_is_answered_on_the_wire_without_colliding_with_dlna() {
        let Ok(responder_sock) = test_socket(false) else {
            eprintln!("skipping: multicast unavailable here");
            return;
        };
        let Ok(searcher) = searcher_socket() else {
            eprintln!("skipping: multicast unavailable here");
            return;
        };

        let responder = two_root_devices(cfg());
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let running = tokio::spawn(async move {
            let _ = responder
                .run_on(responder_sock, async {
                    let _ = stop_rx.await;
                })
                .await;
        });

        // The targeted search a sender's cast button performs.
        let dial = search_ours(&searcher, DIAL_ST, 1).await;
        assert_eq!(dial.len(), 1, "no reply to the DIAL search: {dial:?}");
        let reply = &dial[0];
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        assert_eq!(header(reply, "ST"), Some(DIAL_ST), "{reply}");
        assert_eq!(
            header(reply, "USN"),
            Some(format!("{DIAL_UUID}::{DIAL_ST}").as_str()),
            "{reply}"
        );
        assert_eq!(
            header(reply, "LOCATION"),
            Some("http://127.0.0.1:8080/dial/dd.xml"),
            "{reply}"
        );

        // The root search both devices answer: two replies, two USNs.
        let roots = search_ours(&searcher, "upnp:rootdevice", 2).await;
        assert_eq!(
            roots.len(),
            2,
            "both root devices must answer a root search: {roots:?}"
        );
        let usns: Vec<&str> = roots.iter().filter_map(|m| header(m, "USN")).collect();
        assert_ne!(
            usns[0], usns[1],
            "one USN for two devices is the DIAL/DLNA collision on a live socket"
        );

        let _ = stop_tx.send(());
        running.await.unwrap();
    }
}
