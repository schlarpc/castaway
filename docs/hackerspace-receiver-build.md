# Hackerspace Universal Cast Receiver — Build & Reimplementation Notes

**Scope:** n=1, closed, private single box driving the TV at the space. Consequence:
- **Legal:** moot — private use, no redistribution, no commercial gain.
- **Revocation:** moot — one invisible box, never in a public SPKI list; re-provision if ever burned.
- **Durability / distinctness / hiding:** moot — nobody's aiming at you.

So this is the *easy* version of the problem. The only real cost is engineering effort, and the whole point is the craft. **Goal: one unified receiver, one codebase, max protocol coverage — "throw anything at the wall from any device, no app install."**

Existing implementations below are **reference / RE sources to crib the wire behavior from**, not runtime dependencies. We reimplement for consistency.

---

## Why reimplement instead of gluing five daemons together

The frankenstack (UxPlay + librespot + gmrender + MiracleCast + Shanocast) works, but at the cost of five processes fighting over the network and the framebuffer. Reimplementing buys real consolidation:

- **One mDNS-SD responder** and **one SSDP/UPnP responder** instead of five racing to advertise on the LAN.
- **One media pipeline / render surface / audio sink** instead of five apps racing for the framebuffer and ALSA/PipeWire.
- **Shared RTSP + RTP + crypto** code — AirPlay and Miracast both speak RTSP dialects; AirPlay and Cast both live on mDNS; the FairPlay-SAP and Cast-auth modules are used by multiple front-ends.
- **Unified OSD** ("now casting from <device>"), one config, one static binary, structured logging.

Honest tradeoff: you rebuild well-trodden wheels. Mitigation: the reference impls *are* your spec — diff against them on the wire.

Suggested stack: Rust or Go for a single static binary; GStreamer (or a custom pipeline) for decode/render; one `tokio`/goroutine event loop hosting all adapters.

---

## Architecture

```
                 ┌─────────────────────────────────────────┐
   senders  →    │  DISCOVERY LAYER                         │
 (any device)    │   mDNS-SD responder │ SSDP/UPnP responder│
                 │   Wi-Fi Direct beacon (Miracast, separate)│
                 └───────────────┬─────────────────────────┘
                                 │  advertises service types
                 ┌───────────────▼─────────────────────────┐
                 │  PROTOCOL ADAPTERS (one per protocol)    │
                 │   AirPlay │ Cast │ Miracast │ DLNA │      │
                 │   YouTube Lounge │ Spotify Connect        │
                 └───────────────┬─────────────────────────┘
                                 │  common internal API:
                                 │   PLAY(uri, drm?) · MIRROR(stream)
                                 │   CONTROL(txn)   · advertise()
                 ┌───────────────▼─────────────────────────┐
                 │  SESSION MANAGER  →  MEDIA PIPELINE       │
                 │   (GStreamer) → HDMI surface + audio sink │
                 │   + OSD / control / logging               │
                 └───────────────────────────────────────────┘
```

**Shared subcomponents to build once:** mDNS-SD responder · SSDP/UPnP responder · RTSP server · RTP/RTCP depacketizer · TLS + pairing · FairPlay-SAP · Cast device-auth signer.

---

## Protocol surface (grouped by behavior)

### A. Pixel screen-mirroring (sender encodes live frames)

| Protocol | Discovery | Transport | Auth/crypto blocker | Crib from | Effort |
|---|---|---|---|---|---|
| **AirPlay Mirroring** | mDNS `_airplay._tcp` | RTSP setup → H.264/HEVC over RTP | **FairPlay-SAP** (session-key protection) + optional HomeKit pairing (use transient pairing PIN 3939) | UxPlay, RPiPlay, openairplay | Med |
| **Miracast sink** | **Wi-Fi Direct/P2P** beacon (not mDNS) | RTSP:7236 → MPEG-TS in RTP/UDP | HDCP 2.x only if source demands it (optional) | MiracleCast, GNOME Network Displays | Med-high — **the pain is Wi-Fi P2P driver support, not the protocol** |
| **Cast Streaming** (Chrome tab/desktop mirror) | mDNS `_googlecast._tcp` | CASTv2 offer/answer → custom RTP + AES | **Device-auth** (device certificate — see crypto modules) | openscreen `standalone_receiver`, Shanocast | Med + cert |

### B. Media-URL casting (receiver fetches & decodes the stream itself)

| Protocol | Discovery | Control | Media | Notes | Crib from | Effort |
|---|---|---|---|---|---|---|
| **DLNA MediaRenderer** | SSDP | SOAP AVTransport/RenderingControl | HTTP pull | pure conformance, no crypto | gmrender-resurrect, Rygel | **Low** |
| **Cast media receiver** | mDNS | CASTv2 `LOAD <url>` | HLS/DASH/MP4 (your player) | needs device-auth; this is the Default Media Receiver (`CC1AD845`) role only — see below for hosting *other* apps | openscreen, CAF docs | Med (shares Cast auth) |
| **Cast app receiver** (branded senders) | mDNS | CASTv2 `LAUNCH <appId>` → host the vendor's web receiver | whatever that app streams — in practice DASH/HLS `avc1`+`mp4a`, usually Widevine | **Intended, not built (GAPS.md G56).** Needs a codec-enabled browser (G55 — answered by ECS, D36), the undocumented CAF platform IPC, and a device-auth chain official senders will accept — that last one gates the rest | CAF docs, capture | High |
| **AirPlay Video/AV** | mDNS | RTSP URL handoff | HLS | the iOS "AirPlay a YouTube/Netflix video" path (not mirroring) | AirPlay RE docs | Med |

### C. App-launch / control-handoff (sender is a remote; device plays its own stream)

| Protocol | How it works | Crib from | Effort | Payoff |
|---|---|---|---|---|
| **YouTube Lounge** (a.k.a. MDX) | DIAL launches your "YouTube app" → you register a **screen** with YouTube's Lounge server → obtain `loungeToken` → subscribe to the **bind channel** (BrowserChannel-style long-poll: `gsessionid`, `RID`/`AID`/`SID`) → parse commands (`setPlaylist`, `play`, `pause`, `seekTo`, `getNowPlaying`) → drive your player with the `videoId`s. Two pairing modes: same-LAN auto (DIAL) and cross-network **TV code** (`youtube.com/pair`). | **`yt-cast-receiver`** (Node, implements the *receiver* side — the key one), `ytcast` (Go, sender), thedrhax MDX RE writeups | Med | **The real YouTube cast button** — native queue, full quality, phone-as-remote. Playback backend = embedded Chromium *or* `yt-dlp` → pipeline. |
| **Spotify Connect** | zeroconf `_spotify-connect._tcp` for onboarding + cloud "dealer" WebSocket for control; device pulls audio from Spotify CDN | librespot, go-librespot | Low-med (crib heavily) | Everyone has Spotify; "pick Hackerspace TV" always lands. **Needs a Premium account.** |
| **Netflix & other DIAL apps** | DIAL *launch* is trivial (SSDP + REST); the post-launch protocol is proprietary **and DRM-walled** | pydial (launch only) | — | **Skip.** Discovery works, playback is a Widevine wall. Mention for completeness. |
| **Matter Casting** | Matter fabric commissioning → `ContentLauncher`/`MediaPlayback` clusters; app on TV fetches stream | `connectedhomeip` tv-app | High/low-ROI | Amazon-mostly today. Flex-for-completeness only. |

---

## Crypto / auth modules (the interesting bits — build once)

Each protocol gates its session behind an auth handshake; model each as a module the relevant adapter drives. The engineering interest is speaking the handshake correctly on the wire — treat the credential material each one needs as a configured local input, out of scope for these notes.

- **FairPlay-SAP (AirPlay):** protects the AirPlay *session key* (the "~568 bytes" / v3 flow). This is the gate in front of AirPlay mirroring; crib the handshake *shape* from `airplay2-receiver`. Fragile across iOS versions → expect to revisit per major iOS. Distinct from **FairPlay Streaming** (content DRM — a wall we don't touch).
- **Cast device-auth signer:** CASTv2 requires the receiver to present a device certificate during the auth challenge. Model it as a signer module the Cast adapter calls; at n=1 the credential is a fixed local input, so no rotation or refresh server is needed (that whole AirScreen architecture solves scale problems you don't have).
- ~~**Widevine L3 CDM**~~ — DRM'd media-URL content is out of scope; we don't touch content DRM.

**The one systemic risk that remains:** Google flipping `enforce_nonce_checking = true` in Chrome would break Cast replay — but it'd break every commercial receiver simultaneously, so you'd have loud company and warning. Not yours to solve alone.

---

## Discovery advertisement matrix (make every cast button light up)

| Advertise | Service | Lights up |
|---|---|---|
| mDNS `_airplay._tcp` + `_raop._tcp` | AirPlay mirror/video/audio | all Apple devices |
| mDNS `_googlecast._tcp` | Google Cast | Chrome, Android mirror |
| mDNS `_spotify-connect._tcp` | Spotify Connect | any Spotify app |
| SSDP `urn:dial-multiscreen-org:service:dial:1` | DIAL → YouTube Lounge | YouTube app cast button |
| SSDP `MediaRenderer` | DLNA | Android media apps, VLC |
| Wi-Fi Direct P2P beacon | Miracast | Windows Win+K, some Androids |

---

## Effort / priority tiers

- **Tier 0 — weekend, huge flex/effort ratio:** DLNA renderer · Spotify Connect · AirPlay audio. All clean, no crypto walls.
- **Tier 1 — the core stunt:** AirPlay mirroring (FairPlay-SAP) · Cast mirroring + media (device-auth) · **YouTube Lounge**.
- **Tier 2 — completionist:** Miracast (Wi-Fi P2P driver fight) · AirPlay video handoff.
- **Tier 3 — flex-for-flex:** Matter Casting · multiroom/audio groups.

---

## Reference implementations (your spec, not your dependency)

AirPlay: UxPlay, RPiPlay, shairport-sync (audio), airplay2-receiver, openairplay ·
Cast: openscreen (`standalone_receiver`), Shanocast, pychromecast (sender), cast_channel.proto ·
Miracast: MiracleCast, GNOME Network Displays ·
DLNA: gmrender-resurrect, Rygel, Gerbera ·
YouTube Lounge: **yt-cast-receiver** (receiver), ytcast (sender), thedrhax MDX RE ·
Spotify: librespot, go-librespot.
