# Decision Log

Running log of engineering decisions made while building castaway autonomously.
Each entry: what was decided, why, and what it forecloses/enables. Newest at top.
Review at next sync.

## 2026-07-23

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
