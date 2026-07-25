# Linux → Windows cross-build (`x86_64-pc-windows-msvc`), per docs/cross-build.md.
#
# We target MSVC rather than MinGW because CEF's import libs are MSVC-format and
# windows-rs/WinRT expects MSVC. `cargo-xwin` is the usual turnkey answer, but it wants a
# writable cache it can download into at build time — which a Nix sandbox does not have.
# So we do what cargo-xwin does, statically: take the pinned sysroot from
# ./msvc-sysroot.nix and export the same compiler/linker environment ourselves. The flag
# conventions below are deliberately kept identical to cargo-xwin's `clang_cl.rs`, so its
# behaviour remains a reference (and `XWIN_CACHE_DIR=<sysroot>` still works as a fallback).
#
# The toolchain is all LLVM: clang-cl as the C/C++ driver, lld-link as the linker,
# llvm-lib as the archiver, llvm-rc for resources.
#
# `ffmpegSrc`/`cefSrc` are the raw archives, pinned as flake inputs so they land in
# flake.lock; the derivations beside this file unpack and rearrange them.
{ pkgs, craneLib, commonArgs, rustToolchain, ffmpegSrc, cefSrc }:

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
  cef = pkgs.callPackage ./cef-windows.nix { src = cefSrc; };

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

  # One knob for the CRT, because both halves of the build must agree on it. Rust's std and
  # CEF's C++ wrapper end up in the same image, and two CRTs there means two heaps and two
  # errno/locale states — memory allocated on one side and freed on the other corrupts. This
  # has to be right by construction: lld-link resolves the mismatch without a diagnostic and
  # lets it fail at runtime instead.
  #
  # Static, because the deploy target is an appliance — a static CRT needs no Visual C++
  # redistributable installed on the box. It's also CEF's own default for the wrapper (/MT),
  # so it's the configuration upstream actually tests.
  crtStatic = true;

  # Windows sources are written against a case-insensitive filesystem, so an `#include` may
  # spell a header differently from the file on disk. xwin symlinks every SDK header under
  # its all-lowercase name, which covers most of it, but not mixed-case misspellings.
  # Shimming the spelling that's actually asked for beats patching third-party sources.
  #
  # To regenerate: list every `#include` name in the sources being cross-compiled that has
  # no exact match in the sysroot but does have a case-insensitive one.
  miscasedHeaders = [
    "Softpub.h" # libcef_dll/wrapper/cef_certificate_util_win.cc; on disk as SoftPub.h
  ];

  # Fails the build rather than silently skipping if a name matches nothing at all — a
  # typo'd entry here would otherwise turn into a confusing missing-header error later.
  includeShims = pkgs.runCommand "msvc-include-case-shims" { } ''
    mkdir -p "$out"
    for want in ${lib.escapeShellArgs miscasedHeaders}; do
      for dir in ${lib.escapeShellArgs includeDirs}; do
        actual=$(find -L "$dir" -maxdepth 1 -iname "$want" -print -quit)
        if [ -n "$actual" ]; then ln -s "$actual" "$out/$want"; break; fi
      done
      if [ ! -e "$out/$want" ]; then
        echo "no case-insensitive match for '$want' in the sysroot include path" >&2
        exit 1
      fi
    done
  '';

  allIncludeDirs = [ "${includeShims}" ] ++ includeDirs;

  # `/imsvc` marks these as system includes, which suppresses warnings from Microsoft's
  # headers. clang-cl takes the path as a separate argument, hence the two list elements.
  leadingFlags = [
    "--target=${target}"
    "-Wno-unused-command-line-argument"
    "-fuse-ld=lld-link"
  ] ++ lib.concatMap (dir: [ "/imsvc" dir ]) allIncludeDirs;

  # CEF compiles with `/WX`, calibrated against MSVC's `/W4`. clang-cl maps `/W4` onto a
  # *different* warning set, so warnings MSVC never emits (`-Wmissing-field-initializers`
  # firing on CEF's own `include/internal/cef_types_wrappers.h`) become hard errors in
  # code we don't own and won't patch. Demote them back to warnings rather than playing
  # whack-a-mole with `-Wno-` for each divergence.
  trailingFlags = [ "-Wno-error" ];

  # bindgen drives libclang directly rather than the clang-cl driver, so it wants plain
  # `-I` and an explicit target.
  bindgenFlags = lib.concatStringsSep " "
    ([ "--target=${target}" ] ++ map (dir: "-I${dir}") allIncludeDirs);

  # A wrapper, not bare clang-cl, because CEF's `cmake/cef_variables.cmake` *overwrites*
  # CMAKE_C_FLAGS/CMAKE_CXX_FLAGS wholesale rather than appending — so anything the
  # toolchain file sets there is silently discarded before a single object is compiled.
  # Baking the cross setup into the driver itself makes it un-loseable no matter how a
  # third-party build system manipulates its flag variables. Same idea as the nixpkgs
  # cc-wrapper.
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
    # cef-dll-sys builds CEF's C++ wrapper through CMake + Ninja.
    pkgs.cmake
    pkgs.ninja
  ];

  # CMake needs the same cross setup as cargo, expressed its own way. Only `cef-dll-sys`
  # uses this — it compiles CEF's C++ `libcef_dll_wrapper` rather than just linking a
  # prebuilt library. Modelled on the toolchain file cargo-xwin generates.
  #
  # `CMAKE_*_STANDARD_LIBRARIES` is cleared deliberately: CMake otherwise injects a list
  # of default Windows libs with inconsistent casing, which then need case-correcting
  # symlinks in the sysroot. CEF's build declares what it needs explicitly.
  #
  # The compilers are absolute paths into the `clangCl` wrapper rather than a bare name:
  # clang-unwrapped also ships a `clang-cl`, and which one a bare name resolves to is a
  # PATH-ordering accident. The cross flags themselves live in the wrapper, since CEF
  # overwrites CMAKE_*_FLAGS anyway — see the `clangCl` comment.
  cmakeToolchain = pkgs.writeText "${target}-toolchain.cmake" ''
    set(CMAKE_SYSTEM_NAME Windows)
    set(CMAKE_SYSTEM_PROCESSOR AMD64)

    set(CMAKE_C_COMPILER ${clangCl}/bin/clang-cl CACHE FILEPATH "")
    set(CMAKE_CXX_COMPILER ${clangCl}/bin/clang-cl CACHE FILEPATH "")
    set(CMAKE_AR llvm-lib)
    set(CMAKE_LINKER lld-link CACHE FILEPATH "")
    set(CMAKE_RC_COMPILER llvm-rc CACHE FILEPATH "")

    set(LINK_FLAGS
        /manifest:no
        ${lib.concatStringsSep "\n    " (map (dir: ''-libpath:"${dir}"'') libDirs)})

    string(REPLACE ";" " " LINK_FLAGS "''${LINK_FLAGS}")

    set(CMAKE_EXE_LINKER_FLAGS "''${CMAKE_EXE_LINKER_FLAGS} ''${LINK_FLAGS}" CACHE STRING "" FORCE)
    set(CMAKE_MODULE_LINKER_FLAGS "''${CMAKE_MODULE_LINKER_FLAGS} ''${LINK_FLAGS}" CACHE STRING "" FORCE)
    set(CMAKE_SHARED_LINKER_FLAGS "''${CMAKE_SHARED_LINKER_FLAGS} ''${LINK_FLAGS}" CACHE STRING "" FORCE)

    set(CMAKE_C_STANDARD_LIBRARIES "" CACHE STRING "" FORCE)
    set(CMAKE_CXX_STANDARD_LIBRARIES "" CACHE STRING "" FORCE)

    # The C++ side of the CRT decision — see `crtStatic`. FORCE is required on
    # CMAKE_MSVC_RUNTIME_LIBRARY because cef-dll-sys sets it via a command-line -D, which
    # populates the cache before this file is read. CEF's own CEF_RUNTIME_LIBRARY_FLAG needs
    # no FORCE: it's a plain `set(... CACHE ...)` evaluated later, at find_package(CEF), so
    # seeding the entry here already wins.
    set(CMAKE_MSVC_RUNTIME_LIBRARY "${if crtStatic then "MultiThreaded" else "MultiThreadedDLL"}" CACHE STRING "" FORCE)
    set(CEF_RUNTIME_LIBRARY_FLAG "${if crtStatic then "/MT" else "/MD"}" CACHE STRING "")

    set(CMAKE_TRY_COMPILE_CONFIGURATION Release)
  '';

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
    RCFLAGS = lib.concatStringsSep " " (map (dir: "-I${dir}") allIncludeDirs);

    # clang-cl and lld-link resolve bare `foo.lib` names through LIB, the way the MSVC
    # toolchain does on Windows. Semicolons, not colons — this is a Windows-style path list.
    LIB = lib.concatStringsSep ";" libDirs;

    # `ffmpeg-sys-next` takes this branch instead of pkg-config, reading `include/` for
    # bindgen and `lib/` for the import libraries.
    FFMPEG_DIR = "${ffmpeg}";

    # Without this, cef-dll-sys downloads a CEF distribution at build time — which the
    # sandbox forbids, and which would defeat pinning anyway.
    CEF_PATH = "${cef}";
    CMAKE_GENERATOR = "Ninja";
    CMAKE_SYSTEM_NAME = "Windows";
    "CMAKE_TOOLCHAIN_FILE_${envTarget}" = "${cmakeToolchain}";
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

  # CEF's runtime layout is flat: everything beside the .exe. Upstream splits the
  # distribution into Release/ (libraries) and Resources/ (.pak, ICU data, locales) for its
  # own CMake build, but at runtime CEF resolves all of it relative to the module directory
  # — which is exactly where an empty `Settings::resources_dir_path` points it. cef_browser.rs
  # leaves those empty when CEF_PATH is unset, which is the case on the deploy box: there is
  # no /nix/store there to point at.
  #
  # bootstrap.exe/bootstrapc.exe are deliberately not staged. They're the entry point for
  # CEF's sandboxed "app is a DLL" mode; we initialize with `no_sandbox` and ship a real .exe.
  stageCef = ''
    install -Dm644 -t "$out/bin/" \
      ${cef}/Release/*.dll ${cef}/Release/*.bin ${cef}/Release/*.json
    install -Dm644 -t "$out/bin/" \
      ${cef}/Resources/*.pak ${cef}/Resources/icudtl.dat
    install -Dm644 -t "$out/bin/locales/" ${cef}/Resources/locales/*.pak
    install -Dm644 ${./castaway.exe.manifest} "$out/bin/castaway.exe.manifest"
  '';

  # Cargo refuses `--features` at the root of a virtual workspace, so every feature-
  # selecting build has to name the package too.
  mkCastaway = { pname, features ? [ ], withFfmpeg ? false, withCef ? false }:
    let
      cargoExtraArgs = "--package castaway"
        + lib.optionalString (features != [ ])
        " --features ${lib.concatStringsSep "," features}";
      args = crossArgs // { inherit cargoExtraArgs; };
    in
    craneLib.buildPackage (args // {
      inherit pname;
      cargoArtifacts = craneLib.buildDepsOnly args;

      # Windows has no rpath and no /nix/store to resolve against: the loader looks for
      # DLLs next to the .exe. Anything dynamically linked has to be copied in, or the
      # binary dies at startup on the deploy box with a missing-DLL dialog.
      postInstall = lib.optionalString withFfmpeg ''
        cp ${ffmpeg}/bin/*.dll "$out/bin/"
      '' + lib.optionalString withCef stageCef;
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
  # problem without CEF's ~200 MB of runtime in the way.
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

  # The full deploy artifact: render + the offscreen CEF browser (YouTube leanback via DIAL).
  castaway-cef = mkCastaway {
    pname = "castaway-windows-cef";
    features = [ "cef" ];
    withFfmpeg = true;
    withCef = true;
  };

  # One check per artifact — the staging differs between them, so each needs its own.
  checks = {
    castaway-windows-dll-closure = mkBundleCheck castaway;
    castaway-windows-render-dll-closure = mkBundleCheck castaway-render;
    castaway-windows-hwaccel-dll-closure = mkBundleCheck castaway-hwaccel;
    castaway-windows-cef-dll-closure = mkBundleCheck castaway-cef;
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
