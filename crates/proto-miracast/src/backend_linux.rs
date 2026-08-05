//! The Linux [`castaway_core::MiracastBackend`]: a wpa_supplicant control socket, an
//! autonomous P2P group, and the RTSP session that runs over it.
//!
//! Everything protocol-shaped is elsewhere; what is here is the sequence that turns a
//! radio into a socket. The pure half — command formatting and event parsing — lives in
//! [`crate::p2p`] and is tested against captured strings, so what remains untested without
//! hardware is exactly the datagram send/receive and nothing above it.
//!
//! ## Autonomous group owner, not GO negotiation
//!
//! Windows prefers to be the group owner (intent 14); Android insists the sink be one
//! (intent 0). Negotiating splits the difference badly against one of them, and two peers
//! at intent 15 is a defined hard failure. Bringing the group up unilaterally and letting
//! both associate as clients sidesteps the whole state machine — which is also what
//! Microsoft's own Surface Hub does. See `docs/miracast-protocol-notes.md` §1.8.
//!
//! ## Three sockets, not one
//!
//! wpa_supplicant looks like one daemon and is three control surfaces, and getting this
//! wrong is silent:
//!
//! - **The P2P management socket** (`p2p-dev-<iface>` on drivers with P2P-Device
//!   support, the plain interface otherwise) — where P2P commands belong and, crucially,
//!   where P2P events are *delivered*. Attached to the wrong one, every command answers
//!   `OK` and no event ever arrives.
//! - **The group interface's socket** (`p2p-<iface>-N`), which exists only while a group
//!   does — the AP-mode WPS registrar lives there, so `WPS_PBC` must be sent there, and
//!   `AP-STA-CONNECTED` may be emitted only there.
//! - **The reply path back to us**, which binds in the abstract namespace because a
//!   `/tmp` path does not resolve across the mount namespace `PrivateTmp=` builds.
//!
//! ## What this still cannot prove without hardware
//!
//! The whole sequence — group formation, WPS enrolment, DHCP across the group, the
//! neighbour-table sweep, and the RTSP dial — runs in CI against mac80211_hwsim radios
//! (`nix/miracast-vm-test.nix`). What that cannot vouch for is a *driver*: hwsim is the
//! best-behaved mac80211 implementation there is, and §7.6 of the protocol notes is a
//! table of real chipsets that advertise `P2P-GO` and then fail at group formation. The
//! driver check (#17) remains the hardware's to pass.
//!
//! The peer's IP address is not something wpa_supplicant knows: as group owner we are
//! expected to run a DHCP server on the group interface (the NixOS module does, via
//! networkd, from the same `group_cidr` this backend sweeps), and the address appears in
//! the kernel's neighbour table, which is where [`peer_address`] looks — after making
//! the kernel ask, since in WFD the peer has no reason to speak to us first.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use castaway_core::{CoreError, SessionSink};
use tokio::net::{UdpSocket, UnixDatagram};
use tracing::{debug, info, warn};

use crate::actor::{bind_rtp, connect_control, run_session};
use crate::error::MiracastError;
use crate::ie::{DeviceInformation, ExtendedCapability, WfdInformationElement};
use crate::p2p::{MacAddr, WpaCommand, WpaEvent, WpaReply};
use crate::params::SinkCapabilities;
use crate::session::DEFAULT_CONTROL_PORT;

/// How long to wait for a control-socket reply before giving up on the daemon.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for a peer's address to appear in the neighbour table after it
/// associates. A DHCP exchange is a few round trips on an idle link.
const ADDRESS_TIMEOUT: Duration = Duration::from_secs(15);

/// The WPS primary device type for a television. Nothing filters on it — Android checks
/// only the WFD IE — but it decides the icon in the picker.
pub const WPS_DEVICE_TYPE_DISPLAY: &str = "7-0050F204-1";

/// A connected wpa_supplicant control socket.
///
/// The control interface is a Unix *datagram* socket, so the client binds an address of
/// its own for replies. After `ATTACH`, unsolicited events arrive on the same socket
/// interleaved with command replies, told apart only by the `<priority>` prefix.
pub struct WpaControl {
    socket: UnixDatagram,
    /// The reply socket's filesystem path, when it has one. On Linux the socket lives in
    /// the *abstract* namespace instead and this is `None` — see [`bind_client`].
    client_path: Option<PathBuf>,
}

/// Bind the reply socket, in the abstract namespace.
///
/// Abstract rather than a path in `/tmp`, and the reason is mount namespaces: the
/// deployment runs castaway with `PrivateTmp=`, so a path this process binds resolves to
/// nothing in wpa_supplicant's namespace, and every reply is sent to a file that does not
/// exist — the daemon works, the commands arrive, and the backend times out on all of
/// them. Abstract addresses are kernel-wide, which is why Android's own `wpa_ctrl` client
/// has used them forever.
#[cfg(target_os = "linux")]
fn bind_client() -> Result<(UnixDatagram, Option<PathBuf>), MiracastError> {
    use std::os::linux::net::SocketAddrExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    // Unique per *socket*, not per process: the parent and the group interface each get
    // their own reply socket, and a name collision is a bind failure.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "castaway-wpa-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let address = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .map_err(|e| MiracastError::Backend(format!("abstract address {name}: {e}")))?;
    let socket = std::os::unix::net::UnixDatagram::bind_addr(&address)
        .map_err(|e| MiracastError::Backend(format!("binding @{name}: {e}")))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| MiracastError::Backend(format!("nonblocking @{name}: {e}")))?;
    let socket = UnixDatagram::from_std(socket)
        .map_err(|e| MiracastError::Backend(format!("registering @{name}: {e}")))?;
    Ok((socket, None))
}

/// Bind the reply socket, as a temp-dir path — the non-Linux Unix fallback, where the
/// abstract namespace does not exist.
#[cfg(all(unix, not(target_os = "linux")))]
fn bind_client() -> Result<(UnixDatagram, Option<PathBuf>), MiracastError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let client_path = std::env::temp_dir().join(format!(
        "castaway-wpa-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    // A stale socket from a previous run is a bind failure with a confusing message.
    let _ = std::fs::remove_file(&client_path);
    let socket = UnixDatagram::bind(&client_path)
        .map_err(|e| MiracastError::Backend(format!("binding {}: {e}", client_path.display())))?;
    Ok((socket, Some(client_path)))
}

impl WpaControl {
    /// Connect to the control socket for `interface`.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] if the socket cannot be bound or connected — which on a
    /// normal box means either that wpa_supplicant is not running with a control
    /// interface, or that this process cannot reach its socket (root, or the control
    /// interface's `GROUP=`).
    pub async fn connect(control_dir: &Path, interface: &str) -> Result<Self, MiracastError> {
        let server = control_dir.join(interface);
        let (socket, client_path) = bind_client()?;
        socket
            .connect(&server)
            .map_err(|e| MiracastError::Backend(format!("connecting {}: {e}", server.display())))?;
        Ok(Self {
            socket,
            client_path,
        })
    }

    /// Send a command and wait for its reply.
    ///
    /// Events arriving while a reply is outstanding are dropped: nothing in the bring-up
    /// sequence acts on one, and buffering them here would mean two readers of the same
    /// socket. Once the group is up, [`WpaControl::next_event`] owns the socket.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] on a socket error or a timeout.
    pub async fn command(&self, command: &WpaCommand) -> Result<WpaReply, MiracastError> {
        let text = command.to_string();
        self.socket
            .send(text.as_bytes())
            .await
            .map_err(|e| MiracastError::Backend(format!("sending `{text}`: {e}")))?;
        let mut buf = vec![0u8; 4096];
        let deadline = tokio::time::Instant::now() + REPLY_TIMEOUT;
        loop {
            let read = tokio::time::timeout_at(deadline, self.socket.recv(&mut buf))
                .await
                .map_err(|_| MiracastError::Backend(format!("`{text}` timed out")))?;
            let n = read.map_err(|e| MiracastError::Backend(format!("reading a reply: {e}")))?;
            let line = String::from_utf8_lossy(&buf[..n]).into_owned();
            if WpaEvent::is_event_line(&line) {
                debug!(event = %line.trim_end(), "event while awaiting a reply");
                continue;
            }
            return Ok(WpaReply::parse(&line));
        }
    }

    /// Send a command that must succeed.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] naming the command and the reply — a bare `FAIL` in a
    /// log is unactionable, and every one of these can fail for its own reason.
    pub async fn require(&self, command: WpaCommand) -> Result<(), MiracastError> {
        self.command(&command).await?.require_ok(&command)
    }

    /// Wait for the next unsolicited event.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] on a socket error.
    pub async fn next_event(&self) -> Result<WpaEvent, MiracastError> {
        let mut buf = vec![0u8; 4096];
        loop {
            let n = self
                .socket
                .recv(&mut buf)
                .await
                .map_err(|e| MiracastError::Backend(format!("reading an event: {e}")))?;
            let line = String::from_utf8_lossy(&buf[..n]).into_owned();
            if WpaEvent::is_event_line(&line) {
                return Ok(WpaEvent::parse(&line));
            }
            debug!(reply = %line.trim_end(), "unsolicited command reply, ignored");
        }
    }
}

impl Drop for WpaControl {
    fn drop(&mut self) {
        // On the path-based fallback the client socket is a real file; leaving one per
        // run behind would eventually make the next bind fail for a reason that has
        // nothing to do with Wi-Fi. Abstract sockets vanish with the descriptor.
        if let Some(path) = &self.client_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Where wpa_supplicant's control sockets live on a systemd/NixOS box.
pub const DEFAULT_CONTROL_DIR: &str = "/run/wpa_supplicant";

/// The IPv4 subnet of the group interface: our own address plus the peers' pool.
///
/// The backend needs this for exactly one thing — *finding* a peer. In WFD the sink
/// dials the source, so the peer never has a reason to send us unicast traffic first,
/// and a client that quietly takes its DHCP lease (Windows does exactly this, since we
/// advertise ourselves as neither its router nor its DNS) never puts itself in our
/// neighbour table. Knowing the subnet lets [`nudge_neighbours`] make the kernel ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSubnet {
    address: std::net::Ipv4Addr,
    prefix: u8,
}

impl GroupSubnet {
    /// A subnet from our own address and a prefix length.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] unless `20 <= prefix <= 30`: below /20 the sweep in
    /// [`nudge_neighbours`] stops being a nudge and becomes four thousand datagrams, and
    /// above /30 there is no room for a peer at all.
    pub fn new(address: std::net::Ipv4Addr, prefix: u8) -> Result<Self, MiracastError> {
        if !(20..=30).contains(&prefix) {
            return Err(MiracastError::Backend(format!(
                "group subnet /{prefix} is outside /20..=/30"
            )));
        }
        Ok(Self { address, prefix })
    }

    /// Parse the `a.b.c.d/nn` form the config file uses.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] if it is not an IPv4 CIDR in the accepted range.
    pub fn parse(s: &str) -> Result<Self, MiracastError> {
        let bad =
            || MiracastError::Backend(format!("`{s}` is not an IPv4 CIDR like 192.168.77.1/24"));
        let (address, prefix) = s.split_once('/').ok_or_else(bad)?;
        Self::new(
            address.trim().parse().map_err(|_| bad())?,
            prefix.trim().parse().map_err(|_| bad())?,
        )
    }

    /// Every address a peer could hold: the subnet minus network, broadcast, and us.
    fn peers(self) -> impl Iterator<Item = std::net::Ipv4Addr> {
        let ours = u32::from(self.address);
        let mask = u32::MAX << (32 - self.prefix);
        let network = ours & mask;
        let broadcast = network | !mask;
        ((network + 1)..broadcast)
            .filter(move |&host| host != ours)
            .map(std::net::Ipv4Addr::from)
    }
}

/// Make the kernel resolve every possible peer address on the group subnet.
///
/// One empty datagram to each host's discard port: the payload never matters and mostly
/// never arrives — the side effect is the ARP request the kernel must send to deliver
/// it, and the peer's reply is the neighbour-table entry [`peer_address`] is polling
/// for. Needs no privileges and touches nothing outside the connected route.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: send-only ARP-priming sweep, in the outbound table of \
              crates/app/src/surface.rs — nothing is ever received on it"
)]
async fn nudge_neighbours(subnet: GroupSubnet) {
    let Ok(socket) = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await else {
        return;
    };
    for host in subnet.peers() {
        // An unroutable host is not an error worth stopping a sweep over.
        let _ = socket.send_to(&[], (host, 9)).await;
    }
}

/// How to bring up the group.
#[derive(Debug, Clone)]
pub struct P2pConfig {
    /// The wpa_supplicant control socket directory.
    pub control_dir: PathBuf,
    /// The interface wpa_supplicant manages — the *parent*, not the group interface,
    /// which does not exist yet.
    pub interface: String,
    /// The name shown in Win+K and in Android's cast picker.
    pub device_name: String,
    /// The operating frequency in MHz, or `None` to let wpa_supplicant choose.
    ///
    /// Worth setting. A group left to choose lands on a social channel in the 2.4 GHz
    /// band, which is where every other radio in a hackerspace already is, and mirroring
    /// is the one workload with no slack for it.
    pub freq_mhz: Option<u16>,
    /// Maximum throughput to advertise, in Mbps.
    pub max_throughput_mbps: u16,
    /// The group interface's subnet — must match whatever assigns the interface its
    /// address and serves DHCP on it (the NixOS module does both from the same value).
    pub group: GroupSubnet,
}

/// The subnet the NixOS module configures, as a compile-checked default.
fn default_group_subnet() -> GroupSubnet {
    // A /24 is always in range, so this cannot actually fail; unwrap is still banned
    // here (ground rule 7), and the fallback is the same value field by field.
    GroupSubnet::new(std::net::Ipv4Addr::new(192, 168, 77, 1), 24).unwrap_or(GroupSubnet {
        address: std::net::Ipv4Addr::new(192, 168, 77, 1),
        prefix: 24,
    })
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            control_dir: PathBuf::from(DEFAULT_CONTROL_DIR),
            interface: "wlan0".to_owned(),
            device_name: "castaway".to_owned(),
            freq_mhz: None,
            // What MiracleCast and lazycast advertise, and what shipping senders accept.
            max_throughput_mbps: 200,
            group: default_group_subnet(),
        }
    }
}

/// The Linux backend.
pub struct LinuxMiracastBackend {
    config: P2pConfig,
    caps: SinkCapabilities,
}

impl LinuxMiracastBackend {
    /// A backend that will bring up a group per `config` and answer with `caps`.
    #[must_use]
    pub fn new(config: P2pConfig, caps: SinkCapabilities) -> Self {
        Self { config, caps }
    }

    /// Write the WFD information element into the daemon's beacons and probe responses.
    ///
    /// Called at bring-up and then **again either side of every session**, which is the
    /// point: the IE used to be installed once and never updated, so the availability bit
    /// said "free" for the whole life of the process. A second source therefore saw an
    /// available sink in its picker, joined a busy one, and got nothing — while the panel
    /// is single-source by policy and by construction (`serve_peer` is awaited inline, so
    /// the event loop is not even reading while a session runs). The bit exists precisely
    /// to keep that phone out of the queue (#194).
    ///
    /// # Errors
    /// [`MiracastError::Backend`] if any subelement is refused.
    async fn advertise(
        &self,
        control: &WpaControl,
        availability: Availability,
    ) -> Result<(), MiracastError> {
        let ie = advertisement(self.config.max_throughput_mbps, availability);
        for subelement in &ie.subelements {
            control
                .require(WpaCommand::WfdSubelemSet {
                    id: subelement.id.wire(),
                    hex: WfdInformationElement::subelem_set_hex(subelement),
                })
                .await?;
        }
        Ok(())
    }

    /// Install the advertisement and bring up an autonomous group.
    ///
    /// # Errors
    /// [`MiracastError::Backend`] if any control command is refused.
    async fn bring_up(&self, control: &WpaControl) -> Result<(), MiracastError> {
        control.require(WpaCommand::Attach).await?;
        control
            .require(WpaCommand::Set {
                key: "device_name",
                value: self.config.device_name.clone(),
            })
            .await?;
        control
            .require(WpaCommand::Set {
                key: "device_type",
                value: WPS_DEVICE_TYPE_DISPLAY.to_owned(),
            })
            .await?;
        // Without this the daemon does not attach a WFD IE to anything, and every
        // subelement we set is stored and never transmitted.
        control
            .require(WpaCommand::Set {
                key: "wifi_display",
                value: "1".to_owned(),
            })
            .await?;

        self.advertise(control, Availability::Free).await?;

        control
            .require(WpaCommand::P2pGroupAdd {
                freq: self.config.freq_mhz,
            })
            .await?;
        info!(
            interface = %self.config.interface,
            name = %self.config.device_name,
            "autonomous P2P group requested"
        );
        Ok(())
    }
}

/// The control socket of a running group's own interface.
///
/// A separate P2P group interface is a separate wpa_supplicant interface with a control
/// socket of its own, and two things live only there: `WPS_PBC` (the group interface is
/// the AP-mode registrar; the parent is a P2P management interface that would run the
/// wrong WPS), and — depending on the hostap build — the `AP-STA-CONNECTED` events that
/// start sessions. A backend attached only to the parent authorises nobody and may hear
/// nothing associate; this is the piece a loopback test cannot catch.
struct GroupControl {
    iface: String,
    /// `None` when the group socket could not be opened — the backend then limps along
    /// on the parent socket alone, which loses whatever only the group socket carries
    /// but keeps the group up for the next attempt.
    control: Option<WpaControl>,
}

/// Connect to the interface's *P2P management* socket.
///
/// On drivers with P2P-Device support (iwlwifi, mac80211_hwsim, mt76, ath10k — most of
/// the viable table in protocol notes §7.6), wpa_supplicant runs P2P on a dedicated
/// `p2p-dev-<iface>` interface with a control socket of its own, and that socket is
/// where every P2P event is delivered and where the P2P configuration lives. A backend
/// attached to the netdev's socket issues commands that all answer `OK` — they are
/// routed to the management interface internally — and then never hears
/// `P2P-GROUP-STARTED` come back. Prefer the management socket; fall back to the plain
/// interface for drivers without a P2P Device.
async fn connect_p2p_management(
    control_dir: &Path,
    interface: &str,
) -> Result<WpaControl, MiracastError> {
    let management = format!("p2p-dev-{interface}");
    match WpaControl::connect(control_dir, &management).await {
        Ok(control) => {
            debug!(socket = %management, "using the P2P Device management socket");
            Ok(control)
        }
        Err(_) => WpaControl::connect(control_dir, interface).await,
    }
}

/// The WFD information element this sink advertises, at a given availability.
///
/// Pure, and separate from the socket call that installs it, so what goes on the air is
/// testable without a radio (rule 3).
fn advertisement(max_throughput_mbps: u16, availability: Availability) -> WfdInformationElement {
    let device = DeviceInformation::sink(DEFAULT_CONTROL_PORT, max_throughput_mbps);
    WfdInformationElement::sink(
        match availability {
            Availability::Free => device,
            Availability::Busy => device.busy(),
        },
        ExtendedCapability {
            // We do implement UIBC, so claiming it here is a promise we keep — the sink
            // dials the port the source names and maps panel touch onto the stream
            // (`uibc::UibcSurface`). This bit is the out-of-band half of the negotiation:
            // a source reads it before RTSP starts and decides whether asking is
            // worthwhile.
            uibc: true,
            ..ExtendedCapability::default()
        },
    )
}

/// What the beacon says about whether somebody is already casting.
///
/// A type rather than a `bool` because the two calls that use it are three lines apart and
/// would read identically at the call site (rule 1) — and getting them the wrong way round
/// advertises a free sink for the duration of a session and a busy one forever after,
/// which is the same symptom as the bug it fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Availability {
    /// Nobody is casting; join.
    Free,
    /// A session is running. `DeviceInformation::busy` withdraws the sink from every
    /// picker in the room for its duration.
    Busy,
}

/// How long two `AP-STA-CONNECTED` events for the same peer are treated as one.
///
/// Both the parent and the group socket can emit the association, and serving the same
/// peer twice means dialling an RTSP port whose source has already moved on. Two seconds
/// is far below any real re-association (WPS plus DHCP alone take longer).
const STATION_DEDUPE: Duration = Duration::from_secs(2);

#[async_trait::async_trait]
impl castaway_core::MiracastBackend for LinuxMiracastBackend {
    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        // The daemon and the radio are allowed to be late: castaway, wpa_supplicant and
        // a USB adapter come up in whatever order the box feels like, and a backend
        // that tried once and gave up would strand Miracast until a restart. Retry with
        // a capped backoff instead — each failure says what is missing.
        let mut backoff = Duration::from_secs(1);
        let control = loop {
            match connect_p2p_management(&self.config.control_dir, &self.config.interface).await {
                Ok(control) => match self.bring_up(&control).await {
                    Ok(()) => break control,
                    Err(e) => warn!(error = %e, "Miracast bring-up failed; retrying"),
                },
                Err(e) => {
                    warn!(error = %e, "no wpa_supplicant control socket yet; retrying");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        };

        let mut group: Option<GroupControl> = None;
        let mut last_station: Option<(MacAddr, tokio::time::Instant)> = None;
        loop {
            // One event from either socket. `next_event` is a single `recv().await`, so
            // losing the race in `select!` cancels nothing mid-read.
            let event = match group.as_ref().and_then(|g| g.control.as_ref()) {
                Some(group_control) => tokio::select! {
                    event = control.next_event() => event,
                    event = group_control.next_event() => event,
                },
                None => control.next_event().await,
            }
            .map_err(|e| CoreError::Adapter(e.to_string()))?;
            match event {
                WpaEvent::GroupStarted {
                    iface,
                    group_owner: true,
                } => {
                    info!(iface, "P2P group up; we are the group owner");
                    group = Some(self.attach_group(iface).await);
                }
                WpaEvent::GroupStarted {
                    iface,
                    group_owner: false,
                } => {
                    // We asked for an autonomous group, so this means somebody negotiated
                    // with us and won — a state this backend has no path out of.
                    return Err(CoreError::Adapter(format!(
                        "joined {iface} as a client; a sink must be the group owner"
                    )));
                }
                WpaEvent::GroupRemoved { iface } => {
                    warn!(iface, "P2P group removed");
                    group = None;
                }
                WpaEvent::ProvDiscPbcRequest { peer } | WpaEvent::GoNegRequest { peer } => {
                    // Push-button is the only method a sink with no keypad can offer, and
                    // it is what both platforms use. Authorising immediately is the
                    // "walk up and cast" behaviour this panel wants.
                    info!(%peer, "authorising push-button enrolment");
                    // On the group socket when there is one: WPS for a GO runs on the
                    // group interface's registrar, and the parent would `OK` a PBC that
                    // enrols nobody.
                    let registrar = group
                        .as_ref()
                        .and_then(|g| g.control.as_ref())
                        .unwrap_or(&control);
                    if let Err(e) = registrar
                        .require(WpaCommand::WpsPbc { peer: Some(peer) })
                        .await
                    {
                        warn!(%peer, error = %e, "could not authorise the peer");
                    }
                }
                WpaEvent::StationConnected { peer } => {
                    let Some(g) = &group else {
                        warn!(%peer, "a station associated before the group started");
                        continue;
                    };
                    // The association can arrive on both sockets; the second copy is not
                    // a second phone.
                    if last_station
                        .is_some_and(|(last, at)| last == peer && at.elapsed() < STATION_DEDUPE)
                    {
                        debug!(%peer, "duplicate association event, ignored");
                        continue;
                    }
                    last_station = Some((peer, tokio::time::Instant::now()));
                    let iface = g.iface.clone();

                    // Out of every picker in the room for the duration. Not fatal if it
                    // fails: a beacon that still says "free" is a second phone that joins
                    // and waits, which is worse than this session not happening at all.
                    if let Err(e) = self.advertise(&control, Availability::Busy).await {
                        warn!(error = %e, "could not withdraw the sink while it is busy");
                    }

                    let outcome = self.serve_peer(&iface, peer, &sink).await;

                    // …and back, whichever way the session ended. This has to run on the
                    // error path too: a sink that fails one session and stays advertised
                    // as busy is one nobody can cast to again until the process restarts.
                    if let Err(e) = self.advertise(&control, Availability::Free).await {
                        warn!(error = %e, "could not re-advertise the sink as available");
                    }

                    if let Err(e) = outcome {
                        // One failed session is not a reason to tear the group down; the
                        // next person who walks up should still get a picture.
                        warn!(%peer, error = %e, "the WFD session ended in error");
                    }
                }
                WpaEvent::StationDisconnected { peer } => {
                    debug!(%peer, "station left");
                }
                other => debug!(?other, "control event"),
            }
        }
    }
}

impl LinuxMiracastBackend {
    /// Open and subscribe the just-started group interface's own control socket.
    ///
    /// Infallible by design: the group is already up, and a backend that tore it down
    /// because a *second* socket to the same daemon failed would turn a permissions
    /// mistake into a dead radio. What is lost without it is logged, not guessed at.
    async fn attach_group(&self, iface: String) -> GroupControl {
        match WpaControl::connect(&self.config.control_dir, &iface).await {
            Ok(group_control) => {
                if let Err(e) = group_control.require(WpaCommand::Attach).await {
                    // Commands (WPS_PBC) still work unattached; only events are lost.
                    warn!(iface, error = %e, "could not subscribe to group-interface events");
                }
                GroupControl {
                    iface,
                    control: Some(group_control),
                }
            }
            Err(e) => {
                warn!(
                    iface, error = %e,
                    "no control socket for the group interface; WPS authorisation and \
                     association events may not work"
                );
                GroupControl {
                    iface,
                    control: None,
                }
            }
        }
    }

    /// Run one session against a peer that has just associated.
    async fn serve_peer(
        &self,
        iface: &str,
        peer: MacAddr,
        sink: &SessionSink,
    ) -> Result<(), MiracastError> {
        let address = peer_address(iface, peer, Some(self.config.group), ADDRESS_TIMEOUT).await?;
        let rtp = bind_rtp(self.caps.client_rtp_ports.port()).await?;
        let control = connect_control((address, DEFAULT_CONTROL_PORT).into()).await?;
        run_session(
            control,
            rtp,
            self.caps.clone(),
            sink.with_instance(peer.to_string()),
        )
        .await
    }
}

/// How often to re-sweep the subnet while waiting for a peer's address to resolve.
const NUDGE_INTERVAL: Duration = Duration::from_secs(2);

/// Find a peer's IPv4 address by its MAC, from the kernel's neighbour table.
///
/// wpa_supplicant does not know it: as group owner we hand out addresses over DHCP, and
/// the association event carries only the MAC. Polling `/proc/net/arp` is the dependency-
/// free way to close that gap — but the entry does not appear on its own. In WFD *we*
/// dial *them*, so the peer may never send us the unicast packet that would create one;
/// given `subnet`, every poll round is preceded by a [`nudge_neighbours`] sweep so the
/// kernel resolves the peer the moment DHCP has given it an address.
///
/// # Errors
/// [`MiracastError::Backend`] if no entry appears within `timeout`, which in practice
/// means there is no DHCP server running on the group interface.
pub async fn peer_address(
    iface: &str,
    peer: MacAddr,
    subnet: Option<GroupSubnet>,
    timeout: Duration,
) -> Result<std::net::Ipv4Addr, MiracastError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let wanted = peer.to_string();
    let mut next_nudge = tokio::time::Instant::now();
    loop {
        if let Some(subnet) = subnet {
            if tokio::time::Instant::now() >= next_nudge {
                nudge_neighbours(subnet).await;
                next_nudge = tokio::time::Instant::now() + NUDGE_INTERVAL;
            }
        }
        if let Ok(table) = tokio::fs::read_to_string("/proc/net/arp").await {
            if let Some(address) = find_in_arp(&table, iface, &wanted) {
                return Ok(address);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MiracastError::Backend(format!(
                "no address for {peer} on {iface} within {}s — is a DHCP server running on \
                 the group interface?",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Parse `/proc/net/arp` for a MAC on an interface.
///
/// Columns are `IP address, HW type, Flags, HW address, Mask, Device`. Split out so the
/// parsing is testable without a kernel — the only part of the address lookup that can be
/// wrong in a way a test would catch.
fn find_in_arp(table: &str, iface: &str, mac: &str) -> Option<std::net::Ipv4Addr> {
    table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let address = fields.next()?;
        let hw_address = fields.nth(2)?;
        let device = fields.nth(1)?;
        (device == iface && hw_address.eq_ignore_ascii_case(mac))
            .then(|| address.parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const ARP: &str =
        "IP address       HW type     Flags       HW address            Mask     Device\n\
        192.168.49.15    0x1         0x2         02:aa:bb:cc:dd:ee     *        p2p-wlan0-0\n\
        10.0.0.1         0x1         0x2         aa:bb:cc:dd:ee:ff     *        eth0\n";

    /// Busy and free differ in the availability bits and in nothing else.
    ///
    /// `DeviceInformation::busy()` was implemented, unit-tested, and had **no caller**:
    /// the IE went in once at bring-up and was never updated, so the bit said "free" for
    /// the whole life of the process. A second source saw an available sink in its
    /// picker, joined a busy one, and got nothing — while the panel is single-source both
    /// by policy and by construction, since `serve_peer` is awaited inline and the event
    /// loop is not even reading while a session runs (#194).
    ///
    /// v2.3 §4.5 makes the bit a hard gate: a source *shall not* connect to a device
    /// advertising `0b00`. So this is the mechanism, and the assertion that matters is
    /// that flipping it changes *only* it — a busy advertisement that also dropped the
    /// control port or the UIBC bit would take the sink out of the picker and out of the
    /// protocol.
    #[test]
    fn a_session_withdraws_the_sink_and_finishing_puts_it_back() {
        let free = advertisement(200, Availability::Free);
        let busy = advertisement(200, Availability::Busy);

        assert_eq!(
            free.subelements.len(),
            busy.subelements.len(),
            "the same subelements either way; only one value inside one of them moves"
        );

        let mut differing = Vec::new();
        for (f, b) in free.subelements.iter().zip(busy.subelements.iter()) {
            assert_eq!(
                f.id, b.id,
                "subelement order must not depend on availability"
            );
            if f.body != b.body {
                differing.push(f.id);
            }
        }
        assert_eq!(
            differing.len(),
            1,
            "exactly one subelement should change, got {differing:?}"
        );

        let device_of = |ie: &WfdInformationElement| {
            let body = ie
                .subelements
                .iter()
                .find(|s| s.id == crate::ie::SubelementId::DeviceInformation)
                .expect("a sink advertises device information")
                .body
                .clone();
            DeviceInformation::parse(&body).expect("our own body must parse")
        };

        let (f, b) = (device_of(&free), device_of(&busy));
        assert_eq!(f.availability, crate::ie::SessionAvailability::Available);
        assert_eq!(b.availability, crate::ie::SessionAvailability::NotAvailable);
        // Everything a source needs in order to come back later survives being busy.
        assert_eq!(b.device_type, f.device_type);
        assert_eq!(b.control_port, DEFAULT_CONTROL_PORT);
        assert_eq!(b.max_throughput_mbps, 200);
    }

    #[test]
    fn a_peers_address_is_found_by_mac_and_interface() {
        let peer = MacAddr::parse("02:aa:bb:cc:dd:ee").unwrap();
        assert_eq!(
            find_in_arp(ARP, "p2p-wlan0-0", &peer.to_string()),
            Some(std::net::Ipv4Addr::new(192, 168, 49, 15))
        );
    }

    #[test]
    fn the_same_mac_on_another_interface_is_not_our_peer() {
        // The group interface is not the one we configured, and a MAC can legitimately
        // appear on both — matching on the address alone would dial the wrong network.
        assert_eq!(find_in_arp(ARP, "p2p-wlan0-0", "aa:bb:cc:dd:ee:ff"), None);
        assert_eq!(
            find_in_arp(ARP, "eth0", "aa:bb:cc:dd:ee:ff"),
            Some(std::net::Ipv4Addr::new(10, 0, 0, 1))
        );
    }

    #[test]
    fn an_absent_peer_is_absent_rather_than_the_first_row() {
        assert_eq!(find_in_arp(ARP, "p2p-wlan0-0", "00:00:00:00:00:01"), None);
        assert_eq!(find_in_arp("", "p2p-wlan0-0", "02:aa:bb:cc:dd:ee"), None);
    }

    #[tokio::test]
    async fn a_missing_address_says_what_is_probably_wrong() {
        // The failure mode this replaces is a silent hang, and the cause is almost always
        // the same one.
        let peer = MacAddr::parse("02:00:00:00:00:01").unwrap();
        let err = peer_address("p2p-nonexistent-0", peer, None, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("DHCP"), "{err}");
    }

    #[test]
    fn a_subnet_parses_and_yields_every_possible_peer() {
        let subnet = GroupSubnet::parse("192.168.77.1/30").unwrap();
        // A /30 holds .0 network, .1 us, .2 the one possible peer, .3 broadcast.
        assert_eq!(
            subnet.peers().collect::<Vec<_>>(),
            vec![std::net::Ipv4Addr::new(192, 168, 77, 2)]
        );
    }

    #[test]
    fn a_24_subnet_sweeps_the_pool_but_not_network_broadcast_or_us() {
        let subnet = GroupSubnet::parse("192.168.77.1/24").unwrap();
        let peers: Vec<_> = subnet.peers().collect();
        assert_eq!(peers.len(), 253);
        assert!(!peers.contains(&std::net::Ipv4Addr::new(192, 168, 77, 0)));
        assert!(!peers.contains(&std::net::Ipv4Addr::new(192, 168, 77, 1)));
        assert!(!peers.contains(&std::net::Ipv4Addr::new(192, 168, 77, 255)));
    }

    #[test]
    fn a_subnet_wide_enough_to_be_a_flood_is_refused() {
        // /16 would be sixty-five thousand datagrams per association; the failure names
        // the accepted range rather than sweeping anyway.
        assert!(GroupSubnet::parse("10.0.0.1/16").is_err());
        assert!(GroupSubnet::parse("10.0.0.1/31").is_err());
        assert!(GroupSubnet::parse("not-a-cidr").is_err());
        assert!(GroupSubnet::parse("10.0.0.1").is_err());
    }
}
