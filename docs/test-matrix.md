# Test matrix and exit criteria

A point-in-time audit of what the automated tests actually prove, per protocol and per mode,
against what "this mode works" would have to mean. Audited 2026-08-05 at `37db8a7`.

> **Acted on, same day, and then acted on further.** The audit's own top findings are fixed,
> so the numbers below have moved. **§3.1 (140 tests compiled by no check) and §3.2 (a broken
> build configuration) are closed by D55**: every feature is on by default now,
> `castaway-portable` is a test fixture rather than a product, 23 checks became 15, and
> `cargo nextest run` went from 1784 tests to 2243. §3.3's silent skips are all four closed.
>
> Sections are marked **[CLOSED]** where that applies. The rest is the standing record.
>
> **What has closed since, and where the evidence is.** Listed here rather than edited into
> §4, because §4 is a record of what the tests proved *at the audit* and rewriting it in
> place would destroy the thing it is for. Read §4 as the baseline and this as the diff.
>
> | Was | Now | Issue |
> |---|---|---|
> | DLNA's six "do not fix" rows had no test | `proto-dlna/tests/conformance.rs`, each test quoting its citation, each verified by mutating the thing it guards. Plus the hostile-friendly-name round trip (the bug that shipped) and GENA's two invariants in the real-socket subscriber test | #201 |
> | `moonlight-sys` had no ABI drift guard | `checks.moonlight-bindings`, regenerating from the pinned `Limelight.h` and diffing. The generated file's own layout assertions are self-referential and cannot see upstream drift | #191 |
> | `PinnedServerCert` was never asked to reject anything | three cases, including a truncated presentation | #191 |
> | Matter's passcode stayed on the glass forever | a deadline in `UdcServer::run`, tested at T0, over a real socket with paused time against the shipped 180 s, and in `matter-vm` through the OSD | #197 |
> | `CdError` 1–10 had no producer | three stages produce three codes, derived from a typed `CommissionStage`; the other six are documented as having no producer *and why*. `matter-peer --wrong-passcode` / `--wrong-instance` assert the specific code in `matter-vm` | #198 |
> | Miracast had no idle watchdog and ignored HDCP | a liveness deadline on the source's own `Session:` timeout, and an M4 offering HDCP refused with a typed reason | #195 |
> | `DeviceInformation::busy()` had no caller | re-advertised either side of every session, including the error path | #194 |
> | AAC had no decode coverage at all; the rest were RMS windows | per-channel cross-correlation against a two-channel sweep for SBC/aptX/aptX HD/AAC and LDAC, plus the real iPhone capture through the real depacketiser into the real decoder — the seam that nothing joined | #187 |
> | Darwin checks could not evaluate | `systems` is `[ "x86_64-linux" ]`; aarch64-linux turned out to be broken too (D57) | #207 |
> | `getInfo`'s field set and `spotify_device_id` were unasserted | both pinned, with the test saying plainly that a pin is not a validation | #200 (partial) |
> | `apply_track` / `best_cover` untested | every branch, mutation-checked | #199 (partial) |
> | five `#[ignore]`d files with no recorded decision | a call at each file, and they are three different calls — one is a gap, two are a nightly job that does not exist, two are wiring `cast-app-hosting` already has | #183 |
> | `node.rs` had zero tests | the endpoint tree and the one-based target mapping, mutation-checked. Every cluster *handler* is still unexecuted | #196 (partial) |
> | SSDP `NOTIFY` never seen on a wire | a listener joined to the group watches two alive bursts and a byebye leave the socket; `MX`'s divergence is written down rather than left implicit | #202 (partial) |
> | ALAC was proven to *open*, never to decode | a lossless round trip at 0.999 correlation, using the encoder's own magic cookie — a hand-built one with the wrong `frame_length` produces no audio, which is the #189 failure exactly | #189 (partial) |
> | the UIBC back-channel's socket half had no test — #125 fixed without a regression test | a scripted source offering a port, the sink dialling it, and a touch arriving in the *source's* pixel space; removing the dial reproduces #125. Plus `wfd_uibc_setting: disable`, which used to be acknowledged and ignored | #193 |
> | the mixer's device-vanish test never let the device come back | a factory that refuses for two retry intervals and then opens, asserting sound returns, nothing was dropped, and the retries were paced | #204 (partial) |
>
> Two entries above are **partial and the issues stay open**: #199's `pump_events` and
> `run()` are the bulk of it, and #200 still wants the one LAN capture that would turn a pin
> into a validation. #194 keeps its first bullet — the MICE vendor extension turns out to be
> D-Bus-only in wpa_supplicant 2.11, which is recorded on the issue and changes what option 2
> on #206 costs.

This is a **record**, not a backlog. Where it names a gap, the gap belongs in an issue —
several already are, and those are cited. Nothing here should become a parallel tracker;
see the note in CLAUDE.md about `GAPS.md`.

Read it with `docs/STATUS.md` (what exists), `docs/DECISION-LOG.md` (why), and the
per-protocol records (`dlna-conformance.md`, `miracast-protocol-notes.md`,
`matter-casting-notes.md`, `gamestream-protocol-notes.md`, `airplay-research.md`).

**`docs/STATUS.md` is stale in four places found during this audit** and should be corrected:
its test count (§"Test count"), its Matter claim (#171 is closed; commissioning has run
against a peer), its AirPlay VM claim (`/pair-setup` answers 200 with a 32-byte key, not
`501`), and its GameStream open-item 3 (#33 is closed).

---

## 1. How to read this

### Coverage tiers

| Tier | Meaning |
|---|---|
| **T0** | Pure / sans-I/O unit test against fixtures. No socket, no device. |
| **T1** | Real sockets or a real device, in one process on localhost. |
| **T2** | Nix VM test — multiple nodes or a real kernel subsystem, over a real LAN or a real emulated radio. |
| **T3** | Differential: the peer or the oracle is somebody else's implementation. |
| **T4** | Hardware- or human-gated. Not automatable here. |
| **INHERITED** | Covered only by a dependency's own upstream test suite. Cargo does not run dependency tests, so this means "upstream asserted it at release", not "it runs in our CI". |
| **STRUCTURAL** | Untestable offline because the peer is a third party's cloud. |
| **NONE** | Untested. |

A tier says where the evidence comes from, not how strong it is. A T0 test against a
captured fixture from a real phone can be worth more than a T2 that greps a log line.

### What "end to end" has to mean

Four assertion strengths recur. They are not interchangeable, and most of this document is
about the distance between them:

- **(a) log grep** — the receiver said it did the thing.
- **(b) protocol response** — the peer got a well-formed answer.
- **(c) counter threshold** — N frames/packets/bytes crossed a boundary.
- **(d) signal content** — the decoded audio matches the waveform sent, or the composited
  pixels match the picture sent.

Only (d) distinguishes "the session negotiated and the panel is black" from "it works".

---

## 2. The harness

### The 16 checks

`nix flake check` runs **all 16** checks (23 before D55 collapsed the per-feature
ones; `moonlight-bindings` was added after, closing half of #191), on the one system
this flake now claims — see the structural note below.
There is no opt-in tier inside `checks` — nothing is gated behind an env var or a separate
invocation.

CI (`.github/workflows/test.yml`) is a fan-out of exactly these: a `discover` job runs
`nix eval` over `checks.x86_64-linux` and emits the attribute names as a job matrix, so a
check added to `flake.nix` becomes a CI job automatically and the two cannot drift. One
runner per check, `fail-fast: false`, on every push and PR. **The five VM tests do run in
CI on KVM on GitHub-hosted runners** — verified against run `30967773951` (2026-08-05,
green): `integration-vm` 75m, `bluetooth-vm` 75m, `matter-vm` 74m, `gamestream-vm` 71m,
`miracast-vm` 57m. Total ~2h21m.

So the question is never "did CI run it". It is **"is there a check for it"**.

| Check | Scope / features | Tier | Cost |
|---|---|---|---|
| `build` | the shipped artifact (`packages.default` — the Linux kiosk since D55) | — | ~70m |
| `test` | bare `cargo nextest run`, workspace, **default features** | T0/T1 | 81m |
| `coverage` | `cargoLlvmCov`, default features | — | 68m |
| `clippy` | `--all-targets --deny warnings`, default features | — | 68m |
| `fmt` | `cargoFmt` | — | 4m |
| `openscreen-rtp-fixtures` | 9 openscreen TUs; regenerates and byte-diffs the Cast RTP fixtures | **T3** | 2m |
| `openscreen-device-auth` | 16 openscreen TUs; their *sender* verifier judges 12 of our device-auth vectors | **T3** | 1m |
| `cast-app-hosting` | `-p proto-cast`, `-E 'binary(receiver_sdk) + binary(hosted_app_media)'`, real Electron + Google's pinned receiver SDK | **T3 at T1** | — |
| `integration-vm` | 2-node; receiver from **the real NixOS module** (`castaway-portable`, null pipeline) | T2 | 75m |
| `miracast-vm` | 1 node, 2 `mac80211_hwsim` radios + netns; **real NixOS module** | T2 | 57m |
| `matter-vm` | 2-node; panel from **the real NixOS module**; peer is our own `matter-peer` | T2 | 74m |
| `gamestream-vm` | 2-node; peer is **real nixpkgs `sunshine`**; neither node runs castaway or the module | **T3** | 71m |
| `bluetooth-vm` | 1 node, `hci_vhci` + btvirt; peer is **real BlueZ**; receiver launched ad-hoc, **not** via the module | T2 | 75m |
| `ldac-bindings` | regenerate `ldac-sys/src/bindings.rs` with bindgen and `diff -u` | T3 | 3m |
| `moonlight-bindings` | the same for `moonlight-sys/src/bindings.rs` against the pinned `Limelight.h` | T3 | <1m |
| `castaway-windows-electron-dll-closure` | cross-build + static import-table closure | T2 | 51–70m |

**Gone in D55**, and worth knowing where their coverage went, because the names appear
throughout the per-protocol matrices below: `audio`, `render-pixels`, `media-plane`,
`media-plane-clippy`, `hwaccel-clippy` and three of the four Windows closures were each a
fragment bolted on to reach past a default feature set that is no longer a subset of
anything. They collapsed into `test`, `clippy` and the single Windows closure, which now
build the shipped configuration. What each of them *asserted* is unchanged and still runs
— `render-pixels`' lavapipe adapter and `CASTAWAY_REQUIRE_GPU`, `audio`'s ffmpeg CLI and
`CASTAWAY_REQUIRE_FFMPEG`, and `media-plane`'s `dlna_media_plane` target are all inside
`test` now.

Both structural notes this section used to carry are **closed by #207**, and the second
one is why the first stopped mattering:

- `gamestream-vm` sat in the all-systems attrset above the `optionalAttrs isLinux` guard,
  so `checks.aarch64-darwin.gamestream-vm` would have tried a nixosTest on Darwin — and
  the comment for `openscreen-device-auth` had `gamestream-vm` inserted between it and the
  attribute it describes. Both fixed; the nixosTest is in the nixosTest block.
- **`systems` is now `[ "x86_64-linux" ]`.** It was `nix-systems/default`, and every
  Darwin check was *unbuildable* under the current nixpkgs pin. Forcing every attribute on
  the rest found the same shape one system over: `checks.aarch64-linux.cast-app-hosting`
  cannot evaluate either, because the vendored Electron is `platforms = [ "x86_64-linux" ]`
  by construction, as are the Widevine CDM and the MSVC sysroot. So three of the four
  systems in that list were claims nothing had ever built.

The gate that makes this stay closed already existed and simply had nothing to cover the
other systems: CI's `discover` job forces every check to a `drvPath`, which runs all of its
evaluation. With one system, every check attribute is also a matrix job, so an attribute
that throws on force fails a named job rather than nothing at all.

### Test count

`docs/STATUS.md` claims 1404 tests. Measured at `37db8a7`:

| Configuration | Before D55 | After D55 |
|---|---|---|
| `cargo nextest run` (default features) | 1835 listed / 1784 run | **2243 run** |
| Executed by *some* CI check | 2163 | **2243** |
| **Executed by no CI check** | **140** | **0** |

The remaining 63 skipped are the `#[ignore]`d hardware/network tests in §3.3 — tracked in
#183, and a deliberate skip rather than an invisible one.

---

## 3. Cross-cutting findings

These are the patterns. They matter more than any individual gap because each one produces
gaps faster than they can be closed by hand.

### 3.1 Feature-gate rot is endemic, not fixed — 140 tests never run  **[CLOSED — D55]**

Issue **#98** ("a test CI does not compile is a test that rots, and it rots green") is
**closed**, having fixed exactly two feature sets: `render` for one app test target
(`media-plane`) and `kiosk` for pipeline (`render-pixels`). The same mechanism now applies
unfixed to `stream`, `remote`, `electron`, `hwaccel`, `audio-pipewire`, and to `audio`
in `crates/app`.

Test files that **no check compiles**:

| File | Gate | Tests |
|---|---|---|
| `crates/pipeline/tests/media_url_av.rs` | `ffmpeg` + `render` | 10 |
| `crates/pipeline/tests/remote_negotiation.rs` | `remote` | 9 |
| `crates/pipeline/tests/output_stream.rs` | `stream` | 9 |
| `crates/pipeline/tests/browser_end_to_end.rs` | `electron` (+`#[ignore]`) | 5 |
| `crates/proto-gamestream/tests/links_moonlight.rs` | `stream` | 4 |
| `crates/pipeline/tests/hwaccel_zero_copy.rs` | `hwaccel` | 3 |
| `crates/pipeline/tests/mixer_real_device.rs` | `audio-pipewire` (+`#[ignore]`) | 2 |
| `crates/pipeline/tests/remote_browser.rs` | `remote`+`electron` (+`#[ignore]`) | 1 |
| `crates/pipeline/tests/filter_subscriptions.rs` | `electron` (+`#[ignore]`) | 1 |
| `crates/pipeline` lib units | `stream`(24), `hwaccel`(19), `visualizer`(11), `remote`(8), `render_pipeline`(6), `filterlists`(6), `ubo_scriptlets`(4), `adblock_engine`(3) | 81 |
| `crates/app` lib units | `bluetooth`(3, needs `audio`), `shell_nav`(5, needs `render`), `sponsorblock::actor`(3, needs `electron`) | 11 |
| `proto_gamestream::stream` unit | `stream` | 1 |

Three of these are load-bearing beyond their count:

- **`media_url_av.rs`** covers the single most-used path in the product — every protocol
  that casts a URL (DLNA, Cast, AirPlay) lands there. Nobody would notice it breaking.
- **`output_stream.rs`** is the *only* place anything we box is parsed by something that
  did not write it (libavformat), and the only numeric A/V-sync measurement in the crate.
- **`links_moonlight.rs`** is the only proof that GameStream's 15k lines of linked C link
  at all. `gamestream-vm` builds `gs-probe` **without** `--features stream`, so the
  streaming core is not compiled by any check either.

`crates/app`'s three `audio`-gated tests are the ones enforcing #14's invariant —
*never advertise a Bluetooth codec this build cannot decode*. Their own comment calls that
"the one nothing was checking". Nothing is still checking it.

### 3.2 A feature combination on `main` does not compile  **[CLOSED — #180]**

```
$ cargo check -p pipeline --features ffmpeg
error[E0425]: cannot find function `rescale_to_duration` in this scope
   --> crates/pipeline/src/ffmpeg_decode.rs:189:14
```

`rescale_to_duration` is defined at `ffmpeg_decode.rs:661` under `#[cfg(feature = "audio")]`
and called unconditionally from the **video** path at :189. Its doc comment says it is
"gated with the audio half rather than left as dead code in a video-only build" — which is
precisely the bug. Every `ffmpeg`-without-`audio` combination is broken: `ffmpeg`,
`render,ffmpeg`, `hwaccel`. CI misses it because `hwaccel-clippy` goes through
`castaway/hwaccel`, and `castaway`'s `hwaccel` pulls in `render` → `audio`.

Verified failing at `37db8a7`. This is §3.1's mechanism one combination over.

### 3.3 Tests that pass by skipping  **[CLOSED — #182]**

A skip reports `ok`. `CASTAWAY_REQUIRE_GPU` exists to convert one class of skip into a
failure and works where it is applied (`render-pixels`, `media-plane`) — but it is not
applied everywhere, and there is no equivalent for ffmpeg.

- **The `audio` check silently skips its ffmpeg decode tests.** `commonArgs` sets
  `strictDeps = true`, and `audioArgs` lists `ffmpeg_7` in `buildInputs` only — so the
  ffmpeg **CLI** is not on `PATH` and every test that shells out to make a clip takes its
  skip branch (`ffmpeg_decode.rs:1326`, `:1406`, `null.rs:329`, `audio_decode.rs:710`,
  `stream/aac.rs:394`). `media-plane`'s own comment explains this exact trap in three
  paragraphs; `audio` did not copy the fix. **One-line fix**: add `ffmpeg_7` to
  `audioArgs.nativeBuildInputs`.
- **The pixel tests are not tripwired.** `wgpu_compositor.rs` uses its own
  `compositor_or_skip!` macro and `nv12.rs` uses `.ok()?`/`eprintln!; return`, neither of
  which consults `CASTAWAY_REQUIRE_GPU`. The ten most pixel-specific tests in the crate
  would go green having drawn nothing if the lavapipe ICD path moved — the precise failure
  `test_gpu.rs`'s doc comment was written to end.
- **`output_stream.rs` skip-passes by design** — `publish()` returns `None` without a GPU
  or an H.264 encoder and the test `return`s green. So even enabling `stream` might assert
  nothing.
- **`device_auth_vectors.rs` skips itself** when `has_bundled_identity()` is false. CI is
  safe (the carve is in `commonArgs`); a developer's bare `cargo nextest run` reports green
  having compared nothing.

Five `#[ignore]`d files run nowhere, ever: `browser_end_to_end.rs`, `remote_browser.rs`,
`filter_subscriptions.rs`, `mixer_real_device.rs`, `cast-replay/tests/live_backend.rs`.

### 3.4 Fixes shipped with no regression test

Every one of these was a real, user-visible bug, found by hand, fixed, and closed with the
invariant guarded only by a comment:

| Issue | The bug | Guarded by |
|---|---|---|
| **#125** | Miracast UIBC negotiated end to end, sink never dialled the back-channel, panel touch went nowhere | nothing |
| **#127** | A TS re-key sent resolved time backwards | a PTS test; no PCR is read at all |
| **#133** | A stale `StreamSession` called `LiStopConnection` on the *next* session and wiped its sinks | a `GENERATION` counter, untested |
| **#134** | GameStream `audio_sample` blocking-sent under the `SESSION` lock, stalling video and wedging teardown | comment |
| **#135** | One unanswered Pair wedged the whole GameStream actor | comment |

The Cast proto2 `required`-field bug (Chromium 148 answered "Error parsing packet body." on
*every* connection) and the unanswered Cast `DeviceProber` (an iPhone discovered us,
connected, probed, and silently dropped the panel) were both found **by hand against real
software** and both survived a full green `nix flake check`.

### 3.5 Assertions weaker than their names

- `control-display`'s `power_on_frame_has_header_and_checksum` recomputes the XOR checksum
  **the same way `encode` does** — it cannot fail. Its sibling asserts a constant equals
  itself. Both are vacuous, and the opcodes are labelled placeholders. #21's second comment
  establishes the framing is the **wrong protocol family** entirely.
- `tap::a_tap_captures_what_was_actually_composited` uploads solid magenta, composites it,
  and asserts `png[1..4] == b"PNG"`. The magenta is never decoded. A black frame or a
  channel swap passes.
- `nowplaying_card`'s only pixel check is `assert_ne!(bare_px, playing_px)` — a difference,
  not an expected colour.
- `bluetooth-vm` is named `castaway-bluetooth-a2dp`, and its header and `flake.nix:826-831`
  both describe "a complete A2DP session … BlueZ as an independent A2DP source". **The
  testScript stops at inquiry discovery.** No AVDTP, no stream, no media packets, no audio.
  The PipeWire/`tester` block it configures is never used by a single line.
- `miracast-vm`'s comment claims "keyframe first" and STATUS.md claims "an IDR-first PES the
  pipeline counts as decoded-plane frames" — the keyframe flag is never checked and the
  plane is the *encoded* one.
- `matter-vm`'s comment claims the invoke "landed on the content-app endpoint rather than
  the bare player". It did not: `app=1` is `PLAYER_ENDPOINT`, content apps start at 6, and
  `matter-peer` defaults `--endpoint 1`.

### 3.6 No signal-level assertion exists in any VM test

Grading ~108 discrete assertions across the five VM tests:

| Strength | Count |
|---|---|
| (a) journal/log grep | ~50 |
| (b) protocol-level response | ~50 |
| (c) counter threshold | **1** (`miracast-vm`: `encoded video source ended frames=[1-9]`) |
| (d) decoded media content | **0** |

Signal-level verification does exist, but in the cargo tier, in exactly two places, both of
which are checks:

- `pipeline/tests/ldac_decode.rs` (check `audio`) — RMS over decoded PCM against an
  expected range, exact frame counts (`84 * 128`), per-channel levels, and `rms > 0.05` so
  "decoded to silence" fails.
- `app/tests/dlna_media_plane.rs` (check `media-plane`) — reads the composited surface back
  off lavapipe and asserts the best frame held ≥4 distinct bright colours. The only
  pixel-level assertion in CI, and it claims **video only** (the drainer counts
  `RenderCommand::Video`/`ClearVideo`; flake.nix:895 says so).

Any prose attributing signal-level verification to the VM tests is wrong.

### 3.7 Third-party peers are rarer than they look

Only two VM tests have a peer that is somebody else's program: `gamestream-vm` (real
Sunshine) and `bluetooth-vm` (real BlueZ). `integration-vm`, `matter-vm`, and
`miracast-vm`'s RTSP half all script the peer ourselves — which proves self-consistency
plus a second independent reading of the wire format, and nothing about what a real sender
does. The `openscreen-*` checks are the strongest evidence in the tree on this axis (they
run Chrome's actual code as the oracle) but are fixture-level, not session-level.

**No automated test anywhere uses a real Cast sender, a real AirPlay sender, a real Windows
Miracast source, a real Matter phone, or a real Spotify phone.**

---

## 4. Per-subsystem matrices

Each section lists modes, the tier and location of their coverage, and what is missing.
Exit criteria are marked **[A]** asserted today or **[P]** proposed.

### 4.1 Google Cast

| Mode | Tier | Where | Gap |
|---|---|---|---|
| mDNS `_googlecast._tcp` | T0+T2 | `actor.rs` advert tests; `vm-test.nix` avahi-browse | TXT never judged by a sender's parser; `ca` bits asserted present, not correct |
| TLS on 8009, 4-day cert window | T0+T2 | `actor.rs::tls_certificate_expires_inside_the_senders_four_day_limit`; VM handshake | no concurrent-connection or mid-session rotation test |
| Device auth CHALLENGE/RESPONSE | T0→**T3** | 12 vectors judged by openscreen's real `cast_auth_util.cc` (`openscreen-device-auth`) | **never happened over a socket**; the oracle rebuilds the envelope itself (`oracle.cc:62-68`); the VM sender has no `deviceauth` namespace |
| Both borrowed identities (CKS, AirServer) | **T3** | `cks-chain-google-roots` / `airserver-chain-google-roots` → `ok` vs shipped roots | live endpoints only `#[ignore]`d (`live_backend.rs`); CKS table expires 2027-12-06 with no alarm |
| CRL / revocation | T0 | `cast-replay/src/crl.rs` (8) | openscreen's `cast_crl.cc` **is compiled into the oracle** and never runs — no vector carries a `crl` field |
| proto2 envelope | T0 | `proto2_required.rs` (4), hand-decoded | validated against Chromium 148 **by hand**; not automated |
| Receiver ns, availability, declining launches | T0+T2 | `session.rs` (~10); VM asserts sender view **and** journal | best-covered mode in the subsystem |
| Device prober (`GET_DEVICE_INFO`) | T0 | `session.rs::a_device_prober_is_told_what_this_receiver_is` | derived from a **manual iPhone capture**; bytes not checked in; no automated prober |
| Media ns LOAD/PLAY/PAUSE/SEEK | T0+T2 | `session.rs` (~18); VM | **VM runs the null pipeline**; `SEEK` never over a socket; no Cast LOAD→pixel test |
| Hosted app + real media | **T3 at T1** | `receiver_sdk.rs`, `hosted_app_media.rs` (`cast-app-hosting`) vs Google's pinned SDK | CAF v3 return path not asserted; **`video.muted = true`**, so no audio sample is verified |
| Mirroring RTP + AES-CTR | **T3** | `openscreen_stream.rs` vs openscreen's own packetizer/`FrameCrypto` | strongest evidence in the subsystem; 6 frames, one codec, no loss burst |
| Mirroring OFFER/ANSWER | T0 | `mirror.rs` negotiation tests | **every OFFER is hand-written by us**; no captured Chrome offer; ANSWER never parsed by openscreen's `Answer` parser |
| Mirroring RTCP | T0+T1 | `rtcp.rs` (13); `mirror_udp.rs` | differential in **one direction only**; nothing openscreen wrote parses what we emit |
| Mirroring audio (Opus/AAC) | T0 | codec-name parsing only | **fixture is VP8 video only** (`audio: None`); nothing decodes a mirrored Opus frame |
| Mirroring → pixels | NONE in CI | `render_pipeline.rs::encoded_mirror_decodes_and_composites_pixels` exists | needs `render`+`ffmpeg`; no check enables both |
| Cast Connect / queue | NONE | — | not implemented and **not declared** as a non-goal; no test pins what `QUEUE_LOAD` returns |

**Exit criteria.** [A] all 12 device-auth vectors judged exactly as recorded; both chains
`ok` against shipped roots; 6 decrypted frames byte-identical to openscreen's plaintext in
any packet order; hosted app reaches `state == "playing"` **and** `currentTime > 0`;
`LAUNCH_ERROR/NOT_FOUND` asserted from the sender's seat *and* the journal.
[P] a real CHALLENGE over a socket in `integration-vm`, its reply fed to the same oracle;
a `crl` vector so the linked revocation code executes; an Opus stream added to
`gen_rtp_fixtures.cc`; a captured Chrome OFFER checked in and our ANSWER parsed by
openscreen; `cast_media_plane.rs` proving a Cast LOAD reaches a pixel.

**Worst gap:** no automated test uses a real Cast sender. Both defects that reached users
were found by hand. The CDP recipe (`Cast.enable` → `Cast.startTabMirroring`) exists only
as operator knowledge — `grep` finds no `cast_repro.py`, no puppeteer, no chromedriver.

### 4.2 AirPlay

| Mode | Tier | Where | Gap |
|---|---|---|---|
| Discovery `_airplay`/`_raop` | T0+**T2** | `advert.rs` (17); `vm-test.nix:677-714` | best-covered AirPlay mode — asserts `et=0,1` present, `et=0,3,5` absent, `pk` identical across both records and `/pair-setup` |
| `/info` plist | T0+T2 | `info.rs` (12); VM | the `{qualifier:["txtAirPlay"]}` request — an iOS sender's *first* — is T0 only |
| Pipelined / bare-path RTSP | T0+**T2** | `substrate-rtsp` (8); VM sends two requests in one `sendall` | — |
| Apple-Challenge auth | **T0 only** | `crypto-raop` (4); `session.rs` | **nothing verifies the signature against a real verifier**; a wrong padding scheme passes every test and fails iTunes |
| `/pair-verify` | T0 | `pairing.rs` (6) | **self-play only** — our client half against our server half |
| RAOP audio `et=0` | T0+**T1** | `tests/raop_session.rs` over real sockets | **the payload is the ASCII string `an ALAC frame..`**; nothing decodes it |
| RAOP `et=1` (RSA session key) | **T0 only** | `sdp.rs`, `audio.rs` | **advertised to the LAN and never driven over a socket**; the T1 session uses unencrypted SDP |
| FairPlay handshake | T0+T1 | `crypto-fairplay` (14) | four canned 142-byte replies are transcribed constants with **no upstream drift check**; `airplay-research.md` §6.2 specifies the check and it does not exist |
| FairPlay `ekey` unwrap | **T0 vectors**+T1 | `crypto-playfair/tests/vectors.rs` — 20 published triples, all four modes | never validated against an `ekey` a real iPhone produced |
| Mirroring H.264 | T0+**T1** | `raop_session.rs::a_mirroring_session_delivers_both_video_and_its_audio` | strongest test in the subsystem — real FairPlay vector, SHA-512 stream keys, keystream continuity across frames. **No decode**; payload is `an-access-unit-payload` |
| Mirror audio (AAC-ELD) | T0+**T1** | same test + `audio.rs` | no decode; no decoder-open assertion |
| AirPlay media session (`isMedia`) | T0+**T1** | `raop_session.rs::an_audio_only_session_starts_without_a_picture_to_belong_to` | captured from a real iPhone 2026-07-31; payload still never decoded |
| Pushed-URL video (`POST /play`) | **T0 only** | `video.rs` (13) | **~1000 lines with zero socket-level coverage**; #80 closed without an integration test |
| Volume / DMAP metadata / artwork / progress | **T0 only** | `control.rs`, `session.rs` | Cast's VM greps the journal for its volume change; AirPlay has no equivalent |
| FLUSH / seek | **T0 only** | `audio.rs`, `clock.rs`, `session.rs` | never over a socket |
| Timing / clock sync | **T0 only, and dead in the field** | `clock.rs` (7) | `session.rs:682` populates `sender_ports` **only from an RTSP `Transport` header**, so a plist-negotiated (mirroring) session never probes. **#176**: `clock_samples=0` on every live session |
| Resends | **T0 only** | `clock.rs`, `audio.rs` | the T1 test sends 4 in-order packets; no resend has ever left a socket |
| HEVC | **T0 only** | `advert.rs`, `mirror.rs` | `offer_hevc` is a config flag away from an untested wire format |
| `av_skew_ms` | **NONE** | `diagnostics.rs` asserts only sign and unset-ness | **no bound asserted at any tier**. **#79**: reads ~17 hours, advancing at 3.3× wall rate |

**Exit criteria.** [A] both services advertised on the port that answered with consistent
`et`/`cn`/`pk`/`pi`; 4 RTP packets in → 4 frames out with a 36-byte magic cookie; mirroring
keyframe flag, Annex-B start code, in-band SPS/PPS, and *second frame decrypts only if the
keystream was never restarted*; a real 20-byte sync packet anchors mirror audio.
[P] replace the placeholder payload with a real ALAC tone and assert the dominant FFT bin
is 1000 Hz ±2%; a T1 timing test binding the declared `timing_port` and asserting
`clock_samples >= 1` (**fails today for mirroring**); assert `|av_skew_ms| ≤ 40` and drift
< 5 ms/s in the existing mirroring test (**fails today at ~6×10⁷ ms**); `checks.pyatv-fairplay-tables`
per `airplay-research.md` §6.2.

**Worst gap:** `integration-vm` drives no AirPlay media at all — it stops at
`OPTIONS`/`/info`/`/pair-setup`/`TEARDOWN`. AirPlay is the only advertised protocol whose
media plane has never crossed a real LAN in CI.

### 4.3 Bluetooth audio

| Mode | Tier | Where | Gap |
|---|---|---|---|
| HCI socket transport | **T2** | `bluetooth-vm` journal greps | two log lines only |
| HCI USB / nusb | **T0** | `usb.rs` (6) | **no automated test ever opens a real dongle**; the stall-recovery panic fixed in `c77c87b` was invisible to CI by construction |
| Controller init (Intel, Realtek) | **T0** | `init/intel.rs` (16), `init/realtek.rs` (18) — the real UB500 firmware parses | never downloaded to a real chip. The socket transport **structurally cannot** exercise it (the kernel already initialised the part) |
| Bring-up → discoverable | T0+T1\*+**T2** | `host.rs`; `adapter_end_to_end.rs:441`; VM BlueZ inquiry | class-of-device not asserted (btvirt reports `0x000000`); EIR name never read back off the air |
| Pairing / bonding (Just Works) | T0+T1\* | `host.rs`, `adapter_end_to_end.rs:519` | **the VM never pairs** |
| Link key persistence | T0 | `app/src/bluetooth.rs` (3) | nothing tests a fresh process reading the key file |
| SDP advertisement | T0+T1\* | `substrate-sdp/src/record.rs` (9) | **never parsed by a third-party SDP client**; `server.rs` has *zero* inline tests; the VM has `sdptool` and never browses |
| L2CAP basic + ERTM | T1 (in-memory) | `substrate-l2cap/tests/handshake.rs` (~30), `ertm.rs` (18) | peer is our own mux. The only independent L2CAP evidence in CI is an Echo Request/Response pair in the btmon trace |
| ERTM vs the kernel | **T3, manual** | `examples/ertm_echo.rs` vs `l2test` | requires `--features bench`, `sudo`, hand-started btvirt. Not in any check |
| AVDTP full session | T0+T1\* | `sink_flow.rs` (~25) | no independent AVDTP source drives it in CI |
| Codec negotiation / fallback | T0 | `codec.rs` (13) | the three tests binding the table to what the build can decode are **`#[cfg(feature = "audio")]` in `crates/app` and run in no check** (§3.1) |
| **SBC decode** | T0, feature-gated | `audio_decode.rs::sbc_round_trips_…` | **asserts only `rms > 0.05`** — passes for a channel swap, phase inversion, wrong rate, or half the audio. Input is our own encoder's SBC |
| **AAC (LATM) decode** | **NONE** | `latm.rs` (4) covers framing against a real iPhone capture | **no decode test at all.** `the_codecs_we_advertise_are_the_codecs_we_can_decode` iterates `[Sbc, AptX, AptXHd]` — AAC is absent. This is the codec every iPhone picks |
| aptX decode | T0 | `audio_decode.rs::aptx_decodes_to_audio_that_sounds_like_what_went_in` | best signal assertion here — RMS window **plus** an L/R balance check. Still a self-encode→self-decode round trip |
| aptX HD decode | **T0 claim only** | `can_decode` returns true | no decode or level test. Advertised **second** |
| LDAC decode | T0 | `ldac_decode.rs` (7–11) — exact frame counts, RMS range, mono labelling | fixtures **generated by our own encoder**; LDAC stays opt-in for exactly this reason (#14) |
| AVRCP metadata / settings | T0+T1\*+real capture | `avrcp.rs` (29); `iphone_capture.rs` vs real iPhone bytes | one phone, one app, one moment |
| AVRCP transport / absolute volume | T0+T1\* | `avrcp.rs`, `adapter_end_to_end.rs:2695` | no independent controller ever receives one |
| Cover art (BIP/OBEX over ERTM) | T0+T1\* | `obex.rs` (23) | **never fetched from a real image server**; **#87** is a live field bug (art renders for some VLC-Android tracks, not others) with no reproducing test |
| Output to a real sound card | T0+**T4** | `mixer_real_device.rs` `#[ignore]`d | never runs. **#55** (panel sleep removes the HDMI sink) has no test |

\* **T1\*** = the whole async adapter driven end to end in one process against
`ScriptedTransport` — real state machines, real wire bytes, no socket and no independent
peer. Stronger than T0, weaker than a real T1. Much of this subsystem's confidence rests here.

**Exit criteria.** [A] bring-up command order byte-exact; Just Works yields a
`LinkKeyNotification` with no prompts; link loss returns every ACL credit; LDAC `84 * 128`
frames with RMS ∈ 0.35..0.65; real iPhone element-attributes yield title/artist/album **and**
a cover-art handle; absolute volume round-trips 0↔127 exactly.
[P] **per codec ∈ {SBC, AAC, aptX, aptX HD, LDAC}: a known waveform survives the whole
path** — feed A2DP packets into the real adapter, decode the emitted frames, and assert
normalised cross-correlation ≥ 0.99 at the best lag, lag within ±5 ms, **per channel
independently** (catches L/R swap and mono collapse); `sdptool browse` from BlueZ lists
Audio Sink + AVRCP CT/TG with PSM 25/23; ERTM against `l2test` in the VM; #55's exit —
with the display asleep, `frames_played` advances at ≥0.99× real time for 30 s.

**Worst gap:** `bluetooth-vm` does not test A2DP despite its name and both its comments
(§3.5). Everything above L2CAP is validated exclusively by our own code talking to our own
code. **Second:** nothing joins the protocol half to the decode half — `adapter_end_to_end.rs`
stops at an encoded `SourceMessage::Frame`, and the decode tests start at an `EncodedFrame`
built by hand. The depacketiser's output has never been handed to the decoder.

### 4.4 DLNA / UPnP

| Mode | Tier | Where | Gap |
|---|---|---|---|
| SSDP M-SEARCH | T0+**T2** | `message.rs` (5); VM fetches every `LOCATION` from the other host | **the body is fetched and never parsed** — `descriptions.rs:88` records that a `Bar & Grill` friendly name produced non-well-formed XML this exact assertion could not catch |
| SSDP alive/byebye | **T0 only** | `responder.rs` (1) | **never observed on a wire.** The VM never joins the multicast group and `notify_interval` is 900 s — a responder that sent nothing would pass |
| `MX` response delay | **NONE** | parsed and discarded | UDA 1.0 §1.2.3 asks for a randomised 0–`MX` delay. An unrecorded, untested divergence |
| Device description / SCPD | T0 (weak) | `descriptions.rs`; `state.rs::every_advertised_action_is_answered_and_every_answered_action_is_advertised` | the SCPD sweep is strong; but no well-formedness check, no hostile-name test, and nothing fetches an SCPD over HTTP |
| Transport walk + DIDL metadata | T0+**T2** | `state.rs`; `didl.rs` (10); VM journal-greps `NOW PLAYING.*Windowlicker` | every DIDL fixture is hand-written; `dlna-conformance.md` says captured blobs "do not exist yet" |
| Seek | **T0 only** | `state.rs` (2) | **never over HTTP, never against a real demuxer.** `dlna-ctl` has no `seek` command |
| RenderingControl volume/mute | **T0 only** | `state.rs`, `control.rs` | **no RCS action is ever sent over HTTP anywhere** |
| ConnectionManager default connection | **NONE** | reached by the SCPD sweep, which never inspects out-args | row 1 of the conformance doc's "do not fix" table. A refactor to `PeerConnectionID=0` breaks no test |
| `DMR-1.50`, no `M-DMR`, no `iconList`, namespace | **NONE** | — | four more "do not fix" rows with zero tests |
| Content-type / `protocolInfo` HEAD probe (#99) | T0+**T1** | `probe.rs` (7) incl. real-socket cases; `lib.rs` (3) | strong |
| GENA subscribe/renew/NOTIFY/`SEQ` | T0+**T1** | `gena.rs` (7+); `lib.rs` real callback listener | **publish-on-a-diff is not tested** — the doc names it explicitly; **position/duration excluded from `LastChange`** is asserted nowhere |
| Media plane → pixels | **T1** | `dlna_media_plane.rs` under `media-plane`, lavapipe | **video only** — the clip has an AAC track and nothing asserts a decoded sample |
| Real control point | **T4 / NONE** | — | no T3 anywhere. Bluetooth has BlueZ, GameStream has Sunshine, Cast has openscreen — DLNA, the protocol with the most conformance surface, has none |

**Exit criteria.** [A] transport walks `NO_MEDIA_PRESENT → PLAYING → PAUSED_PLAYBACK →
STOPPED` observed from another host with journal corroboration; sink advertises `video/*`
and `audio/*` and never `image/`; HEAD probe returns 714 for off-table types and 716 for
404, with 8 ambiguous combinations still playing; ≥10 frames composited and the best frame
holds ≥4 distinct bright colours.
[P] `xmllint`/`quick_xml` over the fetched `LOCATION` body, and a `Bar & Grill <TV>`
friendly-name test; a listener joined to 239.255.255.250:1900 seeing ≥2 alive bursts and a
byebye per target within 2 s of SIGTERM; `SetVolume 25` over HTTP followed by `GetVolume →
25` and a journal `CONTROL.*Volume`; a `conformance.rs` asserting the six "do not fix" rows
verbatim; N `GetTransportInfo` polls with no state change yield **zero** NOTIFYs.

### 4.5 DIAL / YouTube Lounge / SponsorBlock

| Mode | Tier | Where | Gap |
|---|---|---|---|
| DIAL SSDP discovery (positive) | **NONE** | `dial.rs` struct-level test only | **untested at every tier.** No VM runs a DIAL-enabled build; `yt-selfplay` takes a base URL and does no SSDP |
| Browser-less honesty (D27) | **T2** | VM asserts absence, 404s, and the log line | — |
| `dd.xml`, app state, launch, DELETE, CORS | **T1** | `dial.rs` (~8); `app/src/screen.rs` | DIAL **does** test XML escaping (`Test & Screen`) where DLNA does not |
| `<screenId>` attach-to-running-app | **T1** | `screen.rs`, `dial.rs` (3) | the real attach exists only in `yt-selfplay --reconnect` |
| Real screen resolver (`screen.rs::fetch`) | **NONE in CI** | `#[cfg(feature = "electron")]` | no check compiles it (§3.1) |
| Lounge bind-channel framing | T0 | `lounge/mod.rs` (3), `sender.rs` (4) | **#41: no golden transcript.** Every fixture is one we invented |
| `lounge::to_event` | T0 | (3) | **tested and unreachable** — no runtime caller. Inflates apparent DIAL coverage |
| SponsorBlock lookup / filter / plan | T0 | `sponsorblock` (~16), incl. a 9-test `Decision` machine | fixtures are ours, not captured API responses |
| SponsorBlock Lounge payload + clock | T0 | `app/src/sponsorblock/mod.rs` (8), against real captured payloads | these **do** run |
| SponsorBlock actor (long poll, UTF-8 boundaries, lookup HTTP) | **NONE in CI** | `actor.rs` `#[cfg(feature = "electron")]` | the three `decodable_prefix` tests exist because a desync is *permanent and silent*, and they have never executed |
| Real YouTube playback | **T4 opt-in** | `nix run .#yt-selfplay` | not a check; needs real internet. Last recorded run **2026-07-26 against the pre-D36 CEF build that no longer exists**. Oracle is the page's own `currentTime`, not pixels |
| `skip_ads` live press | **T4, never observed** | unit-tested against a captured ad payload | YouTube served an unskippable pre-roll during capture |

**Exit criteria.** [A] a browser-less build advertises nothing, mounts nothing and says why;
launch → 201 + `Location`, relaunch → 200; `OPTIONS` → 204 naming `DELETE`;
`yt-selfplay` asserts `currentTime` advances **on the video it queued** (not the state code)
for each of three taps; `--expect-skip` asserts a discontinuity — playback advancing further
than wall time did.
[P] a check that compiles `castaway` with `electron` at all; a VM subtest doing
`ssdp-search ssdp:all` and asserting two distinct root USNs (DLNA and DIAL); a captured
Lounge transcript checked in and replayed at every read-chunking (#41).

### 4.6 Miracast / WFD

| Mode | Tier | Where | Gap |
|---|---|---|---|
| WFD IE encode/decode | **T0** | `ie.rs` (17) — MiracleCast/lazycast/sigma-dut blobs round-trip | fixtures are transcribed hex, not captured. **No `.pcap` exists anywhere in the tree** |
| WFD IE on the air | **T2** | `miracast-vm`: `wpa_cli p2p_peer` shows the name and `1c44` (=7236) | only Device Information is checked on-air; the UIBC/Extended Capability bit is not |
| MICE WSC vendor extension (`0x1049`) | **T0 encode, NOT WIRED** | `mice.rs` reproduces the spec hexdump | **`vendor_extension()` has no non-test caller.** Per [MS-MICE] §3.1.3 Windows "MUST fall back to standard Miracast" without it. **#166 is closed with an empty body** |
| P2P group formation, WPS PBC | T0+**T2** | `p2p.rs` (13); VM subtests 1–3 | GO **negotiation** is deliberately absent (D35), so a source that insists on negotiating — Windows, intent 14 — is untested. The VM sender is wpa_supplicant talking to wpa_supplicant |
| DHCP lease | **T2** | real `udhcpc` takes a lease | **`EmitRouter=false`/`EmitDNS=false` never asserted** — "the phone loses its internet during the cast" would pass |
| Peer resolution by neighbour sweep | T0+**T2** | `backend_linux.rs` (6); VM deliberately never pings from the sender | sweep datagrams themselves never asserted |
| M1–M7 exchange | T0+T1+**T2** | `session.rs`, `params.rs` (19), `actor.rs`; VM uses the real Windows `Server:` header and coalesces M4+M5 in one segment | `microsoft_*` → `none` asserted for 2 of the 11 names §7.2 lists |
| M9 PAUSE / resume | **T0 only** | `session.rs` (1) | **resume is untested at every tier** |
| M13 IDR request | **T0 only** | `session.rs` (2) | **the trigger has never fired.** The rate limiter is untested and the VM source sends an IDR first *"so the sink never needs M13"*. This is WFD's only loss-recovery primitive and the whole justification for the hand-rolled demuxer (D35) |
| M16 keep-alive | **T0 only** | `session.rs` (2) | **no idle-session watchdog** — `;timeout=30` is parsed and discarded; a source that dies without FIN holds the session forever |
| MPEG2-TS demux | T0+**T2** | `ts.rs` (15); VM's Python source hand-rolls PAT/PMT/PES | fixtures synthetic; **no PCR is ever read** (#127's second half); the VM asserts a count but **not the keyframe flag** |
| RTP reorder / dup | **T0 only** | `media.rs` (4) | no loss, no jitter, no depth overflow. The VM medium is lossless |
| UIBC encode + coordinate mapping | **T0** | `uibc.rs` (22) — spec and lazycast golden bytes | excellent |
| UIBC back-channel (the socket half) | **NONE** | — | `actor.rs::open_uibc` — dial, timeout, writer task — has **no test at any tier**. Exactly the surface **#125** was filed about; the fix shipped with no regression test |
| HDCP | T0 advertise / NONE | `params.rs` | a source sending `HDCP2.1 port=N` in M4 is **ignored entirely** — we would proceed and decode ciphertext |
| Session availability withdrawal | **T0 encode, NOT WIRED** | `ie.rs::a_busy_sink_withdraws_itself_from_every_picker` | **`busy()` has no non-test caller.** A second source sees an available sink and joins a busy one |
| MICE mDNS `_display._tcp` | **NONE** | declared in `advertisements()` | no test constructs `with_mice()`; `miracast-vm` has zero MICE assertions **even though `infrastructure` defaults to `true` and the listener is running there** |
| MICE codec + PIN hash + DTLS refusal | **T0**+T1 | `mice.rs` (19) vs the spec's own hexdumps; `tests/mice_control.rs` (4) over real sockets | `mice_actor::{bind,serve,serve_one}` is **never invoked by any test** — the T1 test reimplements the handoff in miniature and says so |
| Concurrent STA uplink + P2P GO | **NONE / T4** | — | the VM's `wlan0` is associated to nothing; hwsim imposes no interface-combination limits |
| Driver capability check | **T4, not built** | — | needs a netlink dependency the workspace does not have (#17) |
| 5 GHz | **T4** | — | `CONFIG_CFG80211_REG_RELAX_NO_IR` is unset on the NixOS kernel, so NO-IR 5 GHz GO is refused. The VM pins 2437 MHz |
| Windows backend | **NONE** | — | §7.7 open; `main.rs:1623` refuses on non-Linux. The spike lives as a comment on #17 |

**What hwsim cannot catch, concretely** (this is the boundary #17 is about): no firmware and
no PHY, so `mt7921`'s `P2P-GROUP-FORMATION-FAILURE`, `brcmfmac`'s firmware gate, and
`EOPNOTSUPP` on `p2p-wlan0-0` are all invisible; no interface-combination limits; a
lossless, jitter-free, instantaneous medium, so the reorder buffer, CC-gap resync,
drop-late-frames and M13 have never been stressed; no regulatory domain, so the 5 GHz
blocker cannot surface; and wpa_supplicant on both ends, so the P2P/WPS half is one
codebase agreeing with itself.

**Exit criteria.** [A] a second radio lists us with the right name and control port; PBC
reaches `wpa_state=COMPLETED` and a real DHCP lease follows; M1→M7 with our unprompted M2
in the same read window; `client_port` equals the already-bound socket; clean triggered
teardown with no error log; `frames=[1-9]` completed access units out of the TS demux.
[P] the lease carries **no** router and **no** DNS option; the first delivered frame has
`keyframe == true`; PTS strictly non-decreasing across a counter restart and a 2³³ wrap;
**signal level** — the source encodes a real H.264 clip with a per-frame pattern and the
sink decodes it, asserting per-frame luma within tolerance; with 2% induced loss an M13
arrives within 1 s, at most once per second, and decoding recovers; a scripted source
advertising a UIBC port receives our dial and our touch frames.

### 4.7 Matter Casting

**#171 is closed** — `matter-vm` now runs UDC → passcode on the glass → a scripted human
reading it → `_matterc._udp` browse → PASE → `AddNOC` → CASE → one `LaunchURL` → `session:
play`. STATUS.md has not caught up.

| Mode | Tier | Where | Gap |
|---|---|---|---|
| `_matterd._udp` record shape | T0 | `discovery.rs`, `adapter.rs` | **never leaves the process**; the VM greps our own log line. `vm-test.nix` does `avahi-browse` for three other protocols and Matter is absent from that file entirely |
| UDC decode / encode | **T0** | `udc.rs` (6+) | `CHIP_IDENTIFICATION` is **hand-transcribed from reading connectedhomeip's C++**, not captured. No reference decoder has ever read our bytes |
| UDC framing rejection | T0+**T1** | `udc.rs`; `tests/udc_over_the_wire.rs` | survives six shapes of garbage and still answers the next real message |
| Passcode generation + stability | T0+T1+**T2** | `server.rs`; `udc_over_the_wire.rs::five_copies_produce_one_passcode` | found the bug that mattered — five retransmits producing five different numbers. The VM peer sends **one** declaration |
| Commissioning window / expiry | T0 | `server.rs` (2) | **the prompt is never taken down on expiry.** `expire` runs only when the next datagram arrives; `UdcServer::run` has no timer; the OSD message is sticky. A phone that walks away leaves an 8-digit passcode on a wall panel indefinitely |
| PASE / `AddNOC` / CASE | **T2** (ours) / **INHERITED** (core) | `matter-vm` journal asserts each stage, `node_id=4096` | `rs-matter`'s own `tests/{pase,case,...}.rs` are **not executed by anything in this repo**; and it is the same library on both sides of the VM |
| Our CA + persistence | T0 | `fabric.rs` (5) | file round-trip only. **Nothing restarts the panel** — `install_fabric` rebuilding an identical NOC and `seed_acls` re-admitting yesterday's phones are reasoned about and proven nowhere |
| ACL privilege (`Operate`, not `Administer`) | **T2 implicit** | the media invoke would fail otherwise | **the privilege level is asserted nowhere.** A regression to `ADMINISTER` passes every check |
| Endpoint tree / Descriptor | **NONE** | — | `src/node.rs` (766 lines) has **zero unit tests**; no descriptor is ever read |
| ContentLauncher `LaunchURL` | **T2 (endpoint 1)** + T0 | `matter-vm`; `player.rs` | the VM comment claims it landed on a content-app endpoint — **it did not** (§3.5). No Content App endpoint is ever invoked |
| MediaPlayback, TargetNavigator, ApplicationBasic, `LaunchContent` | **NONE** | resolution helpers only | every handler unexecuted |
| Other clusters (KeypadInput, ApplicationLauncher, OnOff, …) | **NONE — not implemented** | — | a code gap before a test gap (#172) |
| Session teardown | **NONE — no producer** | — | `CastCommand::End` is constructed nowhere; `MediaPlayback::Stop` maps to `ControlTxn::Stop`. The arm in `pump_commands` is dead code |
| Commissioning failure paths | **NONE** | — | `commission_loop`'s error arm logs and **sends no `CommissionerDeclaration` at all**; `CdError` 1–10 have no producer. A mistyped passcode gets silence |
| Re-cast after CASE session loss | **NONE — known broken** | — | **#173**: no `_matter._tcp` operational record, and the VM always takes rs-matter's session-reuse branch |
| Multi-client | **NONE** | — | `commission_loop` is serial and each attempt can block 60 s; a second phone's 180 s passcode can expire in the queue |

**A connectedhomeip peer is feasible, and cheaply.** nixpkgs ships `python-matter-server`,
which pulls **`home-assistant-chip-core`/`-clusters`** — prebuilt wheels of connectedhomeip's
own Python controller (the real C++ SDK core, its TLV codec, interaction model and cluster
objects), with no GN/pigweed build. It is a *commissioner*, and `rs-matter`'s
`open_basic_comm_window` is already used by `matter-peer`, so a test-only flag lets chip's
controller commission the panel and then read the Descriptor and invoke every media cluster
with the reference implementation's encoders. That is genuine T3 for the whole of the
untested cluster surface. Caveat: it inverts the role, so it judges our *node* half, not our
UDC/commissioner half — for that, one captured `IdentificationDeclaration` from
`tv-casting-app` retires the transcription risk permanently.

Note that unlike Cast's device auth, **nothing here is gated on a credential we lack**:
attestation is inverted (the client presents a DAC and we set `allow_test_attestation`), and
no cloud service is in the loop. The only honest T4 entries are a strict client's refusal of
a non-conformant endpoint, and a real phone's UDC retransmit timing and instance-name casing
(`instance_matches` is case-insensitive "on a guess, not a measurement").

### 4.8 GameStream / Moonlight

| Mode | Tier | Where | Gap |
|---|---|---|---|
| mDNS host discovery | **T0 only** | `discovery.rs` (3) — pure mapping | **no test browses `_nvstream._tcp`.** `gs-probe` takes an address on argv; the VM's two-node topology proves routing, not discovery. The adapter's whole browse loop is unexecuted |
| `/serverinfo` unpaired | T0+T1+**T3** | `nvhttp.rs`; `pairing_over_http.rs`; `gamestream-vm` | — |
| `/serverinfo` paired over TLS | **T3** | `gamestream-vm` | the 401-fallback path (host forgot our cert) is **untested at every tier** |
| Gen-7 pairing, phases 1–4 | **T0 golden**+T1+**T3** | `pairing.rs` vs **Sunshine's own vectors**; `gamestream-vm` pairs with the real binary | strongest-covered mode in the subsystem |
| Phase 5, wrong PIN, forged signature, 200-with-refusal | T0+T1(+T3) | `pairing.rs`, `pairing_over_http.rs` | wrong PIN never tried against real Sunshine |
| Certificate persistence | T0+**T3** | `adapter.rs` (3); VM re-runs with the same `--state-dir` | genuinely well covered |
| Mutual-TLS pinning | **T3 positive only** | implicit in the VM | **`http.rs` has zero unit tests.** No test presents a *different* certificate and asserts rejection — the negative branch of the only security boundary protecting a paired session |
| `/applist` | T0+**T3** | `nvhttp.rs` (5); VM regex | the request builder has no unit test |
| `/launch` | T0+**T3 partial** | `nvhttp.rs` (3); VM subtest 3 | **the VM has never seen a successful `/launch`.** The assertion is `("sessionUrl0=" in log) or ("launch refused" in log)` plus no `(400)` — a 503 passes, and headless it is always the 503 branch |
| `/resume` | **T0 only** | `nvhttp.rs` (1) | the VM host is always idle |
| `/cancel` | **NONE** | — | no unit test, no socket test, unreachable in the VM (only called after a successful launch) |
| App chooser (D38) | T0 screens only | `shell_nav.rs` (5) | navigation never talks to any host; and these are `render`-gated (§3.1) |
| GFE fork | T0+T3 (Sunshine side) | `nvhttp.rs`; VM `sunshine=true` | **T4/NONE for GFE, permanently** — discontinued NVIDIA software needing a Windows host. Worth recording in notes §6 as a permanent T4, not a backlog item |
| moonlight-sys link / ABI | **NONE in CI** | `links_moonlight.rs` is `#![cfg(feature = "stream")]` | never compiled (§3.1). The bindgen size/offset asserts are **self-referential** — bindgen generated both struct and assertion from the same header, so they catch a hand-edit, never upstream drift. And `lib.rs:10`/`bindings.rs:5` both reference a **`moonlight-bindings` flake check that does not exist** |
| RTSP, ENet, video+FEC, Opus audio, A/V pacing, input, teardown | **NONE** | — | linked C, never executed. **FEC recovery under loss and A/V pacing under jitter are the two properties D37 names as the reason for linking**, and nothing induces loss anywhere in this tree. Input is not implemented at all (**#167**) |

**Extending the Sunshine VM past `/launch` is tractable and is the highest-value work here.**
Three pieces: (a) give the host VM a virtual display (Xvfb dummy or `vkms`) with
`encoder = software`, `capture = x11` — the 503 is Sunshine's encoder/display probe;
verify nixpkgs' Sunshine carries an x264-enabled ffmpeg; (b) `gs-probe` is built with
`commonArgs`, i.e. **default features, so it physically cannot stream** — it needs
`--features stream`, `MOONLIGHT_COMMON_C_LIB_DIR`, and a `--stream <secs>` flag printing
`frames=N idr=N audio_packets=N first_frame_ms=N max_av_skew_ms=N` as a greppable sentinel;
(c) assert on those counters, then add `tc qdisc … netem loss 5%` and assert the frame count
holds — that is the FEC criterion, in a VM, with no hardware. ~1 day happy path, ~1 more for
the loss matrix.

**Licensing consequence, to decide explicitly.** moonlight-common-c is GPL-3.0 against an
MIT workspace. Any such check produces a GPL-encumbered artifact in CI. That does **not**
block CI — a check output is a build artifact, not a distribution, and the project is n=1
and private. It does mean the check and a streaming `gs-probe` must not fold into
`castaway-portable`, and that `packages.gs-probe` (user-facing) would change licence status
if the feature were turned on there. Cleanest split: a separate `packages.gs-probe-stream`,
with the licence stated at both sites — the same quarantine `crypto-playfair` gets.

**#33 is closed**, so the largest untested surface in the subsystem — everything between
"the host answered `/launch`" and pixels — is currently **tracked nowhere**. #167 covers
input only.

### 4.9 Spotify Connect

The D30 split matters for reading this: the local surface is ours; everything above the LAN
is librespot's, and its peer is Spotify's cloud.

| Mode | Tier | Where | Gap |
|---|---|---|---|
| mDNS advertisement | T0+T2 | `lib.rs`; VM `avahi-browse` | neither asserts `VERSION`/`Stack`, the **port**, or the `#spotify` suffix — Cast and AirPlay get port assertions in the same script |
| `getInfo` | T0+T1+T2 | `discovery.rs`, `lib.rs`, `tests/pairing.rs`, VM | **the field set has never been checked against a real client**; `status:101`, `deviceID`, `version`, `tokenType` are emitted and asserted by nothing. Same failure shape as #48 and **nobody has filed it** |
| `addUser` DH | T0 | `crypto.rs` (2), `discovery.rs`, `pairing.rs` | no real-client `clientKey` seen; left-padding of a short key (the classic interop bug) exercised only by generated keys |
| Blob outer decrypt | T0 | `crypto.rs` (3), `pairing.rs` (2) | **the core of #48.** Every positive assertion is our encoder against our decoder — if the real framing were `iv‖hmac‖ct` these all still pass |
| Inner blob → librespot `Credentials` | **T3** | `crypto.rs::our_inner_blob_is_one_librespot_can_read` | strongest evidence in the crate: librespot's own decoder reads our bytes. Still no captured `addUser` |
| Hostile blob | T0 | `session.rs` (3) | the one part of #48 genuinely closed |
| `activeUser` / device claim / steal | T1+T0 | `pairing.rs` (3), `session.rs` | that a *failed login* clears `activeUser` is asserted nowhere |
| No account on disk | **NONE** | — | a headline property resting on one `None` argument. Trivially closable in the VM |
| AP login, dealer, connect-state, CDN audio | **STRUCTURAL / T4** | `examples/selfplay.rs` — **never run end to end** | needs one browser OAuth visit (Spotify has no device-code flow; automating it hits reCAPTCHA Enterprise) |
| PCM sink | T0 | `sink.rs` (4) | well tested, incl. the std-vs-tokio-channel bug that killed the first real session |
| Preemption / reattach | T0 | `sink.rs` (3) | the runner side (`serve_reattach`) and its coalescing loop are untested |
| Session reopen republishes | T0 | `session.rs` (3) | uses a `StubRemote` with `ControlCapabilities::NONE`, so Spotify's real capability set is never proven to reach the panel |
| **`pump_events` (phone → panel)** | **NONE** | — | **~175 lines with no test at all**, carrying ≥4 documented past regressions (position ticks blanking the card; `SessionDisconnected` falling into the wildcard; artwork pasted onto a successor track). Every phone-side transport action lands here. **Not a cloud gap** — `PlayerEvent` is plainly constructible |
| **`run()` reconnect policy** | **NONE** | — | **~200 lines with zero tests**, and it exists precisely because librespot 0.8 does *not* reconnect — the one thing D30 says delegation cannot cover. Backoff, `HEALTHY_SESSION` reset, give-up-and-clear-`activeUser`, deliberate-vs-dropped, pairing-interrupts-backoff: all unasserted |
| Panel → `Spirc` dispatch | T0 caps only | `control.rs` (3) | the dispatch `match` is untested — nothing asserts `Stop → disconnect(true)` rather than `pause()` |
| Repeat / shuffle / volume | T0 outbound | `control.rs` (3) | repeat is the best-tested control path (all 3×3 transitions, never both flags set). **Inbound volume is not handled at all** — no `PlayerEvent` arm, so the phone's volume never reaches the panel |
| Queue (`UpNext`) | T0 | `session.rs` (11) + `core` (2) | **#49: the metadata key names are guesses** and every test feeds keys we chose |
| Cover art, `apply_track` | **NONE** | — | `best_cover` and the three `UniqueFields` arms are pure, trivial, and untested |
| Capabilities → transport strip | **NONE (composition)** | both halves asserted separately | nothing feeds `SpotifyRemote::capabilities()` into `TransportModel::from_now_playing`. DLNA's equivalent join *is* asserted in the VM |

**Structural vs closable.** AP login, dealer, connect-state, CDN audio and cloud-reflected
transport are genuinely untestable offline — reaching the AP means implementing Shannon,
login5 and the dealer, which is the reimplementation D30 refuses. The proxy is
`examples/selfplay.rs`, which is written and working by construction and blocked on one
human browser visit plus a repository secret. Packaging it as `packages.spotify-selfplay`
beside `yt-selfplay` and running it **nightly, not per-commit** is the single highest-value
action for the cloud half. But "it's the cloud" does not cover `pump_events`, `run()`,
`apply_track`, `best_cover`, the dispatch match, or a captured `addUser` fixture — all of
which are local and closable today.

### 4.10 Media pipeline and presentation

The shared destination every protocol feeds. Measured test counts: `-p pipeline` default
**181**, `+audio,ldac` **259**, `+kiosk` **429**, union of all CI **507**, compilable **622**.

| Mode | Tier | Where | Gap |
|---|---|---|---|
| ffmpeg software decode | T1, **skips** | `ffmpeg_decode::tests` under `audio` | silently skips for want of the ffmpeg CLI (§3.3) |
| URL session: both streams, pacing, seek | **NEVER RUNS** | `tests/media_url_av.rs` (10) | needs `ffmpeg`+`render`; no check has both (§3.1) |
| Late-frame dropping | T0 | `clock.rs` (2) | runs everywhere |
| Two-lane render channel | T0 | `tests/render_channel.rs` (3) | runs in `render-pixels` |
| wgpu compositor pixels | T1 | `wgpu_compositor::tests` (5) — exact RGBA at sampled coords | **untripwired skip** (§3.3); only sampled coordinates, never a whole frame |
| Corner radius, crop-vs-stretch | T1 | `wgpu_compositor::tests` (2) | same untripwired skip; one radius value |
| Panel model / focus / motion | **T0, strong** | `panel::tests` (17), `motion::tests` (13), + 4 integration files | all 4×4×2 motion steps settle <1 s and land exactly on `CORNER`; back always terminates |
| Shell navigation, transitions, gestures | T1 | 4 files (~27) | runs in `render-pixels`; closes D38's "cancel plumbing has never been sent" |
| Winit kiosk window | **T0 only** | `kiosk::tests` (20) with `window: None` | **no test opens a window.** D46: the edge-drag bug "turned out never to have worked… in the one file with no test harness" |
| Transport strip layout / hit test / capabilities / scrub | **T0, strong** | `transport::tests` (20), `projection::tests` (13) | best-covered surface in the crate; runs by default |
| Now-playing card | T0 + weak raster | `nowplaying_card::tests` (21) | only pixel check is `assert_ne!` (§3.5) |
| **Mixer summation & gain** | **T0 signal** | `mixer::tests` (26) — sample-exact `0.25+0.5 → 0.75`, clipping, mute, `tapped == heard` | genuinely signal-level; the strongest audio coverage in the tree |
| Mixer pacing / `OUTPUT_LEAD` | T0 wall-clock | `mixer::tests` (5) | asserted against a fake whose `frames_played` is **wall-clock derived — correct by construction**. #174/#175/#177 are all live regressions here, found by a human listening |
| Device-vanish retry (#55) | T0, **half** | `a_box_with_no_device_still_drains_its_sources_in_real_time` | the factory refuses *forever*; **the recovery half has no test**, and #55 asks for exactly that |
| LDAC / aptX / SBC / ALAC decode | **T0 signal** | `ldac_decode.rs` (11), `audio_decode.rs` (10) | see §4.3 for the per-codec weaknesses |
| Real audio device | **T4** | `mixer_real_device.rs` `#[ignore]`d | never runs |
| cpal / ALSA / WASAPI backend | **NONE in CI** | — | `audio-out` is **not even compiled** by `nix flake check` on Linux; only the Windows DLL-closure check touches it |
| `/screenshot.png` | T1, content untested | `tap::tests` (4) | asserts three bytes (§3.5) |
| fMP4/HLS boxing | **T0, runs by default** | `fmp4`(16), `hls`(8), `cadence`(7), `timeline`(5), `feed`(8) | self-consistency against the test module's own naive `find_box`. **`hls.rs` never calls `push_audio`** |
| fMP4/HLS vs libavformat | **NEVER RUNS + skip-passes** | `tests/output_stream.rs` (9) | the only reference-parse and the only numeric A/V-sync measurement, dark on both counts |
| RGBA→NV12 GPU pass | T1 numeric | `nv12::gpu` (5), ±2/255 | untripwired skip; a pin, not a differential |
| H.264 encoder, AAC track, stream audio window | **NEVER RUNS** | `encoder`(6), `aac`(4), `audio`(9) | all `stream`-gated; and each self-skips |
| WebRTC `/remote/` | **NEVER RUNS** | `remote_negotiation.rs` (9) + units (8) | `remote`-gated |
| WebRTC fan-out / touch back | T0, runs | `feed::tests` (8), `input_touch::wire` (10), `remote::tests` (11) | strong; note two `feed` tests assert only "no panic" |
| Real browser drives `/remote/` | **T4** | `remote_browser.rs` `#[ignore]` | STATUS.md calls it "the test that matters" |
| Electron browser host | **NEVER RUNS** | `browser_end_to_end.rs` (5) | `electron`-gated **and** `#[ignore]`d |
| Adblock / scriptlets | **NEVER RUNS** | 14 tests | `electron`-gated |
| Hardware decode | **NEVER RUNS** | `hwaccel_zero_copy.rs` (3) + units (19) | `hwaccel-clippy` is clippy-only, by design. D3D11VA/DX12 (943 lines) has **no tests at all** (#58, #64) |
| **`control-display`** | **NONE (vacuous)** | 3 tests | §3.5. Placeholder opcodes, **wrong protocol family** per #21, `serial`/`ddc` are empty feature lists, `app` builds `NullDisplay` unconditionally |
| **`input-touch` evdev / winuser** | **NONE** | — | both are **empty feature lists with zero lines of code**; all touch arrives via winit. The C6522QT's USB-HID touch has never been met (#65) |
| Idle-CPU / A/V-sync numbers | **T4 prose** | D49/D50/STATUS.md | "0 jiffies", "5.4% of a core", "10.000 s / 10.005 s" are one-off dev-box measurements. **No `benches/`, no criterion, no `/proc/self/stat` read anywhere** |
| Full-feature Linux artifact | **NONE** | `nix/linux-kiosk.nix:119` sets `doCheck = false` | it is a *package*, not a check — the full feature union is never test-run |

**Nothing in the render path compares a whole frame.** Every pixel assertion is a handful of
sampled coordinates, a centre-pixel ±2 check, an alpha probe, or a liveness check
(`any(p[0] > 8)`, `len == w*h*4`, `png[1..4] == b"PNG"`). There is no golden image, no hash,
no whole-frame comparison anywhere. "The layout is subtly wrong", "the text did not render",
"the theme is inverted" all pass. D38 defers a font change to avoid invalidating "the
golden-image tests" — **those tests do not exist**. The `examples/{shell,card,attract,screen}_preview`
binaries already produce exactly these PNGs for a human to look at; promoting them to
assertions is mostly wiring.

---

## 5. Consolidated gap ranking

Ranked by expected cost of the failure it hides, weighted by how cheaply it closes. Every row
was filed on 2026-08-05; where an open issue already covered the gap, the finding was added
there as a comment rather than duplicated.

| # | Gap | Cheapest close | Issue |
|---|---|---|---|
| 1 | **`-p pipeline --features ffmpeg` does not compile on `main`** | move `rescale_to_duration` out of the `audio` cfg | **#180** |
| 2 | **140 tests never compiled by any check**, incl. the whole stream/HLS, WebRTC, hwaccel, adblock and `media_url_av` sets | 3 new checks: `stream-plane`, `remote-plane`, `browser-plane`; widen `audio` to `+render`; add `ffmpeg` to `render-pixels` | **#181** (#98 recurring) |
| 3 | **No automated test uses a real Cast sender** — both defects that reached users were found by hand | nixosTest with headless chromium over CDP; the recipe exists | **#184** |
| 4 | **`bluetooth-vm` does not test A2DP** despite its name and comments | `bluetoothctl pair/connect` + `paplay` + btmon assertions + cross-correlate decoded PCM | **#186** |
| 5 | **AirPlay media never crosses a LAN in CI** | package `cliraop` into the sender node; `airplay-research.md` §6.1 specifies it | **#188** |
| 6 | **GameStream: nothing between `/launch` and pixels has ever run**, incl. FEC and A/V pacing — D37's stated reasons for linking | virtual display on the Sunshine host + a streaming `gs-probe` + netem | **#190** (was untracked since #33 closed) |
| 7 | **No signal-level assertion in any VM test**; the two that exist are cargo-tier | per-codec cross-correlation; per-frame luma for Miracast; decode AirPlay payloads | folded into #186, #189, #190, #192 |
| 8 | **AAC has no decode test** — the codec every iPhone picks, advertised 4th | extend `audio_decode.rs`'s codec list; the fixture already exists | **#187** (+ comment on #14) |
| 9 | **AirPlay timing is dead code in the field** — mirroring sessions never probe | a T1 test binding the declared `timing_port` | comment on **#176** |
| 10 | **`av_skew_ms` has no asserted bound at any tier**, and reads 17 hours | assert a bound in the existing mirroring T1 test | comment on **#79** |
| 11 | **Spotify `pump_events` and `run()` — ~375 lines, zero tests**, both local and closable | scripted `PlayerEvent` vectors; injectable starter + `tokio::time::pause()` | **#199** |
| 12 | **Miracast M13 has never fired** — WFD's only loss recovery, and D35's justification | induce loss in the hwsim medium | **#192** |
| 13 | **Miracast UIBC socket half untested** — exactly #125's shape, fixed with no regression test | scripted source offering a UIBC port | **#193** |
| 14 | **MICE vendor extension not wired**, so Windows falls back to the P2P path MICE exists to escape | install the WSC extension; assert it in the probe response | **#194** (#166 closed empty) |
| 15 | **Matter: endpoint tree + every cluster but one unexecuted**; `node.rs` has zero tests | extend `matter-peer`; then a `home-assistant-chip-core` peer for real T3 | **#196** (+ comment on #172) |
| 16 | **`control-display` is vacuous over a wrong-shaped encoder** | correct the frame shape first, then a TCP fake panel on 4661 | comment on **#21** |
| 17 | **Mixer real-time guarantees rest on a wall-clock-derived fake**; #174/#175/#177 all live here | dummy PipeWire sink in a VM so `mixer_real_device.rs` runs | **#204** |
| 18 | **#55's recovery half untested** — the failing-then-succeeding device | a 30-line test | comment on **#55** |
| 19 | **No whole-frame render comparison anywhere** | golden PNGs, mean abs error ≤2/255, diff on failure | **#203** |
| 20 | **Silent skips**: `audio`'s ffmpeg CLI, the untripwired pixel tests, `output_stream`'s skip-pass | `nativeBuildInputs`; route through `test_gpu`; a `CASTAWAY_REQUIRE_FFMPEG` tripwire | **#182 — closed** |
| 21 | **DLNA's six "do not fix" conformance rows have no test** | a `conformance.rs` quoting the citations; ~40 lines | **#201 — closed** |
| 22 | **No DLNA/DIAL third-party control point** anywhere | `gupnp-tools` in the sender VM; ~30 lines of Nix | **#202** |
| 23 | **GameStream mutual-TLS verifier has no negative test** — the only boundary protecting a paired session | ten lines | **#191 — closed** |
| 24 | **moonlight-sys ABI guard documented but absent** (`moonlight-bindings` check does not exist) | port `nix/ldac-bindings.nix` | **#191 — closed** |
| 25 | **`#[ignore]`d tests run nowhere, ever** (5 files) | decide per file: make runnable, or state the hardware gate in notes | **#183** |

Filed alongside these, from findings that were not test gaps but code gaps the audit tripped
over: **#185** (Cast mirroring audio untested, RTCP differential one-directional),
**#189** (nothing decodes AirPlay media), **#195** (Miracast has no idle-session watchdog, and
a source offering HDCP is ignored), **#197** (the Matter passcode prompt is never taken down
on expiry), **#198** (a failed Matter commissioning sends no `CommissionerDeclaration`),
**#200** (Spotify `getInfo`'s field set never validated), **#205** (the idle-CPU and A/V-sync
numbers are prose, not gates), **#206** (the deploy target has no proven Miracast path).

Comments were added to **#14**, **#17**, **#21**, **#40**, **#41**, **#48**, **#49**, **#55**,
**#58**, **#65**, **#79**, **#87**, **#172** and **#176** rather than filing duplicates.

---

## 6. Proposed checks  **[superseded by D55]**

This section proposed three new checks (`stream-plane`, `remote-plane`, `browser-plane`) plus
widening two more, to reach the feature sets the default build could not. D55 removed the
premise: there is no feature set the default build cannot reach, so `checks.test` covers all
of it and five checks went away instead of three arriving.

What is left, and is *not* a feature question:

- **VM work.** An A2DP session in `bluetooth-vm` (#186); a real `cliraop` AirPlay session in
  `integration-vm` (#188); a streaming `gs-probe` against a software-encoding Sunshine
  (#190); a MICE subtest in `miracast-vm`, where the listener already runs (#194); a
  `chip`-controller peer for `matter-vm` (#196); a `gupnp-tools` control point (#202); a
  headless-chromium Cast sender (#184).
- **Two nightly jobs**, because their peers are third-party clouds: `yt-selfplay` both modes
  against a browser build, and `spotify-selfplay` once the one-time OAuth visit is done.
- **A dummy audio sink in a VM**, so `mixer_real_device.rs` stops being `#[ignore]`d and the
  mixer's guarantees stop resting on a wall-clock-derived fake (#204).
