# Cross-build: dev on Linux → target Windows

**Setup:** dev box is Linux (NixOS), deploy target is a Windows box wired to the Dell C6522QT. The design is ~90% portable Rust; the cross-build pain is contained to the same Windows-specific ~10% (WinRT Miracast, DX12/D3D11 interop, CEF).

## Golden rule: don't gate daily dev on the cross-build

The portable crates **build and run natively on the Linux dev box** — AirPlay, Cast (incl. desktop-mirroring), DLNA, Spotify, Lounge, the wgpu compositor (Vulkan), ffmpeg decode (VAAPI). Do daily dev/test here. Cross-build enters only for the Windows slice, and running it also continuously exercises your *future* Linux target.

| Component | Build on Linux? | Test on Linux? | Notes |
|---|---|---|---|
| Portable protocols (AirPlay/Cast/DLNA/Spotify/Lounge) | ✅ native | ✅ native | the daily loop |
| wgpu compositor | ✅ native (Vulkan) | ✅ native | DX12 path + D3D11 interop → Windows only |
| ffmpeg decode | ✅ native (VAAPI) | ✅ native | Windows target needs Windows libav libs (below) |
| `windows` crate / Miracast `backend-windows` | ✅ cross (pure Rust bindings) | ❌ | deploy to Windows box; Wine won't do WinRT/Miracast |
| CEF (cef-rs) | ⚠️ cross = boss fight | ❌ | fallback: build on Windows CI |

## Target + toolchain: `x86_64-pc-windows-msvc` via `cargo-xwin`

Use the **MSVC** target, not MinGW/`windows-gnu`: CEF's Windows import libs are MSVC-format and won't cleanly link against GNU, and windows-rs/WinRT is happiest on MSVC. `cargo-xwin` makes MSVC-from-Linux turnkey by auto-fetching the MSVC CRT + Windows SDK (via `xwin`) and driving `clang-cl` + `lld-link`.

```bash
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin
# needs clang + lld on PATH (nix: see below)
cargo xwin build --target x86_64-pc-windows-msvc --release
```

`.cargo/config.toml` (cargo-xwin injects most flags, but pin the linker explicitly):
```toml
[target.x86_64-pc-windows-msvc]
linker = "lld-link"
# cargo-xwin sets CC/CXX/AR to clang-cl/llvm-lib and the SDK include/lib paths.

[env]
# ffmpeg-sys-next finds prebuilt libav via FFMPEG_DIR (Windows import libs + headers)
FFMPEG_DIR = { value = "vendor/ffmpeg-windows-x64", relative = true }
PKG_CONFIG_ALLOW_CROSS = "1"
```

## The C-dependency sysroot (the fiddly part)

You need **target (Windows) prebuilt libraries** for the native-C deps. Vendor them:

```
vendor/
├─ ffmpeg-windows-x64/       # from BtbN ffmpeg "shared+dev" Windows build
│  ├─ include/               # libav* headers
│  ├─ lib/                   # *.lib import libs
│  └─ bin/                   # *.dll to ship next to the exe
└─ cef-windows-x64/          # CEF Windows binary distribution (Standard/Minimal)
   ├─ include/  Release/  Resources/   # libcef.lib, libcef.dll, ICU, .pak, locales
```

- **ffmpeg:** `ffmpeg-sys-next` (under `ffmpeg-next`) links via `FFMPEG_DIR` → point it at `vendor/ffmpeg-windows-x64`. Ship the DLLs beside the exe.
- **CEF:** cef-rs fetches/uses a CEF binary distribution; for cross you must point it at the **Windows** dist and link `libcef.lib`. **Verify the exact env/knob against your cef-rs version** (it has a downloader; for cross you override to the Windows dist). This is where cross-linking most often fights back — see fallback.
- **windows crate / wgpu:** pure Rust, no vendored libs. Just build.

## CEF fallback: build the CEF-heavy binary on Windows CI

If cross-linking the Windows CEF dist from Linux turns miserable (likely), don't fight it. Keep the Linux dev loop on the portable core with a **stubbed browser** (feature flag `cef` off → Lounge uses a headless ffmpeg/yt-dlp player), and produce the real CEF binary on a `windows-latest` runner.

Minimal GitHub Actions job:
```yaml
# .github/workflows/windows.yml
name: windows
on: [push]
jobs:
  build:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: x86_64-pc-windows-msvc }
      - name: Fetch CEF + ffmpeg (Windows)
        run: |            # vcpkg or the crates' downloaders; cache these
          # e.g. vcpkg install ffmpeg:x64-windows  (or download BtbN build)
      - run: cargo build --release --features cef        # native MSVC, no cross
      - run: cargo test  --release
      - uses: actions/upload-artifact@v4
        with: { name: receiver-windows, path: target/release/*.exe }
```

## Testing matrix — Wine won't save you

- **Native Linux:** portable protocols, wgpu/Vulkan, ffmpeg/VAAPI, all unit tests. This is 90% of dev.
- **Windows CI (`windows-latest`):** real MSVC compile + unit tests for the Windows slice + CEF binary.
- **Physical Windows box (C6522QT-attached):** the *only* place Miracast/WinRT/DX12-interop/CEF-render integration is real. Deploy the artifact here for hardware tests. Wine has no Miracast receiver API, no real Wi-Fi Direct, dicey DX12/CEF — not a test path.

## NixOS notes

- **Dev shell:** provide `clang`, `lld`/`llvm`, `cargo-xwin`, `rustup` (or fenix/oxalica overlay with the `x86_64-pc-windows-msvc` target). `cargo-xwin` fetches the MSVC SDK into its cache at first run (network in the shell, or pre-seed the `xwin` cache).
- **Reproducible Windows sysroot:** wrap `vendor/ffmpeg-windows-x64` (and the CEF dist) as a Nix derivation (fixed-output hash on the BtbN/CEF archives) → deterministic cross sysroot, on-brand and makes CI reproducible. More upfront work; worth it.
- **Alternative:** `pkgsCross.mingwW64` gives a GNU cross toolchain, but MinGW likely can't link CEF — prefer `cargo-xwin`/MSVC. Only reach for MinGW if you drop CEF.

## Bottom line

Daily dev = native Linux. Windows slice = `cargo xwin build --target x86_64-pc-windows-msvc`, with a `windows-latest` CI as the CEF/ffmpeg escape hatch. Integration testing = the physical C6522QT Windows box. The whole cross-build concern stays quarantined in the same ~10% that's already non-portable.
