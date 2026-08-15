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
{ pkgs, craneLib, commonArgs, rustToolchain, gitRev, buildNumber, ffmpegSrc, electronSrc, widevineSrc
, bluetoothFirmware, authenticode }:

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
  # pre-staging that makes first-boot DRM work offline (D36/#66).
  stageBrowser = ''
    # The whole ECS distribution, unmodified: it is what EVS signs, and a modified tree
    # invalidates the VMP signature. Our host app travels beside it.
    mkdir -p "$out/bin/browser"
    cp -r --no-preserve=mode,ownership ${electron}/* "$out/bin/browser/"
    mkdir -p "$out/bin/browser-host"
    cp -r --no-preserve=mode,ownership ${../browser-host}/* "$out/bin/browser-host/"
    install -Dm644 ${./castaway.exe.manifest} "$out/bin/castaway.exe.manifest"
    # The signing step travels with the artifact rather than living only in the repo: it
    # needs the tree beside it, and CI unpacks the artifact and runs *this* copy (#344),
    # so a release signed in CI and a tree signed by hand after a deploy go through the
    # same script.
    install -Dm755 ${../browser-host/vmp-sign.sh} "$out/bin/vmp-sign.sh"
  '' + lib.optionalString (widevine != null) ''
    # Staged for the receiver to copy into the browser profile on first run, not loaded
    # from here: ECS finds its CDM under `<userDataDir>/WidevineCdm/<version>/`, which is
    # a runtime path. `stageWidevine()` in browser-host/main.js does the copy and finds
    # this directory beside the exe — there is no wrapper here to hand it a path, which is
    # why the location matters. #66 has the measurement.
    cp -r --no-preserve=mode,ownership ${widevine}/WidevineCdm "$out/bin/"
  '';

  # The deploy tree as one zip, for getting onto the Windows box, which has no Nix store
  # to copy `result` into: `nix build .#castaway-windows-electron.archive` →
  # `result/castaway-windows-electron.zip`, unzipping to a single folder. zip rather than
  # tar because Explorer opens it.
  #
  # `pname` is passed rather than read off `pkg`, so that `pkg` can be the *same*
  # derivation the package attribute exposes without the reference cycle that reading
  # `pkg.pname` from inside its own `passthru` would create. Interpolating `${pkg}` only
  # forces its output path, which does not force the passthru back.
  mkArchive = pname: pkg: pkgs.runCommand "${pname}-archive"
    {
      nativeBuildInputs = [ pkgs.zip ];
      meta.description = "The ${pname} deploy tree as a single zip";
    } ''
    cp -rL --no-preserve=mode,ownership ${pkg}/bin ${pname}
    # The store's 1970 mtimes predate the zip format's 1980 DOS epoch; pin them at the
    # epoch rather than letting zip clamp them with a warning per file. `-X` and the
    # sorted name list keep the rebuild byte-identical.
    find ${pname} -exec touch -d '1980-01-01 00:00:00 UTC' {} +
    mkdir -p "$out"
    find ${pname} | sort | zip -qX "$out/${pname}.zip" -@
  '';

  # All four artifacts extend one bare dependency tree instead of each compiling every
  # dependency from scratch: the feature variants differ from the canary only by what
  # their features drag in. Crane's `buildDepsOnly` cannot chain, hence the helper.
  depsOnlyFrom = import ./deps-only-from.nix { inherit craneLib lib; };
  crossBaseArtifacts = craneLib.buildDepsOnly (crossArgs // {
    pname = "castaway-windows";
    cargoExtraArgs = "--package castaway --no-default-features";
  });

  # The stable binary the box's scheduled task points at (#342). Its own derivation
  # rather than a second `--package` on the receiver's: crane builds one package per
  # derivation, and this one wants none of the receiver's features, none of ffmpeg and
  # none of the browser — it is a few hundred lines that spawn a process and wait.
  #
  # It reuses the receiver's dependency tree because it shares nearly all of it
  # (`castaway-paths`, `thiserror`, `time`); the `windows` crate is the only thing it
  # compiles on its own.
  launcher = craneLib.buildPackage (crossArgs // {
    pname = "castaway-launcher-windows";
    cargoArtifacts = crossBaseArtifacts;
    cargoExtraArgs = "--package castaway-launcher --bins";
  });

  # Cargo refuses `--features` at the root of a virtual workspace, so every feature-
  # selecting build has to name the package too.
  mkCastaway =
    { pname
    , features ? [ ]
    , withFfmpeg ? false
    , withBrowser ? false
    , withLauncher ? false
    }:
    let
      # `--no-default-features` because the default set is now everything (D55), and two
      # of its entries are Linux-only: `audio-pipewire`, whose dependency table does not
      # cross, and `bluetooth-socket`, which is the kernel HCI socket. Windows names what
      # it wants instead of subtracting.
      cargoExtraArgs = "--package castaway --no-default-features"
        + lib.optionalString (features != [ ])
        " --features ${lib.concatStringsSep "," features}";
      args = crossArgs // { inherit cargoExtraArgs; };
      pkg = craneLib.buildPackage (args // {
        inherit pname;

        # `fixupPhase` strips every PE under `$out/bin`, and `stageBrowser` runs in
        # `postInstall` — *before* fixup — so the castLabs ECS distribution was being
        # stripped along with our own binaries. That rewrites `electron.exe` and every
        # DLL beside it, which silently falsifies the one property the browser tree has
        # to have: `stageBrowser`'s "the whole ECS distribution, unmodified" and
        # docs/cross-build.md's "byte-for-byte as castLabs shipped it" were both untrue
        # in the artifact that shipped. The symptom was EVS refusing to VMP-sign it —
        # `ValidityError: Binary signature denied`, because a stripped binary is not a
        # build castLabs recognises — and before the certificate existed nothing ever ran
        # the signing step, so nothing noticed (#348).
        #
        # Blanket rather than surgical because `stripDirs` has no exclude, and because
        # stripping buys nothing here anyway: these are MSVC-linked PEs whose debug info
        # lives in separate PDBs that the artifact does not carry.
        dontStrip = true;

        cargoArtifacts =
          if features == [ ] then
            crossBaseArtifacts
          else
            depsOnlyFrom crossBaseArtifacts (args // { inherit pname; });

        # The revision the idle screen's footer shows — final builds only, never
        # `crossArgs`, where the deps trees would inherit it and rebuild each commit.
        CASTAWAY_GIT_REV = gitRev;

        # The build number the auto-updater orders against (#343). Same placement and
        # the same reason as the revision above: a value that moves every commit must
        # not reach the dependency trees.
        CASTAWAY_BUILD = buildNumber;

        # Where `hci-transport`'s build.rs finds controller firmware to embed. Windows has
        # no /lib/firmware, so the blobs have to ride inside the .exe (architecture 11.3b).
        # This was set in the devShell and *only* there, so every shipped artifact was
        # built with an empty firmware set and said so at startup — "no bluetooth firmware
        # in this build; only ROM-based controllers will initialise" — which reads like a
        # deliberate build choice rather than a wiring gap.
        CASTAWAY_FIRMWARE_DIR = bluetoothFirmware;

        # Windows has no rpath and no /nix/store to resolve against: the loader looks for
        # DLLs next to the .exe. Anything dynamically linked has to be copied in, or the
        # binary dies at startup on the deploy box with a missing-DLL dialog.
        postInstall = lib.optionalString withFfmpeg ''
          cp ${ffmpeg}/bin/*.dll "$out/bin/"
        '' + lib.optionalString withLauncher ''
          # Beside the receiver, so a release zip carries both. The box installs the
          # launcher once, at the install root (#346), and every version tree afterwards
          # carries a copy it does not run — deliberately: it costs a few hundred
          # kilobytes, and a launcher that ever does have to be replaced is then already
          # sitting in the version that wants it rather than needing its own download.
          cp ${launcher}/bin/launcher.exe "$out/bin/"
        '' + lib.optionalString withBrowser stageBrowser;
      });

      # `.archive` rides along on every artifact (`nix build .#<name>.archive`) instead of
      # doubling the package set in flake.nix. passthru only, so it costs nothing unless
      # asked for and doesn't change the package's own hash.
      #
      # It zips `final`, not `pkg`. Zipping `pkg` made `.archive` depend on the *un*-
      # overridden derivation, which is a different store path from the one the package
      # attribute exposes — so `nix build .#x .#x.archive` linked the Windows binary twice
      # and produced two binaries that were not bit-identical. The self-reference is safe
      # because interpolating `final` forces only its output path; `pname` comes from the
      # function argument precisely so nothing has to read an attribute back off `final`.
      final = pkg.overrideAttrs (prev: {
        passthru = (prev.passthru or { }) // { archive = mkArchive pname final; };
      });
    in
    final;

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
    # Core Audio's device API, in System32 since Vista. The `audio-out` build imports
    # exactly one symbol from it — `ActivateAudioInterfaceAsync`, minimum supported
    # client Windows 8 — which is how cpal's WASAPI host opens the endpoint the settings
    # screen selected. Everything else in that path arrives through COM via ole32, so this
    # is the only WASAPI name that shows up as a link-time import at all.
    "mmdevapi.dll"
    "ntdll.dll"
    "ole32.dll"
    "oleaut32.dll"
    "opengl32.dll"
    "powrprof.dll"
    "propsys.dll"
    "secur32.dll"
    "setupapi.dll"
    # The USB pair `nusb` needs, and so `hci-transport`'s Bluetooth transport. Both were
    # omitted by oversight rather than judgement — their sibling `setupapi.dll` is right
    # above — and `nix flake check` had been red on them since the USB transport landed
    # (#110).
    #
    # `cfgmgr32.dll` is the configuration manager (`CM_Get_Device_*`), used to enumerate
    # devices; in System32 since Vista. `winusb.dll` is WinUSB's user-mode half, and it is
    # in-box for the same span: "WinUSB is a generic driver for USB devices that is
    # included with all versions of Windows since Windows Vista", whose INF "references the
    # in-box driver, Winusb.sys, found in Windows\System32 folder".
    #
    # Worth separating two things that are easy to conflate, because getting it wrong
    # argues for shipping a DLL that cannot be shipped: what `nix run .#windows-winusb`
    # installs is not the library, it is the *binding* of WinUSB to a particular device,
    # which is a driver-ranking problem against the inbox WHQL driver. The library is
    # already there on any supported Windows; only the binding is ours to arrange.
    "cfgmgr32.dll"
    "winusb.dll"
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
  #
  # Every `.exe` in the tree, not just the receiver's. `launcher.exe` (#342) is the one
  # binary the box's scheduled task names, so a missing DLL *there* is a panel that never
  # starts at all rather than one that starts and misbehaves.
  mkBundleCheck = pkg: pkgs.runCommand "${pkg.pname}-dll-closure"
    {
      nativeBuildInputs = [ pkgs.llvm ];
      meta.description = "Every DLL ${pkg.pname} imports is staged or OS-provided";
    } ''
    for exe in ${pkg}/bin/*.exe; do
      llvm-readobj --coff-imports "$exe"
    done \
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

  # Ground rule 6 again, for the other thing that only fails on the panel: an artifact
  # whose Authenticode signature was never actually applied looks exactly like one that
  # was, from here. This signs the *real* cross-built .exe with a throwaway certificate
  # and reads the signature back.
  #
  # A throwaway rather than the release certificate, which is a secret this build cannot
  # and should not have. What it proves is everything that is not the key: that
  # `osslsigncode` accepts a PE this toolchain emits, that our flag set produces a
  # signature `osslsigncode verify` reads back, that the code-signing EKU survives the
  # PKCS#12 round trip, and that the publisher check refuses a certificate the box does
  # not trust. The certificate itself is #348's problem and no check can stand in for it.
  mkAuthenticodeCheck = pkg: pkgs.runCommand "${pkg.pname}-authenticode"
    {
      nativeBuildInputs = [ authenticode pkgs.openssl pkgs.osslsigncode pkgs.coreutils ];
      meta.description = "${pkg.pname}'s executables take an Authenticode signature that verifies";
    } ''
    # Two certificates: the one that signs, and one that stands in for a rotation nobody
    # updated the tree for.
    for name in signer other; do
      openssl req -x509 -newkey rsa:2048 -sha256 -days 1 -noenc \
        -keyout "$name.key" -out "$name.crt" -subj "/CN=castaway check $name" \
        -addext 'basicConstraints=critical,CA:false' \
        -addext 'keyUsage=critical,digitalSignature' \
        -addext 'extendedKeyUsage=critical,codeSigning' 2>/dev/null
    done
    openssl pkcs12 -export -out signer.pfx -inkey signer.key -in signer.crt \
      -name 'castaway check' -passout pass:check

    export CASTAWAY_CODESIGN_PFX="$(base64 -w0 < signer.pfx)"
    export CASTAWAY_CODESIGN_PASSWORD=check

    # Both of ours — the receiver and the launcher (#342) — because they are built from
    # different derivations and only one of them was ever going to be remembered.
    ours=()
    for exe in ${pkg}/bin/*.exe; do
      install -m644 "$exe" "$(basename "$exe")"
      ours+=("$(basename "$exe")")
      # The store copies are unsigned, and `verify` says so — which is what makes the
      # positive result below mean something rather than being true of any PE.
      if osslsigncode verify -in "$(basename "$exe")" -CAfile signer.crt >/dev/null 2>&1; then
        echo "an unsigned $(basename "$exe") verified; this check proves nothing" >&2
        exit 1
      fi
    done
    [ "''${#ours[@]}" -ge 2 ] || {
      echo "expected the receiver and the launcher, found: ''${ours[*]}" >&2; exit 1; }

    CASTAWAY_CODESIGN_CERT=signer.crt castaway-windows-authenticode "''${ours[@]}"
    for exe in "''${ours[@]}"; do
      osslsigncode verify -in "$exe" -CAfile signer.crt > "verify-$exe.txt" 2>&1 || {
        cat "verify-$exe.txt" >&2; exit 1; }
      grep -q 'Signature verification: ok' "verify-$exe.txt" \
        || { cat "verify-$exe.txt" >&2; exit 1; }
    done

    # And the publisher check: signing with a key the checked-in certificate does not
    # name has to fail here rather than on a box that then refuses the binary.
    install -m644 ${pkg}/bin/castaway.exe again.exe
    if CASTAWAY_CODESIGN_CERT=other.crt castaway-windows-authenticode again.exe \
         >mismatch.txt 2>&1; then
      echo "signing succeeded against a certificate the box does not trust" >&2
      exit 1
    fi
    grep -q 'Refusing to sign' mismatch.txt || { cat mismatch.txt >&2; exit 1; }

    cat verify-*.txt > "$out"
  '';

in
rec {
  inherit sysroot crossEnv target toolchainBins;

  # The deploy artifact, and now the only one (D55).
  #
  # There used to be four: a bare canary, `render`, `hwaccel`, and this. Three of them
  # were subsets — `electron` implies `render` and `hwaccel` — so they were bisecting aids
  # that cost three cross-compiles per CI run to tell you which layer broke, which the
  # full build's log says anyway. The canary's job (toolchain vs stack) is the same
  # question one link error answers. #58's D3D11VA → shared-NV12 → D3D12 bridge still
  # compiles here, because `electron` pulls `hwaccel` in; that was the one variant with a
  # reason of its own, and it is preserved by implication rather than by a second artifact.
  #
  # The full deploy artifact: render + the offscreen Electron browser (YouTube leanback
  # via DIAL), and a real PCM device. `audio-out` is WASAPI here (cpal reaches it
  # through the OS, no extra DLLs), which is also what makes the settings screen's
  # output-device selection mean something on the panel. `audio-pipewire` is deliberately
  # absent: its dependency table is Linux-only and the feature would be inert weight.
  castaway-electron = mkCastaway {
    pname = "castaway-windows-electron";
    features = [ "electron" "audio-out" ];
    withFfmpeg = true;
    withBrowser = true;
    withLauncher = true;
  };

  # One artifact, two checks: what the loader will look for, and what Windows will make
  # of the signature. Both are static answers to questions that otherwise only get asked
  # on the panel.
  checks = {
    castaway-windows-electron-dll-closure = mkBundleCheck castaway-electron;
    castaway-windows-electron-authenticode = mkAuthenticodeCheck castaway-electron;
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
