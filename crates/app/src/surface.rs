//! The network-surface registry: every socket this receiver binds, advertises, or
//! dials, as data (#22, #30).
//!
//! One table, three consumers, none of which can drift from the code:
//!
//! - **`docs/network-surface.md`** is generated from here; a test fails when the
//!   checked-in file no longer matches (`CASTAWAY_REGEN_SURFACE=1 cargo test -p
//!   castaway` rewrites it).
//! - **`nix/network-surface.json`** is generated the same way, and is what the NixOS
//!   module's firewall reads — so a port added here opens itself on deploy, and a port
//!   added to the code *without* a registry entry is caught by the
//!   `clippy::disallowed_methods` lint on the raw bind calls (see `clippy.toml`).
//! - **`castaway --network-surface[=json|netsh]`** prints the table resolved against
//!   the loaded config: what *this* box, with *this* `castaway.toml`, will bind.
//!
//! The entries name the same constants the listeners bind
//! ([`proto_cast::CAST_PORT`], [`proto_airplay::AIRPLAY_PORT`],
//! [`substrate_ssdp::SSDP_PORT`], …), and [`protocol_listeners`] matches exhaustively
//! over [`ProtocolKind`] — a new protocol does not compile until its surface is
//! declared, even if that declaration is "none".

use castaway_core::ProtocolKind;
use serde_json::json;

use crate::config::{Config, Enable, IcePortsConfig, MediaPortsConfig};

/// TCP or UDP. No third thing binds here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// TCP listener.
    Tcp,
    /// UDP socket.
    Udp,
}

impl Transport {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// How a listener's port is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSpec {
    /// A protocol constant; senders expect it and nothing can move it.
    Fixed(u16),
    /// A single port read from `castaway.toml` at `key` (dotted path).
    Configurable {
        /// Dotted config path, e.g. `"miracast.rtp_port"`.
        key: &'static str,
        /// The value when the config does not set it.
        default: u16,
    },
    /// The `[media_ports]` range — per-session sockets allocated lowest-free-first.
    MediaRange,
    /// The `[remote.ice_ports]` range — one UDP socket per connected remote peer.
    IceRange,
}

/// Who answers on a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The shared HTTP host (D7): DLNA + DIAL + Spotify on one port.
    SharedHttp,
    /// The process-wide mDNS responder.
    Mdns,
    /// The process-wide SSDP responder.
    Ssdp,
    /// The remote-control web UI's WebRTC peer connections (#18).
    Remote,
    /// One protocol's own socket.
    Protocol(ProtocolKind),
}

impl Owner {
    /// Stable lowercase label, used in JSON and rule names.
    fn label(self) -> &'static str {
        match self {
            Self::SharedHttp => "http",
            Self::Mdns => "mdns",
            Self::Ssdp => "ssdp",
            Self::Remote => "remote",
            Self::Protocol(kind) => kind.slug(),
        }
    }
}

/// Whether the process binds the socket itself, or the deployment provides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Bound by this binary.
    Process,
    /// Bound by something the deployment stands up on the receiver's behalf.
    Deployment(&'static str),
}

/// Who actually picked the port number — the answer to "could we move this?".
///
/// The tiers correlate with the config surface, and a test holds the line: a
/// spec-forced port is always a fixed constant, and a freely-chosen one always has a
/// config knob. The convention tier splits case by case — `miracast.rtp_port` has a
/// knob because captures show senders tolerate variance; Cast's 8009 and AirPlay's
/// 7000 do not, because moving a port every sender on earth expects buys nothing and
/// exercises a path none of them is tested against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Fixed by the protocol itself (a well-known rendezvous port, or an RFC): a
    /// sender's first packet goes there before it knows we exist. Nothing on either
    /// side can move it.
    Spec,
    /// Signaled to senders (an mDNS SRV record, or in-band during session setup), so
    /// movable in principle — but every implementation in the wild uses this number.
    Convention,
    /// Entirely ours: senders only ever learn it from what we advertise or answer.
    Ours,
}

impl Provenance {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Spec => "spec",
            Self::Convention => "convention",
            Self::Ours => "ours",
        }
    }
}

/// When the listener exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Bound whenever the receiver runs.
    Always,
    /// Bound when any of these protocols is enabled.
    AnyOf(&'static [ProtocolKind]),
    /// Bound when the remote-control web UI is enabled. Not a [`ProtocolKind`]: nothing
    /// casts to it, it is the panel's own surface served back out.
    RemoteControl,
}

/// One inbound socket: the unit the firewall, the docs, and `--network-surface` share.
#[derive(Debug, Clone, Copy)]
pub struct Listener {
    /// Who answers.
    pub owner: Owner,
    /// TCP or UDP.
    pub transport: Transport,
    /// How the port is chosen.
    pub port: PortSpec,
    /// Bind address and multicast membership, for the docs.
    pub bind: &'static str,
    /// What runs on the socket.
    pub wire: &'static str,
    /// Transport security, honestly stated.
    pub security: &'static str,
    /// When the socket exists.
    pub gate: Gate,
    /// Who binds it.
    pub provider: Provider,
    /// Who picked the number.
    pub chosen_by: Provenance,
    /// What an operator should know that the columns can't say.
    pub notes: &'static str,
}

/// The `[enable]` flag a protocol is gated by.
///
/// Almost the slug, except DIAL: the flag is named for the discovery protocol
/// (`dial`), the slug for what a person casts (`youtube`).
fn enable_flag(kind: ProtocolKind) -> &'static str {
    match kind {
        ProtocolKind::AirPlay => "airplay",
        ProtocolKind::Cast => "cast",
        ProtocolKind::Miracast => "miracast",
        ProtocolKind::Dlna => "dlna",
        ProtocolKind::YouTubeLounge => "dial",
        ProtocolKind::Spotify => "spotify",
        ProtocolKind::Bluetooth => "bluetooth",
        ProtocolKind::GameStream => "gamestream",
        ProtocolKind::MatterCast => "matter",
        ProtocolKind::FCast => "fcast",
    }
}

/// The flag's value in a loaded config.
fn flag_value(kind: ProtocolKind, enable: &Enable) -> bool {
    match kind {
        ProtocolKind::AirPlay => enable.airplay,
        ProtocolKind::Cast => enable.cast,
        ProtocolKind::Miracast => enable.miracast,
        ProtocolKind::Dlna => enable.dlna,
        ProtocolKind::YouTubeLounge => enable.dial,
        ProtocolKind::Spotify => enable.spotify,
        ProtocolKind::Bluetooth => enable.bluetooth,
        ProtocolKind::GameStream => enable.gamestream,
        ProtocolKind::MatterCast => enable.matter,
        ProtocolKind::FCast => enable.fcast,
    }
}

/// The sockets shared across protocols, bound whenever the receiver runs.
fn shared_listeners() -> Vec<Listener> {
    vec![
        Listener {
            owner: Owner::SharedHttp,
            transport: Transport::Tcp,
            port: PortSpec::Configurable {
                key: "http_port",
                default: Config::default().http_port,
            },
            bind: "0.0.0.0",
            wire: "HTTP/1.1 — UPnP descriptions, DLNA SOAP + GENA, DIAL REST, \
                   Spotify zeroconf pairing, /screenshot.png, /stream/* (HLS), \
                   /remote/* (the remote-control page and its WHEP signalling)",
            security: "plaintext HTTP, unauthenticated — anyone who can reach this \
                       port can *drive* the panel, not merely watch it (#18)",
            gate: Gate::Always,
            provider: Provider::Process,
            chosen_by: Provenance::Ours,
            notes: "One host shared by three protocols (D7); a disabled protocol's \
                    routes are simply not mounted. /screenshot.png and /stream/* \
                    always answer — in a build with no encoder, by saying so. \
                    Fetching /stream/live.m3u8 starts an encoder and holds the \
                    render loop at display rate until ten seconds after the last \
                    request (#101), so it is the one endpoint here that costs the \
                    panel anything. /remote/ serves the control page and answers the \
                    WHEP offer that sets up a peer connection; the media and the input \
                    channel then ride UDP in `[remote.ice_ports]`, not this port. \
                    `remote.input = false` keeps the viewing half and drops every \
                    input message at the boundary.",
        },
        Listener {
            owner: Owner::Mdns,
            transport: Transport::Udp,
            port: PortSpec::Fixed(substrate_mdns::MDNS_PORT),
            bind: "0.0.0.0, multicast 224.0.0.251, SO_REUSEADDR/SO_REUSEPORT",
            wire: "mDNS/DNS-SD responder (mdns-sd daemon)",
            security: "plaintext multicast",
            gate: Gate::Always,
            provider: Provider::Process,
            chosen_by: Provenance::Spec,
            notes: "Advertises only enabled protocols, restricted to the serving \
                    interface. GameStream's host browser runs a second daemon — a \
                    second 5353 socket — when enabled, and Matter's commissionable-node \
                    browse a third. Contends with Avahi/Bonjour for answers (#43); the \
                    NixOS module warns when Avahi is on.",
        },
        Listener {
            owner: Owner::Ssdp,
            transport: Transport::Udp,
            port: PortSpec::Fixed(substrate_ssdp::SSDP_PORT),
            bind: "0.0.0.0, multicast 239.255.255.250, SO_REUSEADDR/SO_REUSEPORT",
            wire: "SSDP M-SEARCH responder + NOTIFY alive/byebye",
            security: "plaintext multicast",
            gate: Gate::Always,
            provider: Provider::Process,
            chosen_by: Provenance::Spec,
            notes: "Bound even with DLNA and DIAL both off; it then answers for no \
                    device type.",
        },
        Listener {
            owner: Owner::Remote,
            transport: Transport::Udp,
            port: PortSpec::IceRange,
            bind: "0.0.0.0 (one socket per connected peer)",
            wire: "WebRTC: SRTP media (the panel's duplicate) and an SCTP data \
                   channel carrying that peer's contacts",
            security: "DTLS-SRTP, but the offer is answered unauthenticated — the \
                       encryption protects the path, not who may use it",
            gate: Gate::RemoteControl,
            provider: Provider::Process,
            chosen_by: Provenance::Ours,
            notes: "Pinned rather than ephemeral, which is not a preference: this \
                    registry generates the firewall, so an ICE candidate outside a \
                    declared range is one the deployed box silently drops — the \
                    connection would negotiate and then carry nothing. One peer takes \
                    one port. Bound by webrtc-rs, which is why the range is handed to \
                    its SettingEngine rather than being bound here.",
        },
    ]
}

/// One protocol's own inbound sockets.
///
/// Exhaustive on purpose: adding a [`ProtocolKind`] fails compilation here until its
/// surface is declared — an empty vec is a declaration too ("rides the shared hosts",
/// "outbound only", "not IP").
fn protocol_listeners(kind: ProtocolKind) -> Vec<Listener> {
    match kind {
        ProtocolKind::AirPlay => vec![
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Tcp,
                port: PortSpec::Fixed(proto_airplay::AIRPLAY_PORT),
                bind: "0.0.0.0",
                wire: "RTSP + HTTP/1.1 on one socket: AirPlay control and RAOP",
                security: "plaintext — pair-verify/FairPlay not implemented (#39)",
                gate: Gate::AnyOf(&[ProtocolKind::AirPlay]),
                provider: Provider::Process,
                chosen_by: Provenance::Convention,
                notes: "Both _airplay._tcp and _raop._tcp advertise this one port. \
                        Nothing binds 7011: it is the AirPlay 1 UDP timing port, not \
                        a listener, and the listener once bound there was removed.",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::MediaRange,
                bind: "the accepted connection's local address",
                wire: "RAOP audio + control + timing (three sockets per session)",
                security: "RTP; AES-CBC when the sender negotiates it",
                gate: Gate::AnyOf(&[ProtocolKind::AirPlay]),
                provider: Provider::Process,
                chosen_by: Provenance::Ours,
                notes: "Bound per sender before SETUP answers, so the Transport \
                        header only ever names ports that are already listening.",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Tcp,
                port: PortSpec::MediaRange,
                bind: "the accepted connection's local address",
                wire: "mirroring data channel (one listener per session)",
                security: "AES-CTR frames (MirrorKeys)",
                gate: Gate::AnyOf(&[ProtocolKind::AirPlay]),
                provider: Provider::Process,
                chosen_by: Provenance::Ours,
                notes: "Answered as dataPort in the second SETUP reply.",
            },
        ],
        ProtocolKind::Cast => vec![
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Tcp,
                port: PortSpec::Fixed(proto_cast::CAST_PORT),
                bind: "0.0.0.0",
                wire: "CASTv2: length-prefixed protobuf over TLS",
                security: "TLS, self-signed or CKS-replayed certificate; the \
                           device-auth signature covers it (D41/D43)",
                gate: Gate::AnyOf(&[ProtocolKind::Cast]),
                provider: Provider::Process,
                chosen_by: Provenance::Convention,
                notes: "",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Tcp,
                port: PortSpec::Fixed(proto_cast::platform::DEFAULT_PLATFORM_PORT),
                bind: "127.0.0.1",
                wire: "the Cast receiver platform: a WebSocket on /v2/ipc carrying \
                       {namespace, senderId, data} frames to a hosted application's page",
                security: "none, and it does not need any — the peer is our own browser \
                           process on this host and nothing else can reach the socket",
                gate: Gate::AnyOf(&[ProtocolKind::Cast]),
                provider: Provider::Process,
                chosen_by: Provenance::Spec,
                notes: "Loopback, unlike every other entry here, and deliberately: this \
                        is the *inside* of the receiver. Anything that could open it \
                        could impersonate the device to the application — set its \
                        volume, claim a sender connected, feed it messages as though \
                        they came from a phone. 8008 is the receiver SDK's own default \
                        and a real Chromecast's port; the browser is told the actual \
                        one, so the two cannot disagree (#16).",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::MediaRange,
                bind: "the listener's address",
                wire: "mirroring RTP + RTCP (one socket per session; audio and video \
                       SSRCs demuxed on it)",
                security: "AES-CTR per Cast mirroring keys",
                gate: Gate::AnyOf(&[ProtocolKind::Cast]),
                provider: Provider::Process,
                chosen_by: Provenance::Ours,
                notes: "Bound before the OFFER is answered; named as udpPort in the \
                        ANSWER.",
            },
        ],
        ProtocolKind::Miracast => vec![
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::Configurable {
                    key: "miracast.rtp_port",
                    default: Config::default().miracast.rtp_port,
                },
                bind: "0.0.0.0 (traffic arrives on the P2P group interface)",
                wire: "MPEG2-TS over RTP from the source",
                security: "plaintext RTP; WPA2 protects the P2P link at layer 2",
                gate: Gate::AnyOf(&[ProtocolKind::Miracast]),
                provider: Provider::Process,
                chosen_by: Provenance::Convention,
                notes: "Advertised in M3 and echoed in SETUP; bound before M3 is \
                        sent. The RTSP control plane is outbound — the sink dials \
                        the source's 7236, so there is no TCP listener for it. The \
                        one below is MICE's, which is a different plane.",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Tcp,
                port: PortSpec::Fixed(proto_miracast::mice::CONTROL_PORT),
                bind: "0.0.0.0 (this is the ordinary LAN, not a P2P group)",
                wire: "[MS-MICE] control channel: the source says where its RTSP \
                       listener is, and the sink dials it",
                security: "plaintext; Windows will not attempt MICE over a WLAN \
                           without WPA2 link-layer security, and the DTLS and PIN \
                           flows are neither advertised nor served",
                gate: Gate::AnyOf(&[ProtocolKind::Miracast]),
                provider: Provider::Process,
                chosen_by: Provenance::Spec,
                notes: "Fixed by [MS-MICE] §1.9 and not IANA-registered despite the \
                        spec citing IANAPORT — 7236 is, 7250 is not. Off when \
                        [miracast] infrastructure = false (#166).",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::Fixed(67),
                bind: "the P2P group interface",
                wire: "DHCP server for the freshly-associated peer (#45)",
                security: "plaintext",
                gate: Gate::AnyOf(&[ProtocolKind::Miracast]),
                provider: Provider::Deployment("systemd-networkd, via the NixOS module"),
                chosen_by: Provenance::Spec,
                notes: "As group owner we must address the peer. The rule is not \
                        interface-scoped because the group interface (p2p-*-N) does \
                        not exist until the group forms.",
            },
        ],
        // DLNA rides the shared HTTP host and the SSDP responder; GENA delivery is
        // outbound. No socket of its own.
        ProtocolKind::Dlna => vec![],
        // DIAL rides the shared HTTP host and SSDP; everything after launch happens
        // between the phone, YouTube's Lounge servers, and the page we open.
        ProtocolKind::YouTubeLounge => vec![],
        // Spotify pairing rides the shared HTTP host and mDNS; playback is
        // librespot's outbound cloud side (D30).
        ProtocolKind::Spotify => vec![],
        // Not IP: an exclusively-claimed HCI controller and L2CAP PSMs (see the
        // non-IP section of the generated doc).
        ProtocolKind::Bluetooth => vec![],
        // The inverted protocol (D37): a pure client. Its mDNS browsing is the second
        // socket noted on the mDNS entry; the media plane dials out (moonlight ports,
        // outbound table).
        ProtocolKind::GameStream => vec![],
        ProtocolKind::MatterCast => vec![
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::Fixed(proto_matter::UDC_PORT),
                bind: "0.0.0.0",
                wire: "User Directed Commissioning: a phone's IdentificationDeclaration, \
                       answered with a CommissionerDeclaration to the port it names",
                security: "plaintext and unauthenticated by construction — UDC runs \
                           before any session exists, and what protects it is that the \
                           passcode it leads to is on the panel's own screen",
                gate: Gate::AnyOf(&[ProtocolKind::MatterCast]),
                provider: Provider::Process,
                chosen_by: Provenance::Spec,
                notes: "Five identical datagrams arrive per request, 100 ms apart, \
                        because the message has no acknowledgement. The reply goes to \
                        the cdPort the client names, which is this same number by \
                        default but is the client's to choose.",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::Fixed(proto_matter::MATTER_PORT),
                bind: "0.0.0.0",
                wire: "the Matter operational node: PASE and CASE, then the interaction \
                       model carrying ContentLauncher / MediaPlayback / ApplicationBasic",
                security: "AES-CCM under a CASE session; the fabric's root of trust is \
                           a certificate authority this panel generates and keeps",
                gate: Gate::AnyOf(&[ProtocolKind::MatterCast]),
                provider: Provider::Process,
                chosen_by: Provenance::Spec,
                notes: "Both roles run on this one socket, which is what Matter Casting \
                        being inverted costs: the panel commissions the phone over it, \
                        and then serves the phone's cluster invokes back over it.",
            },
        ],
        ProtocolKind::FCast => vec![
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Tcp,
                port: PortSpec::Fixed(proto_fcast::FCAST_PORT),
                bind: "0.0.0.0",
                wire: "FCast v1-v4 on one socket: length-prefixed JSON, or — when \
                       `[fcast] announce_v4` is set — a TLS 1.3 upgrade in place after \
                       the plaintext Version exchange, then FlatBuffers",
                security: "plaintext and unauthenticated at v1-v3; at v4, TLS 1.3 with \
                           a self-signed certificate the sender pins by SPKI SHA-256 \
                           from the `fp` TXT record or the on-screen QR (#248)",
                gate: Gate::AnyOf(&[ProtocolKind::FCast]),
                provider: Provider::Process,
                chosen_by: Provenance::Convention,
                notes: "Every published sender dials 46899 regardless of the SRV \
                        record's port, so the number is effectively fixed. 46898, the \
                        WebSocket variant some receiver builds add for browser senders, \
                        is not bound.",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::IceRange,
                bind: "the serving interface and loopback, one socket each per session",
                wire: "WebRTC: SRTP carrying a v4 sender's screen (H.264 or VP8) and \
                       its sound (Opus) — inbound media, no track of ours goes back",
                security: "DTLS-SRTP over a connection whose offer arrived on the \
                           already-authenticated v4 control session; host candidates \
                           only, so nothing off the LAN can pair",
                gate: Gate::AnyOf(&[ProtocolKind::FCast]),
                provider: Provider::Process,
                chosen_by: Provenance::Ours,
                notes: "The same `[remote.ice_ports]` range the remote-control page's \
                        peers use, and the same *allocator*: one range with two pools \
                        would hand the same port to both and the second bind would fail \
                        when a real sender connected. One mirroring session at a time, \
                        so one port (#248).",
            },
        ],
    }
}

/// Every inbound socket, shared hosts first, then per-protocol in [`ProtocolKind::ALL`]
/// order.
pub fn listeners() -> Vec<Listener> {
    let mut all = shared_listeners();
    for kind in ProtocolKind::ALL {
        all.extend(protocol_listeners(kind));
    }
    all
}

/// A discovery record the receiver publishes (or, marked so, only browses).
struct Advert {
    medium: &'static str,
    record: &'static str,
    points_at: &'static str,
    gate: ProtocolKind,
}

/// Everything the receiver makes discoverable, and over what.
fn adverts() -> Vec<Advert> {
    vec![
        Advert {
            medium: "mDNS",
            record: proto_cast::CAST_SERVICE_TYPE,
            points_at: "TCP 8009",
            gate: ProtocolKind::Cast,
        },
        Advert {
            medium: "mDNS",
            record: proto_airplay::advert::AIRPLAY_SERVICE,
            points_at: "TCP 7000",
            gate: ProtocolKind::AirPlay,
        },
        Advert {
            medium: "mDNS",
            record: proto_airplay::advert::RAOP_SERVICE,
            points_at: "TCP 7000 (same socket)",
            gate: ProtocolKind::AirPlay,
        },
        Advert {
            medium: "mDNS",
            record: proto_spotify::SPOTIFY_SERVICE_TYPE,
            points_at: "the shared HTTP port",
            gate: ProtocolKind::Spotify,
        },
        Advert {
            medium: "mDNS (browse only)",
            record: proto_gamestream::NVSTREAM_SERVICE_TYPE,
            points_at: "never advertised — the receiver browses for hosts",
            gate: ProtocolKind::GameStream,
        },
        Advert {
            medium: "mDNS",
            record: proto_matter::discovery::COMMISSIONER_SERVICE,
            points_at: "UDP 5550 — the commissioner, not the node: the panel's \
                        operational identity exists only on a fabric it created, and the \
                        phone learns that address while being commissioned",
            gate: ProtocolKind::MatterCast,
        },
        Advert {
            medium: "mDNS (browse only)",
            record: proto_matter::discovery::COMMISSIONABLE_SERVICE,
            points_at: "never advertised — the panel browses for the phone it was asked \
                        to commission",
            gate: ProtocolKind::MatterCast,
        },
        Advert {
            medium: "mDNS",
            record: proto_fcast::FCAST_SERVICE_TYPE,
            points_at: "TCP 46899; TXT v=3 states the protocol version, which is what \
                        tells a v4-capable sender to run the JSON session rather than \
                        expect a TLS upgrade",
            gate: ProtocolKind::FCast,
        },
        Advert {
            medium: "SSDP",
            record: proto_dlna::descriptions::service_types::MEDIA_RENDERER,
            points_at: "LOCATION → the shared HTTP port",
            gate: ProtocolKind::Dlna,
        },
        Advert {
            medium: "SSDP",
            record: proto_dial::DIAL_SERVICE_TYPE,
            points_at: "LOCATION → the shared HTTP port",
            gate: ProtocolKind::YouTubeLounge,
        },
        Advert {
            medium: "802.11 beacon",
            record: "WFD information element",
            points_at: "carried by wpa_supplicant; the source then listens on 7236 \
                        and the sink dials it",
            gate: ProtocolKind::Miracast,
        },
    ]
}

/// An outbound connection the receiver originates.
struct Outbound {
    to: &'static str,
    purpose: &'static str,
    when: &'static str,
}

/// Everything the receiver dials out to. Documentation, not firewall input: outbound
/// plus established/related is assumed open.
fn outbound() -> Vec<Outbound> {
    vec![
        Outbound {
            to: "the sender's control and timing ports (UDP, sender-declared)",
            purpose: "RAOP resend requests and NTP-style timing probes",
            when: "an AirPlay audio session",
        },
        Outbound {
            to: "youtube.com:443 (TLS)",
            purpose: "Lounge screen-id resolution, and the leanback page the browser \
                      hosts (which fetches whatever YouTube pages fetch)",
            when: "enable.dial, electron builds",
        },
        Outbound {
            to: "sponsor.ajay.app:443 (TLS)",
            purpose: "SponsorBlock segment lookup",
            when: "electron builds, [sponsorblock].enabled",
        },
        Outbound {
            to: "cast.remotetogo.com:443 (TLS)",
            purpose: "CKS credential windows for Cast device auth (D41)",
            when: "enable.cast + cast.replay.network",
        },
        Outbound {
            to: "api.airserver.com:443 (TLS)",
            purpose: "the AirServer credential database, ~14 MB, roughly monthly (D44)",
            when: "enable.cast + cast.replay.airserver_live",
        },
        Outbound {
            to: "Spotify access points, dealer and CDN (TLS; 443 and 4070)",
            purpose: "librespot's entire cloud side — login, connect-state, audio (D30)",
            when: "enable.spotify, once someone pairs",
        },
        Outbound {
            to: "the paired GameStream host: TCP 47989 (NVHTTP), 47984 (mutual TLS), \
             48010 (RTSP); UDP 47998/47999/48000/48010 (video, control, audio, RTSP-enc)",
            purpose: "pairing and launch are ours; the UDP media plane is \
                      moonlight-common-c (D37)",
            when: "enable.gamestream; the UDP half needs the `stream` feature",
        },
        Outbound {
            to: "the Miracast source's TCP 7236, on the P2P link",
            purpose: "the WFD RTSP session — the sink is the RTSP client",
            when: "a Miracast session (plus an ARP-priming UDP sweep to port 9, \
                   discard, on the group subnet)",
        },
        Outbound {
            to: "each DLNA subscriber's GENA CALLBACK URL",
            purpose: "eventing NOTIFYs (ack-only today, D8)",
            when: "enable.dlna, per subscription",
        },
        Outbound {
            to: "the attract widget URL (default wiki.dma.space:443) and adblock \
             filter-list hosts",
            purpose: "the idle screen's widget card; uBO filter lists for the browser",
            when: "electron builds",
        },
        Outbound {
            to: "8.8.8.8:80 (UDP connect only — no packet is ever sent)",
            purpose: "reading the default-route local address off the socket",
            when: "startup, when `interface` is unset",
        },
    ]
}

/// The port column for the spec (docs) view.
fn port_label(spec: PortSpec) -> String {
    match spec {
        PortSpec::Fixed(p) => p.to_string(),
        PortSpec::Configurable { key, default } => format!("{default} (`{key}`)"),
        PortSpec::IceRange => {
            let d = IcePortsConfig::default();
            format!("{}–{} (`[remote.ice_ports]`)", d.first, d.last)
        }
        PortSpec::MediaRange => {
            let d = MediaPortsConfig::default();
            format!("{}–{} (`[media_ports]`)", d.first, d.last)
        }
    }
}

/// The "exists when" column.
fn gate_label(gate: Gate) -> String {
    match gate {
        Gate::Always => "always".to_owned(),
        Gate::RemoteControl => "remote.enable".to_owned(),
        Gate::AnyOf(kinds) => kinds
            .iter()
            .map(|k| format!("enable.{}", enable_flag(*k)))
            .collect::<Vec<_>>()
            .join(" or "),
    }
}

/// `docs/network-surface.md`, exactly as checked in.
#[must_use]
pub fn spec_markdown() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str(
        "# Network surface\n\n\
         > **Generated** from `crates/app/src/surface.rs` — do not edit by hand.\n\
         > Regenerate with `CASTAWAY_REGEN_SURFACE=1 cargo test -p castaway surface`.\n\
         > A test fails whenever this file drifts from the registry; the registry in\n\
         > turn names the constants the listeners actually bind, and raw bind calls\n\
         > are denied by `clippy.toml` outside registered sites. `castaway\n\
         > --network-surface` prints this table resolved against the loaded config.\n\n\
         ## Listening sockets\n\n\
         | Port | Transport | Owner | Carries | Security | Exists when | Chosen by |\n\
         |---|---|---|---|---|---|---|\n",
    );
    for l in listeners() {
        let deployment = match l.provider {
            Provider::Process => String::new(),
            Provider::Deployment(who) => format!(" *(deployment: {who})*"),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {}{} | {} | {} | {} | {} |",
            port_label(l.port),
            l.transport.as_str(),
            l.owner.label(),
            deployment,
            l.wire,
            l.security,
            gate_label(l.gate),
            l.chosen_by.as_str(),
        );
    }
    out.push_str(
        "\n**Chosen by** answers \"could we move this port?\", in three tiers:\n\n\
         - **spec** — fixed by the protocol itself (a well-known rendezvous port or \
         an RFC); a sender's first packet goes there before it knows we exist. \
         Nothing on either side can move it.\n\
         - **convention** — signaled to senders (an mDNS SRV record, or in-band \
         during session setup), so movable in principle; in practice every \
         implementation in the wild uses this one number.\n\
         - **ours** — entirely our choice; senders only ever learn it from what we \
         advertise or answer.\n\n\
         The tiers correlate with the config surface, and a test holds the line: \
         every *spec* port is a fixed constant, every *ours* port has a config knob. \
         *Convention* splits case by case — `miracast.rtp_port` has a knob because \
         captures show senders tolerate variance; Cast's 8009 and AirPlay's 7000 stay \
         constants because moving a port every sender expects buys nothing and \
         exercises a path none of them is tested against. Outbound is the mirror \
         image: every destination port in the table below is the peer's or the \
         service's to pick, never ours.\n",
    );
    out.push_str("\nNotes, per listener that has one:\n\n");
    for l in listeners() {
        if !l.notes.is_empty() {
            let _ = writeln!(
                out,
                "- **{}/{} ({})** — binds {}. {}",
                port_label(l.port),
                l.transport.as_str(),
                l.owner.label(),
                l.bind,
                l.notes,
            );
        }
    }
    out.push_str(
        "\n## Multicast groups\n\n\
         - `224.0.0.251` — mDNS, joined on the serving interface.\n\
         - `239.255.255.250` — SSDP, joined on the serving interface \
         (`multicast_loop` off).\n\n\
         ## What gets advertised\n\n\
         | Medium | Record | Points at | Exists when |\n\
         |---|---|---|---|\n",
    );
    for a in adverts() {
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | enable.{} |",
            a.medium,
            a.record,
            a.points_at,
            enable_flag(a.gate),
        );
    }
    out.push_str(
        "\n## Outbound connections\n\n\
         Documentation, not firewall input — outbound plus established/related is \
         assumed open.\n\n\
         | Destination | Purpose | When |\n\
         |---|---|---|\n",
    );
    for o in outbound() {
        let _ = writeln!(out, "| {} | {} | {} |", o.to, o.purpose, o.when);
    }
    out.push_str(
        "\n## Non-IP surfaces\n\n\
         - **Bluetooth** — an exclusively-claimed HCI controller (USB, or \
         `HCI_CHANNEL_USER` on Linux); inbound audio arrives over L2CAP PSMs \
         (SDP, AVDTP, AVCTP), not sockets a host firewall sees.\n\
         - **wpa_supplicant control sockets** — Unix datagram, abstract namespace \
         `@castaway-wpa-<pid>-<n>` (Miracast, Linux).\n\
         - **The browser** — Electron is driven over stdio pipes (D36); its render \
         processes fetch whatever the hosted page fetches, over ordinary outbound \
         HTTPS.\n\n\
         ## How the firewall stays honest\n\n\
         `nix/network-surface.json` is generated from the same registry, and the \
         NixOS module derives `networking.firewall` from it: fixed ports open under \
         their enable flags, configurable ports resolve through \
         `services.castaway.settings`, and the `[media_ports]` range opens whenever \
         AirPlay or Cast is enabled. On Windows, `castaway --network-surface=netsh` \
         prints `netsh advfirewall` rules resolved against the local config.\n",
    );
    out
}

/// `nix/network-surface.json`, exactly as checked in.
///
/// Only inbound listeners: this is firewall input. `gate` holds `[enable]` flag
/// names, empty meaning always; the Nix side must know every flag named here and
/// fails evaluation on one it does not.
#[must_use]
pub fn spec_json() -> String {
    let listeners: Vec<serde_json::Value> = listeners()
        .into_iter()
        .map(|l| {
            let port = match l.port {
                PortSpec::Fixed(p) => json!({ "fixed": p }),
                PortSpec::Configurable { key, default } => json!({
                    "config": key.split('.').collect::<Vec<_>>(),
                    "default": default,
                }),
                PortSpec::IceRange => {
                    let d = IcePortsConfig::default();
                    json!({
                        "range_config": ["remote", "ice_ports"],
                        "default_first": d.first,
                        "default_last": d.last,
                    })
                }
                PortSpec::MediaRange => {
                    let d = MediaPortsConfig::default();
                    json!({
                        "range_config": ["media_ports"],
                        "default_first": d.first,
                        "default_last": d.last,
                    })
                }
            };
            let gate: Vec<&str> = match l.gate {
                Gate::Always => vec![],
                Gate::RemoteControl => vec!["remote.enable"],
                Gate::AnyOf(kinds) => kinds.iter().map(|k| enable_flag(*k)).collect(),
            };
            json!({
                "owner": l.owner.label(),
                "transport": l.transport.as_str(),
                "provider": match l.provider {
                    Provider::Process => "process",
                    Provider::Deployment(_) => "deployment",
                },
                "port": port,
                "gate": gate,
                "chosen_by": l.chosen_by.as_str(),
                "wire": l.wire,
            })
        })
        .collect();
    let doc = json!({
        "GENERATED": "by crates/app/src/surface.rs; CASTAWAY_REGEN_SURFACE=1 \
    cargo test -p castaway surface — a test fails when this file drifts",
        "listeners": listeners,
    });
    // Pretty so the diff a registry change produces is reviewable line by line.
    let mut text = serde_json::to_string_pretty(&doc).unwrap_or_default();
    text.push('\n');
    text
}

/// A listener resolved against a loaded config: concrete ports, concrete gate.
struct Resolved {
    owner: &'static str,
    transport: Transport,
    provider: Provider,
    /// Inclusive; a single port is `(p, p)`.
    ports: (u16, u16),
    enabled: bool,
    wire: &'static str,
}

fn resolve(config: &Config) -> Vec<Resolved> {
    listeners()
        .into_iter()
        .map(|l| {
            let ports = match l.port {
                PortSpec::Fixed(p) => (p, p),
                PortSpec::Configurable { key, .. } => {
                    let p = match key {
                        "http_port" => config.http_port,
                        "miracast.rtp_port" => config.miracast.rtp_port,
                        // A registry entry naming a config key this match doesn't
                        // resolve is a bug caught by `every_config_key_resolves`.
                        _ => 0,
                    };
                    (p, p)
                }
                PortSpec::MediaRange => (config.media_ports.first, config.media_ports.last),
                PortSpec::IceRange => (config.remote.ice_ports.first, config.remote.ice_ports.last),
            };
            let enabled = match l.gate {
                Gate::Always => true,
                Gate::RemoteControl => config.remote.enable,
                Gate::AnyOf(kinds) => kinds.iter().any(|k| flag_value(*k, &config.enable)),
            };
            Resolved {
                owner: l.owner.label(),
                transport: l.transport,
                provider: l.provider,
                ports,
                enabled,
                wire: l.wire,
            }
        })
        .collect()
}

/// Output shapes `--network-surface` can print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// A human table, resolved against the loaded config.
    Table,
    /// JSON of the resolved view, for scripts.
    Json,
    /// `netsh advfirewall` rules for the Windows deploy target.
    Netsh,
    /// The spec view: `docs/network-surface.md`, exactly as checked in.
    Doc,
    /// The spec view: `nix/network-surface.json`, exactly as checked in.
    Nix,
}

/// Render the surface resolved against `config`, in `format`.
#[must_use]
pub fn render(format: Format, config: &Config) -> String {
    match format {
        Format::Table => render_table(config),
        Format::Json => render_json(config),
        Format::Netsh => render_netsh(config),
        // The spec views ignore the config on purpose: they are the checked-in
        // artifacts, printed for whoever cannot run the tests where they stand.
        Format::Doc => spec_markdown(),
        Format::Nix => spec_json(),
    }
}

fn ports_label(ports: (u16, u16)) -> String {
    if ports.0 == ports.1 {
        ports.0.to_string()
    } else {
        format!("{}-{}", ports.0, ports.1)
    }
}

fn render_table(config: &Config) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(
        "What this configuration binds (castaway --network-surface):\n\n\
         PORT         PROTO  STATE  OWNER      CARRIES\n",
    );
    for r in resolve(config) {
        let state = if r.enabled { "open" } else { "off" };
        let owner = match r.provider {
            Provider::Process => r.owner.to_owned(),
            Provider::Deployment(_) => format!("{}*", r.owner),
        };
        let _ = writeln!(
            out,
            "{:<12} {:<6} {:<6} {:<10} {}",
            ports_label(r.ports),
            r.transport.as_str(),
            state,
            owner,
            r.wire,
        );
    }
    out.push_str(
        "\n* bound by the deployment (see docs/network-surface.md), not this process.\n\
         Media ports are per-session allocations inside the listed range.\n",
    );
    out
}

fn render_json(config: &Config) -> String {
    let listeners: Vec<serde_json::Value> = resolve(config)
        .into_iter()
        .map(|r| {
            json!({
                "owner": r.owner,
                "transport": r.transport.as_str(),
                "provider": match r.provider {
                    Provider::Process => "process",
                    Provider::Deployment(_) => "deployment",
                },
                "first_port": r.ports.0,
                "last_port": r.ports.1,
                "enabled": r.enabled,
                "wire": r.wire,
            })
        })
        .collect();
    let mut text =
        serde_json::to_string_pretty(&json!({ "listeners": listeners })).unwrap_or_default();
    text.push('\n');
    text
}

fn render_netsh(config: &Config) -> String {
    use std::fmt::Write as _;
    let mut out = String::from(
        "rem castaway inbound firewall rules, resolved against the local config.\n\
         rem Generated by `castaway --network-surface=netsh`; run in an elevated\n\
         rem prompt. Remove old rules first:\n\
         rem   netsh advfirewall firewall delete rule name=all program=any \
         | findstr castaway\n\
         rem Deployment-provided listeners (DHCP on the Miracast group) and\n\
         rem disabled protocols are omitted.\n",
    );
    for r in resolve(config) {
        if !r.enabled || !matches!(r.provider, Provider::Process) {
            continue;
        }
        let _ = writeln!(
            out,
            "netsh advfirewall firewall add rule name=\"castaway {} {} {}\" \
             dir=in action=allow protocol={} localport={}",
            r.owner,
            r.transport.as_str(),
            ports_label(r.ports),
            match r.transport {
                Transport::Tcp => "TCP",
                Transport::Udp => "UDP",
            },
            ports_label(r.ports),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::path::Path;

    /// Compare `generated` against the checked-in `path`; under
    /// `CASTAWAY_REGEN_SURFACE=1`, rewrite it instead.
    fn assert_current(relative: &str, generated: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative);
        if std::env::var_os("CASTAWAY_REGEN_SURFACE").is_some() {
            std::fs::write(&path, generated).unwrap();
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "reading {relative}: {e}; regenerate with \
                 CASTAWAY_REGEN_SURFACE=1 cargo test -p castaway surface"
            )
        });
        assert_eq!(
            on_disk, generated,
            "{relative} has drifted from crates/app/src/surface.rs; regenerate with \
             CASTAWAY_REGEN_SURFACE=1 cargo test -p castaway surface"
        );
    }

    #[test]
    fn surface_doc_is_current() {
        assert_current("docs/network-surface.md", &spec_markdown());
    }

    #[test]
    fn surface_json_is_current() {
        assert_current("nix/network-surface.json", &spec_json());
    }

    /// Every gate flag the JSON names must be a real `[enable]` key, or the firewall
    /// would silently treat a typo as "never open".
    #[test]
    fn every_gate_flag_is_a_real_enable_key() {
        for kind in ProtocolKind::ALL {
            let flag = enable_flag(kind);
            let toml = format!("[enable]\n{flag} = false\n");
            let config: Config = toml::from_str(&toml).unwrap();
            assert!(
                !flag_value(kind, &config.enable),
                "enable.{flag} did not reach the {kind:?} flag — the registry names \
                 a key the config schema does not have"
            );
        }
    }

    /// The remote-control gate is not a [`ProtocolKind`], so the loop above cannot
    /// cover it — and the firewall is generated from it, so an entry naming a key the
    /// schema does not have would open a range nothing binds, or worse leave one closed
    /// that something does.
    #[test]
    fn the_remote_gate_is_a_real_config_key() {
        // *Both* users of the range are turned off, which is the honest statement of what
        // closes it now: FCast's mirroring receiver draws from the same range (#248), so
        // `remote.enable = false` alone leaves it legitimately open and asserting
        // otherwise would be asserting a firewall rule that must not exist.
        let off: Config =
            toml::from_str("[remote]\nenable = false\n[enable]\nfcast = false\n").unwrap();
        assert!(!off.remote.enable);
        assert!(
            resolve(&off)
                .iter()
                .filter(|r| r.owner == "remote")
                .all(|r| !r.enabled),
            "remote.enable = false must close the remote page's ICE sockets"
        );
        assert!(
            !render_netsh(&off).contains("41032-41063"),
            "and with nothing else using the range, the firewall must not open it"
        );
        let on = Config::default();
        assert!(on.remote.enable, "the panel is drivable out of the box");
        assert!(
            resolve(&on)
                .iter()
                .any(|r| r.owner == "remote" && r.enabled),
            "and enabled it must open one"
        );

        // The other half of the same fact: the range is open for FCast alone.
        let mirroring_only: Config = toml::from_str("[remote]\nenable = false\n").unwrap();
        assert!(
            render_netsh(&mirroring_only).contains("41032-41063"),
            "a v4 sender's screen arrives on this range whether or not the remote page \
             is served, and a firewall that closed it would negotiate and carry nothing"
        );
    }

    /// Watch-but-do-not-touch does not change the socket surface: the peer connection is
    /// still made and the stream still flows, and only the input messages are dropped.
    /// If this ever started closing the range, the page would stop working entirely.
    #[test]
    fn refusing_input_still_serves_the_stream() {
        let config: Config = toml::from_str("[remote]\ninput = false\n").unwrap();
        assert!(config.remote.enable);
        assert!(!config.remote.input);
        assert!(resolve(&config)
            .iter()
            .any(|r| r.owner == "remote" && r.enabled));
    }

    /// The ICE range must not overlap `[media_ports]`. They are bound by different
    /// subsystems that do not know about each other, so an overlap is a collision that
    /// shows up as a mirroring session failing whenever someone opens the remote page.
    #[test]
    fn the_ice_range_is_clear_of_the_media_range() {
        let c = Config::default();
        let (ice, media) = (c.remote.ice_ports, c.media_ports);
        assert!(ice.first <= ice.last, "the ICE range runs forwards");
        assert!(
            ice.first > media.last || ice.last < media.first,
            "ICE {}–{} overlaps media {}–{}",
            ice.first,
            ice.last,
            media.first,
            media.last
        );
    }

    /// Every configurable port key the registry names must resolve, or
    /// `--network-surface` would print port 0 for it.
    #[test]
    fn every_config_key_resolves() {
        let config = Config::default();
        for r in resolve(&config) {
            assert_ne!(
                r.ports.0, 0,
                "a registry entry ({}) resolved to port 0 — its config key is not \
                 handled in resolve()",
                r.owner
            );
        }
    }

    /// The registry's fixed ports are the constants the listeners bind. Trivially true
    /// today because the entries are built from those constants; this pins it against
    /// someone inlining a number later.
    #[test]
    fn fixed_ports_are_the_listener_constants() {
        let all = listeners();
        let port_of = |owner: &str, transport: Transport| {
            all.iter()
                .find(|l| l.owner.label() == owner && l.transport == transport)
                .map(|l| match l.port {
                    PortSpec::Fixed(p) => p,
                    other => panic!("{owner} is not fixed: {other:?}"),
                })
                .unwrap()
        };
        assert_eq!(port_of("cast", Transport::Tcp), proto_cast::CAST_PORT);
        assert_eq!(
            port_of("airplay", Transport::Tcp),
            proto_airplay::AIRPLAY_PORT
        );
        assert_eq!(port_of("mdns", Transport::Udp), substrate_mdns::MDNS_PORT);
        assert_eq!(port_of("ssdp", Transport::Udp), substrate_ssdp::SSDP_PORT);
    }

    /// Provenance and configurability move together: a spec-forced port must be a
    /// fixed constant (offering a knob for 5353 would be a lie), and a freely-chosen
    /// port must have one (hardcoding a number nothing forces is a choice someone
    /// should be able to unmake). Convention is deliberately unconstrained — that
    /// tier is decided case by case, and the doc's legend explains each call.
    #[test]
    fn provenance_matches_the_config_surface() {
        for l in listeners() {
            match l.chosen_by {
                Provenance::Spec => assert!(
                    matches!(l.port, PortSpec::Fixed(_)),
                    "{} is spec-forced but not a fixed constant",
                    l.owner.label()
                ),
                Provenance::Ours => assert!(
                    !matches!(l.port, PortSpec::Fixed(_)),
                    "{} is ours to choose but offers no config knob",
                    l.owner.label()
                ),
                Provenance::Convention => {}
            }
        }
    }

    #[test]
    fn the_cli_flag_parses_and_rejects_unknown_formats() {
        use clap::Parser as _;
        let parse = |args: &[&str]| {
            crate::Cli::try_parse_from(std::iter::once("castaway").chain(args.iter().copied()))
        };
        // Bare flag means the human table; `=` keeps the old spelling working.
        assert_eq!(
            parse(&["--network-surface"]).unwrap().network_surface,
            Some(Format::Table)
        );
        assert_eq!(
            parse(&["--network-surface=json"]).unwrap().network_surface,
            Some(Format::Json)
        );
        assert_eq!(
            parse(&["--network-surface=netsh"]).unwrap().network_surface,
            Some(Format::Netsh)
        );
        assert!(parse(&["--something-else"]).is_err());
        assert!(parse(&["--network-surface=yaml"]).is_err());
    }

    /// `clippy.toml`'s two `tokio` bind entries are exempt from clippy's own check that
    /// the path still resolves (`allow-invalid = true`, #155). This is that check, moved
    /// somewhere it can actually run.
    ///
    /// Two ways those entries can go dead, and one assertion each. A rename upstream is
    /// caught by *naming* the functions: `castaway` depends on tokio with `net`, so the
    /// references below stop compiling if either moves. A typo in the file is caught by
    /// reading it back — the exempt set has to be exactly the set pinned here, so
    /// exempting a third entry, or misspelling one of these two, fails.
    ///
    /// Both matter because the lint is the only thing standing between an unregistered
    /// bind and a port the firewall never opens, and an entry that silently matches
    /// nothing fails open.
    #[expect(
        clippy::disallowed_methods,
        reason = "naming these two binds is the point — it is what makes an upstream \
                  rename a compile error; no socket is bound here"
    )]
    #[test]
    fn allow_invalid_entries_are_pinned_by_a_compile_time_reference() {
        // Naming them *is* the test: this does not compile if the path moves.
        let _ = tokio::net::TcpListener::bind::<std::net::SocketAddr>;
        let _ = tokio::net::UdpSocket::bind::<std::net::SocketAddr>;
        const PINNED: &[&str] = &[
            "tokio::net::TcpListener::bind",
            "tokio::net::UdpSocket::bind",
        ];

        // One entry per line, which is how the file is written and has to stay for this
        // scan to mean anything — hence the parse failure below rather than a skip.
        // Comments are dropped first: the header above this table discusses
        // `allow-invalid` in prose, and prose is not an entry.
        let exempt: Vec<&str> = include_str!("../../../clippy.toml")
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .filter(|line| line.contains("allow-invalid = true"))
            .map(|line| {
                let rest = line
                    .split_once("path = \"")
                    .unwrap_or_else(|| panic!("clippy.toml entry has no quoted path: {line}"))
                    .1;
                rest.split_once('"')
                    .unwrap_or_else(|| panic!("clippy.toml path is unterminated: {line}"))
                    .0
            })
            .collect();

        assert_eq!(
            exempt, PINNED,
            "every clippy.toml entry with `allow-invalid = true` must also be named \
             above, so that a rename is a compile error here instead of an entry that \
             silently matches nothing"
        );
    }

    #[test]
    fn disabling_a_protocol_closes_its_ports_in_the_resolved_view() {
        let toml = "[enable]\nairplay = false\ncast = false\n";
        let config: Config = toml::from_str(toml).unwrap();
        for r in resolve(&config) {
            if r.owner == "airplay" || r.owner == "cast" {
                assert!(!r.enabled, "{} should be off", r.owner);
            }
        }
        // The media range is airplay/cast-gated, so nothing else may claim it.
        let netsh = render_netsh(&config);
        assert!(!netsh.contains("41000-41031"), "{netsh}");
    }
}
