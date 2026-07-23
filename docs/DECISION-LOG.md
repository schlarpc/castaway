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
