# The CEF Windows binary distribution, arranged so `cef-dll-sys` accepts it via `CEF_PATH`.
#
# This is *not* the same shape as the Linux `cefDist` in flake.nix. On Linux, cef-dll-sys
# only emits `-l cef` and wants a flattened directory. On Windows it additionally runs
# CMake over the distribution to build `libcef_dll_wrapper` — the C++ shim that adapts
# CEF's C API to C++ — so the full upstream layout has to survive intact:
#
#   CMakeLists.txt  cmake/  include/  libcef_dll/   ← needed to build the wrapper
#   Release/  Resources/                            ← libcef.dll/.lib, ICU data, .pak files
#
# Two additions on top of the upstream tree:
#
#   archive.json  — cef-dll-sys refuses a CEF_PATH without one, and treats a version
#                   newer than the crate's as a mismatch. Normally its downloader writes
#                   this; we hand-write it because we supply the distribution ourselves.
#   libcef.lib    — the build script emits `rustc-link-search=native={CEF_PATH}` (the root)
#                   but `rustc-link-lib=dylib=libcef`, and upstream puts libcef.lib under
#                   Release/. Link it at the root so the search path actually finds it.
#
# The archive itself is the `cef-windows-src` flake input, so the URL and its hash live in
# flake.nix/flake.lock. Only the unpacking and the layout fixups are here.
#
# One CEF version is pinned in three unrelated places, and they must agree:
#
#   nixpkgs `cef-binary`  — what the Linux dev shell runs against (flake.nix `cefDist`)
#   this file             — what ships on Windows
#   `cef`/`cef-dll-sys`   — crates/pipeline/Cargo.toml
#
# Only the first moves on its own, on `nix flake update`. cef-dll-sys already enforces the
# third against ours by parsing archive.json below; the assert enforces the first.
{ lib, stdenvNoCC, src, cef-binary }:

let
  cefVersion = "147.0.10";
  archiveName = "cef_binary_${cefVersion}+gd58e84d+chromium-147.0.7727.118_windows64_minimal";
in
assert lib.assertMsg (cef-binary.version == cefVersion) ''
  cef-windows: nixpkgs cef-binary is ${cef-binary.version} but this pins ${cefVersion}.
  Linux dev and the Windows artifact would then run different CEF builds, which turns
  every browser bug into "does it reproduce on the box?". Bump the `cef-windows-src` flake
  input (and the `cef`/`cef-dll-sys` crates, if the major moved), or pin nixpkgs back.
'';
stdenvNoCC.mkDerivation {
  pname = "cef-windows-x64";
  version = cefVersion;

  inherit src;

  # A `file+` flake input is the raw archive, and it lands in the store named bare
  # `source` — no extension for stdenv's unpackPhase to dispatch on — so unpack by hand.
  # `archiveName` is duplicated from the flake input's URL out of necessity (flake input
  # URLs have to be literals), so checking for the directory keeps the two in step.
  unpackPhase = ''
    runHook preUnpack
    tar -xf "$src"
    if [ ! -d ${archiveName} ]; then
      echo "cef-windows-src does not contain ${archiveName}/; it has: $(echo */)" >&2
      echo "Bump \`archiveName\` here to match the URL in flake.nix." >&2
      exit 1
    fi
    cd ${archiveName}
    runHook postUnpack
  '';

  installPhase = ''
    runHook preInstall

    mkdir -p "$out"
    cp -r . "$out/"

    # `sha1` is only consulted by cef-dll-sys' downloader to verify an archive it fetched
    # itself; the version parsed out of `name` is what the CEF_PATH check actually reads.
    # Nix already guarantees integrity via the flake input's locked hash.
    cat > "$out/archive.json" <<'JSON'
    {
      "type": "minimal",
      "name": "${archiveName}",
      "sha1": "af0bd26423b06c5f3f172c66bfef466f035ea3e1"
    }
    JSON

    ln -s Release/libcef.lib "$out/libcef.lib"

    runHook postInstall
  '';

  dontFixup = true;

  meta = {
    description = "CEF ${cefVersion} Windows x64 minimal distribution for cross-compilation";
    license = lib.licenses.bsd3;
    platforms = lib.platforms.all;
  };
}
