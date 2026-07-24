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
# The version must stay in lockstep with the `cef`/`cef-dll-sys` crates in
# crates/pipeline/Cargo.toml — bump all three together.
{ lib, stdenvNoCC, fetchurl }:

let
  cefVersion = "147.0.10";
  archiveName = "cef_binary_${cefVersion}+gd58e84d+chromium-147.0.7727.118_windows64_minimal";

  src = fetchurl {
    # `+` has to be percent-encoded or the CDN reads it as a space.
    url = "https://cef-builds.spotifycdn.com/${lib.replaceStrings ["+"] ["%2B"] archiveName}.tar.bz2";
    hash = "sha256-Qapn73n50gJjbl8jZcaPRH3nsmschObzGLvcwOA3wWw=";
  };
in
stdenvNoCC.mkDerivation {
  pname = "cef-windows-x64";
  version = cefVersion;

  inherit src;
  sourceRoot = archiveName;

  installPhase = ''
    runHook preInstall

    mkdir -p "$out"
    cp -r . "$out/"

    # `sha1` is only consulted by cef-dll-sys' downloader to verify an archive it fetched
    # itself; the version parsed out of `name` is what the CEF_PATH check actually reads.
    # Nix already guarantees integrity via the fetchurl hash above.
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
