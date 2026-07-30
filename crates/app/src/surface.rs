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

use crate::config::{Config, Enable, MediaPortsConfig};

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

/// When the listener exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// Bound whenever the receiver runs.
    Always,
    /// Bound when any of these protocols is enabled.
    AnyOf(&'static [ProtocolKind]),
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
                   Spotify zeroconf pairing, /screenshot.png",
            security: "plaintext HTTP (LAN control plane)",
            gate: Gate::Always,
            provider: Provider::Process,
            notes: "One host shared by three protocols (D7); a disabled protocol's \
                    routes are simply not mounted. /screenshot.png always answers.",
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
            notes: "Advertises only enabled protocols, restricted to the serving \
                    interface. GameStream's host browser runs a second daemon — a \
                    second 5353 socket — when enabled. Contends with Avahi/Bonjour \
                    for answers (Q5); the NixOS module warns when Avahi is on.",
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
            notes: "Bound even with DLNA and DIAL both off; it then answers for no \
                    device type.",
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
                security: "plaintext — pair-verify/FairPlay not implemented (Q1)",
                gate: Gate::AnyOf(&[ProtocolKind::AirPlay]),
                provider: Provider::Process,
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
                notes: "",
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
                notes: "Advertised in M3 and echoed in SETUP; bound before M3 is \
                        sent. The RTSP control plane is outbound — the sink dials \
                        the source's 7236, so there is no TCP listener.",
            },
            Listener {
                owner: Owner::Protocol(kind),
                transport: Transport::Udp,
                port: PortSpec::Fixed(67),
                bind: "the P2P group interface",
                wire: "DHCP server for the freshly-associated peer (Q7c)",
                security: "plaintext",
                gate: Gate::AnyOf(&[ProtocolKind::Miracast]),
                provider: Provider::Deployment("systemd-networkd, via the NixOS module"),
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
            to: "the attract widget URL (default digitalclock.live:443) and adblock \
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
         | Port | Transport | Owner | Carries | Security | Exists when |\n\
         |---|---|---|---|---|---|\n",
    );
    for l in listeners() {
        let deployment = match l.provider {
            Provider::Process => String::new(),
            Provider::Deployment(who) => format!(" *(deployment: {who})*"),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {}{} | {} | {} | {} |",
            port_label(l.port),
            l.transport.as_str(),
            l.owner.label(),
            deployment,
            l.wire,
            l.security,
            gate_label(l.gate),
        );
    }
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
            };
            let enabled = match l.gate {
                Gate::Always => true,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Did the command line ask for the network surface, and in which shape?
///
/// # Errors
/// If `--network-surface=` names an unknown format.
pub fn requested(args: impl Iterator<Item = String>) -> Option<Result<Format, String>> {
    for arg in args {
        match arg.as_str() {
            "--network-surface" | "--network-surface=table" => return Some(Ok(Format::Table)),
            "--network-surface=json" => return Some(Ok(Format::Json)),
            "--network-surface=netsh" => return Some(Ok(Format::Netsh)),
            "--network-surface=doc" => return Some(Ok(Format::Doc)),
            "--network-surface=nix" => return Some(Ok(Format::Nix)),
            other => {
                if let Some(fmt) = other.strip_prefix("--network-surface=") {
                    return Some(Err(format!(
                        "unknown --network-surface format {fmt:?}; expected table, json, \
                         netsh, doc or nix"
                    )));
                }
            }
        }
    }
    None
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

    #[test]
    fn the_cli_flag_parses_and_rejects_unknown_formats() {
        let args = |s: &str| Some(s.to_owned()).into_iter();
        assert_eq!(
            requested(args("--network-surface")),
            Some(Ok(Format::Table))
        );
        assert_eq!(
            requested(args("--network-surface=json")),
            Some(Ok(Format::Json))
        );
        assert_eq!(
            requested(args("--network-surface=netsh")),
            Some(Ok(Format::Netsh))
        );
        assert_eq!(requested(args("--something-else")), None);
        assert!(matches!(
            requested(args("--network-surface=yaml")),
            Some(Err(_))
        ));
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
