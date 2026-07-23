This is a report-writing task with all source material provided inline. No tools needed — I'll synthesize the dossiers and verdicts directly.

# Streaming-Receiver, Screen-Mirroring & Content-Casting Protocols: Landscape & Open-Receiver Scope

*Audience: a systems engineer sizing the effort to build an open receiver. Verdicts override dossiers where they conflict.*

---

## 1. Taxonomy

The single most useful framing is that "casting" is **five different things** that share transports but differ radically in what crosses the wire and what blocks an open receiver.

| Category | What crosses the wire | Sender's role | Protocols |
|---|---|---|---|
| **A. Media-URL casting** | A *content URL* + playback commands; receiver fetches & decodes the stream itself | Remote control only (not in media path) | Google Cast (URL mode), AirPlay Video/AV, DLNA/UPnP AV, Roku ECP media-launch, Amazon Fling |
| **B. Pixel screen-mirroring** | Live-encoded framebuffer (H.264/HEVC) streamed in real time | Captures, encodes, streams pixels | Miracast/WiDi, Google Cast (mirroring/"Cast Streaming"), AirPlay Mirroring, WebRTC screen-share |
| **C. App-launch / DIAL** | "Launch app X (with small payload)"; a separate protocol takes over | Discovers + launches; nothing after | DIAL, Roku ECP (launch), Presentation API |
| **D. App-specific control-handoff** | Redirect *which cloud-fed device* plays; control over proprietary channel | Controller; audio comes from cloud | Spotify Connect, Sonos, Matter Casting, YouTube Lounge (post-DIAL) |
| **E. Automotive projection** | Car-optimized UI streamed as H.264 + input/sensor backchannel | Renders dedicated UI, streams it | Wireless CarPlay, Wireless Android Auto |

Cross-cutting substrate layer (not a category — everything rides it): **mDNS/DNS-SD, SSDP/UPnP, Wi-Fi Direct, RTSP/RTP, TLS, HDCP/DTCP-IP/DRM** (Section 2).

**Key discriminator for an open-receiver builder:** categories A and C are usually easy (you're just an HTTP/REST endpoint or a media player). Category B is moderate (open pipelines exist; HDCP/device-auth gate premium content). Categories D and E are where cloud dependencies, fused keys, and hardware auth chips wall you off.

Several protocols are **multi-mode** and appear in more than one row: Google Cast (A+B+C), AirPlay (A+B+audio), Samsung Smart View (A+B+C+file-transfer), Roku ECP + Fire TV (A+B+C).

---

## 2. The Shared Substrate

Three strata sit under everything. **The discovery and transport strata are open (RFC/spec-implementable). Every hard blocker lives in the auth/pairing and link-protection strata.**

**Discovery**
- **mDNS / DNS-SD** (RFC 6762/6763, UDP 5353) — AirPlay (`_airplay._tcp`, `_raop._tcp`), Google Cast (`_googlecast._tcp`), Spotify Connect (`_spotify-connect._tcp`), Matter, OSP
- **SSDP / UPnP** (UDP 1900) — DLNA, DIAL, Roku ECP (`roku:ecp`)
- **DIAL** (SSDP + HTTP REST) — layered app-launch on top of SSDP
- **Wi-Fi Direct / P2P** (+ WPS) — Miracast, and the media path of Samsung/Xiaomi/Huawei mirroring
- **Bluetooth RFCOMM bootstrap** — Wireless CarPlay & Android Auto (hands off Wi-Fi creds), Quick Share

**Session control & media transport**
- **RTSP dialect** — AirPlay, Miracast/WFD (control on TCP 7236), CarPlay
- **RTP/RTCP over UDP** — AirPlay audio/mirroring, Miracast (as MPEG-TS-in-RTP), Cast Streaming (custom, non-RFC-strict)
- **TLS-over-TCP + protobuf** — Google Cast (CASTv2, TCP 8009), Android Auto (AAP)
- **SOAP/GENA over HTTP** — DLNA/UPnP AV, Sonos
- **QUIC + TLS 1.3 + CBOR** — Open Screen Protocol (the would-be open successor)
- **HLS/DASH/CMAF over HTTPS** — the actual bytes in all media-URL casting

**Codecs & protection**
- Video: H.264 (universal baseline), HEVC (AirPlay 2, Miracast R2, Cast), VP8/VP9/AV1 (Cast)
- Audio: AAC/AAC-ELD, ALAC, Opus, LPCM
- **Link protection**: HDCP 2.x (Miracast), DTCP-IP (DLNA) — *these protect the wire, not the content*
- **Content DRM**: Widevine/PlayReady (CENC `cenc`/AES-CTR), FairPlay (CENC `cbcs`/AES-CBC)

**Dependency map**

| Protocol | Discovery | Control | Media | Auth blocker |
|---|---|---|---|---|
| Google Cast | mDNS (+DIAL legacy) | CASTv2 protobuf/TLS:8009 | URL fetch **or** custom RTP/UDP | SoC-fused device cert → Google CA |
| AirPlay 2 | mDNS | RTSP + ChaCha20-Poly1305 | RTP/UDP (audio), H.264/HEVC (mirror), URL (video) | FairPlay-SAP (RE'd) + MFi chip (certified only) |
| Miracast | Wi-Fi Direct | RTSP:7236 | MPEG-TS/RTP/UDP | HDCP 2.x (content-gated only) |
| DLNA/UPnP | SSDP | SOAP/GENA | HTTP pull | none (DTCP-IP only for protected) |
| DIAL | SSDP | HTTP REST | *n/a — launch only* | none |
| Spotify Connect | mDNS (onboard) + cloud | dealer WebSocket (cloud) | HTTPS from CDN | Premium acct + PlayPlay DRM |
| CarPlay | BT→Wi-Fi | iAP2 + AirPlay | H.264/RTP | MFi auth coprocessor (hard) |
| Android Auto | BT→Wi-Fi / AOAP | AAP protobuf/TLS | H.264 | mTLS w/ reusable cert (soft) |
| Matter Casting | mDNS/DNS-SD | Matter clusters (PASE/CASE) | *app fetches own stream* | CSA cert + DAC vendor whitelist |

---

## 3. Per-Protocol Rundown

### Category A/B/C — Google Cast / Chromecast *(multi-mode: URL + mirroring + legacy DIAL)*
- **What:** Sender discovers a receiver via mDNS, opens a TLS control channel (CASTv2, protobuf w/ 4-byte length prefix, TCP 8009), then either sends `LOAD <url>` (receiver fetches HLS/DASH/MP4 itself) or negotiates a WebRTC-style Offer/Answer to stream AES-encrypted RTP/UDP mirroring.
- **Platforms:** Senders everywhere (Android/iOS/Chrome/ChromeOS + CAF SDK). Receivers = Chromecast dongles, Google TV Streamer (2024), Android/Google TV, licensed "Google Cast" TVs/speakers.
- **Auth/DRM:** Two layers. TLS (self-signed, doesn't authenticate device) + **device-auth**: receiver signs a challenge with a **SoC-fused private key**, presenting a cert chain → Google Cast Root CA. Enforcement is *sender-side*: Chrome/official SDKs enforce; **open senders (pychromecast, node-castv2, catt) skip it**. URL DRM = Widevine/PlayReady via a Custom Web Receiver.
- **Open impls:** `chromium/openscreen` (Google's own C++ ref — discovery, control, streaming, `standalone_receiver`), Shanocast (mirroring receiver via auth *replay*), pychromecast/node-castv2/catt (senders).
- **Closed/barrier:** CAF Web Receiver runtime; Cast partner cert-provisioning program (NDA). Barrier = fused key.
- **Docs:** Sender/receiver-app APIs public; wire protocol "open via source" (openscreen); **device-auth RE-only** (Penman, OakBits).
- **Receiver feasibility:** **Split.** Protocol mechanics fully doable on openscreen. *Interop with open senders = weekend project* (ignore device-auth). *Interop with Chrome/YouTube/Netflix = effectively blocked* by fused-key cert chain. **[Verdict-confirmed nuance]** Chrome's nonce-enforcement is a Chromium Feature `kEnforceNonceChecking`, **disabled-by-default** (verified in `cast_auth_util.cc`) — so replayed device-auth signatures *are* accepted, but **only ~48h** (receiver cert/signature validity), so Shanocast-class bypasses must refresh signatures every ~2 days.

### Category B — Miracast (Wi-Fi Display / WFD) & Intel WiDi
- **What:** "Wireless HDMI." Wi-Fi Direct P2P link (WPS/WPA2), RTSP dialect (M1–M7) negotiates codecs, then H.264 (opt. HEVC) muxed into MPEG-TS over RTP/UDP.
- **Platforms:** Sources = Windows (Win+K), some Android (Google dropped the generic API in Android 6.0; OEMs kept vendor stacks). Sinks = Windows "Wireless Display" optional feature, ScreenBeam/MiraScreen dongles, most smart TVs.
- **Auth/DRM:** WPA2 link + **HDCP 2.x — [Verdict: REFUTED that HDCP is mandatory]**. Per WFD spec v2.3, HDCP is *optional and content-gated*: invoked only when both peers support it AND protected content is sent. Unprotected mirroring needs no HDCP. Some Windows sources *request* it, blurring "optional" in practice.
- **Open impls:** MiracleCast (Linux sink solid, source partial, **no HDCP**), intel/wds (archived reference), GNOME Network Displays (source).
- **Docs:** WFD spec **publicly downloadable** (v2.3, 2024) — unusually open. Cert test plans + HDCP keys gated.
- **Receiver feasibility:** **Buildable-and-done for unprotected content** (MiracleCast). Blockers: (a) Wi-Fi Direct/P2P driver support (the real pain), (b) HDCP keys from DCP LLC for premium content — un-open-sourceable. WiDi note: **[Verdict-nuanced]** WiDi ≠ Miracast; pre-2013 WiDi was proprietary, only v3.5+ became Miracast-compatible; discontinued ~2015-16.

### Category A/B — AirPlay (1 & 2)
- **What:** A *bundle*: RAOP/AirTunes audio (RTSP/RTP), video/AV (URL handoff, *not* pixels), and mirroring (H.264/HEVC on ~port 7100). AP2 added HomeKit pairing, ChaCha20-Poly1305 control, PTP timing, multi-room.
- **Platforms:** Senders = Apple OSes (no low-level SDK; apps use AVFoundation external-playback). Receivers = Apple TV, HomePod, MFi-licensed TVs/Roku/Sonos.
- **Auth/DRM:** **[Verdict: REFUTED that AP2 pairing/ChaCha20 is mandatory for ALL sessions]** — it's *feature-flag-negotiated*. Legacy-pairing bit off + HomeKit bits off ⇒ pairing omitted; AP2 hardware stays RAOP-compatible (no pairing). The common open path is **transient pairing** (fixed PIN 3939, SRP, no pair-verify) → ChaCha20 session key. Distinguish **FairPlay-SAP** (protects the *session key* — RE'd, hardcoded reply bytes) from **FairPlay Streaming** (content DRM — a hard wall). MFi chip = certified products only.
- **Open impls:** Shairport-Sync (audio, mature), UxPlay/RPiPlay (mirroring, mature), airplay2-receiver (Python, FairPlay v3), pair_ap (pairing core), pyatv (control).
- **Docs:** No official wire spec; three RE efforts (nto.github.io AP1, openairplay HKP, Cozzi AP2 internals) + source-as-spec.
- **Receiver feasibility:** **Audio + mirroring for personal use = very feasible.** Blockers: MFi chip (certified only, sidestepped via transient pairing), FairPlay *Streaming* DRM (premium video wall), imperfect PTP multi-room. **[Verdict-nuanced]** AirPlay 1 audio ≈ always ALAC (defensible); AP2 buffered "always AAC" is **wrong** — ALAC used for 24/48 buffered; the AAC-256 outcome is Apple Music *transcoding*, not the transport.

### Category A/C — DLNA / UPnP AV
- **What:** MediaServer (DMS) / MediaRenderer (DMR) / ControlPoint (DMC). SSDP discovery, SOAP control, GENA events, DIDL-Lite metadata. Bytes travel **out-of-band over plain HTTP** — SOAP is control-only.
- **Auth/DRM:** Essentially **none** (LAN-trust, cleartext). Only DRM is **DTCP-IP** (DTLA "5C", NDA keys) — optional, protected content only.
- **Open impls:** gmrender-resurrect (DMR sink, libupnp+GStreamer), Rygel/Gerbera (servers), libupnp/GUPnP (stacks), upmpdcli.
- **Docs:** Base UPnP AV **public** (OCF/upnp.org). DLNA Guidelines semi-paywalled (cert now via SpireSpark since 2017 org dissolution). DTCP-IP NDA.
- **Receiver feasibility:** **Very tractable** (gmrender-resurrect = existence proof). Hard parts are *conformance* (GENA eventing, DIDL-Lite, `DLNA.ORG_PN` flags), not crypto. Sole blocker = DTCP-IP for protected content (rare in home/NAS use).
- **[Verdict-confirmed] Push/pull clarified:** DLNA "push" (DMR) = the *controller pushes a URI* via `SetAVTransportURI`; the **renderer still HTTP-GETs the bytes** from the server. Byte-level "push" only happens on isochronous transports (IEEE-1394), a *separate* UPnP axis. DMR = passive, DMC-driven; DMP = control-point+renderer that browses/drives itself (**[Verdict-confirmed]**, though real devices often implement both roles).

### Category C — DIAL
- **What:** Two operations only: SSDP discovery (ST `urn:dial-multiscreen-org:service:dial:1` — **[Verdict-confirmed]** exact value) + a tiny HTTP REST API (`GET`/`POST`/`DELETE` on `{Application-URL}/{AppName}`) to launch/query/stop a named app. **Not** streaming or mirroring.
- **Auth:** None in core (LAN-trust, plaintext); 2.x adds CORS-origin controls. Real auth is app-specific *after* launch (YouTube `screenId`→Lounge `loungeToken`).
- **Open impls:** pydial, peer-dial (both sides), ytcast (DIAL + RE'd YouTube Lounge), leapcast (abandoned).
- **Docs:** **Fully public** (v1.6.4 © 2012 Netflix, BSD-style; through v2.2.1). **[Verdict-confirmed]** Authored by Netflix + YouTube, with Sony/Samsung as reviewers (Netflix holds copyright + mark).
- **Receiver feasibility:** **Easiest here** — a few hundred lines. Blockers aren't in DIAL: convincingly impersonating YouTube/Netflix needs the real (closed, DIAL-registered) TV app + undocumented post-launch protocol + DRM. This killed leapcast when Cast moved to mDNS.

### Category D — Spotify Connect
- **What:** Control/handoff, **not streaming**: the *receiver* pulls audio directly from Spotify's CDN; the phone is only a remote. Coordination via cloud "dealer" WebSocket; local mDNS zeroconf (`_spotify-connect._tcp`) only for password-less account onboarding (`addUser`).
- **Auth/DRM:** Token/blob-based (never shares password). AP session uses Shannon cipher. Content = AES-128-CTR, but key provisioning migrated to **PlayPlay** (obfuscated w/ hardcoded client key, not publicly cracked) + Widevine/FairPlay for lossless.
- **Open impls:** librespot (Rust, de-facto), spotifyd, go-librespot, librespot-java. Closed: eSDK (NDA C-API binary for hardware partners).
- **Docs:** Only zeroconf onboarding is official; everything else RE-only (some DMCA'd).
- **Receiver feasibility:** **Well-trodden but fragile.** No hardware chip. Blockers: **Premium account required**, PlayPlay/Widevine (no lossless), protocol churn (keymaster/OAuth breakages), legal/ToS exposure.

### Category D — Sonos
- **What:** Multi-room mesh (SonosNet, AES). Historically UPnP AV (SOAP on :1400 — AVTransport/RenderingControl/ZoneGroupTopology). Now pushing an official OAuth cloud REST Control API; local UPnP being gradually deprecated. Music services via SMAPI (SOAP/WSDL). Ingests AirPlay 2 + Spotify Connect.
- **Auth:** Local UPnP ≈ unauthenticated on LAN; cloud API = OAuth 2.0; SMAPI = several models; AirPlay 2 ingest needs FairPlay+HomeKit.
- **Open impls:** SoCo, node-sonos-http-api (controllers); bonob (SMAPI music-service side); svrooij docs.
- **Receiver feasibility:** **Impersonating a Sonos player = impractical** (proprietary SonosNet mesh, no open "fake Sonos"). The tractable open targets are the protocols Sonos *ingests*: UPnP/OpenHome (easy) and AirPlay 2 (medium, FairPlay blocker).

### Category D — Matter Casting
- **What:** CSA app-handoff on Matter's media clusters. Casting Client commissions onto the TV's fabric, then sends `ContentLauncher`/`MediaPlayback` cluster commands telling an **app already on the TV** to fetch and play. **No pixel/media stream from sender; no DRM of its own** (the content app owns DRM).
- **Auth:** Standard Matter PASE→CASE; the client's **DAC vendor** is an access-control signal (apps whitelist client vendors).
- **Open impls:** `connectedhomeip` tv-casting-app (sender, iOS/Android/Linux), tv-app (receiver ref — *dummy content apps only, no real playback*). Spec **public** (CSA Application Cluster spec, freely downloadable).
- **Receiver feasibility:** Commissioning + control = weekend on the SDK. Blockers: it's a *control* protocol — usefulness = which content apps (w/ their own DRM + CSA cert) you can host; mainstream apps won't run unsanctioned. Audio-only casting essentially unspecified. Adoption ≈ Amazon-only (Fire TV/Echo Show; Prime Video, Tubi). **[Verdict correction from a dossier]** Matter Casting does *not* wrap Google Cast Widevine / AirPlay FairPlay — it is DRM-agnostic.

### Category A/B/C — Roku ECP & Amazon Fling/Whisperplay
- **Roku ECP:** Open, unauthenticated REST on **:8060** (SSDP `roku:ecp`) — keypress, launch, deep-linked media-URL launch, query. Plus separate DIAL target + Miracast/AirPlay 2 sinks. Only gate = on-device "Control by mobile apps" toggle. **Fully documented.** *Building an ECP/DIAL receiver is trivial (HTTP+SSDP)* but no polished open sink exists (python-roku's "emulator" is a stub).
- **Amazon Fling:** Media-URL fling on **Whisperplay/WhisperLink** = proprietary **Apache Thrift over HTTP** (RE-only, single-origin). Fire TV also does DIAL (via Whisperplay) + Miracast. **EOL 2026-03-05**, steered to Matter Casting.
- **Receiver feasibility:** ECP+DIAL = easy/open. Miracast = moderate (MiracleCast). **Fling/Whisperplay = hard/blocked** (proprietary Thrift, no open impl, and now EOL — not worth targeting).

### Category E — Wireless CarPlay
- **What:** iPhone renders a *dedicated* CarPlay UI, H.264-streams it via AirPlay-mirroring stack; BT/iAP2 bootstraps Wi-Fi; input/sensors flow back. **[Verdict-nuanced]** "just AirPlay mirroring" is directionally true but wrong: adds iAP2/MFi layer, a purpose-built UI (not raw phone mirror), bidirectional input. A plain AirPlay receiver *cannot* accept a CarPlay session.
- **Auth:** Two layers — **MFi Authentication Coprocessor** (Apple silicon, non-emulatable) + HomeKit/AirPlay pairing.
- **Open impls:** **[Verdict-nuanced]** react-carplay/node-carplay/pycarplay/FastCarPlay are **host software that drives a licensed Carlinkit/Autobox dongle** over USB — *the dongle is the actual receiver + auth chip*, not the open code. okcar-os claims native no-dongle (dubious provenance).
- **Receiver feasibility:** **Native open receiver = licensing wall** (MFi chip). Pragmatic open path = buy a licensed dongle + write USB host software. Dongle-driven = weekend-to-months; native standalone = blocked.

### Category E — Wireless Android Auto (AAP)
- **What:** Phone renders car UI, H.264 over protobuf-over-TLS, multiplexed channels (video/audio/mic/input/sensor). USB (AOAP) or BT→Wi-Fi. Distinct from Android Automotive OS.
- **Auth:** **[Verdict-corrected]** In-protocol TLS where the *head unit is the TLS client presenting a hardcoded, reusable cert* (verified: `Cryptor::cCertificate` in aasdk — a real 2014 **JVC/Kenwood** cert signed by Google's Automotive Link CA, valid to 2045, *not* a "Google reference receiver" cert). **Head unit sets `SSL_VERIFY_NONE`** — so it's *not* symmetric mutual TLS; the phone verifies the head unit, not vice-versa. **No mandatory hardware auth chip** — the key CarPlay contrast.
- **Open impls:** aasdk/openauto (foundational), LIVI (AA+CarPlay, cross-platform), headunit-revived, WirelessAndroidAutoDongle/aa-proxy-rs (bridges). Google's x86 `headunit` emulator.
- **Receiver feasibility:** **Comparatively feasible and done** — because no HU auth chip. Blockers are *practical*: RE'd protobufs + possibly-non-redistributable cert, protocol churn, GPLv3, and no *certification* to ship a logo'd product. **[Verdict-nuanced]** Wireless bootstrap: phone = TCP client, HU = TCP server (confirmed); but port 5288/IP 10.0.0.1 are dongle-project *defaults*, not constants, and **Wi-Fi Direct is the norm for real OEM head units** (SoftAP is the DIY-dongle pattern) — the dossier's "hosted-AP vs Wi-Fi Direct" framing inverts typical reality.

### Category B — WebRTC screen-share (for contrast)
Real WebRTC (`getDisplayMedia` + `RTCPeerConnection`, DTLS-SRTP, ICE) is peer-to-peer, **not a way to reach a Cast sink** — it's your own endpoint with your own signaling. Chrome's Chromecast mirroring is *Cast Streaming* (custom RTP + AES from a JSON offer/answer over CASTv2), **commonly mislabeled "WebRTC."** Fully open, but a different problem shape.

---

## 4. Receiver-Side Scope Assessment (Tiered)

### Tier 1 — Realistically buildable open (specs public / clean RE, no crypto wall)
| Protocol | Effort | Notes |
|---|---|---|
| **DIAL receiver** | Hours–days | Public spec; SSDP + tiny REST. Useless without a real launchable app, though. |
| **Roku ECP receiver** | Weekend | Public docs; plaintext HTTP:8060 + SSDP. No open sink exists yet — build from docs. |
| **DLNA/UPnP DMR** | Weekend | gmrender-resurrect proves it. Effort = conformance, not crypto. |
| **Matter Casting receiver** | Weekend (commissioning) | Public CSA spec + `tv-app` ref. But needs real content apps to be useful. |
| **Open Screen Protocol sink** | Modest | Royalty-free, QUIC/TLS1.3/CBOR/SPAKE2-PIN pairing — *no vendor-key gatekeeper*. But near-zero senders to talk to. |
| **Google Cast receiver (open-sender interop only)** | Weekend on openscreen | Ignore device-auth → works with pychromecast/Home Assistant/catt. **Not** Chrome/YouTube/Netflix. |

### Tier 2 — Hard-but-done (reverse-engineered, fragile, legal grey)
| Protocol | Blocker character |
|---|---|
| **AirPlay audio + mirroring** | Shairport-Sync/UxPlay work via transient pairing + RE'd FairPlay-SAP (hardcoded reply bytes). iOS updates periodically break it. |
| **Miracast sink (unprotected)** | MiracleCast works; pain = Wi-Fi Direct/P2P driver support. |
| **Spotify Connect** | librespot works but needs **Premium**, breaks on protocol churn, PlayPlay blocks lossless, ToS/DMCA risk. |
| **Wireless Android Auto** | aasdk/LIVI work via a hardcoded reusable cert; no HU chip. Fragile to Google app changes; GPLv3; can't ship certified. |
| **Cast Streaming (mirroring) receiver** | openscreen `standalone_receiver` decodes it, but sits behind CASTv2 device-auth for Chrome senders. |

### Tier 3 — Effectively locked (fused keys / hardware chips / DRM / NDA / cloud)
| Protocol | Specific blocker |
|---|---|
| **Google Cast (Chrome/Netflix/YouTube interop)** | Per-device **SoC-fused private key** → Google Root CA. Only bypass = extract a real device's key (Shanocast, ~48h replay window). Not shippable. |
| **Wireless CarPlay (native)** | **Apple MFi Authentication Coprocessor** — non-emulatable Apple silicon. Only open path = drive a licensed dongle. |
| **Any HDCP-protected content** | HDCP 2.x device keys from **DCP LLC** — un-open-sourceable. Gates premium Miracast/streaming. |
| **Any FairPlay-Streaming / Widevine / PlayReady content** | Licensed CDM + secure hardware (Widevine L1, PlayReady SL3000). Distinct from RE'd FairPlay-*SAP*. |
| **DLNA protected content** | **DTCP-IP** keys from DTLA (NDA). |
| **Sonos-native player** | Proprietary AES SonosNet mesh; no open "fake Sonos." |
| **Amazon Fling/Whisperplay** | Proprietary Thrift substrate + SID registration; and EOL 2026-03-05. |

**Blocker taxonomy (the walls, named):**
- **Fused-key / device-cert PKI** → Google Cast (the canonical "protocol is open, the cert isn't" case).
- **Hardware auth chip** → CarPlay MFi coprocessor (hard); AirPlay MFi (soft — sidestepped by transient pairing).
- **Link-protection keys** → HDCP (DCP LLC), DTCP-IP (DTLA) — *link, not content* protection, but DRM demands them downstream.
- **Content DRM CDM** → Widevine/PlayReady/FairPlay-Streaming.
- **Cloud dependency** → Spotify Connect, Sonos cloud API, Matter Casting content apps.
- **protobuf-over-TLS device auth** → Google Cast CASTv2 challenge (the specific mechanism).
- **Certification / registration** → Cast partner program, CSA cert + DAC whitelist, DLNA-app registry — blocks *shipping a certified product*, not a working one.

---

## 5. Cross-Cutting Notes

### Commonly-confused distinctions, clarified

**Google Cast vs Chromecast vs Chromecast built-in vs Google Cast (2024)** — **[Verdict-nuanced]**
- **Chromecast** = the *hardware/brand* (never went away).
- **Google Cast** = the *protocol + SDK* (developer SDK called "Google Cast SDK" continuously; preview Jul 2013, full release Feb 2014).
- **"Chromecast built-in"** = the *licensed-receiver-tech label in TVs/speakers* — this is what got renamed, not the SDK. The 2016 flip was messier than usually told (the consumer *app* went Chromecast→Google Cast earlier in 2016, then Nov 2016 reversed to "Chromecast built-in"). The 2024 reversal back to "Google Cast" was a **quiet website-wording change (~May 26 2024), NOT a Google I/O announcement** (I/O was May 14–15).

**Google Cast vs DIAL** — DIAL is *app-launch only* (SSDP + REST, no media). First-gen Chromecast used DIAL-over-SSDP; Cast v2 moved to mDNS + proprietary CASTv2. YouTube still uses DIAL to launch, then hands off to the RE'd Lounge API. "The cast button uses DIAL" is only true for the *discover+launch* step of built-in TV apps, never the streaming.

**Miracast vs WiDi** — **[Verdict-nuanced]** *Not the same thing.* Miracast = the open WFA standard. WiDi = Intel's brand/implementation, proprietary pre-2013, only Miracast-compatible from v3.5 (Sept 2012); discontinued ~2015-16 once OSes shipped native Miracast.

**AirPlay variants** — AirPlay 1 (RSA/FairPlay legacy pairing, NTP) vs AirPlay 2 (HomeKit pairing, ChaCha20, PTP, multi-room). And within AP2, **auth is feature-flag-negotiated, not universally mandatory** (transient pairing, RAOP fallback). Critically: **FairPlay-SAP** (session-key protection, RE'd) ≠ **FairPlay Streaming** (content DRM, blocked).

**HDCP/DTCP-IP vs DRM** — link protection (device-to-device wire) ≠ content DRM (encrypts the media). DRM *requires* HDCP downstream as a chain-of-trust condition, but they're separate mechanisms with separate licensors.

**Cast Streaming vs WebRTC** — Chrome's mirroring borrows WebRTC's offer/answer *concept* but uses custom RTP/AES, not DTLS-SRTP/ICE. Not interchangeable.

**Media-URL casting vs mirroring** — the single most load-bearing distinction: URL casting = receiver fetches the stream (low sender load, high quality, sender is a remote); mirroring = sender encodes/streams live pixels. Matter Casting and Spotify Connect are pure *control-handoff* — the sender streams *nothing*.

### Notable trends
- **Chromecast device EOL & the March 2025 cert-expiry outage** — the gen-2 Intermediate CA expired (~10yr from 2015), bricking device auth for official senders — a vivid demonstration that the fused-key PKI is both the moat and a single point of failure. Google is retiring the Chromecast line in favor of the **Google TV Streamer (2024)**.
- **Matter Casting** — CSA's royalty-free, cross-ecosystem bid to replace Cast/AirPlay's proprietary auth with open Matter commissioning. Real but **Amazon-only adoption** so far; video-centric, no audio/multi-room yet.
- **Open Screen Protocol** — W3C's intended open successor to Cast/DIAL (backs the Presentation + Remote Playback APIs over QUIC/CBOR/SPAKE2). Architecturally the friendliest open receiver (no cert gatekeeper) but **negligible installed base** — nothing to talk to.
- **DLNA org dissolution (Jan 2017)** — certification moved to **SpireSpark**, now open to non-members. UPnP moved to OCF. Base specs stay public; the ecosystem is mature but stagnant.
- **Amazon Fling EOL (2026-03-05)** — Amazon collapsing its proprietary Fling into Matter Casting.
- **Convergence on standards underneath, divergence on top** — every OEM stack (Samsung Smart View, Xiaomi, Huawei) rides Miracast + DLNA as the interoperable common denominator, then wraps proprietary discovery/control (Huawei Cast+, Samsung MSF WebSocket) that only interoperates within-brand. For an open receiver, **target the Miracast + DLNA substrate** and treat the proprietary layers (Cast+, Tap View, MSF casting) as out-of-reach.

---

**Bottom line for scoping:** Build the **discovery + transport + decode substrate** open — that's solved and RFC-backed. Expect exactly two walls: the **auth/pairing gate** (fused keys, MFi chips, device-cert PKI) and the **link-protection/DRM gate** (HDCP, DTCP-IP, Widevine/FairPlay-Streaming). The most valuable open receivers to build today are DLNA DMR, an AirPlay audio+mirroring sink, a Miracast sink, and a Google Cast receiver scoped to the open-sender ecosystem — accepting that Chrome/Netflix/YouTube/CarPlay interop is a licensing wall, not an engineering one.