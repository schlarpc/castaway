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
**SUPERSEDED by D30.** Playback landed; the reasoning below (that the remaining stack was
too large to own) is exactly what D30 resolves by not owning it.

Spotify Connect's full value (dealer WS + CDN audio) is credential-gated (Premium) and a
large stack. The autonomous slice is the reimplementable, testable part: zeroconf
advertise + `getInfo` + `addUser` DH pairing and blob decryption (librespot algorithm),
matching the shape of the other HTTP protocols (`router()` + `mdns_service()`). Playback
is logged in #47. This keeps "appears in the Spotify picker and pairs"
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
The captures rule 9 asks for need hardware and senders this session can't drive. Where a golden
capture is needed (FairPlay-SAP bytes, a real Cast auth challenge, a Lounge bind
transcript), the core is built to the documented wire shape and tested with
hand-constructed fixtures; the need for a *real* capture is logged in OPEN-QUESTIONS.

### D11 — Cast device-auth signer is protobuf-agnostic; proto-cast bridges it
`crypto-cast-auth` returns a `SignedAuth` (raw signature + cert bytes), not a proto
`AuthResponse`, so the dependency flows `proto-cast → crypto-cast-auth` (never a cycle).
`proto-cast::auth::CastAuthResponder` binds the shared signer to one connection's TLS
cert and assembles the proto. `generate_dev()` makes an ephemeral key for local runs;
real gen-1 cert material is provisioned out of band (#40).

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
media plane slots in once #39's captures land.

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
CEF is real now, behind the `cef` feature. The doc's "boss fight" (cross-build.md/#44) turned
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
#39 is a swap of that transform rather than a rewrite of the loop.

Both stay OFF by default, but D16's reason has narrowed: the listeners answer now. What's
still missing is a real Cast device key (#40/#51) and AirPlay pairing (#39) — so AirPlay
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

### D28 — the receiver publishes its screen id, because the second cast has no other way in
Reported symptom: casting from a phone connected, but browsing and tapping videos played
nothing. It reproduced against a `cef` build, so it was not the launch path — that one
works, and `yt-selfplay` proves it end to end.

What it was: a sender can reach a Lounge screen two ways. It can supply a pairing code —
but supplying one means making a DIAL launch, which only the *first* sender does. Or it
can use the screen id, which is enough on its own: `get_lounge_token_batch` mints a token
from it and the sender drives the screen directly. Verified against the live service
before any code was written — a screen id alone attaches and plays, no launch, no pairing
code. A real TV publishes that id under `<additionalData>` in its DIAL app-info XML.

We published nothing. So the first cast worked (it launched, so it held a pairing code),
the app stayed `running` forever after — nothing sends `DELETE` in practice — and every
sender arriving afterwards found a running app it could not attach to. Connected, and
unable to queue anything. The failure is invisible from the DIAL surface alone, which is
why every test we had passed while a phone in the room did not work.

Resolving the id is I/O, so it splits along ground rule 3: `proto-dial` owns `ScreenId`
(parsed, not trusted — it lands in XML the whole LAN reads) and a `ScreenSlot` the routes
clear on launch and stop, and `app` does the `get_screen` lookup off-runtime and fills it.
A slot rather than a constructor argument because the id genuinely does not exist yet when
a launch is answered — the page has to load and register first — and empty is the honest
state until then. Stale is worse than empty: it points senders at a screen that is gone.

`yt-selfplay --reconnect` is the regression test, and it is the shape the original tests
were missing: they all cast onto a *fresh* receiver. The bug lived one cast later.

### D29 — SponsorBlock is a second remote control, not a hook into the player
The receiver skips sponsors the same way a phone would change the video: it attaches to
its *own* screen as a `REMOTE_CONTROL` Lounge session, watches `nowPlaying`, and sends
`seekTo`. This is what iSponsorBlockTV does (read as wire behaviour, not as code — it is
GPLv3 and we reimplement, ground rule 9), and it turned out to be nearly free here,
because the screen id was already resolved for DIAL (D28) and the bind-channel framing
already existed in `proto-dial`.

The alternative was injecting JavaScript into the leanback page through CEF. Rejected:
reading player state back needs a render-process handler and process messages — more FFI,
more `unsafe` — and it breaks whenever YouTube reshapes their minified player, which is
their schedule rather than ours. The Lounge protocol is one we already depend on, so
riding it adds no new way to break.

The split follows ground rule 3 exactly. `sponsorblock` is pure: hashing, parsing,
filtering, and *when* to skip. `proto_dial::lounge::sender` is pure: what a controller
puts on the wire, as a typestate so a command before the handshake does not compile.
`app` holds what is genuinely I/O — plus dead reckoning, which cannot live anywhere else:
the screen pushes `nowPlaying` on change rather than on a tick, so between reports the
position is extrapolated from the wall clock. Without that a skip fires whenever the next
event happens to arrive instead of at the boundary.

Two decisions that are behavioural commitments rather than structure:
- **Skip only, never mute.** SponsorBlock's `mute` action would mean driving volume over
  the Lounge and restoring it exactly; a stuck mute on a wall display is worse than the
  sponsor. Same reasoning for YouTube's own unskippable ads: they play.
- **The toast is load-bearing.** A screen that silently jumps looks broken to a room that
  did not ask for the skip, and the overlay is also where CC BY-NC-SA attribution is
  actually shown rather than buried in a README.

Privacy and licence are not incidental. Lookups go to the hash-prefix endpoint, so the
server learns four hex characters of a hash and never the video; the filtering that makes
that work happens on our side. The database is CC BY-NC-SA: non-commercial use fits a
hackerspace display, attribution is required, and there is deliberately no way to persist
segments — writing them to disk is redistribution, which pulls ShareAlike in.

Tested at both tiers: the rules are unit tests against fixtures (a skip fires once; a
rewind re-arms but a stale report does not; overlapping submissions merge), and
`yt-selfplay --expect-skip` proves it end to end from the sender's seat, asserting a
*discontinuity* — playback advancing further than wall time did — rather than asking
SponsorBlock the same question the implementation asked.

### D30 — Spotify is the one protocol we do not reimplement
Ground rule 9 names librespot as an RE source, not a runtime dependency. That rule is
right for every other protocol here and wrong for this one, so it now carries an
explicit carve-out (CLAUDE.md rule 9).

The distinction is what the peer *is*. AirPlay, Cast, DLNA and Bluetooth are stable,
device-side, published-or-well-reversed specs: reimplementing is a one-time cost that
buys a durable asset we own, can test against fixtures, and can fix on our own schedule.
Spotify Connect is a cloud service that changes unilaterally and has repeatedly done so —
login5 replaced password auth, the dealer replaced Mercury, keymaster started returning
403s, audio key provisioning moved to PlayPlay. Reimplementing that is not a one-time
cost; it is a subscription to someone else's churn, and every break lands as silence on
a wall-mounted panel that nobody is watching a log for.

librespot is also not the kind of dependency the rule was written against. It is not a C
reference binary to shell out to — it is an idiomatic, tokio-based Rust crate family on
crates.io (`librespot-core`, `-connect`, `-playback`, `-metadata`) with a community that
absorbs exactly the churn above.

The split is not "use librespot": it is **ours from the LAN, theirs from the cloud**.
- Ours: `_spotify-connect._tcp`, `getInfo`/`addUser`, the DH + blob decryption. This has
  to stay ours because it shares the receiver's single HTTP host and single mDNS
  responder. `librespot-discovery` stands up its own server and its own mDNS responder,
  which is precisely the "five racing responders" that D7 and architecture §1d exist to
  prevent.
- Theirs: access point, login5, dealer WebSocket, connect-state, audio keys, CDN fetch,
  Vorbis decode.
- The seam: `proto-spotify::session` — credentials in, `SessionEvent`s out.

What this buys beyond time: the credentials-free onboarding survives. Whoever walks up
picks castaway in their own Spotify app and the receiver logs in as them; nothing is
stored, and the next person to pair replaces them. The alternatives all end in an account
on disk or a QR code to scan.

What it forecloses: we do not own this state machine, so a Spotify change we would want
to handle differently is an upstream conversation rather than a local patch, and
`librespot-playback`'s decode/normalisation sits inside our audio path rather than
alongside it. Both are accepted. The version is pinned in the workspace and moves
deliberately.

Feature flags matter here. `default-features = false` keeps librespot's own audio
backends (rodio/cpal/alsa) out — the sink is ours — and `rustls-tls-webpki-roots` keeps
the tree on `ring` rather than dragging OpenSSL or a second crypto backend into the
Windows cross-build. Verified: no `aws-lc-rs` in the graph.

### D31 — Compile the sender rather than reason about it
The question "will Chrome accept this receiver's device auth?" had been answered by
reading `cast_auth_util.cc`. That answer was right, and it was the wrong shape. Source
reading gives one bit — "no" — when the useful thing is the vector: which of the
sender's many checks already pass, which one fails, and whether the failure is the
credential we cannot get or something we could have fixed this afternoon.

So `checks.openscreen-device-auth` compiles openscreen's sender-side verifier —
`cast/sender/channel/cast_auth_util.cc` plus the path builder under
`cast/common/certificate/`, the code Chrome runs — and has it judge auth responses this
receiver really produced. It is Q13's pattern with the arrow reversed: there openscreen
generates bytes and our receiver consumes them; here we generate and openscreen rules.
Ground rule 9 forbids reference implementations in the shipping binary and says nothing
about using them as oracles, which is the whole of why this is allowed.

The immediate return paid for it twice over. Two failures were found that had nothing to
do with the missing credential, and that no amount of further source reading would have
surfaced in the right order:

- The receiver's TLS certificate carried rcgen's default 1975→4096 validity window, and
  a sender rejects any peer certificate whose `notAfter` is more than four days out.
  Every official sender was walking away *before* device auth was considered.
- The `_googlecast._tcp` TXT record had no `st` key, which openscreen's record parser
  treats as mandatory: the whole advertisement is discarded. A discovery failure and a
  protocol failure are indistinguishable from the room — the panel is simply not listed.

Both are the project's signature failure mode: a receiver that looks completely healthy
and is refused by everything, with nothing on either side saying why. Both are now
regression-locked, the first as a vector, the second as an assertion on the advertisement.

What it does not do is prove Chrome specifically. openscreen is the reference
implementation of the same logic, not the binary on anybody's laptop, and Chrome's
discovery stack is its own code. The honest claim is: our device-auth response is correct
in every respect the reference sender checks except the trust root, and that is now a
result rather than a belief.

### D32 — Decline app launches we cannot host, in the sender's own vocabulary
`launch()` accepted any `appId` and answered `RECEIVER_STATUS` with a fresh session and
transport id. For `CC1AD845` that is true. For Netflix, Spotify or YouTube's own receiver
it is a lie with consequences: the sender opens a virtual connection to a transport id
nothing is listening on, starts talking on a custom namespace, and gets silence. A
connected phone and a black panel (issue #16).

`App::classify` now sorts an `appId` into what we can actually do with it — a media URL
we play ourselves, an RTP stream we terminate, or somebody else's web receiver — and an
enum rather than a boolean because those first two are genuinely different roles. What we
cannot host gets `LAUNCH_ERROR` / `NOT_FOUND`; what we could host but cannot right now —
mirroring with no RTP socket bound — gets `SYSTEM_ERROR`. Both strings are openscreen's,
because a sender's error handling is written against its own vocabulary and an invented
one degrades to "unknown failure".

The same classification answers `GET_APP_AVAILABILITY`, which had been ignored entirely.
That was the more expensive omission: a sender asks what a device can run *before*
offering it, so an unanswered query means the receiver never appears as somewhere to cast
to at all. Advertised, discoverable, authenticating — and absent from the picker.

Hosting the vendor receivers stays G56 and stays unbuilt. This decision is only that
declining out loud beats claiming success, which is the same call D16 made for
advertise-gating and D27 made for DIAL without a browser.

### D33 — The panel's transport is drawn from capabilities, not from protocols
The C6522QT is a touch screen that, until now, could only be touched to drive the CEF
browser. An audio session — Spotify, or a phone over Bluetooth — put a card on the wall
you could look at and not touch, while the controls stayed on whichever device had
started it. `RemoteControl` had existed since Bluetooth landed; nothing on the panel was
wired to it.

The design decision worth recording is what decides *which* buttons exist. Not the
protocol: the session's `ControlCapabilities`. A Bluetooth phone advertises what AVRCP
passthrough can express and gets previous / play-pause / next; a Spotify session
advertises shuffle and repeat as well and gets six buttons and a draggable scrubber.
There is no per-protocol branch in the renderer, and there is no way to add one without
noticing — the strip is built by asking the capability set, and `RemoteControl::issue`
refuses anything outside it independently. So the panel cannot offer a control the sender
will refuse, which is the same rule D16 applied to advertisement and D32 to app launches.

Three smaller calls inside it:

- **One layout, two consumers.** `transport::layout` produces the rectangles; the
  renderer draws into them and `Layout::hit` tests against them. A button cannot be drawn
  where it cannot be pressed, or pressed where nothing is drawn. `to_strip_local` is a
  free function for the same reason — the panel-normalized → strip-local mapping is
  testable without a GPU, and its failure mode is the quiet one where the buttons still
  look right and answer to a different part of the glass.

- **Absolute intent, not toggles.** A press becomes a transaction against the state
  *currently on screen*: the play button sends `Pause` because the panel is showing
  "playing". Shuffle sends the wanted state rather than a flip. A stale view then costs
  one wrong-looking press instead of a control that means the opposite of its glyph.

- **Its own layer, its own cadence.** The card repaints per track and the scrubber per
  second; at 4K those are a 33 MB upload and a 4 MB one. `proto-spotify` had already
  refused to republish the position tick for exactly this reason and was right to. The
  strip advances from the local clock between snapshots, keeps the source's reading as the
  base rather than restamping, and only moves while playback is active — a paused track
  whose position crept forward would send a seek nobody asked for.

What it does not do: draw a live preview while a scrub drag is in progress (G60), or
offer shuffle and repeat over Bluetooth, which passthrough cannot express (G59).

### D34 — Audio is the master clock on the media-URL path
Adding sound to `Play(url)` (G61) turned out to require a clock, because that path never
had one. Nothing in it was real-time: the decoder never slept, the compositor presented
whatever it was last handed, and frames landed in a three-deep channel that dropped the
rest. Playback speed was decode speed. Whatever else was decided, *something* had to
become the clock.

Audio, and not as a coin toss. A video frame arriving late can be dropped and one
arriving early can be held, and at 24–60 Hz nobody sees either. Audio has no equivalent
slack — stretching it changes the pitch and gapping it is a click — and both are obvious
across a room. So the audio thread paces itself to real time and publishes where it has
got to; video waits for it.

It earns the name at two scales, which is what makes it more than a policy label:

- **Coarse.** Decoded audio goes into a *bounded* queue drained by a real-time consumer.
  A full queue blocks the demuxer, and with it the video decoder, so audio throttles the
  entire session before any per-frame reasoning happens. The queue bound is load-bearing,
  not a tuning knob.
- **Fine.** `MediaClock` publishes the audio's queue position minus the output lead not
  yet heard, and each frame is held until due. A frame slightly late is still shown — it
  is the best picture available; one hopelessly late is dropped, because catching up frame
  by frame never finishes.

Three consequences worth stating so they are not rediscovered:

- The output lead lives in `clock.rs` and `audio_session` uses it, rather than each
  keeping a copy. Two copies drift, and the symptom is lip sync quietly wrong by exactly
  the difference — the kind of wrong people notice and cannot describe.
- With no audio stream there is nothing to follow, so the clock seeds off the first frame
  and runs on the wall. Same policy, stated twice, rather than a special case.
- Pause freezes the clock rather than signalling three threads (G63). Everything is
  already waiting on it, so one flag stops the chain in step and resuming lands where it
  left off. A clock that kept running while the room was silent would resume by dumping
  every frame it had "missed".

Explicitly *not* used for live mirroring. A Cast or AirPlay pixel stream is paced by its
sender, and ground rule 4 says to drop late frames there rather than wait: holding a
mirrored frame to match a clock adds latency to the one path where latency is the
complaint.

### D35 — Miracast: reimplement the protocol, defer the radio, and say which is which
`proto-miracast` lands as the complete wire protocol with no working link under it, which
looks like the wrong half to build first. It is the right one, and the reasoning is worth
keeping because it will come up again for the Windows backend.

The protocol and the link fail for unrelated reasons and are testable by unrelated means.
Everything above the link — the information element, the `wfd-kv` grammar, M1–M16, the
transport stream, UIBC — is deterministic, fixture-testable, and *the same on every
platform*. The link is driver-dependent, needs root and a radio, and is the one part that
differs between Linux and Windows. Building them together would have produced a system
where a failure is ambiguous: no picture could mean a driver, a negotiation, a demuxer, or
a bug in any of them. Built apart, the protocol is provably correct before a radio is ever
involved, and the remaining unknown is exactly one layer thick.

Four decisions inside it, each of which had a plausible alternative:

- **Our own TS demuxer rather than ffmpeg's `rtp_mpegts`.** architecture-substrate.md §1a
  correctly notes libav will eat the stream whole, and that would have been less code. It
  also hands libav the socket, which ground rule 3 forbids, and it hides the elementary
  stream — but an IDR arriving is what answers a source's `wfd_idr_request`, and that
  request is the *only* loss-recovery primitive WFD has. With AOSP's encoder putting IDRs
  fifteen seconds apart, giving that up to save 400 lines is a bad trade.

- **Autonomous group owner, no GO negotiation.** Windows wants intent 14, Android insists
  on 0, and two peers at 15 is a defined hard failure — every negotiating strategy loses
  against one of the two senders we actually have. Bringing the group up unilaterally
  deletes the negotiation state machine rather than tuning it.

- **`wfd_content_protection: none`.** HDCP 2.x is the one crypto module in this project we
  are choosing not to build. Both open-source sinks answer `none`, Windows' *own* sink
  answers `none`, and both platforms proceed with an unencrypted stream. What it costs is
  DRM-locked video, which a hackerspace panel was never going to play anyway.

- **The negotiation oracle.** AOSP's format chooser is reimplemented in `video.rs` so tests
  can assert what an Android source *would* pick from a given advertisement. That was not
  planned; it fell out of needing to test the negotiation without a phone, and it
  immediately earned its place by surfacing something no amount of reading had: the
  resolution is an intersection but the profile and level are **not** — a source reads the
  lowest bit of each side and takes the minimum, so it can settle on a profile the sink
  never claimed. That is why the sink advertises both H.264 profiles and why
  `NegotiatedVideo::sink_can_decode` is a question the M4 handler asks rather than an
  assumption it makes.

What is deferred with eyes open: Miracast-over-Infrastructure (MS-MICE) is documented and
unbuilt — it removes the P2P *data* path but not the beacon, so it does not rescue us from
the driver question, and it is only worth building once a group forms. See #45 and
#17 for what has to be true before either can be promised on the deploy target.

### D36 — The browser runtime: Electron over CEF, gated on one spike
The browser stopped being an implementation detail the moment G56 became intent. Hosting
other vendors' CAF receiver pages makes it a general commercial web-media runtime, and that
runtime needs three things at once: H.264+AAC (G55 — measured absent, compiled out of every
prebuilt CEF that exists), a VMP-verified Widevine host on Windows (G46/G56 — the Windows
CDM is compiled with host verification; an unsigned host is refused licences by exactly the
services G56 exists to host), and a renderer sandbox we can defend while executing other
people's pages and their ad networks. Precompiled CEF can never have the first, castLabs
will not sign it for the second (their free EVS is scoped to their own Electron fork), and
on Windows the third requires inverting castaway into a DLL loaded by CEF's
`bootstrap.exe` (cross-build.md). Self-built CEF buys only the codecs, at the price of a
Chromium build per security bump and a Linux→MSVC Chromium cross under Nix, forever.

So: the browser layer moves to an **Electron subprocess** — castLabs ECS
(`castlabs/electron-releases`, MIT) on *both* platforms, pinned by us as a flake input
beside `cef-windows-src`, and patchelf'd on Linux the way nixpkgs' own `electron-bin`
patchelfs the upstream prebuilt.

**One runtime everywhere, and it is not a detail.** The first draft of this decision had
stock nixpkgs `electron` for Linux dev/CI and ECS only for the Windows artifact. That
would have put Chrome 146 under development and Chrome 150 on the panel — different
Chromium majors, so every codec, DRM and OSR behaviour verified in CI would have been
verified against a browser we do not ship. Pinning ECS on both deletes that class of bug,
and takes the "nixpkgs bumps Electron under us" drift with it.

It also collapses Widevine to a single mechanism, which is the larger win. Measured
2026-07-28: ECS's `components` API fetches the CDM to
`<userDataDir>/WidevineCdm/<version>/_platform_specific/<linux_x64|win_x64>/`, and the
file it fetched is **byte-identical** (same md5) to `pkgs.widevine-cdm` 4.10.3050.0,
which this flake already pins for CEF and already pins a Windows counterpart of. So the
CDM is *pre-stageable from a derivation we already have*, into a documented path, the
same way on both platforms — no network at first launch, which is G46's property. That
replaces the reverse-engineered CEF arrangement wholesale: the
`DIR_COMPONENT_PREINSTALLED` → `DIR_ASSETS` → `DIR_MODULE` scan, the Linux-only hint
file, and the platform split in `crates/pipeline/src/widevine.rs` all go away.

What each recorded problem gets:

- **G55**: official Electron builds set `proprietary_codecs=true ffmpeg_branding="Chrome"`
  (`build/args/all.gn`). Someone else maintains the codec-enabled Chromium, permanently.
- **VMP**: ECS ships Widevine with VMP-signed development builds; production signing is EVS
  — verified 2026-07-27: *"It is a free service, but requires signup to use"*, a PyPI CLI
  (`castlabs-evs`), signing Windows/macOS packages built from ECS releases. Linux needs no
  signing at all — the Linux CDM has no VMP, which G46 already established. EVS is a
  network step over exact bytes, a hole in ground rule 6 the same shape as the unfree CDM,
  handled the same way: a deploy-time step outside `nix build`, never inside it.
- **The sandbox**: Electron owns its own process bootstrap and ships with the renderer
  sandbox on by default on both platforms. castaway never links libcef, stays a real
  `.exe`, and the Windows inversion dissolves rather than being solved.
- **The process model**: castaway stops being re-exec'd as Chromium's subprocess binary;
  the bootstrap-must-be-first `main()` ordering and the Nix wrapper-identity constraint go
  with it, and the CEF pump thread domain — one of architecture-substrate §6's three —
  is deleted. The browser becomes an out-of-process actor castaway supervises: a wedged
  browser is killed and relaunched without dropping live AirPlay/DLNA sessions, which
  in-process CEF could never offer an unattended panel.
- **The frame path**: Electron's shared-texture OSR (`useSharedTexture`) delivers GPU
  handles on all three platforms — NT HANDLE, IOSurface, `NativePixmapHandle` plane fds;
  the last is the dmabuf shape the VA-API import already consumes — replacing the
  33 MB/frame CPU `on_paint` copy that CEF's buggy accelerated OSR (#44) forced us onto,
  and letting the browser keep GPU compositing and decode.
- **Q19's triple pin**: the version-locked FFI ABI (cef crate ↔ cef-binary ↔ forged
  `archive.json`) is replaced by an IPC protocol we define — sans-I/O parser, golden
  transcripts, a fake browser in CI. The browser boundary becomes testable per rules 3/6,
  where today the whole path is `doCheck = false`.

Priced rather than waved at: a Node runtime and a JS host app join a Rust appliance. The
conditions that keep that honest are this decision's own (D30's carve-out logic fits — the
peer is cloud infrastructure that changes unilaterally — but its "idiomatic Rust crate"
condition does not transfer): the host app is **ours**, small, and dependency-free
(Electron APIs only, no npm tree); the adblock engine stays in castaway and answers over
IPC rather than being re-adopted as `adblock-rs` npm; the IPC protocol is fixture-tested
like any wire protocol here. Remaining costs: castLabs is a third party under the Windows
DRM artifact; ECS is not in nixpkgs (packaging + unfree handling); `cef_browser.rs` and
`cef_adblock.rs` are rewritten against different mechanisms (`session.webRequest` for the
request veto — async, so the engine round-trip fits; main-world injection for uBO
scriptlets, which preloads alone do not give); touch goes through CDP
`Input.dispatchTouchEvent` (`sendInputEvent` has no touch type); and G46's offline CDM
staging is re-proven under ECS's component handling.

Considered and rejected: **self-built CEF** (above — the smallest slice of the problem at
the largest recurring cost); **WebView2/wry** (no offscreen texture access — composition
hosting yields DComp visuals wgpu cannot sample — and no Linux story); **a full Chromium
under CDP** (screencast is JPEG-grade, useless as a 4K playback surface); **Servo** (not a
YouTube/EME runtime).

**The gate.** CEF stays behind its feature flag until the spike (Q40) proves
shared-texture handles importing into the wgpu compositor at 4K with sane pacing on
Linux; the Windows import (NT handle → D3D12 `OpenSharedHandle` + keyed mutex) is
deploy-critical and is proven separately on the box. The honest worst case, recorded so
it is recognized if met: shared-texture import turns out flaky on a platform we ship, the
fallback is software OSR frames crossing a process boundary — worse than today's
in-process copy — and this decision reopens.

### D37 — GameStream: link the Moonlight core, own everything on the LAN
The GameStream/Sunshine receiver (#33) is split down the middle. `proto-gamestream`
implements the LAN-facing half — mDNS host discovery, the NVHTTP API, the gen-7 pairing
crypto, the client identity, the adapter — and **links** moonlight-common-c for
everything past `/launch`: the RTSP handshake, the ENet control stream, FEC'd RTP video,
encrypted Opus audio, and input encoding.

That is a second carve-out from ground rule 9, alongside D30, and it was a direct
instruction rather than a derivation — worth recording as such, because the reasoning
that justifies D30 does *not* transfer cleanly. Sunshine is not a cloud service that
changes unilaterally; it is a device-ish peer speaking a spec that mostly holds still,
which is exactly the case rule 9 says to reimplement. What argues for linking anyway is
volume and shape: the streaming half is ~15k lines of C whose correctness is measured in
FEC recovery under real loss and A/V pacing under real jitter — properties a fixture
suite models badly and a hackerspace LAN exercises immediately. The pairing half, by
contrast, is a few hundred lines with published golden vectors, and is where the
protocol's interesting decisions live.

**What the split buys, concretely.** The half we own is the half that is testable without
hardware, and it is tested against Sunshine's own checked-in vectors rather than against
itself: the AES key derived from salt+PIN reproduces its `clientchallenge` ciphertext, the
X.509 `signatureValue` extraction reproduces its phase-4 hash, and its golden
`clientpairingsecret` signature verifies. Two upstream notes found doing that, recorded so
nobody re-derives them: the comment beside that vector claims the third hashed field is the
ASCII `"SECRET  "` — it is not, it is the 16-byte client secret — and Sunshine's own test
overrides the server challenge to *eight* bytes where a real one is sixteen, which works
only because nothing in the hash is length-prefixed.

**What it costs, priced rather than waved at.**
- **Licence.** moonlight-common-c is GPL-3.0; this workspace is MIT. Linking it makes the
  combined binary GPL-3.0 if distributed. Per the build notes' scope (n=1, private, no
  redistribution) that is moot in practice, but it is not nothing, so the `stream` feature
  is **opt-in and off in `castaway-portable`** — the build CI produces stays MIT-clean, and
  turning it on is a deliberate act. Same quarantine logic as `crypto-playfair`, one level
  up.
- **A C library in the closure.** Built static from a pinned revision with its two
  submodules grafted in (the upstream CMake fetches them from the network, which a Nix
  build cannot). Bindings are pregenerated and checked in against that same revision.
- **A process singleton.** `LiStartConnection` is documented not thread-safe, keeps global
  state, and several of its callbacks take no context pointer — so the safe wrapper owns a
  global and refuses a second concurrent session rather than letting two sets of callbacks
  race one static. This is a real constraint on the design, not an implementation detail:
  one GameStream session per receiver, by construction.

**The inversion, which is the part with no precedent here.** Every other adapter is a
receiver: it advertises, waits, and learns what to do from whoever connected. This one
browses and dials, which forced two new things. `substrate-mdns` grew a browse API (it was
advertise-only). And `SourceAdapter::run(sink)` is push-only with no reverse channel, so
the adapter is constructed with a command channel instead. Config is its only sender today;
the channel exists so a panel-side chooser becomes the second one without touching the
adapter.

**What is deferred with eyes open.** There is no chooser. A person cannot walk up to the
panel and pick a host, because nothing in this codebase can yet put a list on screen and
take a touch on it (the idle scene is a bitmap rendered once at startup, and the transport
strip is scoped to the active session). Until that exists, the only way to start a session
is config, and the honest consequence is that GameStream is operator-configured rather than
walk-up — the opposite of this project's whole premise. That gap is the next piece of work,
not a property of this design.

**The gate.** No session has been run against a real Sunshine host. The pairing half is
proven against Sunshine's vectors and against a scripted host over real sockets; the
streaming half is proven to link and answer its own queries, and nothing more. The honest
worst case, recorded so it is recognized if met: the linked core turns out to need
per-platform threading or timing care we have not done, the panel shows a black screen
with a healthy control stream, and the debugging happens inside a C library we chose not
to learn.

### D38 — The app shell is native, and the panel gets a home to return to
`docs/app-shell.md` is the design; this is what was decided and what it cost.

The panel had no way to *offer* anything. Every protocol until GameStream was
sender-initiated — someone picks the screen on their phone and the receiver accepts what
arrives — so the panel never had to ask a question and never grew a way to ask one. D37
broke that: as a Moonlight client the panel chooses the host and the app, and with nowhere
to choose from, the choice went into `castaway.toml`. The one protocol that could be
walk-up became the only one that is not.

Eight issues turn out to be waiting on the same missing thing, which is why #23 has no
body — it could not be written until something needed it. #33's picker, #28's PiP, #27's
"kick back to home screen", #29's intercom-over-PiP, #11's file picker, #12's device
selection, #15's visualizer, and #24's theming all need a screen model, and #16 will need
the same escape hatch from a fullscreen browser that YouTube does.

**Native, not a web page.** Three facts decided it. There is exactly *one* browser
instance and it is spoken for — YouTube leanback today, Cast app surfaces tomorrow — so a
web shell would contend with cast content for the same window and put navigation inside a
process whose crash takes it away. `castaway-portable` has no browser at all and must
still have a UI. And the argument *against* native turned out to be wrong: an antialiased
SDF rasteriser (circle, rounded box, segment, triangle) already exists in `transport.rs`,
and `nowplaying_card.rs` can already draw an arbitrary decoded image centre-cropped into a
square. Both are private, so the work is promoting them, not writing them. The cost, stated
plainly: no CSS, no layout engine, no free animation curves, and #24's mascot-and-flourish
ambitions are more work than they would be in a page.

**A swipe and a pill, from the left edge.** Left rather than bottom because the transport
strip already claims the bottom-centre 62%×20% of the glass and takes touches first. Both
affordances rather than one: the pill is discoverable for a guest who has never used the
panel, the gesture is fast for someone who has, and they cost the same hit-test.

**The landmine, recorded because it is invisible until it isn't.** Stealing a contact
mid-drag leaves Electron holding a `touchStart` that never ends, and the browser host
keeps that contact in its map for the life of the session — so the page believes a finger
is down forever. Every stolen id needs a synthesised touch-cancel. The plumbing exists end
to end and *nothing has ever sent it*, so it is untested; it is the first test to write,
not the last.

**What lands first is invisible.** `LayerId` is a closed six-variant enum drawn by sorting
on `z`, and `NowPlaying` already collides with the browser's attract-widget role at
`z = -5`, resolved by `HashMap` iteration order. That is nondeterministic today and
harmless only because the two never coexist. A shell adds surfaces, so layer identity gets
reworked *before* any screen is built on it — no visible change, and the riskiest commit in
the sequence.

**Two pre-existing bugs surfaced by the mapping**, neither caused by this work and both
recorded in the design doc: the browser's coordinate mapper clamps out-of-rect touches
instead of rejecting them, so on the idle screen a touch anywhere on the 65-inch panel is
squashed into the small clock card and delivered to that page; and `transport_owns`
answers from its layout rect without knowing whether the strip is visible, so a video
session that also publishes metadata could leave an invisible strip swallowing part of the
glass. The second is not proven reachable and wants a test before a fix.

**Deferred with eyes open.** The font stays DejaVu. dma.space uses Inter, and switching
moves every glyph on every existing surface and invalidates the golden-image tests — that
belongs to the theming work, not smuggled in with the shell. Brand assets are vendored at
`crates/pipeline/assets/brand/` with provenance recorded; they were ripped from the live
site rather than handed over as a kit, and the gold in the palette is authored in oklch
outside sRGB, so the hex we render is duller than intended.

**The gate.** The honest worst case, recorded so it is recognised if met: the native shell
turns into a widget toolkit. Screens are hand-composed the way the now-playing card is, and
if that becomes painful the answer is more primitives, not a framework — if we find
ourselves writing a layout engine, this decision reopens and the web shell deserves
another look with the one-browser problem solved some other way.

### D39 — One crate owns "which directory?", and the panel keeps its own logs
Two small things that turned out to be the same thing.

**The directory question was being answered three times, differently, and only for
Linux.** `filterlists.rs` resolved `XDG_CACHE_HOME` itself and fell back to the temp
directory; `Config::state_dir()` resolved `XDG_STATE_HOME` itself and fell back to the
working directory; `[gamestream] state_dir` was the literal `/var/lib/castaway/gamestream`.
On the Windows deploy target all three are wrong, and wrong in the way this project keeps
finding: silently. G31 is exactly that failure already collected once — an unwritable
cache directory under `DynamicUser`, every write swallowed by design, the receiver looking
healthy while injecting no scriptlets at all. The same trap was sitting on the state side
untriggered, where the cost is link keys that never persist and every phone re-pairing
after a restart.

So `castaway-paths`, and two properties it is built for. **The platform seam is a value,
not a `cfg`.** `Layout::{Xdg, LocalAppData}` is a parameter of the resolution function and
`Layout::HOST` is the crate's only `cfg`, so Linux CI runs the branch that will run on the
deploy box — including its absoluteness rule, because `Path::is_absolute` answers for the
*host* and calls `C:\Users\kiosk` relative on Linux. Borrowing the build host's answer for
a deploy-target question is how the Windows branch becomes untestable from the machine it
is written on. **Resolution is pure**: it reads an `Environment` trait rather than the
process environment, so tests hand it a map instead of mutating global state — which under
a parallel runner is racy and, since Rust 2024, `unsafe`.

And it does not quietly pick somewhere when it finds nothing. `Origin::Fallback` is
returned rather than concealed, and the app says so at startup. That log line is the whole
point: the failure mode being designed against is not "the path was wrong", it is "nobody
found out".

**Logs on disk, because the deploy target has no journald.** A panel on a wall with nobody
attached to a terminal has no answer to "what happened last Tuesday" — on Linux journald
provides one, on Windows nothing does. `tracing-appender`, daily rotation, fourteen files
kept.

Two choices inside that. **The file sink has its own filter, and it is not inherited from
the console's.** `RUST_LOG=debug` is a debugging act with an audience; the file belongs to
a machine running unattended for a month, and the mirroring paths log per frame. Wiring
them together would mean every diagnostic session silently commits the panel to filling its
own disk. So `[log] file_level` defaults to `info` and stays there while the console goes
to `debug`.

**Writes are synchronous** — no `tracing_appender::non_blocking` worker. The interesting
lines are the last ones before something died, the release profile is `panic = "abort"`,
and an aborting process runs no destructors: a background writer's buffered tail is
precisely the part that would be lost. Local-file writes at `info` are cheap enough to pay
for that.

The deployment half is one line — `XDG_STATE_HOME = "%S"` in the unit, the state-side twin
of the `XDG_CACHE_HOME = "%C"` that closed G31. It resolves to `/var/lib/castaway`, which
is where the GameStream pairing store was hardcoded to anyway, so that credential keeps its
existing path rather than moving under an operator.

### D40 — Settings on the glass, and the config file written back without being rewritten
The first slice of #12 ("GUI configurable"): a Settings tile, a menu behind it, one
setting in the menu — which device sound comes out of — and the piece that made the
feature worth a decision entry: the receiver now *writes* `castaway.toml`.

**The write is `toml_edit`, not serde, and that is the decision.** The config file is the
operator's document: comments explaining why Miracast is off, hand-grouped sections, keys
in the order they think in. Serializing `Config` back out would parse all of that into
structs and print a stranger's file — every save flattening every comment — which teaches
operators the config is not really theirs. So `app::settings::ConfigStore` parses the file
into `toml_edit`'s lossless document, changes exactly one key, and writes atomically
(sibling temp file + rename; the moment a panel loses power is mid-write to the only file
it boots from). The store's tests pin the property byte-for-byte, and pin the refusals:
a file that does not parse is reported, never clobbered — broken is the operator's to
mend, and a receiver that "fixes" it by rewriting it has destroyed the evidence.

**An unwritable config demotes the save, never the setting.** Apply order is runtime
first: the shared `OutputSelector` moves, the choice list shows its check moved, and the
receiver honours the choice until restart. Persistence failing (the NixOS module points
`$CASTAWAY_CONFIG` into the read-only store; a panel's disk can be full) comes back as
`Applied::NotSaved(why)` — not an error — and surfaces as an OSD toast naming the file
and the reason, while the screen carries on. The failure being designed against is the
same one as D39's: not "the save failed", but "nobody found out"; the one a settings
screen must not have is a pick that visibly did nothing because a file elsewhere refused.

**Settings are a catalog, not screens.** `app::settings::Setting` is the seam — id,
title, current-value summary, choices, apply — and `shell_nav` renders whatever the
catalog describes, knowing no setting by name. All three levels are pickers (D38's one
list screen), which bought back, scrolling and transitions for free; the sole primitive
added is `PickerItem::marked`, because a choice row is a selection rather than a doorway
and drawing the go-somewhere chevron on it would promise a screen that never comes.
Adding the next setting is one trait impl and one line in `main`.

**Output-device selection is keyed per backend, and Linux got a native PipeWire backend
to make it real.** `[audio.output]` carries `pipewire` / `windows` / `alsa` keys because
device ids share no vocabulary across backends, and one file travels between the dev box
and the panel — each build reads its own key and leaves the rest alone ("default" is the
stated policy, not a device name). Through the ALSA shim, PipeWire is one device called
"pipewire", which reduces the setting to a joke; so `pipeline::audio_pw` (feature
`audio-pipewire`, Linux-only, via the idiomatic `pipewire` crate — the D30 dependency
conditions, though this is a system API like cpal, not a protocol) enumerates real sinks
from the registry and routes per-stream with `target.object`. Its internals deliberately
mirror `CpalAudioOut` — bounded channel, never-blocking callback, stream owned by its own
thread — and both backends share the fallback policy: a chosen device that has left the
building plays through the default *with a warning*, because the wrong speakers saying so
beat a panel gone silent over an unplugged DAC. Selection reaches sessions through one
shared `OutputSelector` read at stream-open, so a pick applies to every source's next
session with no restart, and sessions already playing keep the device they opened — the
same rule as their sample rate. The Windows deploy artifact gains `audio-out` (WASAPI),
which it had never actually carried; until now the full Windows build had no PCM device
at all.

### D41 — Cast device auth by replay: someone else's identity, deliberately, with the exit named
#40 — "we need a real Cast device credential" — has been the last thing standing between
this receiver and an official sender, and it was posed as a hardware problem: device keys
are fused into licensed silicon, so get one off a panel you own. That framing was right
about where keys live and wrong about what the protocol requires.

**What the protocol actually requires.** Openscreen builds the blob it verifies from the
nonce the *receiver echoes*, not the one the sender issued, and `enforce_nonce_checking`
defaults to false. Echo nothing and the signed message is the peer certificate alone — so
a signature is bound to a certificate, not to a session, and stays valid for that
certificate's whole life. Every shipping software receiver exploits this the same way:
generate the peer certificate on a fixed 2-day schedule from a fixed key, and hold one
precomputed signature per window. No device key anywhere in the design. The omitted nonce
is itself the evidence — a party holding a device key could sign the nonce and would have
no reason to ship a 900-entry table.

**The decision** is to do that too, in `cast-replay`: a reimplementation of the CKS request
and its response cipher, plus the 900-window table (2023-01-01 → 2027-12-06, 1800
signatures) checked in as fixtures. Backend first — it keeps working as the calendar moves
— with the table behind it so an unattended panel that loses its uplink keeps
authenticating. `checks.openscreen-device-auth` records the result against Chrome's own
verifier: `cks-chain-google-roots` is **ok**, where `dev-chain-google-roots` is still
`kCastV2CertNotSignedByTrustedCa`.

**What it costs, stated plainly.** The identity is AirReceiver's, not ours. It is shared
with every install of that app, `AuthResponse` carries a `crl` field and Chrome fetches the
Cast device CRL, so Google can revoke it and Cast stops working with no warning and no way
for us to see it coming. The table expires 2027-12-06; the chain's own ceiling is
2032-12-12, set by `Eureka Root CA`. This is a borrowed credential with a known end date,
not a solved problem — which is why the exit stays first in precedence: `cast.credential`,
an operator's own provisioned key, wins over CKS whenever it is set, and #40 is rewritten
rather than deleted.

**Two invariants went into the type system rather than into comments**, because both fail
after a *clean* TLS handshake and are therefore invisible from either end. The signature
covers the certificate, so `CastCredential` hands out the TLS identity and the signature
together and `CastIdentity` enumerates the three ways to be a Cast device — there is no way
to pair a certificate with a signature from another window. And a replayed signature can
only be answered with `NonceEcho::Empty`, which now travels on `SignedAuth` instead of
being decided at the call site. The `cks-nonce-echoed` vector is the negative control that
gives that second one teeth.

This is ground rule 9 working as intended, not a carve-out from it: the RE landed as
fixtures and notes, the wire behaviour is reimplemented, and nothing links or ships
AirReceiver. It is also the third protocol whose hard part turned out to be a default in
someone else's verifier rather than a cryptographic barrier.

### D42 — Outbound Cast app identification: not implemented, and the reach measured first
D41 gave the receiver an identity for answering an `AuthChallenge`. A Cast device
identity has a second use that D41 did not touch: attaching device identity to a
receiver app's *outbound* requests to its own backend. AirServer's credential database
carries the material for it — 4380 pre-signed RS256 JWTs beside the 2190 receiver-auth
signatures — so "we already replay inbound, why not outbound" is the obvious next step.

**The two credentials are not one thing, which is the first correction.** The four token
families in that database split evenly. Templates 1 and 2 name no audience and no scope
and are issued by the bare `${CAST-APP-DEVICE-ID}` — device attestation that a receiver
app's *own* backend verifies. Templates 3 and 4 are Google service-account bearer grants
minting an OAuth token for `…@cast-devices.gserviceaccount.com` under
`scope=accounts/OAuthLogin`. An earlier reading of the RE notes described all four as the
latter; that was wrong and is corrected in the notes.

**What it would actually buy was measured rather than assumed.** `cast_shell` embeds
Google's per-app device-identification whitelist as one 3000-byte JSON literal,
byte-identical across the TCL RT51, an NVIDIA SHIELD, a Google first-party image and a
standalone `tv/ams` copy, at four different offsets. Ten app families. **Seven need only
headers** — device identity injected on requests to whitelisted servers, no signed
assertion, no device key required. Three sign a JWT: HomeView (Google's own Home panel
surface) and IMAX (all-zeros dummy app id) both use the service-account template and both
carry `"groups.claims.jwt.cast.google.com": "1234:dogfood"`, which reads as internal;
**BSkyB NowTV is the only third party in the table.** Netflix, HBO/Max, Disney+, Prime,
Hulu, Spotify and YouTube appear nowhere in it. The premise that these JWTs are what gate
the large DRM services — the whole reason to want them — is not supported by Google's own
configuration.

**The decision is not to implement it**, on two grounds that are worth keeping separate.

The first is that the reach is one UK streaming app, so the feature does not pay for
itself under any reading. That alone settles it.

The second is a line about kind, and it is drawn deliberately because D41 does not
generalise to cover this. D41's replayed signature convinces a sender on the operator's
own LAN — their phone, their Chrome, their panel — and no third party with a stake in the
answer is told anything false. An attestation JWT convinces a streaming service's backend
of a *robustness class* that is not true, and that backend uses it to decide what content
protection is safe to release under. Attestation is not an entitlement check, so
misrepresenting the class is the function of presenting the token rather than a side
effect. That is a deceived third party with a real interest, which the LAN case does not
have, and it is also what D32 already decided in the small: decline what we cannot host
in the sender's own vocabulary rather than fake it.

Four costs were priced before deciding, and they hold even for the attestation half taken
alone: the identity is a *second* vendor's (AirServer's, where D41 borrows AirReceiver's),
so a burn costs two products' users; the tokens stop 2027-03-21, ahead of the CKS table's
2027-12-06; unlike an inert receiver-auth signature a pre-signed JWT *is* the credential,
so it could never live in this repository; and the two identities are different devices —
`CN=2001805200936810051` under `Widevine Cast Subroot` versus `CN=RYW0O FA8FCA6AC5A0`
under `Eureka Gen1 ICA` — so any backend correlating the attested device against the Cast
device that authenticated sees two unrelated machines. The design would not have worked
as described even if it had been built.

The G56 consequence is the useful part: app identification is **not** a blocker for
hosting third-party receiver apps, and a D32-style "declines because it needs attestation"
path was scoped and dropped as near-dead code. The RE record is
`re-shell/artifacts/airreceiver-cast-signatures/APP-IDENTIFICATION.md`, with
`extract_app_whitelist.py` reproducing the table from any `cast_shell` build.

### D43 — A second borrowed identity, because revocation is the risk we could not price
D41 shipped one borrowed Cast identity and named the thing it could not fix: the
AirReceiver credential is shared with every install of that app, `AuthResponse` carries a
`crl` field, Chrome fetches the Cast device CRL, and Google can revoke it whenever it
likes. The failure mode is the worst shape available — a clean TLS handshake, then every
official sender quietly refusing to talk, with nothing in our logs to say why.

**This does not reduce that likelihood. It changes the response from "reflash the panel"
to "edit one line".** `cast-replay` now carries AirServer's identity beside AirReceiver's,
and `[cast.replay] identity_order` picks between them.

**The identities are genuinely independent**, which is the property that makes a second
one worth 960 KiB:

| | `cks` | `airserver` |
|---|---|---|
| device CN | `RYW0O FA8FCA6AC5A0` | `2001805200936810051` |
| issuer | `Eureka Gen1 ICA` | `NVidia mdarcy … Cast ICA` |
| root path | `Eureka Root CA` | `Widevine Cast Subroot` |
| covers | 2023-01-01 → 2027-12-06 | 2024-03-20 → 2027-03-21 |

Different device, different intermediate, different *branch* of the Cast PKI — the
AirServer leaf comes through the Widevine-backed provisioning path this project probed in
D42, not from a Eureka ICA. A revocation or a root-level problem that kills one has no
particular reason to touch the other.

**What this is not is a horizon improvement, and the docs say so rather than implying
otherwise.** AirServer stops eight months *before* the CKS table, so on expiry grounds
alone the second entry is nearly vacuous: any instant it can serve, CKS can serve too.
The single exception is a preferred-but-exhausted identity falling through, which is
tested. The CKS backend remains the only path that outlives every table.

**Two structural surprises in the data**, both of which shaped the implementation rather
than being absorbed silently:

* **Windows overlap.** AirServer steps 1 day with 2-day validity, where CKS steps 2 days
  and tiles. So two windows are valid at any instant, and `index_at` returns the later
  one — more remaining life, less chance of a roll landing mid-session.
* **The certificates cannot be re-issued from a template.** `cks` ships one template and
  rebuilds each window's certificate from it, because CKS's differ only in validity.
  AirServer's also differ in serial (linear in the index, so derivable) and in **subject
  CN, which is a fresh random UUID per window**. Not derivable from anything, and the
  device signature covers exact DER, so a rebuilt certificate would be rejected. Hence
  790 KiB of certificates checked in verbatim. Paying disk to remove a silent-failure
  class is the trade ground rule 1 asks for, and a test asserts the UUIDs really are
  distinct so that a future database without them makes the cheaper representation
  discoverable.

**The live AirServer endpoint is wired (see D44).** An earlier revision of this entry
declined it on the grounds that an unattended panel refreshing on a schedule is the "do
not run this in a loop" case the RE handoff warns about. That was the wrong call, and the
reason is in the sentence above about horizons: without it, the checked-in database *is*
the receiver's expiry date, and a CKS revocation leaves nothing rolling at all, because
the CKS endpoint keeps vending the same revoked identity.

**Fallback is a declarative ordered list, not a chain of flags** — `identity_order` — so
"which identity is this panel presenting" has one answer readable from config instead of
being reconstructed from which branches happened to be taken. Nothing in the receiver can
*detect* a revocation, since the signal is a sender refusing us, which is exactly why the
order is operator policy rather than an inference. A table whose fixtures fail to load
degrades that one identity and logs at `error`; startup still succeeds on the other, which
is half the point of carrying two.

The crate was renamed `cast-cks` → `cast-replay` in the same breath: "CKS" is one of the
two vendors, and the crate is the mechanism both share. Provenance is in
`crates/cast-replay/fixtures/airserver/README.md`; no JWTs came across (D42).

**Verified in CI as of D44.** `checks.openscreen-device-auth` now judges both chains with
openscreen's own sender-side verifier: `airserver-chain-google-roots` is **ok** and
`airserver-nonce-echoed` is `kCastV2SignedBlobsMismatch`. So this identity is as
well-evidenced as D41's, and the claim that reversing `identity_order` after a revocation
actually works no longer rests on the extraction tool's word.

### D44 — Both identities get a live endpoint, so the checked-in databases stop being an expiry date
D43 carried two borrowed identities but only one live path: CKS had a backend, AirServer
had a 1095-window table and a hard 2027-03-21 wall. That asymmetry undid most of the point.
A checked-in table is a *floor*, not a plan, and two floors with fixed end dates are still
two fixed end dates — worse, the case D43 exists for is a CKS revocation, and the CKS
backend keeps vending the same revoked identity, so after one there was nothing rolling.

**Both identities are now `cache → live → checked-in table`**, tried in
`cast.replay.identity_order` before the next identity is considered. Reordering therefore
changes which identity the panel *presents*, not merely which table it falls back to.
`offline_order` became `identity_order` and `OfflineIdentity` became `Identity`, neither
being offline-only any more.

**AirServer's cache and live source are one artifact**, which is what makes rollover cheap:
the endpoint answers with a whole ~14 MB SQLite database covering about 30 rolling windows,
so one request buys a month and the panel spends nearly all its time answering from the
cached file with no network at all. A database within three days of its end triggers a fetch
*before* being used — the set is replaced while the old one still works rather than after it
stops — and if that fetch fails the old database is still used, because an expiring
credential beats none.

**The two live sources hold down independently.** Sharing one backoff timer would let a dead
CKS backend suppress AirServer refreshes for the whole hold-down, which is exactly the
coupling a second identity exists to remove.

**Reading the response meant reading SQLite, and the first attempt at that was wrong.** The
initial approach was ~400 lines of hand-written b-tree, varint and overflow-page parsing,
justified by keeping a `forbid(unsafe_code)` crate free of C for the MSVC cross-build. That
premise was false — `moonlight-sys` already links moonlight-common-c, enet and libcrypto
into the Windows artifact — and applying new hand-written parsing to a 13 MB blob off the
network was the wrong trade regardless. It was deleted in favour of `rusqlite` with
`bundled`. The same instinct had started hand-composing XSalsa20 + Poly1305 to avoid a
pre-release `crypto_secretbox`; that was also reversed. Rolling your own to dodge a version
tag is the worse half of that trade.

**Three properties of the container were established by experiment, not assumption**, each
of which fails confusingly rather than loudly: `crypto_secretbox`'s AEAD impl keeps
libsodium's tag-*first* layout rather than RustCrypto's usual appended tag; `metadata.json`
is the one column that is not a secretbox, stored as plaintext `{"generated": <unix>}`; and
the BLAKE2b KDF is pinned to a vector taken from the reference implementation so its
salt/personal parameterisation cannot drift unnoticed. The strongest test is
cross-implementation: a credential built through the database path must be byte-identical to
one built from the checked-in fixtures, which were exported from the same source by the
Python tool.

**`jwt_token` is never read.** A live response carries 20 520 of them — bearer credentials
for the outbound identification D42 declined — and skipping the table is both the policy and
the reason a 14 MB response is cheap to ingest.

**What is still not faithful.** The POST body is `[]`. The real body is a JSON array
assembled at `0x14012e100` over a `QList` whose element shape was never recovered; an empty
array is accepted and returns a complete set, so it is sufficient but not faithful, and a
future `AD-Db-Schema-Version` could start requiring an element. Nothing in CI exercises
either live endpoint — both are third-party services, so the tests cover the reader, the
resolution order and the rollover decisions, not the network.

`cast.replay.airserver_live` turns the endpoint off for an operator on a metered link,
leaving that identity on its bundled table.

**All four derivations are now written down where the code is.**
`crates/cast-replay/PROVENANCE.md` records both static tables and both remote
protocols: source-artifact SHA-256s and the exact product versions (AirReceiver Lite
5.1.7 arm64-v8a; AirServer 5.7.2 and 2025.7.23, which differ in whether the database
is a loose file or linked into the executable), the `dbio` container offset and KEK
recovery, the CKS provider vtable layout and its `MD5(secret || ts)` request, the
BLAKE2b constants' addresses in `.rdata`, the Qt request builder's addresses, and the
commands. The reason it is that detailed: the tooling lives in `re-shell/artifacts/`,
which is **gitignored there**, so those scripts are not under version control and
this repo holds the only durable copy of how any of it was obtained.

### D45 — The network surface is a registry, and everything that names a port is generated from it
Issues #22 and #30 asked for two things that sound like documentation — a central record of
every bound port and a description of the exposed surface — and the hand-kept firewall list
in the NixOS module showed why documentation alone would rot: it had already drifted three
ways from the code it described. TCP 7011 was open with nothing behind it (the second
AirPlay listener was removed long ago; 7011 is the AirPlay 1 UDP *timing* port, never a
listener). Cast and AirPlay were gated on `cfg.settings.enable.* or false` while the binary
defaults every enable flag to true, so a stock deploy ran Cast on 8009 with the firewall
closed — masked in CI because the integration VM pins `enable.cast = true` explicitly. And
the mirroring media planes bound OS-assigned ephemeral ports, which no rule could name in
advance: on a firewalled box every control plane looked perfect and the media died in
silence, masked in the VMs that disable their firewalls.

**The registry is code, in `crates/app/src/surface.rs`**: every listener with its owner,
transport, port spec (fixed / config-keyed / the `[media_ports]` range), security posture,
enable-flag gate and provider (process or deployment); every advertisement (mDNS service
types, SSDP STs, the WFD IE); every outbound destination; the non-IP surfaces. Entries are
built from the constants the listeners bind (`proto_cast::CAST_PORT`,
`proto_airplay::AIRPLAY_PORT`, `substrate_ssdp::SSDP_PORT`, `substrate_mdns::MDNS_PORT`),
and the per-protocol table is an exhaustive match over `ProtocolKind` — a new protocol
does not compile until its surface is declared, an empty declaration being a declaration.
`ProtocolKind` lost `#[non_exhaustive]` for exactly this: the attribute contradicted the
enum's own "closed enum" doc comment and would have handed every downstream match the `_`
arm this forcing function exists to deny.

**Everything else is generated and held to the registry by a test or a lint.**
`docs/network-surface.md` (#30's answer) and `nix/network-surface.json` are emitted by the
registry; freshness tests fail on any drift and `CASTAWAY_REGEN_SURFACE=1` rewrites them.
The NixOS module derives `networking.firewall` from the JSON — gates resolve against
`cfg.settings` with the binary's own defaults, and an unknown gate flag fails evaluation
rather than staying closed. `nix flake check` runs the freshness tests, so a stale JSON
cannot build. In the other direction, `clippy.toml` disallows the raw bind calls
workspace-wide (CI runs `--deny warnings`); each registered site carries `#[expect]`
naming its entry, so an unregistered bind is a lint error, not a doc gap. The receiver
answers for itself with `castaway --network-surface[=json|netsh]` — the resolved view of
the loaded config, and the netsh form is the Windows half of #22's firewall ask.

**The media planes had to stop being ephemeral for any of this to be true**, so AirPlay's
per-session sockets (audio/control/timing UDP, mirror-data TCP) and Cast's mirroring RTP
socket now allocate lowest-free-first from `[media_ports]` (default 41000–41031, 32 ports;
a session takes at most four). `MediaPorts::Ephemeral` still exists for tests, but it is a
required constructor argument — choosing the unfirewallable behaviour has to be written at
the call site. A broken configured range is a boot error, not a fallback: the operator who
wrote it meant to control where media lands, and booting ephemeral would undo that quietly.

**The miracast exception is stated rather than smoothed over**: its firewall gate follows
the module's explicit opt-in (`enable.miracast or false`), not the binary's default-on,
because the module's radio units grabbing `wlan0` on an unconfigured box is a worse
failure than a closed port on a protocol that needs a radio anyway.

Not done, and known: nothing yet asserts at runtime that the set of actually-bound sockets
equals the registry's resolved view (`ss` in the integration VM against
`--network-surface=json` would close that loop), and the generated doc's hardening section
describes the systemd unit but nothing diffs it against `flake.nix`.

### D46 — The panel is one model, and its motion is derived from that model rather than authored per transition
Two rounds of the same bug produced this. A now-playing card was drawn over a service
screen's text, and Spotify came back from a reclaim as a nameless session with no controls
and an "up next" list in an otherwise empty card. Both were the same shape of failure:
"what is on the glass" was a *product of independent variables* — `RenderLoop::shell_front`,
the screen stack's depth, `ElectronHost::role` plus a `widget_covered` flag it refreshed by
hand — and the combinations nobody had decided about were the bugs. `shell_front` says
whether the shell is above the media layers and nothing whatever about which screen it is
on, so demoting into the *Home screen's* widget slot while two screens deep was
representable, and therefore happened.

**`pipeline::panel` is the one authority.** Which screens are stacked, which surfaces exist,
and whether the shell has been asked forward; everything else is derived — placement,
suppression, hit testing, the browser's viewport. Four things stop being representable:
focus on a session that is not there (`Focus` is derived, never stored); a demoted surface
on a screen with nowhere to put it (`Placement` is Panel/Widget/Hidden, total over focus ×
depth × surface); the page as a second state machine (the browser host reads `page_view()`
each pump instead of owning a `BrowserRole` it mutates); and the idle clock being the same
thing as a minimised cast page (they shared a browser, a layer and a slot, and were told
apart by comparing URLs — now `Surface::IdleWidget` vs `Surface::CastPage`, one of which can
never be full-panel or restored).

The kiosk's `back_one_level` if-chain — minimise a page, else pop a screen, else bring the
shell forward, across two objects in whatever order they were written — became
`Panel::back() -> Left`, matched exhaustively. The ordering falls out of focus: you cannot
pop a screen you are not looking at. `pop_screen` stays separate, because a back affordance
drawn *on* a screen must not demote a session as a side effect of a press that session is
not even covering.

**`pipeline::motion` is the continuous half, and it takes no new inputs.** The two things it
needs — presence changed, placement changed — are exactly what the model already reports,
which is why the motion was tractable after the model and not before. Three rules, which is
where Apple's, Google's and Microsoft's languages agree, and each is physical honesty rather
than taste: a surface that exists before and after **travels, never cross-fades** (`Motion`
interpolates a rectangle; there is no cross-fade path for a surface that persists); a
surface that appears comes **from where it was summoned** (`Origin`, with `Nowhere` a
variant somebody has to choose, so a forgotten tile rect is visible in a diff rather than
invisible on the glass); and **entrances decelerate while exits accelerate and are faster**,
asserted pair by pair against the `Choreography` table.

Springs rather than curves, because a spring accepts an initial velocity and that is exactly
what a released drag hands over. Each of a rect's four components springs independently in
normalized units — five scalars where a normalized progress needs one — and that buys the
property making interruption free: **retargeting is just a different target**, so a reversal
mid-flight carries real velocity through the turn with no rebasing. Damping is 1.0
everywhere except the summon (0.85), because a display people see out of the corner of their
eye all day should not wobble, and the one exception is the transition somebody asked for
with a finger.

**Input must never read the animation.** Coverage was answered from live layer transforms, so
a press 100 ms into a 300 ms arrival meant something different from the same press once it
landed. `Panel::covered_by_any` answers from placement, and the caller names *which* surfaces
to ask about, because "covered" is relative to depth: the transport strip is drawn on the card
it belongs to, and that card is no reason for its own controls to stop answering. The same
audit found the two strip-coverage checks had drifted — they carried the rule independently,
with a comment saying it had to be repeated rather than assumed, and when the rule changed
only one changed, so a covered strip stopped owning presses while still acting on them.

**Three things the renderer had to gain**, and one it did not. Corner radius, animatable, so a
screen keeps its tile's corner and flattens as it grows: a square-cornered rectangle emerging
from a rounded one reads as two objects. An independent source rect, because the tiles are
exactly square and the panel is 1.778 — drawing a full-panel texture into a tile rect
compressed it 44%, and `cover_source` crops instead, which is provably a no-op once the shapes
agree and so can be applied unconditionally. Both live in the uniform, whose sizes are now
asserted in a `const` block: a WGSL/Rust layout drift does not fail to compile, it draws every
layer from the wrong bytes. What it did *not* gain is transient content layers, so a container
transform still scales a finished screen rather than crossfading contents inside a morphing
container — the growing screen is a small screenshot until that lands.

**The slot and the PiP corner are deliberately still square**, because that is what the art
does: the widget card frame is drawn with `fill_rect`. Rounding either is now one constant
rather than a shader change, which is why the mechanism landed separately from any cosmetic
call about it.

Two consequences worth stating. The floor scales *up* 2% while receding rather than down,
because it is the bottom layer and insetting it would open a black border where a phone shows
a backdrop; the mascot's transform is *composed* with it, since she is a sub-rect and handing
her the floor's transform stretches line art across the panel. And she leans on the **slot**,
not on the clock: `MascotOverlay` moved above the card so what the slot holds is under her
arms, and what removes her became `slot_veil` — how far the occupant has expanded between its
demoted width and the whole panel. Degree rather than presence, so the change is a fade with
no threshold, and a demoted *video* (which goes to the far corner, not the slot) correctly
leaves her alone.

**The edge drag turned out never to have worked**, and finding it is the clearest argument for
the model. The kiosk computed how far the finger had come and called `drive_transition` — but
nothing *began* a transition, so it drove one that did not exist and nothing followed the hand.
Worse, the flag meaning "a drag is in hand" was set and never cleared, which made the
completed-swipe branch (`if complete && !dragging`) unreachable: **the swipe-to-home gesture
stopped working permanently after the first time anyone dragged from the edge and let go
early.** Two missing cases in an if-chain over five variables, in the one file with no test
harness — the kiosk owns the winit event loop.

So the decision is now `overlay::edge_drag`, pure and total: Ignore / Home / Begin / Carry, with
the one ordering that matters stated (a completed swipe fires only with nothing in hand, because
with a navigation being carried the swipe *is* that navigation). Only a screen-to-screen back is
carried, because its whole animation is a position and a finger can be halfway through one;
handing the glass back at Home is a change of focus rather than of place, so it stays a
threshold gesture. And the incoming screen is carried too — `Floor::drive` sets a position with
no spring while a contact is down, and the spring resumes from exactly there on release, which
is what makes letting go part-way put it back.

**One integrator, one feel.** The screen transition kept its own — a velocity decay plus a
proportional pull, with its own settle thresholds — beside the springs everything else uses. Two
mechanisms for one thing is the shape of problem this whole entry is about, so it now takes a
`Spring` from the same choreography table. The hand-rolled `SETTLE_RATE` is gone, and a flick
works because a spring accepts an initial velocity rather than because a decay term was tuned
against a pull term.

`Origin::From`'s live user is the arriving *screen* (`Floor::launch`), which is the one thing
whose whole path from a press is local. A session surface always arrives `Nowhere`, because every
route that starts one — a phone casting, a DIAL launch, a track beginning — crosses an async
round trip that no touch survives. That is said in the code rather than left to be inferred: the
`RenderLoop` briefly carried an `origin` field nothing ever set, which read as if it were wired.

Not done, and known: no transient content layers (above), so a container transform still scales a
finished screen; no elevation shadow, so a demoted card reads as a flat inset rather than as a
lifted card; and `CLEAR_GRACE` still debounces presence separately from the exit animation —
right for VLC's stop-then-reload scrubbing, redundant for preemption. `Floor::launch` and
`Motion::enter` are still two implementations of "arrive from a place", one for the floor and one
for surfaces, which is the next duplication to collapse. The kiosk's input routing has no test
harness of its own; extracting `edge_drag` moved the part that had bugs in it out, but
`route_input`'s ordering is next.

### D47 — LDAC: link Sony's own codec, and advertise it only when asked
LDAC is the one A2DP codec libav has no decoder for, and it was the last unimplemented thing in
the Bluetooth sink. It is now implemented by **linking** `libldacBT` — Sony's own library, via
open-vela's fork — behind `ldac-sys` and a safe wrapper in `pipeline::ldac_decode`, with the
endpoint kept out of the advertised table until the config names it (OPEN-QUESTIONS Q22,
architecture-substrate.md §11.4a).

**This is not a third carve-out from ground rule 9,** and the distinction is worth stating so
nobody reads it as one. D30 and D37 are exceptions about *protocol* stacks: rule 9 tells us to
reimplement the wire, and in those two cases we did not. LDAC is a *codec*. This pipeline already
links libavcodec for SBC, AAC, aptX and aptX HD, and nobody proposed reimplementing those — the
rule was never about DSP. What would have been a rule-9 question is the A2DP framing around the
codec, and that stays ours: the capability block, the payload header, the transport-frame walk,
all reimplemented and fixture-tested. The line is the same one D37 draws, in a place where it
happens not to be contentious.

Three findings, each of which looked settled and was not:

- **A reverse-engineered decoder was never needed.** Q22 had it that AOSP's `libldac` is
  encoder-only, so decode meant the RE'd `libldacdec`. The premise is true and the conclusion is
  false: open-vela's fork builds Sony's complete codec, decoder included.
- **`pkgs.ldacbt` is not that library.** Under the nixpkgs this flake pins, it is EHfive/ldacBT
  built `_ENCODE_ONLY`: `libldacBT_enc.so`, no `ldacBT_decode`, and a header that does not declare
  one. A newer nixpkgs has the right one, but reaching it means bumping ffmpeg and Electron for a
  codec. So `nix/ldacbt.nix` builds it from a pinned source and fails the build if the symbol ever
  goes missing again — the check exists because the failure mode is a plausible-looking library.
- **Apache-2.0, so nothing is bound.** Unlike D37's GPL-3.0 core, this composes with the MIT tree.
  The feature is off by default for a build-dependency reason — it needs `LDACBT_LIB_DIR` at link
  time — and not a licence one.

**Advertising is now separated from decoding, in both directions.** Q22 was the failure in one
direction: `can_decode` answered `cfg!(feature = "ldac")` while the feature bound nothing, so a
build advertised LDAC, a phone picked it — LDAC is *first* in preference order — and every packet
failed. A connected phone, a running session, and silence. That is fixed at the root: `can_decode`
allocates a decoder handle and reports what happened, so the flag and the fact cannot disagree.

The other direction is this decision's own reticence, and it is deliberate. A decoder existing is
not a reason to let every sender use it tomorrow. Because LDAC sorts first, enabling it does not
add an option — it changes what every capable phone negotiates, immediately, on a panel nobody is
watching, from a decoder that has never seen a real Android encoder. So `bluetooth::OPT_IN` holds
it back and `codecs = ["ldac", "sbc"]` turns it on, with SBC named alongside so a sender that
cannot do LDAC still connects and the experiment does not present as a broken receiver. The
condition for removing it is named rather than left to taste: one real sender streaming to it.

**The test vectors are the substance of the work, more than the FFI is.** Four sharp edges in
`ldacBT_decode` are invisible until you hear them, and all four survive a test that checks for
`Ok`: the input buffer needs two bytes of slack past the frame because the bit reader fetches
three bytes at a time; one A2DP payload holds a *sequence* of transport frames and each call
consumes one, so a per-packet loop plays a sixth of the audio; a `-1` return carrying
`LDACBT_ERR_DEC_CONFIG_UPDATED` is a success that reconfigured the handle and produced audio; and
the *stream* states its sample rate, not the negotiation, so following our own AVDTP answer plays
a 96 kHz stream at 44.1.

So the fixtures are generated by Sony's encoder and checked against a pure Rust parser that shares
no code with it. The encoder reports how many transport frames it packed into each MTU; the parser
walks the same bytes and must reach the same number, at 44.1 kHz stereo and at 96 kHz dual channel
— the second because it puts 256 samples in a frame instead of 128, and because a rate assumed
rather than read is exactly the Q25 failure in the one codec where it can be caught. That check
runs in every build, with no FFI. The decode assertions are on the audio: level, both channels,
balance, and the exact PCM frame count the fixture implies.

What that does not prove is interoperability — the bytes came from our own encoder, not a phone —
and being honest about which half is proven is the whole reason the endpoint is opt-in rather than
on. A capture from a real Android sender is the missing fixture, and the thing that retires it.

Also landed here, because it is the same class of problem: `proto-bluetooth-audio` gained a pure
LDAC frame-header parser. It decodes nothing. It exists because LDAC's A2DP payload needs a
one-byte header stripped, and without a syncword to check against, failing to strip it decodes to
noise rather than to an error — the same silent shape as treating classic aptX as RTP. It also
surfaces the stream's own rate for comparison against the negotiated one, which is the only
codec here where that comparison is possible at all.

Not done, and known: no capture from a real sender, which is the one thing the fixtures cannot
substitute for. `ldacBT_get_bitrate` is bound and unread, so nothing reports LDAC's actual rate
the way `Depacketizer::bitpool` reports SBC's — the on-screen card says only the codec and rate.
The ABR library (`libldacBT_abr`) is not built and would be meaningless in a sink anyway, since
adaptive bitrate is the sender's decision. And the wrapper follows a mid-stream rate change by
re-reporting the new rate on its blocks, which is correct as far as it goes; the output device is
not reopened, so a sender that switches rate mid-session will play at the wrong pitch until the
session restarts. That is a pre-existing property of `audio_session::run` rather than something
LDAC introduced — but LDAC is the first codec that can actually trigger it.

### D48 — Advertise A2DP's profile UUID as a service class, because KDE's label is unreachable otherwise

The panel showed up in Plasma's Bluetooth applet as "Other device". Nothing about that was
our bug, and the fix is still ours, which is the whole reason this entry exists.

The chain, traced against BlueZ-Qt 6.26.0 and bluedevil 6.6.5 on the dev box. Our class of
device is `0x00240414` — major Audio/Video, minor Loudspeaker, service bits Audio and
Rendering. BlueZ-Qt's `classToType` reads the minor class, finds no special case for a
loudspeaker, and returns `Device::AudioVideo`. Correct. Then `DeviceItem.qml` switches on
that type and tests `case BluezQt.Device.OtherAudio` — an enumerator that exists in no
version of BlueZ-Qt, master included. In QML that expression is `undefined`, never equals
an integer, and the "Audio device" branch is dead code. Every device whose minor class maps
to `AudioVideo` — which is to say every loudspeaker — falls through to a fallback that
searches the UUID list for `0x110D` (Advanced Audio Distribution) and never for `0x110B`
(Audio Sink). A2DP puts `0x110D` in the profile descriptor list, not the class list, so a
strictly conformant sink does not publish it where BlueZ collects device UUIDs from. The
fallback finds nothing and prints "Other device".

So a spec-correct A2DP loudspeaker is *guaranteed* to be mislabelled by Plasma 6.6.5. Two
stacked upstream defects, neither of which we can reach from the wire.

Decided: publish `0x110D` in the sink record's `ServiceClassIDList` alongside `0x110B`, and
list it in the extended inquiry response for the same reason. This is a deviation and is
marked as one at both sites, with a test that fails if someone tidies it away.

What justifies it: nothing a sender acts on changes. The record still describes AudioSink
over AVDTP on PSM 0x0019, the profile descriptor list is untouched, and a sender searching
for `0x110B` finds exactly what it found before. A sender that searches for `0x110D` now
matches and gets a record that correctly identifies itself as a sink. The cost is one extra
UUID in a list; the benefit is the device naming itself correctly in the desktop environment
most likely to be sitting next to it.

What it forecloses: little, but it is a precedent worth bounding. The test that this is a
*claim about a service*, not a *claim about a capability we lack* — the profile genuinely is
implemented, it is merely being announced in the adjacent field. A future request to
advertise something we do not implement, to satisfy some other control point's parser, does
not follow from this and should be refused.

Corroboration worth keeping: the AirPods Pro paired to the same adapter also omit `0x110D`.
They escape the bug only because their minor class is Headphones, which is a live branch in
that switch. That is the control which proves the class list, not our SDP, is what decides
the label.

Verified on hardware, 2026-08-01. After redeploying, `bluetoothctl remove` and a fresh scan
showed the panel arriving with its name already attached and all five service classes
present at `Paired: no` — so the list came from the extended inquiry response rather than
from SDP — and Plasma's applet now labels it "Audio" instead of "Other device". Worth noting
which prediction was wrong on the way there: the first hypothesis was a stale `Class=` in
BlueZ's cache from before the class of device was set, and it was not. BlueZ had the class,
the icon, the name and the UUIDs correct the entire time. The bug was two layers above.

Not done: the bluedevil bug is unreported upstream. Both halves are one-line fixes there
(`OtherAudio` → `AudioVideo`, and checking `AudioSink` in the fallback), and reporting it is
strictly better than our workaround for everyone who is not us.
