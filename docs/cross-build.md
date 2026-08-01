# Cross-build: dev on Linux → target Windows

**Setup:** dev box is Linux (NixOS), deploy target is a Windows box wired to the Dell C6522QT.
The design is ~90% portable Rust; the cross-build pain is contained to the same Windows-specific
~10% (WinRT Miracast, DX12/D3D11 interop).

**Status:** the cross-build is real and lands from Nix. `nix build .#castaway-windows-electron`
produces a directory that can be copied to the Windows box and run; append `.archive` to any
Windows artifact for the same tree as a single zip. Nothing here needs a Windows machine or
Windows CI to *build*; the physical box is still the only place to *test* the hardware paths.

> Historical note: this document used to describe a CEF-based deploy artifact
> (`.#castaway-windows-cef`, CEF's C++ wrapper cross-compiled with clang-cl and its runtime
> flattened beside our exe). D36 replaced the browser layer with a prebuilt Electron (castLabs
> ECS) subprocess on both platforms; the CEF-specific staging and CMake machinery is gone, and
> what it taught us is folded into "the things that bite" below.

## Golden rule: don't gate daily dev on the cross-build

The portable crates **build and run natively on the Linux dev box** — AirPlay, Cast (incl.
desktop-mirroring), DLNA, Spotify, Lounge, the wgpu compositor (Vulkan), ffmpeg decode (VAAPI),
and the browser subprocess, because D36 pins the *same* ECS build on both platforms. Do daily
dev/test here. Cross-build enters only for the Windows slice, and running it also continuously
exercises your *future* Linux target.

| Component | Build on Linux? | Test on Linux? | Notes |
|---|---|---|---|
| Portable protocols (AirPlay/Cast/DLNA/Spotify/Lounge) | ✅ native | ✅ native | the daily loop |
| wgpu compositor | ✅ native (Vulkan) | ✅ native | DX12 path + D3D11 interop → Windows only |
| ffmpeg decode | ✅ native (VAAPI) | ✅ native | Windows target links vendored libav import libs |
| `windows` crate / Miracast `backend-windows` | ✅ cross (pure Rust bindings) | ❌ | deploy to Windows box; Wine won't do WinRT/Miracast |
| Electron browser (castLabs ECS) | ✅ prebuilt, staged as-is | ✅ native, same ECS version | separate process — nothing to cross-compile (D36) |

## Artifacts

| Output | Features | What it's for |
|---|---|---|
| `.#castaway-windows` | none | toolchain canary — if it stops linking, the toolchain broke, not the media stack |
| `.#castaway-windows-render` | `render` | DX12 compositor + kiosk, no browser; bisect render problems without the browser runtime in the way |
| `.#castaway-windows-hwaccel` | `hwaccel` | the D3D11VA → shared-NV12 → D3D12 decode bridge. Exists as its own artifact because it is the one part of Q20 Linux cannot exercise: the VA-API half has an offscreen readback test, this half has only the compiler until it reaches the Dell |
| `.#castaway-windows-electron` | `electron` | the deploy artifact: render + hwaccel + the Electron browser subprocess, with the ECS distribution, our host app, and the Widevine CDM staged |
| `.#msvc-sysroot` | — | the MSVC CRT + Windows SDK sysroot, built and cached independently |

Every Windows artifact also carries an `archive` passthru:
`nix build .#castaway-windows-electron.archive` yields
`result/castaway-windows-electron.zip` — the `bin/` tree as one zip, unzipping to a single
folder on the box. Same content as the directory artifact, in the shape a USB stick or a
remote-desktop clipboard wants.

For an incremental loop, `nix develop .#windows` exports the whole cross environment (including
`CARGO_BUILD_TARGET`), so plain `cargo build` cross-compiles. It's a **separate shell from
`default` on purpose** — exporting `CARGO_BUILD_TARGET` into the default shell would silently
hijack the native dev loop.

## Deploying to the box

The archive is the *artifact*; getting it onto the panel and running is `nix run
.#deploy-windows` (`nix/deploy-windows.nix`). The box address is not in the repo — it is one
machine on one LAN — so both scripts read `CASTAWAY_WINDOWS_HOST=user@address`.

```
export CASTAWAY_WINDOWS_HOST=user@panel
nix run .#windows-firewall            # once per box; --close takes it down again
nix run .#deploy-windows              # build → wipe → copy → verify → launch → stream the log
nix run .#deploy-windows -- --force   # re-copy even if the box already has these bits
nix run .#deploy-windows -- --no-launch castaway-windows-render
```

`deploy-windows` is built around the assumption that **a stale tree that looks deployed is the
expensive failure**: you change something, the copy silently doesn't land, and you spend the
session debugging the old binary against the new source. So every step that could leave the box
half-updated is checked rather than assumed — `rmdir` is followed by asking whether the
directory is still there (it reports success while leaving a locked tree behind), the
transferred zip is hashed on the box against the local one, and the extracted `castaway.exe` is
hashed against the one in the store. Only after all of that does it write
`.deployed-sha256`, which is what lets the *next* run skip the 235 MB copy — a half-finished
deploy leaves no stamp, so the fast path cannot inherit a lie.

Three things about the box drove the design, all measured rather than assumed:

- **An SSH login lands in session 0.** Start `castaway.exe` from `ssh` and it runs on the
  services desktop: no window anywhere, and from the Linux side it looks exactly like a
  successful launch. The panel is the *console* session, and `schtasks /create … /IT` — run with
  the interactive token of the logged-on user — is what reaches it. `tasklist /V` confirming
  `Session#: 2` is how this was established, and is how to re-establish it if a launch ever
  stops showing up.
- **The Windows tree has no wrapper.** The Linux artifact is `wrapProgram`ped
  (`nix/linux-kiosk.nix`) and so has `$CASTAWAY_ELECTRON` handed to it; the Windows tree is a
  flat directory, and a receiver that only knew the environment variable looked for a bare
  `electron` on `PATH`. Fixed in the receiver rather than in the launcher: `Config::
  browser_program`/`browser_app_dir` fall back to `browser/electron[.exe]` and `browser-host/`
  beside `current_exe()`, and `stageWidevine()` finds `WidevineCdm/` the same way. `run.cmd`
  therefore sets nothing, on purpose — if the sibling resolution regresses, it should surface
  here rather than be papered over by the deploy script.
- **The SSH session is elevated and cmd.exe is the shell.** `netsh`/`New-NetFirewallRule` work
  without a UAC dance, which is what makes `windows-firewall` possible at all; and `;` is not a
  separator, `&` is.

Output comes back over a small PowerShell tail (`tail.ps1`, staged beside the exe) rather than
`Get-Content -Wait`, for two reasons: it opens with `FileShare.ReadWrite` so it cannot trip the
writer, and it exits when `castaway.exe` is gone — a receiver that died three seconds in
otherwise leaves you watching an idle stream that looks identical to one that is working.

`windows-firewall` generates its rules from `nix/network-surface.json`, the same source of truth
as the Linux `open-firewall` and as the app's own `--network-surface`, so it cannot drift from
the code that binds the sockets (`crates/app/src/surface.rs` regenerates the file and a test
fails on drift). It differs from the Linux script in one way: `New-NetFirewallRule` takes a port
*range* as one rule, so the 32-port AirPlay/Cast media range is one rule rather than 32 holes.
Everything is tagged `-Group castaway`, which makes re-running idempotent and `--close` a single
`Remove-NetFirewallRule`.

## Target + toolchain: `x86_64-pc-windows-msvc`

Use the **MSVC** target, not MinGW/`windows-gnu`: windows-rs/WinRT is happiest on MSVC, and the
vendored import libraries are MSVC-format. The toolchain is all LLVM — `clang-cl` as the C/C++
driver, `lld-link` as the linker, `llvm-lib` as the archiver, `llvm-rc` for resources.

`cargo-xwin` is the usual turnkey answer and it's what `nix/windows.nix` is modelled on, but it
is **not** what runs the build: it wants a writable cache it can download the SDK into, which the
Nix sandbox denies. Instead `nix/msvc-sysroot.nix` pins the sysroot as a fixed-output derivation
and `nix/windows.nix` exports the same compiler/linker environment statically. The flag
conventions are kept deliberately identical to cargo-xwin's `clang_cl.rs` so its behaviour stays
a usable reference — and the derivation leaves cargo-xwin's `DONE` marker in the sysroot, so
`XWIN_CACHE_DIR=<sysroot> cargo xwin build` still works as an escape hatch without re-downloading.

There is no `.cargo/config.toml` for this. Everything comes from the environment in
`nix/windows.nix`, so there's one source of truth rather than two that drift.

## Vendored Windows dependencies

The prebuilt archives are **flake inputs** (`ffmpeg-windows-src`, `electron-windows-src`,
`electron-linux-src`, `widevine-windows-src`, all `flake = false`), so their URLs and hashes
live in `flake.lock` alongside nixpkgs and crane — one place to audit every external blob, one
update story. They use the `file+https://` scheme, which yields the raw archive rather than an
unpacked tree, so unpacking and layout policy live in the `.nix` files instead of being fixed
by the fetcher. A `file+` input lands in the store named bare `source`, with no extension for
stdenv to dispatch on, so each derivation unpacks explicitly.

The MSVC sysroot is deliberately *not* an input: it isn't fetched, it's *generated* by running
`xwin`, so it stays a fixed-output derivation (`nix/msvc-sysroot.nix`) pinned by `outputHash`. Note
that this couples it to the nixpkgs `xwin` package — a `nix flake update` that changes `xwin`'s
splat behaviour can break the hash even though `crtVersion`/`sdkVersion` haven't moved.

- **ffmpeg** (`nix/ffmpeg-windows.nix`) — a prebuilt LGPL BtbN build, pinned to an immutable
  `autobuild-*` release tag rather than `latest`, whose assets are replaced daily. The archive
  already ships the exact `FFMPEG_DIR` layout (`include/`, `lib/*.lib`, `bin/*.dll`), so the
  install is a straight copy, plus an assert on the expected top-level directory name — which
  is what catches a URL bump in `flake.nix` that forgot the matching `.nix` file.

  Prebuilt rather than source-built because nixpkgs marks `pkgsCross.mingwW64.ffmpeg` broken on
  64-bit MinGW, and it is: trimming it to a decode-only build just walks into the next transitive
  dependency with no mingw platform support. MSVC import libraries are ABI-neutral across a DLL
  boundary (plain C ABI), which is why gcc-built ffmpeg DLLs link fine under `lld-link`.

- **Electron** — castLabs "Electron for Content Security" (ECS), the browser runtime (D36).
  Two inputs, `electron-windows-src` for the deploy artifact and `electron-linux-src` for
  dev/CI, and they must be **bumped together**: one Chromium major everywhere is the point,
  or every codec, DRM, and offscreen behaviour verified in CI was verified against a browser
  we do not ship. The win32 zip is unpacked verbatim in `nix/windows.nix` with no layout
  fixups — deliberately, see the runtime-layout section: EVS signs these exact files.

- **Widevine CDM** (`nix/widevine-windows.nix`) — the CRX3 that Chrome's own component updater
  installs, pre-staged so DRM-gated video plays on a panel that has never been online (Q42).
  Pinned by hand to the same version nixpkgs' `widevine-cdm` pins for Linux; there is **no
  eval-time assert** tying the two, so a `nix flake update` that moves the nixpkgs side is a
  drift to catch by hand at review time. The CDM is unfree: unpacking is gated by
  `allowUnfreePredicate`, and the build `tryEval`s it — a build that cannot have the CDM is a
  receiver without DRM rather than no receiver at all.

## Cross-compiling C/C++: the things that bite

All handled in `nix/windows.nix`. Most were learned cross-building CEF's C++ wrapper — no
third-party CMake build remains in the tree since D36, and the CMake toolchain file and
case-shim machinery went with it (both live in git history) — but the wrapper and the CRT
knob stay, because they are also how every build-script (`cc`-crate) compile works, and each
lesson comes back with the next native dependency.

1. **Third-party build systems lose flags you hand them.** CEF's cmake *overwrote*
   `CMAKE_C_FLAGS`/`CMAKE_CXX_FLAGS` rather than appending, discarding everything a toolchain
   file set before the first object compiled. So the cross flags live in a `clang-cl`
   **wrapper script**, where no build system can lose them — same idea as the nixpkgs
   cc-wrapper. The wrapper injects flags on *both* sides of the caller's arguments: leading
   ones a caller may override, trailing ones must win. Trailing flags land **before** any `--`
   separator — everything after that is an input filename.

2. **`/WX` third-party code is calibrated against MSVC's `/W4`, and clang-cl's `/W4` is a
   different warning set** — warnings MSVC never emits become hard errors in code we don't own.
   The wrapper's trailing `-Wno-error` demotes them back rather than playing whack-a-mole with
   per-divergence `-Wno-` flags.

3. **Case-sensitivity.** Windows sources are written against a case-insensitive filesystem, so
   an `#include` may spell a header differently from the file on disk. xwin symlinks every SDK
   header under its all-lowercase name, which covers most of it but not mixed-case misspellings
   (the CEF-era worked example: `<Softpub.h>` vs the SDK's `SoftPub.h`). The fix is to shim
   the requested spelling into an overlay include dir rather than patching third-party sources
   — the `msvc-include-case-shims` derivation in `nix/windows.nix`'s git history does exactly
   that; resurrect it when the next mixed-case include appears. To find them: list every
   `#include` name in the sources being cross-compiled that has no exact match in the sysroot
   but does have a case-insensitive one.

4. **CRT linkage must agree across the whole image.** Rust's std and every build-script-compiled
   C object end up in one binary, and two CRTs there means two heaps and two errno/locale states
   — memory allocated on one side and freed on the other corrupts. `lld-link` resolves a
   mismatch *without a diagnostic* and lets it fail at runtime, so it's one knob (`crtStatic`)
   driving rustc's `target-feature=+crt-static` and the flags handed to any C compile.

   We pick **static**, so the deploy box needs no Visual C++ redistributable. That costs one
   host-side workaround: cc-rs turns `crt-static` into a bare `-static` for *any* GNU-family
   compile, including `ffmpeg-sys-next`'s deliberately-host version probe
   (`.target(HOST) // don't cross-compile this`). Supplying `glibc.static` is the obvious fix and
   the wrong one — it puts a lib dir holding `libc.a` and no `libc.so` on the link path for
   everything, so ordinary build scripts resolve `-lc` to the archive and segfault as half-static
   binaries. `nix/windows.nix` wraps the host compiler and strips the flag instead.

## Runtime layout

Windows has no rpath and no `/nix/store` to resolve against: the loader looks for DLLs next to
the `.exe`. So everything our binary links dynamically is staged into `bin/` beside
`castaway.exe` — the libav DLLs — and the electron artifact stages the browser beside it as its
own subtree, since Electron is a separate process with its own DLLs and nothing browser-side
needs flattening into ours:

- `bin/browser/` — the ECS win32 distribution, **byte-for-byte as castLabs shipped it**. EVS
  signs these exact files; a modified tree invalidates the VMP signature, and an unsigned
  Widevine host is refused licences by exactly the services it exists to host.
- `bin/browser-host/` — our Electron host app, launched from the receiver.
- `bin/WidevineCdm/` — staged for the receiver to copy into the browser profile on first run,
  not loaded from here: ECS resolves its CDM under `<userDataDir>/WidevineCdm/<version>/`, a
  runtime path (see `browser-host/stage-widevine.sh` and Q42). Present only when the unfree
  gate allows it.
- `bin/vmp-sign.sh` — the VMP signing step travels with the artifact rather than living only
  in the repo: it runs on whoever deploys, after Authenticode, and needs the tree beside it.

### The DLL closure is checked, not eyeballed

A DLL that is neither staged nor OS-provided doesn't fail the build — it fails at process startup,
as a modal dialog on a wall-mounted panel with nobody standing there to dismiss it. So
`nix flake check` cross-references each artifact's import table against the staged files plus an
audited allowlist of DLLs Windows itself guarantees (`systemDlls` in `nix/windows.nix`). Windows
binaries can't be executed on the builder, so a static check of what the loader will go looking
for is the closest thing to a smoke test available without the hardware.

The check reads `castaway.exe`'s import table (including delay-loads). The ECS tree is out of
scope by design: it's a separate process shipping its own complete DLL set, verified upstream.

If a legitimate new system DLL shows up, add it to `systemDlls` *with a note on where it comes
from* — the list is an allowlist, not a suppression list.

Note that `d3d12.dll`/`dxgi.dll` never appear in the import table: wgpu loads them at runtime.
Their absence is expected and is **not** evidence the DX12 backend is missing.

## Render backend

`wgpu_compositor::create_instance()` requests `Backends::DX12` under `#[cfg(windows)]` and
`Backends::all()` elsewhere, with `wgpu::util::backend_bits_from_env()` as an override so
`WGPU_BACKEND=vulkan` still works for debugging on the box. (`Backends::from_env()` is wgpu 23+;
this tree is on 22.1.) `OPENGL32.dll` still shows in the import table because wgpu's GL backend is
compiled in — it isn't selected.

## Testing matrix — Wine won't save you

- **Native Linux:** portable protocols, wgpu/Vulkan, ffmpeg/VAAPI, the ECS browser (same build
  that ships), all unit tests. This is 90% of dev.
- **Cross-build checks (`nix flake check`):** the Windows artifacts link, and their DLL closures
  are satisfied. No execution.
- **Physical Windows box (C6522QT-attached):** the *only* place Miracast/WinRT/DX12-interop/
  browser-frame-import integration is real. `nix run .#deploy-windows` puts the artifact there
  and streams its log back (see "Deploying to the box"). Wine has no Miracast receiver API, no
  real Wi-Fi Direct, dicey DX12 — not a test path.

## Bottom line

Daily dev = native Linux. Windows slice = `nix build .#castaway-windows-electron` (append
`.archive` for the zip), fully from the Linux box, browser included — the Windows-CI escape
hatch this document used to recommend turned out not to be needed. Integration testing = the
physical C6522QT Windows box. The whole cross-build concern stays quarantined in the same ~10%
that's already non-portable.
