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
The captures rule 9 asks for need hardware and senders this session can't drive. Where a golden
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
connected phone and a black panel (GAPS.md G56).

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
the driver question, and it is only worth building once a group forms. See OPEN-QUESTIONS
Q7 and Q26 for what has to be true before either can be promised on the deploy target.

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

So: the browser layer moves to an **Electron subprocess** — stock nixpkgs `electron` on
Linux and in CI, castLabs ECS (`castlabs/electron-releases`, MIT) for the Windows deploy
artifact. What each recorded problem gets:

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
  33 MB/frame CPU `on_paint` copy that CEF's buggy accelerated OSR (Q6) forced us onto,
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
