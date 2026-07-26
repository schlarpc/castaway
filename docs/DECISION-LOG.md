# Decision Log

Running log of engineering decisions made while building castaway autonomously.
Each entry: what was decided, why, and what it forecloses/enables. Newest at top.
Review at next sync.

## 2026-07-23

### D9 — Cast protobuf messages hand-written, no `protoc` build dependency
`proto-cast` derives `CastMessage`/`DeviceAuthMessage` with `#[derive(prost::Message)]`
and explicit tags rather than compiling `cast_channel.proto` via `prost-build`. The
tags/types match Google's proto exactly (wire-compatible), and the build needs no
`protoc` binary — important for the reproducible Nix cross-build (cross-build.md). If
the proto surface grows a lot we can revisit codegen.

### D10 — Spotify scope: onboarding + pairing crypto only, playback deferred
Spotify Connect's full value (dealer WS + CDN audio) is credential-gated (Premium) and a
large stack. The autonomous slice is the reimplementable, testable part: zeroconf
advertise + `getInfo` + `addUser` DH pairing and blob decryption (librespot algorithm),
matching the shape of the other HTTP protocols (`router()` + `mdns_service()`). Playback
is logged in OPEN-QUESTIONS Q9. This keeps "appears in the Spotify picker and pairs"
working now without sinking the session into one protocol.

### D7 — HTTP-mounted protocols expose `router()` + `ssdp_device()`, not `SourceAdapter`
`SourceAdapter::run()` models an *active* actor that owns a socket (Cast, AirPlay,
Spotify, the Lounge bind channel). DLNA (and later DIAL) are *passive*: their work
happens inside axum handlers on the **shared** HTTP host, driven by the **shared**
SSDP responder. Forcing them through `run()` would mean a handler that just parks.
So `proto-dlna` exposes `router() -> axum::Router` + `ssdp_device()` +
`description_path()`, and the `app` composes: it merges every protocol router onto one
`axum` server and registers every `SsdpDevice` with one `Responder`. This is exactly
the "advertise once, not five racing responders" goal (architecture §1d). Socket
protocols still implement `SourceAdapter`. Revisit if we want a uniform lifecycle.

### D8 — DLNA GENA eventing is stubbed (ack-only); control points poll instead
Full GENA (LastChange NOTIFY callbacks over the subscriber's URL) is conformance-heavy
and not needed for the core "cast a video" flow: control points that don't get events
fall back to polling `GetTransportInfo`/`GetPositionInfo`, which we answer fully. The
event endpoints return `200 OK` with a plausible SID/TIMEOUT so subscription setup
succeeds. Logged for revisit if a specific control point misbehaves.

### D1 — Workspace-wide lints via `[workspace.lints]`, crates opt in
The template had per-package lint config. Moved the full lint table (ground rules
1/7/8) into `[workspace.lints]`; every crate does `[lints] workspace = true`. Keeps
ground rules enforced uniformly without copy-paste drift. `unsafe_code = "forbid"`
is set *per-crate* in the pure crates (rule 8) rather than workspace-wide, because
FFI crates (`pipeline`, `control-display`, `input-touch`) legitimately need `unsafe`.

### D2 — Shared dep versions pinned in `[workspace.dependencies]`
All third-party versions live in the root manifest; crates reference
`foo.workspace = true`. One place to bump, no version skew across the 15 crates.

### D3 — `tokio` with `default-features = false` at the workspace level
Each crate turns on only the tokio features it needs (`net`, `sync`, `rt`, `macros`,
`time`…). Keeps leaf crates (pure protocol cores) from dragging in the full runtime;
honors rule 3 (pure cores shouldn't need a socket to test).

### D4 — Heavy native deps (ffmpeg/wgpu/CEF) are feature-gated, OFF by default
Per cross-build.md's "golden rule": the daily loop must build with no system libav /
Vulkan / CEF present. `pipeline` ships a `null` compositor + `log` sink by default;
`ffmpeg`, `wgpu`, `cef` are additive features. This keeps `cargo build`/`nix flake
check` green on a stock dev box and in CI without vendoring a Windows sysroot.

### D5 — Pure protocol cores are sans-I/O state machines returning output vecs
Every `proto-*` crate exposes `fn step(state, Input) -> (State, Vec<Output>)`-shaped
logic (rule 3). The tokio actor lives in a separate module (`actor.rs`) behind a
`net` feature where a socket is involved, so the core compiles + unit-tests with no
tokio. This is what makes wire-fixture tests (rule 6) possible.

### D6 — No real RE fixtures yet; cores tested against constructed/synthetic vectors
RE happens in `~/re-shell` (rule 9) which this session can't drive. Where a golden
capture is needed (FairPlay-SAP bytes, a real Cast auth challenge, a Lounge bind
transcript), the core is built to the documented wire shape and tested with
hand-constructed fixtures; the need for a *real* capture is logged in OPEN-QUESTIONS.

### D11 — Cast device-auth signer is protobuf-agnostic; proto-cast bridges it
`crypto-cast-auth` returns a `SignedAuth` (raw signature + cert bytes), not a proto
`AuthResponse`, so the dependency flows `proto-cast → crypto-cast-auth` (never a cycle).
`proto-cast::auth::CastAuthResponder` binds the shared signer to one connection's TLS
cert and assembles the proto. `generate_dev()` makes an ephemeral key for local runs;
real gen-1 cert material is provisioned out of band (Q2).

### D12 — Cast mirroring: pure negotiation now, RTP receive loop deferred
Implemented the webrtc-namespace OFFER→ANSWER negotiator (picks one video + one audio
stream, echoes the sender-provided AES key/IV) and the per-frame AES-128-CTR crypt, both
pure + tested. The session surfaces a `MirrorConfig` via `Reaction::start_mirror`; the
actor (future) pre-binds the UDP port, receives Cast RTP, decrypts, and emits
`SessionEvent::Mirror`. The RTP depacketize/reassembly + exact per-frame IV derivation
need a real capture to validate (Q12/Q13).

### D13 — RTP depacketization stays per-protocol; only parse + reorder are shared
`substrate-rtp` hand-rolls the RFC 3550 header parse (no webrtc-rs pull-in — keeps the
dep light) and a bounded reorder buffer that *skips* gaps on overflow rather than
stalling (live latency > freshness, architecture §6). Payload depacketization (MPEG-TS,
ALAC/AAC, AirPlay's bespoke mirror framing) is deliberately NOT here — it differs per
protocol and lives in each `proto-*` crate (architecture §1b).

### D14 — RTSP framing via rtsp-types + a ByteTransform slot; semantics per-protocol
`substrate-rtsp` wraps `rtsp-types` for the message codec + CSeq correlation and exposes
a `ByteTransform` trait (Identity for Miracast; ChaCha20 for AirPlay 2 post-pairing) as
the one crypto concession the shared layer makes. Method dispatch, body parsers, and
state machines stay per-protocol. AirPlay takes method/path/body as plain args so its
core tests without constructing rtsp-types messages.

### D15 — AirPlay: control plane real, media plane gated on FairPlay/pairing
`proto-airplay` ships working mDNS advertisements (`_airplay._tcp` + `_raop._tcp`), the
`/info` capabilities plist, and the full RTSP dispatch state machine. Mirroring media is
gated on the FairPlay-SAP session key (`crypto-fairplay`, stubbed at the captured-tables
boundary) and HomeKit transient pairing (not implemented) — both return `501` at the
gate. This makes castaway appear in the AirPlay picker and models every transaction; the
media plane slots in once Q1's captures land.

### D16 — app composes HTTP protocols live; socket protocols advertise-gated
`app` wires the null pipeline + null display into the session manager and stands up ONE
axum host (DLNA + Spotify + DIAL routers merged), ONE SSDP responder (DLNA + DIAL
devices), and ONE mDNS responder — the "advertise once" goal, now real. DLNA/Spotify/
DIAL are live end-to-end (verified via curl). Cast and AirPlay have complete pure cores
but their TCP actors (CASTv2 TLS server, AirPlay RTSP server) aren't wired, so their mDNS
advertisement is behind config flags defaulting OFF — advertising a service with no
listener only frustrates senders. Config is TOML (`castaway.toml`), interface auto-detected.

### D17 — Crane source filter extended to keep `include_str!` XML assets
proto-dlna `include_str!`s its SCPD/description XML; `craneLib.cleanCargoSource` drops
non-Rust files, breaking the nix build (but not `cargo`). Fixed the flake's `src` filter
to keep `.xml` alongside cargo sources. Same pattern will cover future `.proto`/fixture
assets.

### D18 — Real render/decode backends land behind `render`/`ffmpeg`/`kiosk` features
The render path is now real, not stubbed: `pipeline` gains a wgpu `WgpuCompositor`
(GPU-verified by offscreen readback) and an ffmpeg `decode()` producing RGBA
`DecodedFrame`s (verified against a testsrc clip). Both stay behind default-off features
so the pure-protocol daily loop and the default `nix build` remain native-dep-free (D4).
The devShell adds the native toolchain (pkg-config, `ffmpeg_7` — pinned to match
`ffmpeg-sys-next` 7.1, libclang + `BINDGEN_EXTRA_CLANG_ARGS` for bindgen, and
Vulkan/Wayland/X11 for wgpu+winit at runtime). Feature named `render` not `wgpu` because a
Cargo feature can't share a dependency's name.

### D19 — RenderPipeline + winit kiosk; app render mode restructures threading
`RenderPipeline` implements the core `Pipeline`: `play(url)` spawns a blocking decode
thread that pushes RGBA frames over a bounded sync-channel (drops when full — latency >
freshness); `mirror(Decoded)` forwards frames directly; the `RenderLoop` (render thread)
uploads frames to the wgpu compositor and presents. The winit `kiosk` runs the RenderLoop
against a fullscreen surface and MUST own the main thread, so under the app `render`
feature `main()` is a plain `fn` that builds a tokio runtime, spawns the session manager +
`serve()` on it, and runs the kiosk on main; the default (no render) build keeps a plain
`block_on(serve())`. The whole Play→decode→composite→pixels path is proven by an offscreen
readback test; the on-screen kiosk is compile-verified (not run here — a fullscreen window
would hijack the dev box's live display).

### D20 — Idle/attract scene: CPU-rasterized background layer, DejaVu embedded
The kiosk now shows an idle "lobby" when nothing is casting: the receiver name, a tagline,
per-protocol "how to cast" rows, and a network footer. Text is rasterized on the CPU with
ab_glyph over a gradient (module attract.rs) into an RGBA image the compositor shows as a
background layer at z=-10 — a playing video (z=0) simply covers it, and it reappears on
stop, so no explicit idle/active state machine is needed. DejaVu Sans (regular + bold) is
embedded via include_str-style include_bytes (permissive Bitstream Vera license, asset +
crane .ttf filter). The scene is config-driven: the app builds rows only for enabled
protocols (honest — no "Cast from Chrome" row until the Cast actor lands). attract::to_png
exports a preview (examples/attract_preview.rs). This also lays the groundwork for real OSD
text (same rasterizer), which is still logged rather than drawn.

### D21 — OSD is a core multi-producer channel, not a Pipeline method
Refactored OSD out of the `Pipeline` trait into its own subsystem so *any* source can
inject overlay messages, not just the session manager. `core::osd` defines `OsdMessage`,
`OsdCommand`, a cloneable `OsdSink` (multi-producer, std mpsc), and one `OsdReceiver` —
the same shape as `SessionSink`, and in `core` (not `pipeline`) so protocol crates that
don't depend on the GPU can still post. `Pipeline::osd` is removed; the session manager
now holds an optional `OsdSink` (`with_osd`) and is just one producer. Consumers vary by
mode: the render backend's `OsdController` (pipeline, feature `render`) rasterizes banners
via the shared `text` rasterizer and shows them as the `LayerId::Osd` layer (z=10, above
video) with TTL auto-clear; headless, the app drains the channel to the log. The text
rasterizer was extracted from `attract` into `pipeline::text` (source-over alpha, so the
OSD banner composites as a transparent overlay). The app posts a startup banner as a
worked example of a non-session producer.

### D22 — CEF pulled in, reproducibly, against nixpkgs cef-binary (the boss fight, won)
CEF is real now, behind the `cef` feature. The doc's "boss fight" (cross-build.md/Q6) turned
out tractable: the `cef` crate 147.1.0 exactly matches nixpkgs `cef-binary` 147.0.10, which is
already NixOS-linked. The flake builds a *flattened* `cefDist` (libcef.so + .pak/ICU/locales at
the root, not the Release/Resources split CEF ships) + a crafted `archive.json` to pass the
crate's version check, and sets `CEF_PATH`. Result: no download, no patchelf, no cross-version
API mismatch. `pipeline::cef_browser` wraps libcef (wrap_app!/wrap_client!/wrap_render_handler!/
wrap_request_handler!/wrap_resource_request_handler!): bootstrap (subprocess entry point — MUST
be first in main), initialize (windowless + external pump), create_offscreen (CPU on_paint →
CefFrameSink). Proven: renders real pages offscreen headlessly (SwiftShader) to PNG. FFI unsafe
is allowed in this crate per ground rule 8.

### D23 — Adblock is host-side request blocking, not a Chrome extension
CEF can't host uBlock Origin (its extension APIs don't implement webRequest/declarativeNetRequest,
and the OSR path we need is the Alloy runtime with limited extensions anyway). So blocking lives in
the host: `pipeline::cef_adblock::AdBlocker` (Brave adblock-rust) checks every resource load in the
CEF `ResourceRequestHandler` and returns RV_CANCEL for blocked requests, logging each on the
`castaway::adblock` target. Default is a compact built-in list; a full EasyList loads via
`from_list_text`/`set_adblock`. Dropped adblock's default `single-thread` feature so the Engine is
Arc-based (Send+Sync) — CEF calls the handler across threads. Proven live: blocks
imasdk.googleapis.com (Google video-ad loader) + brightline.tv on cnn.com.

### D24 — Task 16 (app-main CEF merge): DIAL is an event stream; CEF lives in the winit loop
The kiosk merge is done and smoke-verified end-to-end (headless Xvfb + real network: DIAL POST →
YouTube leanback rendered on the compositor, doubleclick ad request blocked, DELETE → attract
scene back, ctrl-c → clean exit). Shape of the merge:
- `proto-dial` now emits `DialEvent::{Launched(LaunchParams), Stopped}` (not just launches) —
  a phone's disconnect must dismiss the surface, and DIAL advertises `allowStop="true"`.
  `LaunchParams::leanback_url()` is the pure YouTube contract: launch body fields pass through
  as `youtube.com/tv?<query>` so the sender's pairingCode binds its Lounge session to us.
- `pipeline::cef_browser::BrowserHost` owns the initialized `Cef` + lazily-created offscreen
  browser on the **main thread**; `kiosk` pumps it each redraw before the render pump (one CEF
  message-loop iteration, then upload the newest paint via `CefFrameSink::take` — upload once
  per paint, not per redraw). Navigation crosses threads as `BrowserCommand::{Navigate, Hide}`
  over std mpsc; `app::serve` hands `DialEvent`s to an injected `on_dial` closure that maps them
  to commands. `kiosk::run_with_browser` shuts CEF down on the main thread after the loop exits.
- `app::main` bootstraps CEF before *everything* (subprocess re-exec), and registers the ctrl-c
  handler **after** `cef_initialize` — Chromium installs its own SIGINT handler during init and
  silently replaces any earlier one (found live: ctrl-c was swallowed). The winit loop polls an
  `AtomicBool` exit flag since a borderless-fullscreen kiosk has no chrome to close.
- Found+fixed on the way: the compositor requested `Limits::downlevel_defaults()` (2048 max
  texture), which can't even configure a 4K surface — the Dell panel is 3840×2160 and would have
  crashed on first boot. Now `.using_resolution(adapter.limits())`.

### D25 — Tier-2 VM harness: two nodes, and the module under test is the deploy module
Ground rule 6's integration tier existed only as a docs promise; the "verified" claims in
STATUS.md were hand-run `curl`s. `nix/vm-test.nix` (`checks.integration-vm`) now boots the
receiver from `nixosModules.castaway` — the same module a deploy uses, so the tested path
and the deploy path can't drift — and drives it from a **second** VM. Two nodes is the
whole point: discovery is what breaks in the field, and loopback hides all of it. Bind to
the wrong interface or advertise `127.0.0.1` and this fails, where a localhost curl passes.

Scripted senders (SSDP control point, DLNA control point, CASTv2 sender, AirPlay sender)
are hand-written Python rather than generated from our own definitions. For Cast that's
deliberate: encoding `CastMessage` by hand is a *second, independent* reading of the wire
format, where generating from the same `.proto` the receiver uses would make the test agree
with the implementation by construction — exactly the agreement not worth assuming.

Every protocol assertion is doubled: the sender's own view, plus a `journalctl` grep on the
receiver proving the event crossed adapter → session manager → pipeline. A SOAP 200 only
says the state machine answered; `null pipeline: PLAY` says the stack carried it.

Two field lessons are baked in. Multicast egress needs pinning (`IP_MULTICAST_IF` + bind):
`239.255.255.250` matches no route, so an unbound socket in a two-NIC VM sends M-SEARCH out
QEMU's NAT and the receiver never sees it. And the sender node disables its firewall —
SSDP replies are unicast from `:1900` to an ephemeral port, which conntrack can't associate
with a datagram sent to the multicast group, so the default firewall drops every reply and
the test fails for entirely the wrong reason.

### D26 — Socket actors: the adapter advertises itself; the app supplies only the hostname
Q15's Cast TLS actor and AirPlay RTSP actor are the first adapters that own real listeners,
which forced the question of who decides what gets advertised. Answer: the adapter.
`Advertisement::MdnsService` gained a required `instance` field, and the actor fills it from
the port it will actually bind — so a TXT record can't advertise a port nothing answers, and
per-protocol naming conventions (RAOP's `<deviceid>@<name>`) live with the protocol that
requires them instead of as a special case in `app`'s wiring. The app contributes exactly one
thing: `MDNS_HOST`, the box's single name.

Both actors are the same shape, per ground rule 3: accept → one pure session per connection →
read, hand bytes to the session, write what it says. `SessionSink::with_instance` retags per
connection so two senders are two sources rather than one interleaved mess. Neither actor
makes a protocol decision.

Where they differ is the crypto seam. Cast keeps its TLS certificate DER because device-auth
signs over it — self-signed is *correct* there, not a shortcut, since CASTv2 authenticates
the device and senders never build a chain. AirPlay carries a `Box<dyn ByteTransform>` that
is `Identity` today; the encrypted control channel only starts after pair-verify, so landing
Q1 is a swap of that transform rather than a rewrite of the loop.

Both stay OFF by default, but D16's reason has narrowed: the listeners answer now. What's
still missing is a real Cast device key (Q2/Q11) and AirPlay pairing (Q1) — so AirPlay
answers `501` at the pairing gate rather than faking a 200 and leaving a sender waiting
forever for a media plane that can't start.

### D27 — DIAL is gated on a launch target, and the YouTube path is tested by a scripted phone
DIAL carries no media. It launches an app and stops it; *everything* else a YouTube sender
does — pairing, the Lounge session, transport commands, the video itself — happens between
the phone, YouTube's servers, and the **page** the receiver was supposed to open. So a
receiver with no browser is not a degraded YouTube target, it is a non-functional one that
looks fine from the outside: `201 Created`, `<state>running</state>`, an OSD banner saying
"Launching YouTube…", and a phone sitting on a connected cast that can never play. That is
worse than not offering it, and it is D16's rule ("advertising a service with no listener
only frustrates senders") with the launch target as the missing listener.

So `serve` takes `on_dial: Option<impl Fn(DialEvent)>` rather than a function that might be
a logger, and a `None` means DIAL is neither mounted nor advertised — the config flag can
ask for it, but a build with nothing to launch it in says so and declines. The type carries
the requirement, so a future non-CEF launch target (the native bind-channel client the
lounge parser is still there for) enables DIAL by existing, not by someone remembering to.

The test story splits three ways, by what each tier can honestly prove:
- **proto-dial's own tests** — launch/stop/state semantics against the router, no sockets.
- **the two-VM test** — that a browser-less build advertises nothing and says why.
- **`nix run .#yt-selfplay`** — the whole path, because neither of the above touches the
  part that actually breaks. It is a scripted phone: invent a pairing code, DIAL-launch it,
  wait for the receiver's page to register that code with YouTube, bind to the Lounge
  session as a remote control, queue videos, and assert the screen plays them. It needs the
  real internet — YouTube's Lounge servers are a third party to the session and there is
  nothing to fake them with — which is why it is a `nix run`, not a `nix flake check`, and
  it sits alongside the hardware-only paths ground rule 6 carves out.

Its oracle is the *clock*, not the state code: `onStateChange` reports PLAYING without
saying which video, so a screen still rolling the previous tap satisfies it — precisely the
"I browsed and it kept playing the first thing" failure. It asks `getNowPlaying` and
requires the position to advance on the video it queued. (The documented state set is
incomplete anyway: `1081` appears with playback plainly running.) And it taps more than
once, because a cast is a session someone browses, not a single launch.
