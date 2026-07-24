# Prebuilt Windows libav* libraries for the cross-build, laid out the way
# `ffmpeg-sys-next` expects to find them via `FFMPEG_DIR`.
#
# Why prebuilt rather than source-built: nixpkgs marks `pkgsCross.mingwW64.ffmpeg` broken
# on 64-bit MinGW, and it is broken — trimming it down to a decode-only build just walks
# into the next transitive dependency that has no mingw platform support. docs/cross-build.md
# already calls this out and picks the same escape hatch, with the note that wrapping the
# archive as a fixed-output derivation is what keeps it reproducible. That's this file.
#
# The archive itself is the `ffmpeg-windows-src` flake input, so the URL and its hash live
# in flake.nix/flake.lock. Only the unpacking is here.
#
# LGPL rather than GPL: we only decode, and the LGPL build keeps the receiver's licensing
# options open. The DLLs must ship next to castaway.exe — see `stageCef`'s sibling copy in
# nix/windows.nix.
{ lib, stdenvNoCC, src, unzip }:

let
  # The asset name inside BtbN's release, which is also the archive's single top-level
  # directory. Duplicated from the flake input's URL out of necessity — flake input URLs
  # have to be literals — so unpackPhase checks the two still describe the same archive.
  asset = "ffmpeg-n7.1.5-10-g2aefd64d48-win64-lgpl-shared-7.1";
in
stdenvNoCC.mkDerivation {
  pname = "ffmpeg-windows-x64";
  # ffmpeg-next/ffmpeg-sys-next 7.1 expects the 7.1 ABI (libavcodec 61, libavutil 59).
  version = "7.1.5";

  inherit src;
  nativeBuildInputs = [ unzip ];

  # A `file+` flake input is the raw archive, and it lands in the store named bare
  # `source` — no extension for stdenv's unpackPhase to dispatch on — so unpack by hand.
  # Naming the expected directory also catches a flake.nix URL bump that forgot this file.
  unpackPhase = ''
    runHook preUnpack
    unzip -qq "$src"
    if [ ! -d ${asset} ]; then
      echo "ffmpeg-windows-src does not contain ${asset}/; it has: $(echo */)" >&2
      echo "Bump \`asset\` here to match the URL in flake.nix." >&2
      exit 1
    fi
    cd ${asset}
    runHook postUnpack
  '';

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
