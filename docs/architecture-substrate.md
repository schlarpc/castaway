# Substrate & Pipeline Architecture (Rust, cross-platform)

Design for the unified receiver. Targets Linux + Windows. Decode via libav (ffmpeg), composite via wgpu, browser surfaces via CEF OSR, display control via DDC/CEC/serial.

---

## 1. The shared substrate — what's *actually* shared vs. what only looks shared

The honest rule: **share the wire framing and connection plumbing; do NOT try to share the protocol semantics.** Every one of these protocols has a bespoke state machine; forcing a common one is where these projects go to die.

### 1a. RTSP — AirPlay vs Miracast

Both are "RTSP," but that word means two different amounts of standard.

**Shared (build once):** the RTSP *message codec* — request line (`METHOD uri RTSP/1.0`), headers, `CSeq` correlation, `Content-Length` body framing, response encoding — plus a connection actor that owns the TCP socket, reads/writes messages, and matches responses to requests by CSeq.
- Crate: **`rtsp-types`** (sdroege / GStreamer-adjacent) — pure parser/encoder for RTSP messages. This *is* your shared framing layer. Don't hand-roll it.
- Wrap it in a `RtspConn` actor with a **pluggable transform layer** on the byte stream: identity for Miracast, **ChaCha20-Poly1305** for AirPlay 2 post-pairing (the control channel gets encrypted after pair-verify — Miracast has no such thing). That transform slot is the one crypto concession the shared layer makes.

**Divergent (per-adapter, do NOT share):**
| | Miracast (WFD) | AirPlay |
|---|---|---|
| Methods | OPTIONS, GET/SET_PARAMETER, SETUP, PLAY, TEARDOWN (M1–M7) | SETUP, RECORD, FLUSH, SET/GET_PARAMETER, TEARDOWN + custom `POST /pair-setup`, `/pair-verify`, `/fp-setup`, `/feedback` |
| Body format | newline-separated `wfd_*` key:value text | **binary plists** (`bplist00`) and SDP (RAOP `ANNOUNCE`) |
| Port | TCP 7236 | TCP 7000 (+ 7100 media, 7011 events…) |
| Encryption | none | ChaCha20 envelope after pairing |
| State machine | capability negotiation → stream | pair → fp-setup → setup → record |

So: `substrate-rtsp` gives you `RtspConn` + `Message`. `proto-airplay` and `proto-miracast` each own their method dispatch, body parsers (plist / SDP / wfd-kv), and state machine. "Closer than they look" is true *at the framing/transaction layer and nowhere above it.*

### 1b. RTP/RTCP — shared parse, divergent depacketization

**Shared:** RTP header parse (seq/timestamp/SSRC), reorder/jitter buffer, RTCP. Crate: **`webrtc-rs` `rtp`/`rtcp`** or `rtp-rs`.

**Divergent — the payload is where they split:**
- **Miracast:** RTP PT 33 = **MPEG-TS in RTP**. Depacketize → concat 188-byte TS → demux (PAT/PMT → PES → H.264 + AAC/AC3). **Shortcut:** ffmpeg's `rtp_mpegts` demuxer eats this stream *whole* — you can hand libav a `udp://`/`rtp://` and skip custom depacketization entirely. Strong argument to let ffmpeg own the Miracast media path.
- **AirPlay audio (RAOP):** RTP with ALAC (AP1) / AAC-ELD (AP2) payload + **separate control (retransmit) and timing (NTP/PTP) streams**. The hard part isn't the codec (ffmpeg has ALAC) — it's the **audio clock recovery / sync**. Reference: shairport-sync. This *uses* the shared RTP crate.
- **AirPlay mirroring (video):** **NOT standard RTP.** Apple's own framing on port 7100: a custom packet header, then **AES-encrypted H.264 NALUs** (key from FairPlay-SAP), SPS/PPS out-of-band, plus timing. Bespoke depacketizer — does *not* share the RTP crate. Reference: UxPlay/RPiPlay.

Takeaway: the RTP crate pays off for Miracast (or skip via ffmpeg) and RAOP; AirPlay mirroring is its own thing regardless.

### 1c. mDNS-SD — fully shared, no caveats

One responder advertising every service instance with its TXT records. This is the *cleanest* shared layer.
- Crate: **`mdns-sd`** (pure Rust, responder + browser, cross-platform).
- Advertises: `_airplay._tcp`, `_raop._tcp`, `_googlecast._tcp`, `_spotify-connect._tcp`.
- **Cross-platform gotcha:** port 5353 contention. Linux → Avahi may hold it (disable Avahi on the kiosk box, or use its API via `zeroconf`). Windows → Bonjour (from iTunes) may hold it; `mdns-sd` generally coexists with `SO_REUSEADDR` + multicast join, but test. For a controlled hackerspace box, owning 5353 yourself is cleanest.

### 1d. SSDP/UPnP — shared responder + HTTP description; SOAP vs REST on top

**Shared:** SSDP responder (answer `M-SEARCH`, periodic `NOTIFY ssdp:alive` on UDP 1900 multicast) + an HTTP server hosting the device/service description XML. Roll SSDP over `tokio::net::UdpSocket` (it's simple); HTTP via **`axum`/`hyper`**; XML via `quick-xml`.

**Divergent on top of the same HTTP server:**
- **DLNA MediaRenderer:** SOAP actions (`AVTransport`/`RenderingControl`) + GENA eventing (subscriptions/`NOTIFY`). Conformance-heavy, no crypto.
- **DIAL → YouTube Lounge:** REST app endpoints (`GET/POST/DELETE {app-url}/YouTube`), then the Lounge bind-channel client (separate, talks to YouTube's cloud). DIAL advertises ST `urn:dial-multiscreen-org:service:dial:1`.

So `substrate-ssdp` = responder + description host; `proto-dlna` and `proto-dial` are modules mounting their handlers on the shared `axum` router.

### 1e. The odd one out — Miracast Wi-Fi Direct discovery

Miracast doesn't use IP multicast discovery at all; it's **Wi-Fi Direct / P2P** at L2, which is OS-specific and privileged:
- Linux: `wpa_supplicant` P2P (control socket) + kernel nl80211. Needs a P2P-capable adapter and often exclusive control of the interface.
- Windows: WinRT `Windows.Devices.WiFiDirect`; note Windows *ships its own Miracast receiver* (`Windows.Media.Miracast`) you could lean on instead of reimplementing.

Recommendation: treat Miracast as a **platform-abstracted, deferrable** adapter behind a trait — it won't share the IP substrate, and the Wi-Fi P2P driver fight is the single biggest yak-shave in the project. Get everything else working first.

---

## 2. Cargo workspace layout

```
receiver/
├─ Cargo.toml                 # [workspace]
├─ crates/
│  ├─ core/                   # session mgr, internal API, event bus, traits
│  ├─ substrate-mdns/         # mdns-sd wrapper: advertise N services
│  ├─ substrate-ssdp/         # SSDP responder + UPnP description host (axum)
│  ├─ substrate-rtsp/         # rtsp-types + RtspConn actor + transform slot
│  ├─ substrate-rtp/          # RTP/RTCP parse + jitter buffer
│  ├─ crypto-fairplay/        # FairPlay-SAP (lift/emulate)
│  ├─ crypto-cast-auth/       # Cast device-auth signer (one carved gen-1 cert)
│  ├─ proto-airplay/          # AirPlay: mirror + audio + video (uses rtsp,rtp,mdns,fairplay)
│  ├─ proto-cast/             # CASTv2 (prost) mirror + media (uses mdns, cast-auth)
│  ├─ proto-miracast/         # WFD (uses rtsp, ffmpeg rtp_mpegts, platform wifi-direct)
│  ├─ proto-dlna/             # MediaRenderer (uses ssdp)
│  ├─ proto-dial/             # DIAL + YouTube Lounge bind channel (uses ssdp, http)
│  ├─ proto-spotify/          # Spotify Connect (uses mdns)
│  ├─ pipeline/               # ffmpeg decode + wgpu compositor + winit + CEF
│  ├─ control-display/        # DDC (ddc-hi) / CEC (cec-rs) / serial (serialport)
│  └─ app/                    # the binary: config, wiring, kiosk output
```

Key third-party crates: `tokio`, `rtsp-types`, `mdns-sd`, `webrtc`(rtp/rtcp), `prost`, `ffmpeg-next`, `wgpu`, `winit`, `axum`, `quick-xml`, `ddc-hi`, `cec-rs`, `serialport`, plus a CEF binding (see §5, immaturity risk).

---

## 3. Core traits (the internal API everything funnels into)

```rust
// crates/core

/// What an adapter needs advertised on the network to be discoverable.
pub enum Advertisement {
    MdnsService { ty: String, port: u16, txt: Vec<(String, String)> },
    SsdpDevice  { st: String, description: DeviceXml },
    WifiDirect  { /* Miracast P2P params */ },
}

/// The single internal command surface. Every protocol reduces to these.
pub enum SessionEvent {
    /// Media-URL casting: receiver fetches & decodes (Cast LOAD, AirPlay video, DLNA, Lounge).
    Play { source: MediaUri, drm: Option<DrmContext>, start: Option<Duration> },
    /// Live pixel mirroring: a stream of encoded frames the pipeline decodes.
    Mirror { video: FrameSource, audio: Option<FrameSource> },
    /// Transport control (play/pause/seek/volume/queue) over the active session.
    Control(ControlTxn),
    End,
}

pub enum FrameSource {
    /// Adapter hands us a URL; pipeline opens it with libav.
    Url(MediaUri),
    /// Adapter depacketizes/decrypts and pushes *encoded* frames (AirPlay mirror, Linux Miracast).
    Encoded(tokio::sync::mpsc::Receiver<EncodedFrame>),
    /// Adapter (or the OS) hands us *already-decoded* frames / GPU surfaces.
    /// REQUIRED because Windows `MiracastReceiver` decodes for you (MediaSource → frames),
    /// whereas Linux Miracast gives Encoded. Bake this in from day one so the Linux
    /// backend swap is not a core-trait change. On Windows this is a D3D11 texture.
    Decoded(tokio::sync::mpsc::Receiver<DecodedFrame>),
}

#[async_trait::async_trait]
pub trait SourceAdapter: Send + Sync {
    fn advertisements(&self) -> Vec<Advertisement>;
    /// Runs the adapter's whole lifecycle; emits SessionEvents to the session mgr.
    async fn run(self: Arc<Self>, tx: SessionSink) -> anyhow::Result<()>;
}
```

The session manager arbitrates (one active source at a time, or main+PiP), owns the OSD, and drives the pipeline. Adapters never touch the GPU — they only emit `SessionEvent`s.

**Miracast is split behind a `MiracastBackend` trait** (the one genuinely per-OS adapter):
```rust
#[async_trait::async_trait]
pub trait MiracastBackend: Send + Sync {
    /// Acquire a P2P-GO interface + run the WFD/RTSP session; emit frames.
    async fn run(self: Arc<Self>, tx: SessionSink) -> anyhow::Result<()>;
}
// backend-windows: Windows.Media.Miracast::MiracastReceiver via `windows` crate → FrameSource::Decoded
// backend-linux:   MiracleCast/wpa_supplicant + own RTSP/MPEG-TS demux   → FrameSource::Encoded
```
Everything else is OS-agnostic. Target today is **Windows** (§7.5) — write `backend-windows` first; the Linux move later is this one crate.

---

## 4. Video pipeline: libav → wgpu compositor → kiosk surface

```
 EncodedFrame / Url ─▶ ffmpeg decode (hwaccel) ─▶ AVFrame
                                                    │  (GPU surface or CPU)
                                                    ▼
      CEF OSR OnPaint ─▶ browser texture ─▶  ┌─ wgpu Compositor ─┐
      OSD text ────────▶ overlay texture ─▶  │ layers: quads +   │ ─▶ winit surface (fullscreen)
      cast video ──────▶ video texture ───▶  │ transforms + z    │
                                             └───────────────────┘
```

- **Decode:** `ffmpeg-next` (maintained libav bindings). Hardware decode via ffmpeg hwaccel: **d3d11va** (Windows) / **vaapi** (Linux). Codecs: H.264/HEVC (mirroring, HLS), ALAC/AAC (AirPlay audio).
- **Compositor:** `wgpu` (DX12 on Windows, Vulkan on Linux). A `Compositor` owns device/queue/surface and a `Vec<Layer { texture, transform: Mat3, z, opacity }>`. Each present: update dirty textures, one render pass drawing textured quads back-to-front. **PiP is just a layer with a scale+translate transform** — the whole point of a real compositor over five apps fighting the framebuffer.
- **Zero-copy path (the perf endgame, not MVP):** ffmpeg hwaccel decodes to a GPU surface; import into wgpu without a round-trip:
  - Windows: D3D11 texture → DXGI **shared handle** → wgpu external texture.
  - Linux: VAAPI surface → **dmabuf** → EGL/Vulkan import.
  - MVP first: decode → CPU `AVFrame` → upload to wgpu texture. Wire zero-copy once it works.
- **Kiosk output:** `winit` fullscreen borderless on the HDMI output. Linux kiosk options: run under a minimal Wayland/X, or go direct **DRM/KMS** (via `drm`/`smithay`) for no-compositor fullscreen. Windows: borderless fullscreen window.

## 5. CEF integration (PiP browser + doubles as the YouTube Lounge renderer)

- Run CEF in **offscreen rendering (OSR)** mode: it renders pages to a pixel buffer (`OnPaint` with dirty rects) or, with `shared_texture_enabled`, to a **D3D11 shared texture** (Windows) for zero-copy into the compositor.
- Feed that surface in as a compositor **layer** → free PiP, overlays, transparent HUDs.
- **Double duty:** the YouTube "app" you host for the Lounge protocol can *be* a CEF instance loading YouTube's TV surface — the Lounge command channel drives the page, CEF renders it, the compositor shows it. One browser solves both PiP *and* the Lounge playback backend.
- **Risk, corrected (verified 2026-07):** the binding itself is *fine* — use **`tauri-apps/cef-rs`** (the canonical one; several stale forks exist — space-07, hamaluik, dylanede, Julusian/rust-cef — ignore them). It tracks upstream tightly: release `cef-v150.2.1` (Chromium 150) dated 2026-07-21, all three platforms incl. ARM64, ~8 open issues. The *real* risks are narrower:
  - **Accelerated / shared-texture OSR is buggy in upstream CEF itself**, not the Rust layer — e.g. `OnAcceleratedPaint` handle comes back null on CEF 143/Windows ([cef#4057](https://github.com/chromiumembedded/cef/issues/4057)), and accelerated OSR doesn't emit damage rects ([cef#3730](https://github.com/chromiumembedded/cef/issues/3730)). **So: MVP on the plain CPU `OnPaint` buffer path (mature); treat GPU zero-copy OSR as aspirational and keep it off the critical path.**
  - **CEF integration tax** (language-independent): multi-process model needs a subprocess entry point (`cef_execute_process` early in `main`, or a helper exe), and you ship the ~hundreds-of-MB CEF distribution + ICU + resources.
  - `wry`/WebView2 is still *not* a substitute — it renders to its own window, not into your compositor, so no PiP.

## 6. Threading model (a real constraint — get it right early)

Three thread domains, because each subsystem has non-negotiable affinity:
- **Main thread:** `winit` event loop + `wgpu` present. (winit strongly prefers main thread.)
- **CEF thread(s):** CEF's own message pump (`do_message_loop_work` or multi-threaded loop). Must not block on your async runtime.
- **Tokio runtime (worker pool):** all protocol adapters, network I/O, decode orchestration.

Cross-domain comms via channels. Decoded frames flow decode-thread → render-thread via an mpsc of GPU-uploadable frames; **for live mirroring, drop late frames** (latency > freshness). Textures are only touched on the render thread.

## 7. Cross-platform matrix

| Subsystem | Linux | Windows |
|---|---|---|
| mDNS | `mdns-sd` (mind Avahi on 5353) | `mdns-sd` (mind Bonjour on 5353) |
| SSDP/HTTP | portable (`tokio`+`axum`) | portable |
| RTSP/RTP | portable | portable |
| Decode hwaccel | VAAPI | D3D11VA |
| GPU compositor | wgpu/Vulkan | wgpu/DX12 |
| Zero-copy import | dmabuf/EGL | DXGI shared handle |
| **Miracast Wi-Fi Direct** | wpa_supplicant P2P (privileged) | WinRT WiFiDirect / OS Miracast |
| Kiosk output | DRM/KMS or minimal WM | borderless fullscreen |
| Display control | RS-232 (`serialport`) + DDC (`ddc-hi` via i2c-dev) | RS-232 + DDC (monitor config API) |
| Touch input | `evdev` (USB HID) | Raw Input / WM_POINTER |

Miracast is the only genuinely hard cross-platform gap. Everything above it is portable.

## 7.5 Network & radio topology (Miracast vs everything else)

**Only Miracast needs Wi-Fi Direct.** AirPlay, Cast, DLNA, Spotify, YouTube Lounge all ride the normal LAN (mDNS/SSDP). So plan two independent planes:

- **LAN plane** (Ethernet *or* a STA Wi-Fi radio): upstream internet + all the mDNS/SSDP protocols. The box must be on the **same L2 subnet as the casting devices** or multicast discovery dies — watch for AP/client isolation on hackerspace Wi-Fi.
- **P2P plane** (a dedicated Wi-Fi radio in Group-Owner mode): Miracast only.

**Do not try to run STA-upstream + Miracast-P2P on one radio.** A single radio time-shares the two roles; most chipsets pin the P2P group to the STA's current channel (single-channel concurrency) or time-slice with jitter (multi-channel) — either way real-time mirroring drops frames and/or the upstream flaps.

**Baseline decision: Ethernet upstream + the single internal Wi-Fi radio for Miracast.** This is the config for the current **Windows** target, and it's not just "cleaner" there — it's the *only* reliable option, because:
- **Windows can't pin Wi-Fi Direct to a chosen adapter.** No public API selects the radio (`WiFiDirectAdvertisementPublisher` takes none), Windows creates exactly **one** un-pinnable "Wi-Fi Direct Virtual Adapter" regardless of how many physical radios you add, and it *contends* with Mobile Hotspot for it. A second Wi-Fi card does **not** buy you a dedicatable P2P interface on Windows.
- So on Windows, the dual-radio trick fails; make the one Wi-Fi radio's *only* job Wi-Fi Direct by putting upstream on Ethernet. Then Windows has nothing to arbitrate.
- **Linux (future target) can pin** — bind wpa_supplicant/MiracleCast to a named `wlanX`, so the second-dedicated-radio trick *does* work there (verify `iw list` for P2P-GO; `mt76`/Intel good, out-of-tree Realtek a coin flip). This asymmetry is a point in Linux's favor for a Miracast-heavy crowd — but Ethernet-upstream is the right baseline either way.

## 8. Display / input control — target hardware: Dell C6522QT (commercial panel)

The display is a **Dell C6522QT** — a 65" 4K commercial *interactive touch* monitor, not a consumer TV. That's much friendlier: it has documented control interfaces and proper DDC/CI, and **CEC is irrelevant** (drop it — no dongle needed).

Control, in priority order for this panel:
- **RS-232 (primary)** — Dell publishes an *RS232 External Control Application* / documented command set (power, input-source select, etc.; straight-through cable). Most reliable path on a commercial panel. Crate: **`serialport`** (cross-platform) + Dell's command table.
- **DDC/CI (secondary/alt)** — the panel implements it properly (Dell Display Manager drives it), including input-source switch via VCP `0x60`. Crate: **`ddc-hi`** (`i2c-dev` on Linux, monitor config API on Windows).
- ~~HDMI-CEC~~ — deleted. Consumer-TV mechanism; this panel doesn't need it.

Design `control-display` behind a trait with RS-232 + DDC backends; the session manager fires `DisplayControl::PowerOn` / `SelectInput(self)` on session start.

**Bonus interfaces this panel unlocks (a TV wouldn't):**
- **Touch input** — it's an interactive panel; touch arrives over **USB HID**. This is an *input* vector, not just display control: read the HID touch device and route events into the compositor / CEF, so the 65" surface actually drives the UI. New workspace crate: **`input-touch`** (`evdev` on Linux, Raw Input / WM_POINTER on Windows). Big flex: a 4K touch wall driven by your compositor + CEF.
- **USB-C single-cable** — the panel's USB-C does DP-alt (DP 1.4, **4K60**) + USB data + up to 90W PD. One cable from the PC can carry **video-out + touch-in** (and, if the PC is USB-C-powered, power too — moot here since you already have a full PC). Output target: **3840×2160 @ 60**.

## 9. Suggested build order

Priorities are set by the **actual crowd** (NixOS/Rust/Windows programmers + some Mac), not a phone-heavy mall. Screen-mirroring is the headline, so **Cast desktop-mirroring and Miracast are core, not deferred.** Dev happens **natively on Linux** (the portable ~90%); the Windows-specific slice cross-builds (§10).

1. `core` traits + session mgr + null pipeline (log events).
2. `pipeline` MVP: ffmpeg decode a test URL → wgpu → fullscreen. Prove the render path *on Linux*.
3. `substrate-ssdp` + `proto-dlna` — easiest end-to-end "cast a video from VLC and see it." First real win.
4. `substrate-mdns` + `proto-spotify` (Tier-0 flex) + `proto-cast` media-URL mode.
5. **`crypto-cast-auth` + `proto-cast` mirroring** — the *workhorse* for this crowd (Chrome/Edge "Cast desktop" from Linux **and** Windows, LAN-based, no Wi-Fi Direct). Carve one gen-1 cert.
6. `substrate-rtsp`/`substrate-rtp` + `proto-airplay` audio → mirroring (+ `crypto-fairplay`). Mac contingent.
7. `proto-dial` + YouTube Lounge (+ CEF — the risky/cross-build-heavy bit; CPU-OnPaint MVP).
8. **`proto-miracast` `backend-windows`** — promoted to core (Win+K + Linux GNOME Network Displays senders). *Cheap on Windows* (thin `MiracastReceiver` OS-call), so it's not the yak it is on Linux. Needs Ethernet-upstream (§7.5).
9. `control-display` (RS-232 primary / DDC) + `input-touch` (USB HID touch → CEF/compositor).
10. *Later:* `proto-miracast` `backend-linux` when/if the box migrates (the one crate the move touches).

## 10. Cross-build (dev Linux → target Windows)

See **`cross-build.md`**. TL;DR: portable ~90% runs native on the Linux dev box; the Windows slice targets **`x86_64-pc-windows-msvc` via `cargo-xwin`** (MSVC needed for CEF's import libs + windows-rs). CEF is the cross-link boss fight — fall back to a **`windows-latest` CI build** for the CEF-heavy binary if cross-linking fights back. Wine can't test Miracast/WinRT/DX12/CEF — the physical C6522QT-connected Windows box is the integration rig.

---

## 11. Bluetooth audio sink — the owned stack

The one castable surface that needs **no LAN, no app, and no ecosystem membership**: any
phone, including a guest's locked-down one, can pair and play. Audio-only, ~100–200 ms
latency, so it is a *music + now-playing screen* surface, not a mirroring one. It is also
the first source that gives us **playback control back toward the sender** and **rich track
metadata**, which is why it forces a new core interface rather than reusing `SessionEvent`
as-is (§11.5).

### 11.1 Why we own the stack instead of using the OS

Both OS-provided routes fail the ground rules, in different places:

| | Linux (BlueZ) | Windows (inbox) |
|---|---|---|
| Codecs in sink role | all of them — BlueZ delegates codec choice to *our* `MediaEndpoint1` | **SBC only**, 44.1 kHz forced. aptX/LDAC are vendor codecs shipped by IHVs for the *source* role only. |
| Stream access | yes — `MediaTransport1.Acquire()` hands us an fd of codec payloads | **no** — `AudioPlaybackConnection` is an open/close toggle; decoded PCM goes to the default render endpoint. We'd have to WASAPI-loopback the system mix. |
| Metadata | `MediaPlayer1` gives Title/Artist/Album/Duration/Status/Position | via GSMTC, coarser |
| Album art | **no** — `bluetoothd` never surfaces AVRCP attribute 8 and `obexd` has no BIP client | unknown; possibly free if the inbox stack fetches it |
| Reproducible test | D-Bus daemon in the loop | untestable from Linux CI |

There is also no user-mode L2CAP on Windows at all — Winsock's `AF_BTH` is RFCOMM-only,
and AVDTP (PSM `0x19`) / AVCTP (PSM `0x17`) are therefore unreachable without a
kernel-mode profile driver. So "implement the profile ourselves on top of the OS stack" is
available on Linux and *impossible* on Windows.

**Decision: own everything above HCI.** One portable stack, one set of fixtures, identical
behaviour on both platforms, every codec, and album art — none of which is reachable any
other way. The platform seam drops all the way down to *"give me a byte stream of HCI
packets"*, which is the smallest and most stable seam available (ground rule 5).

Two things make this smaller than it sounds:
- **BR/EDR pairing crypto lives in the controller.** Secure Simple Pairing does P-192/P-256
  ECDH and link-key derivation on the chip. Just Works needs no `crypto-*` crate — the host
  does IO-capability exchange and confirmation only. (Contrast FairPlay, Q1.)
- **These are small, well-documented binary protocols** in exactly the sans-I/O shape the
  project already builds (`fn(state, bytes) -> (state, outputs)`), so they are fixture-tested
  without a radio.

### 11.2 Crate layout

```
substrate-hci/        HCI packet codec (cmd/event/ACL) + the HciTransport trait
hci-transport/        The backends: Linux HCI_CHANNEL_USER socket, USB/WinUSB via nusb
substrate-l2cap/      BR/EDR L2CAP: signaling, basic-mode channels, PSM routing
substrate-sdp/        SDP data elements, record server, minimal client
proto-bluetooth-audio/  AVDTP/A2DP + AVCTP/AVRCP + OBEX-BIP cover art
```

**Why the transports are a separate crate from `substrate-hci`:** ground rule 8 has every
`substrate-*` crate at `unsafe_code = "forbid"`, and the Linux `HCI_CHANNEL_USER` socket
needs raw syscalls. So the backends live in `hci-transport`, declared as an FFI/interop
crate alongside `pipeline` and `input-touch`. Note the USB backend needs no `unsafe` at all
— `nusb` is a safe API — which also makes the Realtek firmware uploader (Q21) pure safe Rust
on both platforms.

Dependencies flow toward `core` as always; `proto-bluetooth-audio` is the only one that
knows what a track is. Codec *decode* stays in `pipeline` with the rest of libav — the proto
crate depacketizes and hands up `EncodedFrame`s, exactly as `proto-cast` does for video.

### 11.3 The platform seam: `HciTransport`

```rust
/// A byte-pipe to a Bluetooth controller, framed as HCI packets. The whole
/// platform-specific surface of the Bluetooth stack is this trait.
#[async_trait::async_trait]
pub trait HciTransport: Send + Sync {
    async fn send(&self, packet: HciPacket) -> Result<(), HciError>;
    async fn recv(&self) -> Result<HciPacket, HciError>;
}
```

| Backend | Mechanism |
|---|---|
| Linux | `AF_BLUETOOTH` raw socket on `HCI_CHANNEL_USER` — exclusive userspace control of a downed adapter. BlueZ is not in the path. |
| Windows | `nusb` (pure-Rust USB, WinUSB backend) against a dedicated dongle. HCI-over-USB is a standard class interface (`0xE0/0x01/0x01`): cmd on control transfers, events on interrupt IN, ACL on bulk. |

Windows needs the dongle bound to WinUSB rather than the Microsoft Bluetooth driver — a
one-time provisioning step on a kiosk we control, and it also means the machine's *internal*
radio stays available to the OS. Both backends see the same controller-side behaviour, so
everything above this line is tested once.

### 11.4 Protocol stack

```
 A2DP sink                     AVRCP CT/TG              cover art
 (AVDTP, PSM 0x19)             (AVCTP, PSM 0x17)        (OBEX/BIP, PSM from SDP)
        └──────────────┬────────────────┴────────────────────┘
                  L2CAP (BR/EDR basic mode)
                       │
                  HCI ACL / events
                       │
              HciTransport (platform)
```

- **SDP** advertises us as A2DP Sink + AVRCP Controller/Target, and is *also* used as a
  client to read the peer's cover-art PSM out of its AVRCP Target record.
- **AVDTP** negotiates one stream endpoint: DISCOVER → GET_CAPABILITIES → SET_CONFIGURATION
  → OPEN → START. Our SEP table offers, in preference order: **LDAC, aptX HD, aptX, AAC, SBC**.
  The sender picks; we decode whatever it picks.
- **AVRCP** gives metadata (`GetElementAttributes`), playback status and position
  (`RegisterNotification`), and takes passthrough commands (play/pause/next/previous) plus
  absolute volume. Both directions matter — see §11.5.
- **Cover art** is AVRCP 1.6: metadata attribute **8** carries a BIP image handle, fetched by
  OBEX `GET` (`GetLinkedThumbnail`/`GetImage`) over a *separate* L2CAP channel to the PSM
  found via SDP. Art arrives asynchronously after the track change, so it is a second event,
  never part of an atomic track update.

### 11.5 What this forces into `core` (the new interface)

Every existing adapter is one-directional: it emits `SessionEvent`s and never hears back.
Bluetooth is the first source where the receiver can *drive the sender* — the panel is a
touch screen, so a user tapping pause on the 65" surface must reach the phone. That is a new
core surface, not a new `SessionEvent`:

```rust
/// A handle back into a live session, so the receiver can drive the *sender*.
/// Adapters that can't do this simply never publish one.
#[async_trait::async_trait]
pub trait RemoteControl: Send + Sync {
    /// Which verbs this peer actually advertised support for.
    fn capabilities(&self) -> ControlCapabilities;
    async fn issue(&self, txn: ControlTxn) -> Result<(), CoreError>;
}
```

`ControlCapabilities` is populated from the peer's AVRCP supported-features bitmask, so the
UI cannot offer a button the phone will reject — the illegal state is unrepresentable at the
point of construction rather than checked at the point of use (ground rule 1).

Alongside it, two additions the existing enums can't express:
- **`SessionEvent::Audio`** — a live audio-only session. `Play{url}` is wrong (there is no
  URL) and `Mirror{video, audio}` is wrong (there is no video).
- **`SessionEvent::NowPlaying`** — track metadata with optional artwork, emitted repeatedly
  over a session's life. Every protocol here has some version of this (Cast `MediaStatus`,
  DLNA `AVTransportURIMetaData`, Spotify), so it belongs in `core` rather than in the
  Bluetooth crate; Bluetooth is just the first adapter rich enough to justify it.

### 11.6 Audio output

**Image decode for cover art:** JPEG is effectively the only format on this path. BIP fixes
the *linked thumbnail* (`x-bt/img-thumb`) at 200×200 JPEG with no descriptor to negotiate,
which is why we fetch that rather than `x-bt/img-img` — the full-image form requires
describing the exact encoding and dimensions wanted, and responders disagree. General decode
still costs nothing, because ffmpeg is already linked for audio and brings `mjpeg`/`png`/
`gif`/`bmp` with it; no `image` crate is needed. `ImageFormat` stays a closed set parsed at
the boundary, and anything outside it is refused rather than handed to a decoder that can't
read it — a text-only card beats a decoder failure three layers down.

The pipeline is video-only today. A2DP needs decoded PCM to reach a speaker, so `pipeline`
grows an audio sink (`cpal`, cross-platform) alongside the compositor, plus libav decoders
for SBC/AAC/aptX/aptX HD. **LDAC is the one codec libav lacks** — AOSP's `libldac` is
encoder-only, so decode uses the reverse-engineered `libldacdec` behind FFI, feature-gated
so a build without it degrades to refusing the LDAC SEP rather than failing to build.

### 11.7 Testing without a radio

Everything above `HciTransport` is pure, so the tier-1 tests drive the whole stack against a
scripted controller: a fake transport replays HCI events, and the L2CAP/AVDTP/AVRCP cores are
asserted on the bytes they emit. Tier-2 puts two of these back-to-back — our sink against a
scripted *source* over an in-memory L2CAP — so a full pair → discover → configure → stream →
metadata → cover-art flow runs in CI with no hardware at all.

For byte-level ground truth, **Fuchsia's Bluetooth profile layer is Rust and BSD-3**
(`src/connectivity/bluetooth/profiles/bt-a2dp`, `bt-avrcp`, `lib/bt-avdtp`) and implements the
sink role. It is not a dependency — it is pinned as a **differential-test oracle** the way
`nix/openscreen-fixtures.nix` pins openscreen's packetizer to settle Cast's IV derivation
(Q13). AVDTP capability records and AVRCP attribute encodings are exactly the bit-packed
surfaces where a golden encoder beats careful reading (ground rule 9: findings land here,
the reference impl never ships).
