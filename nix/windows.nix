# Linux → Windows cross-build (`x86_64-pc-windows-msvc`), per docs/cross-build.md.
#
# We target MSVC rather than MinGW because windows-rs/WinRT expects MSVC and the vendored
# import libraries are MSVC-format. `cargo-xwin` is the usual turnkey answer, but it wants a
# writable cache it can download into at build time — which a Nix sandbox does not have.
# So we do what cargo-xwin does, statically: take the pinned sysroot from
# ./msvc-sysroot.nix and export the same compiler/linker environment ourselves. The flag
# conventions below are deliberately kept identical to cargo-xwin's `clang_cl.rs`, so its
# behaviour remains a reference (and `XWIN_CACHE_DIR=<sysroot>` still works as a fallback).
#
# The toolchain is all LLVM: clang-cl as the C/C++ driver, lld-link as the linker,
# llvm-lib as the archiver, llvm-rc for resources.
#
# `ffmpegSrc`/`electronSrc` are the raw archives, pinned as flake inputs so they land in
# flake.lock; the derivations beside this file unpack and rearrange them.
{ pkgs, craneLib, commonArgs, rustToolchain, ffmpegSrc, electronSrc, widevineSrc }:

let
  inherit (pkgs) lib;

  target = "x86_64-pc-windows-msvc";
  # Cargo's per-target env var suffixes: lowercase-underscored for the `cc`-crate style
  # vars, uppercase-underscored for `CARGO_TARGET_*`.
  envTarget = lib.replaceStrings [ "-" ] [ "_" ] target;
  envTargetUpper = lib.toUpper envTarget;
  # xwin names the arch directories with LLVM's canonical spelling, not MS's `x64`.
  arch = "x86_64";

  sysroot = pkgs.callPackage ./msvc-sysroot.nix { };
  ffmpeg = pkgs.callPackage ./ffmpeg-windows.nix { src = ffmpegSrc; };
  # The ECS archive, unpacked. No layout fixups: an Electron distribution is already
  # flat, and it must stay byte-identical anyway because EVS signs these exact files (D36).
  electron = pkgs.runCommand "electron-ecs-win32-x64" { nativeBuildInputs = [ pkgs.unzip ]; } ''
    mkdir -p $out && cd $out && unzip -q ${electronSrc}
  '';

  # The Widevine CDM, staged beside the .exe so DRM-gated video plays on a panel that has
  # never been online. `tryEval` for the same reason as the Linux side (flake.nix
  # `widevineLinuxFor`): the CDM is unfree, and a build that cannot have it should be a
  # receiver without DRM rather than no receiver at all.
  widevine =
    let
      attempt = builtins.tryEval
        ((pkgs.callPackage ./widevine-windows.nix { src = widevineSrc; }).outPath or null);
    in
    if attempt.success && attempt.value != null then attempt.value else null;

  includeDirs = [
    "${sysroot}/crt/include"
    "${sysroot}/sdk/include/ucrt"
    "${sysroot}/sdk/include/um"
    "${sysroot}/sdk/include/shared"
    "${sysroot}/sdk/include/winrt"
  ];

  libDirs = [
    "${sysroot}/crt/lib/${arch}"
    "${sysroot}/sdk/lib/um/${arch}"
    "${sysroot}/sdk/lib/ucrt/${arch}"
  ];

  # One knob for the CRT, because everything in the image must agree on it. Rust's std and
  # every build-script-compiled C object end up in the same image, and two CRTs there means
  # two heaps and two errno/locale states — memory allocated on one side and freed on the
  # other corrupts. This has to be right by construction: lld-link resolves the mismatch
  # without a diagnostic and lets it fail at runtime instead.
  #
  # Static, because the deploy target is an appliance — a static CRT needs no Visual C++
  # redistributable installed on the box.
  crtStatic = true;

  # `/imsvc` marks these as system includes, which suppresses warnings from Microsoft's
  # headers. clang-cl takes the path as a separate argument, hence the two list elements.
  #
  # If a third-party source ever fails on a mixed-case `#include` (Windows code is written
  # against a case-insensitive filesystem; xwin only symlinks the all-lowercase spellings),
  # shim the requested spelling into an overlay include dir rather than patching the source
  # — see the `msvc-include-case-shims` derivation in this file's git history.
  leadingFlags = [
    "--target=${target}"
    "-Wno-unused-command-line-argument"
    "-fuse-ld=lld-link"
  ] ++ lib.concatMap (dir: [ "/imsvc" dir ]) includeDirs;

  # Third-party C/C++ built with `/WX` is calibrated against MSVC's `/W4`, and clang-cl
  # maps `/W4` onto a *different* warning set — so warnings MSVC never emits become hard
  # errors in code we don't own and won't patch. Demote them back to warnings rather than
  # playing whack-a-mole with `-Wno-` for each divergence.
  trailingFlags = [ "-Wno-error" ];

  # bindgen drives libclang directly rather than the clang-cl driver, so it wants plain
  # `-I` and an explicit target.
  bindgenFlags = lib.concatStringsSep " "
    ([ "--target=${target}" ] ++ map (dir: "-I${dir}") includeDirs);

  # A wrapper, not bare clang-cl, because a third-party build system can rewrite its flag
  # variables wholesale (CMAKE_C_FLAGS and friends) and silently discard whatever a
  # toolchain file or the environment set. Baking the cross setup into the driver itself
  # makes it un-loseable no matter how the caller manipulates its flags. Same idea as the
  # nixpkgs cc-wrapper.
  #
  # Flags go on both sides of the caller's, because clang resolves conflicts last-wins and
  # the two groups want opposite precedence: `leadingFlags` may be overridden by a caller
  # that knows better, `trailingFlags` must beat whatever the caller asked for. Trailing
  # flags land *before* a `--` separator — everything after it is an input filename, so
  # appending there would make clang look for a source file called `-Wno-error`.
  clangCl = pkgs.writeShellScriptBin "clang-cl" ''
    args=()
    trailing=(${lib.escapeShellArgs trailingFlags})
    for arg in "$@"; do
      if [ "$arg" = "--" ]; then
        args+=("''${trailing[@]}")
        trailing=()
      fi
      args+=("$arg")
    done
    exec ${pkgs.llvmPackages.clang-unwrapped}/bin/clang-cl \
      ${lib.escapeShellArgs leadingFlags} "''${args[@]}" "''${trailing[@]}"
  '';

  # clang-unwrapped, not the nixpkgs `clang` wrapper: the wrapper injects glibc include
  # paths and host flags that make no sense when the target is Windows. clang's own
  # builtin headers (stddef.h, the intrin headers) still come along via its resource dir.
  toolchainBins = [
    # Ahead of clang-unwrapped, which also ships a (bare) clang-cl.
    clangCl
    pkgs.llvmPackages.clang-unwrapped # clang, clang++
    pkgs.lld # lld-link
    pkgs.llvm # llvm-lib, llvm-rc, llvm-dlltool
  ];

  # The cross environment, shared by the package build and the cross dev shell.
  crossEnv = {
    CARGO_BUILD_TARGET = target;

    # rustc invokes the linker itself; it needs both the flavor (so it emits MSVC-style
    # `/LIBPATH:` arguments rather than GNU `-L`) and the library search paths.
    "CARGO_TARGET_${envTargetUpper}_LINKER" = "lld-link";
    "CARGO_TARGET_${envTargetUpper}_RUSTFLAGS" = lib.concatStringsSep " " ([
      "-C"
      "linker-flavor=lld-link"
      # The Rust side of the CRT decision — see `crtStatic`.
      "-C"
      "target-feature=${if crtStatic then "+" else "-"}crt-static"
    ] ++ map (dir: "-Lnative=${dir}") libDirs);

    # The `cc` crate picks these up when a build script compiles C/C++ for the target.
    # The cross flags come from the wrapper, so only what's genuinely per-language is here.
    "CC_${envTarget}" = "${clangCl}/bin/clang-cl";
    "CXX_${envTarget}" = "${clangCl}/bin/clang-cl";
    "AR_${envTarget}" = "llvm-lib";
    "CXXFLAGS_${envTarget}" = "/EHsc";
    "BINDGEN_EXTRA_CLANG_ARGS_${envTarget}" = bindgenFlags;
    # bindgen (in ffmpeg-sys-next's build script) dlopens libclang rather than shelling
    # out to the driver, so it needs the library pointed out explicitly in a Nix env.
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    RCFLAGS = lib.concatStringsSep " " (map (dir: "-I${dir}") includeDirs);

    # clang-cl and lld-link resolve bare `foo.lib` names through LIB, the way the MSVC
    # toolchain does on Windows. Semicolons, not colons — this is a Windows-style path list.
    LIB = lib.concatStringsSep ";" libDirs;

    # `ffmpeg-sys-next` takes this branch instead of pkg-config, reading `include/` for
    # bindgen and `lib/` for the import libraries.
    FFMPEG_DIR = "${ffmpeg}";
  };

  # A host compiler that refuses to be static, for build scripts that compile *host* helper
  # programs while we cross-compile.
  #
  # cc-rs turns `crt-static` into a bare `-static` for any GNU-family compiler, reading it
  # from CARGO_CFG_TARGET_FEATURE — which describes the *Windows* target, and which it
  # consults even when a `Build` overrides `.target()` to the host. ffmpeg-sys-next hits this:
  # it compiles and runs a small Linux program to print libav's version macros, commented
  # `.target(HOST) // don't cross-compile this`. With `-static` that needs a static glibc.
  #
  # Supplying one (`glibc.static`) is the obvious fix and the wrong one: it puts a lib dir
  # holding `libc.a` and no `libc.so` on the link path for *everything*, so ordinary build
  # scripts resolve `-lc` to the archive and segfault as half-static binaries. Strip the flag
  # instead — a throwaway host helper has no reason to be statically linked either way.
  hostCc = pkgs.writeShellScriptBin "host-cc-no-static" ''
    args=()
    for arg in "$@"; do
      [ "$arg" = "-static" ] || args+=("$arg")
    done
    exec ${pkgs.stdenv.cc}/bin/cc "''${args[@]}"
  '';

  # cc-rs looks these up by the literal triple, dashes and all.
  hostTriple = pkgs.stdenv.buildPlatform.config;
  hostCcEnv = { "CC_${hostTriple}" = "${hostCc}/bin/host-cc-no-static"; };

  # Dependency artifacts must be built for the same target as the final binary, so this
  # cross build gets its own `buildDepsOnly` rather than reusing the native one.
  crossArgs = commonArgs // crossEnv // hostCcEnv // {
    strictDeps = true;
    nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ toolchainBins;
    # Windows binaries can't be executed on the Linux builder.
    doCheck = false;
  };

  # The browser ships beside the .exe as its own tree: Electron is a separate process
  # with its own DLLs, so nothing browser-side has to be flattened into ours. What we
  # stage is the ECS distribution plus our host app, and the Widevine CDM for the profile
  # pre-staging that makes first-boot DRM work offline (D36/Q42).
  stageBrowser = ''
    # The whole ECS distribution, unmodified: it is what EVS signs, and a modified tree
    # invalidates the VMP signature. Our host app travels beside it.
    mkdir -p "$out/bin/browser"
    cp -r --no-preserve=mode,ownership ${electron}/* "$out/bin/browser/"
    mkdir -p "$out/bin/browser-host"
    cp -r --no-preserve=mode,ownership ${../browser-host}/* "$out/bin/browser-host/"
    install -Dm644 ${./castaway.exe.manifest} "$out/bin/castaway.exe.manifest"
    # The signing step travels with the artifact rather than living only in the repo: it
    # runs on whoever deploys this, after Authenticode, and needs the tree beside it.
    install -Dm755 ${../browser-host/vmp-sign.sh} "$out/bin/vmp-sign.sh"
  '' + lib.optionalString (widevine != null) ''
    # Staged for the receiver to copy into the browser profile on first run, not loaded
    # from here: ECS finds its CDM under `<userDataDir>/WidevineCdm/<version>/`, which is
    # a runtime path. See browser-host/stage-widevine.sh, and Q42 for the measurement.
    cp -r --no-preserve=mode,ownership ${widevine}/WidevineCdm "$out/bin/"
  '';

  # The deploy tree as one zip, for getting onto the Windows box, which has no Nix store
  # to copy `result` into: `nix build .#castaway-windows-electron.archive` →
  # `result/castaway-windows-electron.zip`, unzipping to a single folder. zip rather than
  # tar because Explorer opens it.
  mkArchive = pkg: pkgs.runCommand "${pkg.pname}-archive"
    {
      nativeBuildInputs = [ pkgs.zip ];
      meta.description = "The ${pkg.pname} deploy tree as a single zip";
    } ''
    cp -rL --no-preserve=mode,ownership ${pkg}/bin ${pkg.pname}
    # The store's 1970 mtimes predate the zip format's 1980 DOS epoch; pin them at the
    # epoch rather than letting zip clamp them with a warning per file. `-X` and the
    # sorted name list keep the rebuild byte-identical.
    find ${pkg.pname} -exec touch -d '1980-01-01 00:00:00 UTC' {} +
    mkdir -p "$out"
    find ${pkg.pname} | sort | zip -qX "$out/${pkg.pname}.zip" -@
  '';

  # Cargo refuses `--features` at the root of a virtual workspace, so every feature-
  # selecting build has to name the package too.
  mkCastaway = { pname, features ? [ ], withFfmpeg ? false, withBrowser ? false }:
    let
      cargoExtraArgs = "--package castaway"
        + lib.optionalString (features != [ ])
        " --features ${lib.concatStringsSep "," features}";
      args = crossArgs // { inherit cargoExtraArgs; };
      pkg = craneLib.buildPackage (args // {
        inherit pname;
        cargoArtifacts = craneLib.buildDepsOnly args;

        # Windows has no rpath and no /nix/store to resolve against: the loader looks for
        # DLLs next to the .exe. Anything dynamically linked has to be copied in, or the
        # binary dies at startup on the deploy box with a missing-DLL dialog.
        postInstall = lib.optionalString withFfmpeg ''
          cp ${ffmpeg}/bin/*.dll "$out/bin/"
        '' + lib.optionalString withBrowser stageBrowser;
      });
    in
    # `.archive` rides along on every artifact (`nix build .#<name>.archive`) instead of
    # doubling the package set in flake.nix. passthru only, so it costs nothing unless
    # asked for and doesn't change the package's own hash.
    pkg.overrideAttrs (prev: {
      passthru = (prev.passthru or { }) // { archive = mkArchive pkg; };
    });

  # DLLs Windows itself guarantees. Everything else has to travel with the binary.
  #
  # The `api-ms-win-*` API sets and `ext-ms-*` are matched by prefix rather than listed:
  # they're virtual names the loader redirects via the API set schema, and rustc/LLVM emit
  # different ones as the toolchain moves. `opengl32` is Windows' own software GL, pulled in
  # by wgpu's GL backend even though we select DX12.
  systemDlls = [
    "advapi32.dll"
    "bcrypt.dll"
    "bcryptprimitives.dll"
    "comctl32.dll"
    "crypt32.dll"
    "cryptbase.dll"
    "d3d11.dll"
    "d3d12.dll"
    "d3dcompiler_47.dll"
    "dwmapi.dll"
    "dxgi.dll"
    "gdi32.dll"
    "imm32.dll"
    "iphlpapi.dll" # IP Helper — interface enumeration for mDNS/SSDP advertisement
    "kernel32.dll"
    "ntdll.dll"
    "ole32.dll"
    "oleaut32.dll"
    "opengl32.dll"
    "powrprof.dll"
    "propsys.dll"
    "secur32.dll"
    "setupapi.dll"
    "shell32.dll"
    "shlwapi.dll"
    "user32.dll"
    "userenv.dll"
    "uxtheme.dll"
    "version.dll"
    "winmm.dll"
    "ws2_32.dll"
  ];

  # Ground rule 6: prefer a harness over manual verification. A DLL that is neither staged
  # nor OS-provided doesn't fail the build — it fails at process startup, on the panel, as a
  # modal dialog nobody is standing there to dismiss. Catch it here instead.
  #
  # Covers delay-loaded imports too: llvm-readobj lists those under their own heading, and a
  # missing one merely defers the crash to whenever that symbol is first touched.
  mkBundleCheck = pkg: pkgs.runCommand "${pkg.pname}-dll-closure"
    {
      nativeBuildInputs = [ pkgs.llvm ];
      meta.description = "Every DLL ${pkg.pname} imports is staged or OS-provided";
    } ''
    llvm-readobj --coff-imports ${pkg}/bin/castaway.exe \
      | grep -oP 'Name: \K\S+\.dll' | tr 'A-Z' 'a-z' | sort -u > imports.txt

    missing=""
    while read -r dll; do
      case "$dll" in
        api-ms-win-*|ext-ms-*) continue ;;
      esac
      for known in ${lib.escapeShellArgs systemDlls}; do
        [ "$dll" = "$known" ] && continue 2
      done
      [ -e "${pkg}/bin/$dll" ] && continue
      missing="$missing $dll"
    done < imports.txt

    if [ -n "$missing" ]; then
      echo "${pkg.pname} imports DLLs that are neither staged beside the .exe nor" >&2
      echo "known to ship with Windows:$missing" >&2
      echo >&2
      echo "Either stage them in postInstall, or add them to systemDlls in" >&2
      echo "nix/windows.nix if Windows really does provide them." >&2
      exit 1
    fi

    echo "checked $(wc -l < imports.txt) imported DLLs" > "$out"
  '';

in
rec {
  inherit sysroot crossEnv target toolchainBins;

  # No optional features: the portable protocol core only. This is the canary — if it
  # stops linking, the toolchain broke, not the render/browser stack.
  castaway = mkCastaway { pname = "castaway-windows"; };

  # DX12 compositor + winit kiosk, no browser. Useful on its own for bisecting a render
  # problem without the browser runtime's hundreds of MB in the way.
  castaway-render = mkCastaway {
    pname = "castaway-windows-render";
    features = [ "render" ];
    withFfmpeg = true;
  };

  # Same, plus the D3D11VA → shared-NV12-texture → D3D12 decode bridge. It exists as its
  # own artifact because it is the *only* part of Q20 that Linux cannot exercise: the
  # VA-API half is proven by an offscreen readback test on the dev box, but the Windows
  # half has no substitute for the Dell. Building it here at least keeps the winapi/d3d12
  # interop compiling, so it does not rot silently between visits to the panel.
  castaway-hwaccel = mkCastaway {
    pname = "castaway-windows-hwaccel";
    features = [ "hwaccel" ];
    withFfmpeg = true;
  };

  # The full deploy artifact: render + the offscreen Electron browser (YouTube leanback via DIAL).
  castaway-electron = mkCastaway {
    pname = "castaway-windows-electron";
    features = [ "electron" ];
    withFfmpeg = true;
    withBrowser = true;
  };

  # One check per artifact — the staging differs between them, so each needs its own.
  checks = {
    castaway-windows-dll-closure = mkBundleCheck castaway;
    castaway-windows-render-dll-closure = mkBundleCheck castaway-render;
    castaway-windows-hwaccel-dll-closure = mkBundleCheck castaway-hwaccel;
    castaway-windows-electron-dll-closure = mkBundleCheck castaway-electron;
  };

  # Cross dev shell: `nix develop .#windows` then plain `cargo build`, which picks the
  # target up from CARGO_BUILD_TARGET. Incremental, unlike rebuilding through Nix.
  devShell = pkgs.mkShell (crossEnv // hostCcEnv // {
    nativeBuildInputs = [ rustToolchain pkgs.cargo-xwin ] ++ toolchainBins;
    # Escape hatch: `cargo xwin build` reuses the pinned sysroot instead of downloading
    # its own, because the derivation leaves cargo-xwin's `DONE` marker in place.
    XWIN_CACHE_DIR = "${sysroot}";
  });
}
