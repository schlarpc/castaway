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
{ pkgs, craneLib, commonArgs, rustToolchain }:

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
  ffmpeg = pkgs.callPackage ./ffmpeg-windows.nix { };

  # clang-unwrapped, not the nixpkgs `clang` wrapper: the wrapper injects glibc include
  # paths and host flags that make no sense when the target is Windows. clang's own
  # builtin headers (stddef.h, the intrin headers) still come along via its resource dir.
  toolchainBins = [
    pkgs.llvmPackages.clang-unwrapped # clang-cl
    pkgs.lld # lld-link
    pkgs.llvm # llvm-lib, llvm-rc, llvm-dlltool
  ];

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

  # `/imsvc` marks these as system includes, which suppresses warnings from Microsoft's
  # headers. clang-cl takes the path as a separate argument, hence the space.
  clFlags = lib.concatStringsSep " " ([
    "--target=${target}"
    "-Wno-unused-command-line-argument"
    "-fuse-ld=lld-link"
  ] ++ map (dir: "/imsvc ${dir}") includeDirs);

  # bindgen drives libclang directly rather than the clang-cl driver, so it wants plain
  # `-I` and an explicit target.
  bindgenFlags = lib.concatStringsSep " "
    ([ "--target=${target}" ] ++ map (dir: "-I${dir}") includeDirs);

  # The cross environment, shared by the package build and the cross dev shell.
  crossEnv = {
    CARGO_BUILD_TARGET = target;

    # rustc invokes the linker itself; it needs both the flavor (so it emits MSVC-style
    # `/LIBPATH:` arguments rather than GNU `-L`) and the library search paths.
    "CARGO_TARGET_${envTargetUpper}_LINKER" = "lld-link";
    "CARGO_TARGET_${envTargetUpper}_RUSTFLAGS" = lib.concatStringsSep " "
      ([ "-C" "linker-flavor=lld-link" ] ++ map (dir: "-Lnative=${dir}") libDirs);

    # The `cc` crate picks these up when a build script compiles C/C++ for the target.
    "CC_${envTarget}" = "clang-cl";
    "CXX_${envTarget}" = "clang-cl";
    "AR_${envTarget}" = "llvm-lib";
    "CFLAGS_${envTarget}" = clFlags;
    "CXXFLAGS_${envTarget}" = "${clFlags} /EHsc";
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

  # Dependency artifacts must be built for the same target as the final binary, so this
  # cross build gets its own `buildDepsOnly` rather than reusing the native one.
  crossArgs = commonArgs // crossEnv // {
    strictDeps = true;
    nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ toolchainBins;
    # Windows binaries can't be executed on the Linux builder.
    doCheck = false;
  };

  # Cargo refuses `--features` at the root of a virtual workspace, so every feature-
  # selecting build has to name the package too.
  mkCastaway = { pname, features ? [ ], withFfmpeg ? false }:
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
      '';
    });

in
{
  inherit sysroot crossEnv target toolchainBins;

  # No optional features: the portable protocol core only. This is the canary — if it
  # stops linking, the toolchain broke, not the render/browser stack.
  castaway = mkCastaway { pname = "castaway-windows"; };

  # The real deploy artifact: DX12 compositor + winit kiosk.
  castaway-render = mkCastaway {
    pname = "castaway-windows-render";
    features = [ "render" ];
    withFfmpeg = true;
  };

  # Cross dev shell: `nix develop .#windows` then plain `cargo build`, which picks the
  # target up from CARGO_BUILD_TARGET. Incremental, unlike rebuilding through Nix.
  devShell = pkgs.mkShell (crossEnv // {
    nativeBuildInputs = [ rustToolchain pkgs.cargo-xwin ] ++ toolchainBins;
    # Escape hatch: `cargo xwin build` reuses the pinned sysroot instead of downloading
    # its own, because the derivation leaves cargo-xwin's `DONE` marker in place.
    XWIN_CACHE_DIR = "${sysroot}";
  });
}
