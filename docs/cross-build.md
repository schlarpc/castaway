# Cross-build: dev on Linux → target Windows

**Setup:** dev box is Linux (NixOS), deploy target is a Windows box wired to the Dell C6522QT.
The design is ~90% portable Rust; the cross-build pain is contained to the same Windows-specific
~10% (WinRT Miracast, DX12/D3D11 interop, CEF).

**Status:** the cross-build is real and lands from Nix — including CEF. `nix build
.#castaway-windows-cef` produces a directory that can be copied to the Windows box and run.
Nothing here needs a Windows machine or Windows CI to *build*; the physical box is still the only
place to *test* the hardware paths.

## Golden rule: don't gate daily dev on the cross-build

The portable crates **build and run natively on the Linux dev box** — AirPlay, Cast (incl.
desktop-mirroring), DLNA, Spotify, Lounge, the wgpu compositor (Vulkan), ffmpeg decode (VAAPI).
Do daily dev/test here. Cross-build enters only for the Windows slice, and running it also
continuously exercises your *future* Linux target.

| Component | Build on Linux? | Test on Linux? | Notes |
|---|---|---|---|
| Portable protocols (AirPlay/Cast/DLNA/Spotify/Lounge) | ✅ native | ✅ native | the daily loop |
| wgpu compositor | ✅ native (Vulkan) | ✅ native | DX12 path + D3D11 interop → Windows only |
| ffmpeg decode | ✅ native (VAAPI) | ✅ native | Windows target links vendored libav import libs |
| `windows` crate / Miracast `backend-windows` | ✅ cross (pure Rust bindings) | ❌ | deploy to Windows box; Wine won't do WinRT/Miracast |
| CEF (cef-rs) | ✅ cross | ❌ | wrapper built with clang-cl; see the gotchas below |

## Artifacts

| Output | Features | What it's for |
|---|---|---|
| `.#castaway-windows` | none | toolchain canary — if it stops linking, the toolchain broke, not the media stack |
| `.#castaway-windows-render` | `render` | DX12 compositor + kiosk, no browser; bisect render problems without CEF's ~200 MB in the way |
| `.#castaway-windows-hwaccel` | `hwaccel` | the D3D11VA → shared-NV12 → D3D12 decode bridge. Exists as its own artifact because it is the one part of Q20 Linux cannot exercise: the VA-API half has an offscreen readback test, this half has only the compiler until it reaches the Dell |
| `.#castaway-windows-cef` | `cef` | the deploy artifact: render + offscreen browser + hwaccel, with the full CEF runtime staged |
| `.#msvc-sysroot` | — | the MSVC CRT + Windows SDK sysroot, built and cached independently |

For an incremental loop, `nix develop .#windows` exports the whole cross environment (including
`CARGO_BUILD_TARGET`), so plain `cargo build` cross-compiles. It's a **separate shell from
`default` on purpose** — exporting `CARGO_BUILD_TARGET` into the default shell would silently
hijack the native dev loop.

## Target + toolchain: `x86_64-pc-windows-msvc`

Use the **MSVC** target, not MinGW/`windows-gnu`: CEF's Windows import libs are MSVC-format and
won't cleanly link against GNU, and windows-rs/WinRT is happiest on MSVC. The toolchain is all
LLVM — `clang-cl` as the C/C++ driver, `lld-link` as the linker, `llvm-lib` as the archiver,
`llvm-rc` for resources.

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

Both archives are **flake inputs** (`ffmpeg-windows-src`, `cef-windows-src`, both `flake = false`),
so their URLs and hashes live in `flake.lock` alongside nixpkgs and crane — one place to audit
every external blob, one update story. They use the `file+https://` scheme, which yields the raw
archive rather than an unpacked tree, because each needs layout fixups afterwards that a tarball
input can't express. A `file+` input lands in the store named bare `source`, with no extension for
stdenv to dispatch on, so both derivations unpack explicitly and assert on the expected top-level
directory name — that assert is what catches a URL bump in `flake.nix` that forgot the matching
`.nix` file.

The MSVC sysroot is deliberately *not* an input: it isn't fetched, it's *generated* by running
`xwin`, so it stays a fixed-output derivation (`nix/msvc-sysroot.nix`) pinned by `outputHash`. Note
that this couples it to the nixpkgs `xwin` package — a `nix flake update` that changes `xwin`'s
splat behaviour can break the hash even though `crtVersion`/`sdkVersion` haven't moved.

- **ffmpeg** (`nix/ffmpeg-windows.nix`) — a prebuilt LGPL BtbN build, pinned to an immutable
  `autobuild-*` release tag rather than `latest`, whose assets are replaced daily. The archive
  already ships the exact `FFMPEG_DIR` layout (`include/`, `lib/*.lib`, `bin/*.dll`), so the
  install is a straight copy.

  Prebuilt rather than source-built because nixpkgs marks `pkgsCross.mingwW64.ffmpeg` broken on
  64-bit MinGW, and it is: trimming it to a decode-only build just walks into the next transitive
  dependency with no mingw platform support. MSVC import libraries are ABI-neutral across a DLL
  boundary (plain C ABI), which is why gcc-built ffmpeg DLLs link fine under `lld-link`.

- **CEF** (`nix/cef-windows.nix`) — the Windows binary distribution, keeping the **full** upstream
  layout, unlike the flattened Linux `cefDist` in `flake.nix`. On Linux `cef-dll-sys` only emits
  `-l cef`; on Windows it additionally runs CMake over the distribution to build
  `libcef_dll_wrapper`, so `CMakeLists.txt`, `cmake/`, `include/` and `libcef_dll/` all have to
  survive. Two additions on top: an `archive.json` (cef-dll-sys refuses a `CEF_PATH` without one)
  and a root `libcef.lib` symlink (the build script emits `link-search=native={CEF_PATH}` at the
  root but upstream puts the import lib under `Release/`).

### One CEF version, three pins

The CEF version is fixed in three unrelated places and all three must agree:

| Pin | Where | Moves when |
|---|---|---|
| nixpkgs `cef-binary` | `cefDist` in `flake.nix` — what the **Linux** dev shell runs | `nix flake update` |
| `cef-windows-src` | `flake.nix` input — what ships on **Windows** | never, by hand |
| `cef`/`cef-dll-sys` crates | `crates/pipeline/Cargo.toml` | never, by hand |

Only the first moves on its own, which is the whole hazard: nixpkgs drifting ahead silently turns
every browser bug into "does it reproduce on the box?". `cef-dll-sys` already enforces the third
against the second by parsing `archive.json`, so `nix/cef-windows.nix` closes the loop with an
eval-time assert against `cef-binary.version`. A mismatch fails `nix flake check` with a message
naming both versions rather than producing two subtly different builds.

## Cross-building CEF: the four things that bite

All fixed in `nix/windows.nix`; recorded here because each is non-obvious and each will come back
on a CEF version bump.

1. **CEF's cmake overwrites `CMAKE_C_FLAGS`/`CMAKE_CXX_FLAGS`** rather than appending, so
   everything a toolchain file sets is discarded before the first object compiles. The fix is to
   move the cross flags into a `clang-cl` **wrapper script**, where no third-party build system
   can lose them. Same idea as the nixpkgs cc-wrapper.

2. **`/WX` is calibrated against MSVC's `/W4`, and clang-cl's `/W4` is a different warning set.**
   `/MP` (unimplemented in clang-cl) and `-Wmissing-field-initializers` (firing on CEF's own
   `cef_types_wrappers.h`) both became hard errors. The wrapper injects flags on *both* sides of
   the caller's arguments: leading ones a caller may override, trailing ones must win. Trailing
   flags have to land **before** any `--` separator — everything after that is an input filename.

3. **Case-sensitivity.** `cef_certificate_util_win.cc` includes `<Softpub.h>`; the SDK ships
   `SoftPub.h`. xwin symlinks every SDK header under its all-lowercase name, which covers most
   of it but not mixed-case misspellings. `nix/windows.nix` shims the requested spelling into an
   overlay include dir rather than patching CEF. To find more after a bump: list every `#include`
   name in the sources being cross-compiled that has no exact match in the sysroot but does have
   a case-insensitive one.

4. **CRT linkage must agree across the whole image.** CEF builds its wrapper `/MT` while Rust's
   windows-msvc target defaults to the dynamic CRT. Two CRTs in one image means two heaps and two
   errno/locale states, and memory allocated on one side and freed on the other corrupts —
   `lld-link` resolves the mismatch *without a diagnostic* and lets it fail at runtime. It's one
   knob (`crtStatic`) driving both `CMAKE_MSVC_RUNTIME_LIBRARY`/`CEF_RUNTIME_LIBRARY_FLAG` and
   rustc's `target-feature=+crt-static`.

   We pick **static**, so the deploy box needs no Visual C++ redistributable. That costs one
   host-side workaround: cc-rs turns `crt-static` into a bare `-static` for *any* GNU-family
   compile, including `ffmpeg-sys-next`'s deliberately-host version probe
   (`.target(HOST) // don't cross-compile this`). Supplying `glibc.static` is the obvious fix and
   the wrong one — it puts a lib dir holding `libc.a` and no `libc.so` on the link path for
   everything, so ordinary build scripts resolve `-lc` to the archive and segfault as half-static
   binaries. `nix/windows.nix` wraps the host compiler and strips the flag instead.

## Runtime layout

Windows has no rpath and no `/nix/store` to resolve against: the loader looks for DLLs next to
the `.exe`. So everything dynamically linked is staged into `bin/` beside `castaway.exe` — the
libav DLLs, and for the CEF build the whole flat CEF runtime.

CEF splits its distribution into `Release/` and `Resources/` for its own CMake build, but at
runtime it resolves everything relative to the module directory, which is where an empty
`Settings::resources_dir_path` points it. `cef_browser.rs` deliberately leaves those empty when
`CEF_PATH` is unset — which is the case on the deploy box, since there's no `/nix/store` there to
point at. `bootstrap.exe` is **not** staged: it's the entry point for CEF's sandboxed
"app is a DLL" mode, and we initialize with `no_sandbox` and ship a real `.exe`.

### The DLL closure is checked, not eyeballed

A DLL that is neither staged nor OS-provided doesn't fail the build — it fails at process startup,
as a modal dialog on a wall-mounted panel with nobody standing there to dismiss it. So
`nix flake check` cross-references each artifact's import table against the staged files plus an
audited allowlist of DLLs Windows itself guarantees (`systemDlls` in `nix/windows.nix`). Windows
binaries can't be executed on the builder, so a static check of what the loader will go looking
for is the closest thing to a smoke test available without the hardware.

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

- **Native Linux:** portable protocols, wgpu/Vulkan, ffmpeg/VAAPI, all unit tests. This is 90% of dev.
- **Cross-build checks (`nix flake check`):** the Windows artifacts link, and their DLL closures
  are satisfied. No execution.
- **Physical Windows box (C6522QT-attached):** the *only* place Miracast/WinRT/DX12-interop/
  CEF-render integration is real. Deploy the artifact here for hardware tests. Wine has no
  Miracast receiver API, no real Wi-Fi Direct, dicey DX12/CEF — not a test path.

## Bottom line

Daily dev = native Linux. Windows slice = `nix build .#castaway-windows-cef`, fully from the Linux
box, CEF included — the Windows-CI escape hatch this document used to recommend turned out not to
be needed. Integration testing = the physical C6522QT Windows box. The whole cross-build concern
stays quarantined in the same ~10% that's already non-portable.
