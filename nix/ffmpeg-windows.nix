# Prebuilt Windows libav* libraries for the cross-build, laid out the way
# `ffmpeg-sys-next` expects to find them via `FFMPEG_DIR`.
#
# Why prebuilt rather than source-built: nixpkgs marks `pkgsCross.mingwW64.ffmpeg` broken
# on 64-bit MinGW, and it is broken — trimming it down to a decode-only build just walks
# into the next transitive dependency that has no mingw platform support. docs/cross-build.md
# already calls this out and picks the same escape hatch, with the note that wrapping the
# archive as a fixed-output derivation is what keeps it reproducible. That's this file.
#
# The build is pinned to an immutable `autobuild-*` release tag, *not* BtbN's rolling
# `latest` tag, whose assets are replaced daily and would break `outputHash` at random.
#
# LGPL rather than GPL: we only decode, and the LGPL build keeps the receiver's licensing
# options open. The DLLs must ship next to castaway.exe — see `dlls` below.
{ lib, stdenvNoCC, fetchurl, unzip }:

let
  # BtbN release tag + the exact asset inside it. Both must be bumped together.
  releaseTag = "autobuild-2026-07-24-13-32";
  asset = "ffmpeg-n7.1.5-10-g2aefd64d48-win64-lgpl-shared-7.1";

  src = fetchurl {
    url = "https://github.com/BtbN/FFmpeg-Builds/releases/download/${releaseTag}/${asset}.zip";
    hash = "sha256-uAnlYSVMwGNNn+TORpwCrC4ZTVYOB5uyQrlpBpSP++s=";
  };
in
stdenvNoCC.mkDerivation {
  pname = "ffmpeg-windows-x64";
  # ffmpeg-next/ffmpeg-sys-next 7.1 expects the 7.1 ABI (libavcodec 61, libavutil 59).
  version = "7.1.5";

  inherit src;
  nativeBuildInputs = [ unzip ];
  sourceRoot = asset;

  # The archive already ships `include/`, `lib/*.lib` (MSVC-format import libraries, which
  # is why BtbN's shared builds are usable from lld-link at all) and `bin/*.dll`. That is
  # exactly the FFMPEG_DIR layout, so this is a straight copy — no restructuring.
  installPhase = ''
    runHook preInstall
    mkdir -p "$out"
    cp -r include lib bin "$out/"
    runHook postInstall
  '';

  # Nothing here is an ELF object; skip the fixup machinery that would try to treat the
  # PE files as such.
  dontFixup = true;

  meta = {
    description = "Prebuilt LGPL ffmpeg ${asset} import libs + DLLs for x86_64-pc-windows-msvc";
    license = lib.licenses.lgpl21Plus;
    platforms = lib.platforms.all;
  };
}
