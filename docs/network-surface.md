# Network surface

> **Generated** from `crates/app/src/surface.rs` — do not edit by hand.
> Regenerate with `CASTAWAY_REGEN_SURFACE=1 cargo test -p castaway surface`.
> A test fails whenever this file drifts from the registry; the registry in
> turn names the constants the listeners actually bind, and raw bind calls
> are denied by `clippy.toml` outside registered sites. `castaway
> --network-surface` prints this table resolved against the loaded config.

## Listening sockets

| Port | Transport | Owner | Carries | Security | Exists when | Chosen by |
|---|---|---|---|---|---|---|
| 8080 (`http_port`) | tcp | http | HTTP/1.1 — UPnP descriptions, DLNA SOAP + GENA, DIAL REST, Spotify zeroconf pairing, /screenshot.png | plaintext HTTP (LAN control plane) | always | ours |
| 5353 | udp | mdns | mDNS/DNS-SD responder (mdns-sd daemon) | plaintext multicast | always | spec |
| 1900 | udp | ssdp | SSDP M-SEARCH responder + NOTIFY alive/byebye | plaintext multicast | always | spec |
| 7000 | tcp | airplay | RTSP + HTTP/1.1 on one socket: AirPlay control and RAOP | plaintext — pair-verify/FairPlay not implemented (#39) | enable.airplay | convention |
| 41000–41031 (`[media_ports]`) | udp | airplay | RAOP audio + control + timing (three sockets per session) | RTP; AES-CBC when the sender negotiates it | enable.airplay | ours |
| 41000–41031 (`[media_ports]`) | tcp | airplay | mirroring data channel (one listener per session) | AES-CTR frames (MirrorKeys) | enable.airplay | ours |
| 8009 | tcp | cast | CASTv2: length-prefixed protobuf over TLS | TLS, self-signed or CKS-replayed certificate; the device-auth signature covers it (D41/D43) | enable.cast | convention |
| 41000–41031 (`[media_ports]`) | udp | cast | mirroring RTP + RTCP (one socket per session; audio and video SSRCs demuxed on it) | AES-CTR per Cast mirroring keys | enable.cast | ours |
| 1028 (`miracast.rtp_port`) | udp | miracast | MPEG2-TS over RTP from the source | plaintext RTP; WPA2 protects the P2P link at layer 2 | enable.miracast | convention |
| 67 | udp | miracast *(deployment: systemd-networkd, via the NixOS module)* | DHCP server for the freshly-associated peer (#45) | plaintext | enable.miracast | spec |

**Chosen by** answers "could we move this port?", in three tiers:

- **spec** — fixed by the protocol itself (a well-known rendezvous port or an RFC); a sender's first packet goes there before it knows we exist. Nothing on either side can move it.
- **convention** — signaled to senders (an mDNS SRV record, or in-band during session setup), so movable in principle; in practice every implementation in the wild uses this one number.
- **ours** — entirely our choice; senders only ever learn it from what we advertise or answer.

The tiers correlate with the config surface, and a test holds the line: every *spec* port is a fixed constant, every *ours* port has a config knob. *Convention* splits case by case — `miracast.rtp_port` has a knob because captures show senders tolerate variance; Cast's 8009 and AirPlay's 7000 stay constants because moving a port every sender expects buys nothing and exercises a path none of them is tested against. Outbound is the mirror image: every destination port in the table below is the peer's or the service's to pick, never ours.

Notes, per listener that has one:

- **8080 (`http_port`)/tcp (http)** — binds 0.0.0.0. One host shared by three protocols (D7); a disabled protocol's routes are simply not mounted. /screenshot.png always answers.
- **5353/udp (mdns)** — binds 0.0.0.0, multicast 224.0.0.251, SO_REUSEADDR/SO_REUSEPORT. Advertises only enabled protocols, restricted to the serving interface. GameStream's host browser runs a second daemon — a second 5353 socket — when enabled. Contends with Avahi/Bonjour for answers (#43); the NixOS module warns when Avahi is on.
- **1900/udp (ssdp)** — binds 0.0.0.0, multicast 239.255.255.250, SO_REUSEADDR/SO_REUSEPORT. Bound even with DLNA and DIAL both off; it then answers for no device type.
- **7000/tcp (airplay)** — binds 0.0.0.0. Both _airplay._tcp and _raop._tcp advertise this one port. Nothing binds 7011: it is the AirPlay 1 UDP timing port, not a listener, and the listener once bound there was removed.
- **41000–41031 (`[media_ports]`)/udp (airplay)** — binds the accepted connection's local address. Bound per sender before SETUP answers, so the Transport header only ever names ports that are already listening.
- **41000–41031 (`[media_ports]`)/tcp (airplay)** — binds the accepted connection's local address. Answered as dataPort in the second SETUP reply.
- **41000–41031 (`[media_ports]`)/udp (cast)** — binds the listener's address. Bound before the OFFER is answered; named as udpPort in the ANSWER.
- **1028 (`miracast.rtp_port`)/udp (miracast)** — binds 0.0.0.0 (traffic arrives on the P2P group interface). Advertised in M3 and echoed in SETUP; bound before M3 is sent. The RTSP control plane is outbound — the sink dials the source's 7236, so there is no TCP listener.
- **67/udp (miracast)** — binds the P2P group interface. As group owner we must address the peer. The rule is not interface-scoped because the group interface (p2p-*-N) does not exist until the group forms.

## Multicast groups

- `224.0.0.251` — mDNS, joined on the serving interface.
- `239.255.255.250` — SSDP, joined on the serving interface (`multicast_loop` off).

## What gets advertised

| Medium | Record | Points at | Exists when |
|---|---|---|---|
| mDNS | `_googlecast._tcp` | TCP 8009 | enable.cast |
| mDNS | `_airplay._tcp` | TCP 7000 | enable.airplay |
| mDNS | `_raop._tcp` | TCP 7000 (same socket) | enable.airplay |
| mDNS | `_spotify-connect._tcp` | the shared HTTP port | enable.spotify |
| mDNS (browse only) | `_nvstream._tcp` | never advertised — the receiver browses for hosts | enable.gamestream |
| SSDP | `urn:schemas-upnp-org:device:MediaRenderer:1` | LOCATION → the shared HTTP port | enable.dlna |
| SSDP | `urn:dial-multiscreen-org:service:dial:1` | LOCATION → the shared HTTP port | enable.dial |
| 802.11 beacon | `WFD information element` | carried by wpa_supplicant; the source then listens on 7236 and the sink dials it | enable.miracast |

## Outbound connections

Documentation, not firewall input — outbound plus established/related is assumed open.

| Destination | Purpose | When |
|---|---|---|
| the sender's control and timing ports (UDP, sender-declared) | RAOP resend requests and NTP-style timing probes | an AirPlay audio session |
| youtube.com:443 (TLS) | Lounge screen-id resolution, and the leanback page the browser hosts (which fetches whatever YouTube pages fetch) | enable.dial, electron builds |
| sponsor.ajay.app:443 (TLS) | SponsorBlock segment lookup | electron builds, [sponsorblock].enabled |
| cast.remotetogo.com:443 (TLS) | CKS credential windows for Cast device auth (D41) | enable.cast + cast.replay.network |
| api.airserver.com:443 (TLS) | the AirServer credential database, ~14 MB, roughly monthly (D44) | enable.cast + cast.replay.airserver_live |
| Spotify access points, dealer and CDN (TLS; 443 and 4070) | librespot's entire cloud side — login, connect-state, audio (D30) | enable.spotify, once someone pairs |
| the paired GameStream host: TCP 47989 (NVHTTP), 47984 (mutual TLS), 48010 (RTSP); UDP 47998/47999/48000/48010 (video, control, audio, RTSP-enc) | pairing and launch are ours; the UDP media plane is moonlight-common-c (D37) | enable.gamestream; the UDP half needs the `stream` feature |
| the Miracast source's TCP 7236, on the P2P link | the WFD RTSP session — the sink is the RTSP client | a Miracast session (plus an ARP-priming UDP sweep to port 9, discard, on the group subnet) |
| each DLNA subscriber's GENA CALLBACK URL | eventing NOTIFYs (ack-only today, D8) | enable.dlna, per subscription |
| the attract widget URL (default digitalclock.live:443) and adblock filter-list hosts | the idle screen's widget card; uBO filter lists for the browser | electron builds |
| 8.8.8.8:80 (UDP connect only — no packet is ever sent) | reading the default-route local address off the socket | startup, when `interface` is unset |

## Non-IP surfaces

- **Bluetooth** — an exclusively-claimed HCI controller (USB, or `HCI_CHANNEL_USER` on Linux); inbound audio arrives over L2CAP PSMs (SDP, AVDTP, AVCTP), not sockets a host firewall sees.
- **wpa_supplicant control sockets** — Unix datagram, abstract namespace `@castaway-wpa-<pid>-<n>` (Miracast, Linux).
- **The browser** — Electron is driven over stdio pipes (D36); its render processes fetch whatever the hosted page fetches, over ordinary outbound HTTPS.

## How the firewall stays honest

`nix/network-surface.json` is generated from the same registry, and the NixOS module derives `networking.firewall` from it: fixed ports open under their enable flags, configurable ports resolve through `services.castaway.settings`, and the `[media_ports]` range opens whenever AirPlay or Cast is enabled. On Windows, `castaway --network-surface=netsh` prints `netsh advfirewall` rules resolved against the local config.
