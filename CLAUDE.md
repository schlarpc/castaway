# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this project is

**castaway** is a single unified "universal cast receiver" — one Rust binary that advertises and
terminates many casting protocols (AirPlay, Google Cast, Miracast, DLNA, YouTube Lounge, Spotify
Connect) so any device on the LAN can throw media at the display with no app install. It drives a
Dell C6522QT commercial touch panel; the deploy target is a Windows box, development happens
natively on Linux (NixOS).

The design docs are the spec — read them before touching a subsystem:

- **docs/hackerspace-receiver-build.md** — the "why", protocol surface, crypto/auth modules, effort tiers, build priority.
- **docs/architecture-substrate.md** — what is *actually* shared vs. what only looks shared; the Cargo workspace layout; the core traits (`SourceAdapter`, `SessionEvent`, `FrameSource`); pipeline and threading model.
- **docs/dlna-conformance.md** — the DLNA/UPnP conformance record: what is confirmed
  correct and must not be "fixed", where real control points diverge from the spec, and
  what the citations are worth. Read before changing `proto-dlna`.
- **docs/miracast-protocol-notes.md** — the WFD/Miracast protocol record `proto-miracast` is built from: the information element, the `wfd-kv` grammar, the M1–M16 exchange, MPEG2-TS-over-RTP, UIBC, and what real Windows and Android senders actually do. Read before changing `proto-miracast`; §7 is the platform reality and is where the project's remaining risk lives.
- **docs/gamestream-protocol-notes.md** — the GameStream/Moonlight record. The one *inverted* protocol (we are the client) and the one that is half-linked rather than reimplemented: NVHTTP + pairing are ours, the streaming core is moonlight-common-c (D37). Read before changing `proto-gamestream`; §6 is where its remaining risk lives.
- **docs/cross-build.md** — Linux→Windows cross-build (`cargo-xwin`-modelled MSVC toolchain), the vendored Electron/ffmpeg/Widevine blobs, and the testing matrix.

Reference implementations named in the docs (UxPlay, openscreen, librespot, yt-cast-receiver, …)
are **RE sources / wire-behavior specs, not runtime dependencies** — we reimplement.

**Work items live in GitHub issues, not in `docs/`.** `docs/` holds decisions and records —
why a thing is the way it is, what a protocol actually does, what has been measured. It does
**not** hold backlogs. A defect, a gap, or a question for the next sync goes straight to
`gh issue create` — but *first* check whether an open issue already covers it: several do,
and a duplicate is worse than a comment on the real one. Do not start a new tracking
document for it, under any name. Reference issues from code and docs as `#<n>`, or
"issue `#<n>`" where the prose needs a noun. `docs/GAPS.md` and
`docs/OPEN-QUESTIONS.md` were exactly that mistake — both were migrated to issues and deleted
on 2026-08-01, after the tracker and the files had already drifted apart.

`G##` and `Q##` in code comments and older docs refer to those two files. The manifests that
map every number to its outcome are issues **#104** (`GAPS.md`, the 2026-07-26 audit) and
**#105** (`OPEN-QUESTIONS.md`); full text is at `git show a556938:docs/GAPS.md` and
`git show a556938:docs/OPEN-QUESTIONS.md`. Do not add new `G##`/`Q##` anchors — cite the issue.

## Ground rules

These are binding engineering constraints for this project. They override general defaults.

1. **Correct by construction — lean hard on the type system.** Make illegal states unrepresentable.
   Prefer enums over booleans/strings, newtypes over bare primitives, typestate/session-type
   patterns for protocol state machines (a message that can't be sent in the current state should
   not typecheck). Parse, don't validate: convert wire bytes into rich types at the boundary and
   pass those inward. If a `match` could be non-exhaustive tomorrow, model it so the compiler forces
   the update.

2. **Workspace build, one concern per crate.** Follow the layout in architecture-substrate.md §2:
   `core` (traits/session), `substrate-*` (shared wire framing/plumbing), `crypto-*`, `proto-*`
   (one per protocol), `pipeline`, `control-display`, `input-touch`, `app` (wiring/binary). Share
   the wire framing and connection plumbing; **do not** share protocol *semantics* — each protocol
   owns its own state machine. Dependencies flow toward `core`; `proto-*` crates never depend on
   each other.

3. **Pure protocol, composed with I/O — never interleaved.** Protocol logic is
   `fn(state, input_bytes) -> (new_state, outputs)` — sans-I/O, deterministic, synchronous, unit-
   testable without a socket. The async/socket layer is a thin actor that owns the connection and
   feeds bytes to the pure core. No parsing inside `tokio::select!`; no network calls inside a
   state transition. This is what makes the wire-fixture tests (rule 6) possible.

4. **Async everything.** Media playback must run fluidly, so I/O is `tokio`-based and non-blocking
   end to end. Adapters are async actors emitting `SessionEvent`s; blocking/CPU work (decode,
   crypto) goes on dedicated threads or `spawn_blocking`, never on the runtime. Respect the
   three-thread-domain model (winit/wgpu main + browser pump, decode threads, tokio pool) in architecture-substrate.md §6;
   for live mirroring, **drop late frames** (latency beats freshness).

5. **Cross-platform is a ground rule; Linux-first is the tactic.** Build the portable core
   correct-and-complete on Linux natively, but every platform-specific seam goes behind a trait or
   `cfg`/feature flag from day one so the Windows slice (and later the Windows→Linux Miracast move)
   is a new backend impl, not a core-trait change. Never bake a platform assumption into a portable
   crate. The `FrameSource::{Encoded,Decoded}` split and `MiracastBackend` trait exist precisely for
   this — keep that discipline everywhere.

6. **Test without a human in the loop; use Nix to make it reproducible.** Two tiers:
   - *Pure-protocol tests* run the sans-I/O cores against captured/golden byte fixtures
     (pcap transcripts, `bplist`/SDP/`wfd-kv` bodies, RTSP exchanges) — fast, deterministic, no network.
     Capture real fixtures from reference impls / live senders during RE and check them in.
   - *Integration tests* drive whole adapters against scripted senders inside **Nix-built VMs**
     (`nixosTest`/`pkgs.testers`) so end-to-end discovery+session flows run in CI with no hardware.
   Prefer building a harness over manual verification. Hardware-only paths (Miracast Wi-Fi Direct,
   DX12/browser render, the physical panel) are the *only* things allowed to require the real box; isolate them.

7. **Errors: typed in libraries, `anyhow` in the app.** Every `substrate-*`/`proto-*`/`crypto-*`/
   `core` crate exposes a `thiserror` enum whose variants enumerate its real failure modes (so
   callers can match and protocol errors are exhaustive). Only the `app` crate uses `anyhow` for
   top-level wiring. No `unwrap`/`expect`/`panic!` on runtime-reachable paths in library crates
   (`unwrap_used` is already a warn — treat it as deny outside tests).

8. **`unsafe` is quarantined to FFI.** Pure crates (`core`, `substrate-*`, `proto-*`, `crypto-*`)
   set `unsafe_code = "forbid"`. The FFI/interop crates (`pipeline` for ffmpeg/wgpu, `control-display`,
   `input-touch`, the `windows` glue) may use `unsafe`, but every `unsafe` block carries a
   `// SAFETY:` comment stating the invariant it upholds. Keep FFI surface thin and wrapped in safe
   types at the crate boundary.

9. **Reverse engineering lands as fixtures, not dependencies.** Use capture tools, Frida, and
   the reference impls to derive wire behavior and crypto flows. Land the *findings* here as
   checked-in fixtures + notes; never add a reference impl as a runtime dependency.

   **Carve-out — cloud-side protocols (D30).** This rule assumes the peer is a *device*
   speaking a spec that holds still: reimplementing is then a one-time cost that buys an
   asset we own and can test offline. It does not hold when the peer is a **cloud service
   that changes unilaterally** — there, owning the wire buys a maintenance treadmill, and
   every upstream change lands as silence on an unattended panel. Spotify Connect is the
   one protocol in this project that qualifies, and `proto-spotify` uses the librespot
   crates for everything above the LAN. Two conditions on any future carve-out: the
   dependency must be an idiomatic Rust crate (not a shelled-out reference binary), and
   the *local* surface — discovery, advertisement, anything sharing our single HTTP host
   or mDNS responder — still has to be ours.

   **Carve-out — the GameStream streaming core (D37).** The second and, for now, last
   exception, and a different one: the peer *is* a device speaking a stable spec, so the
   rule's own logic says reimplement. It was overridden deliberately on volume and shape
   — the streaming half is ~15k lines of C whose correctness is FEC recovery under real
   loss and A/V pacing under real jitter. The split holds the line where it matters: the
   LAN-facing half (mDNS, NVHTTP, pairing crypto, the adapter) is ours and is tested
   against the reference implementation's own vectors; only the post-`/launch` media
   plane is linked, behind an off-by-default feature, and it is GPL against this MIT
   tree. Do not read this as a general licence to link — read D37 for what it cost.

   Everything else here is still reimplemented.

10. **Commit semi-regularly, straight to `main`.** Commit at independent logical boundaries — often
    several commits within a single feature build-out, each one a coherent, self-contained change.
    Run `cargo fmt` and `cargo clippy --all-targets` (clean) before **every** commit. No feature
    branches or PRs; work lands directly on `main`.

## Development Commands

This is a Rust project using Nix flakes with a pinned toolchain. First load the environment:

- `direnv allow` — load the development environment (or `nix develop`)

### Build & run

- `cargo run` — build (debug) and run
- `cargo build --release` — optimized build
- `nix run` — build and run via Nix
- `./result/bin/castaway` — the nix-built binary

### Test, lint, format

- `cargo nextest run` — fast parallel test runner
- `cargo llvm-cov nextest` — tests with coverage
- `cargo clippy --all-targets` — lint
- `cargo fmt` — format

### Nix

- `nix build` — build the package
- `nix flake check` — run all checks (build, clippy, fmt, test, coverage)
- `nix flake update` — update flake inputs

## Architecture

Built from the [rust-flake](https://github.com/schlarpc/rust-flake) template. The repo is still
single-crate template scaffolding; the **target** structure is the Cargo workspace in
architecture-substrate.md §2 (see ground rule 2). Migrate to the workspace before adding real
subsystems.

- **src/main.rs** — application entry point (template scaffolding; becomes the `app` crate)
- **Cargo.toml** — package manifest; lints configured under `[lints.rust]` and `[lints.clippy]`
- **flake.nix** — Nix build (Crane), dev shell, and CI checks
- **rust-toolchain.toml** — single source of truth for the Rust version; Nix reads it via
  `rust-bin.fromRustupToolchainFile`, so builds stay reproducible. Bump `channel` to upgrade.

## Keeping in sync with the base template

Pull upstream template updates with [cruft](https://cruft.github.io/cruft/):

- `cruft update --checkout template`
