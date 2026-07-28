# moonlight-common-c — the GameStream client core castaway *links* rather than
# reimplements (DECISION-LOG D37).
#
# This is the one place in the tree where a protocol's wire behaviour lives outside our
# own code. The carve-out is deliberate and narrow: `proto-gamestream` still owns the
# LAN-facing half (mDNS discovery, NVHTTP, the pairing crypto, the app chooser), and
# this library owns everything past `/launch` — RTSP, ENet, FEC'd RTP, input encoding.
#
# Built static, not shared, for the same reason the rest of the closure is: the deploy
# artifact is one binary plus a Nix closure, and a `.so` here would need wrapping on
# both platforms. `moonlight-sys/build.rs` reads `MOONLIGHT_COMMON_C_LIB_DIR` to find
# the archives this produces.
#
# The upstream CMake build defaults to a shared library and pulls its two submodules
# (the cgutman ENet fork and nanors) from the network, so both are pinned as their own
# flake inputs and grafted into place here — `cmake` never fetches anything.
{ pkgs, src, enetSrc, nanorsSrc }:

pkgs.stdenv.mkDerivation {
  pname = "moonlight-common-c";
  version = src.shortRev or "pinned";
  inherit src;

  nativeBuildInputs = [ pkgs.cmake ];
  # PlatformCrypto.c wants libcrypto for the AES-GCM/CBC the control, audio, and
  # encrypted-RTSP paths need. OpenSSL rather than mbedTLS because that is upstream's
  # default and the path Moonlight itself ships.
  buildInputs = [ pkgs.openssl ];

  postUnpack = ''
    # The submodules are empty directories in the tarball; fill them from the pinned
    # inputs rather than letting CMake reach for the network.
    rm -rf "$sourceRoot/enet" "$sourceRoot/nanors"
    cp -r ${enetSrc} "$sourceRoot/enet"
    cp -r ${nanorsSrc} "$sourceRoot/nanors"
    chmod -R u+w "$sourceRoot/enet" "$sourceRoot/nanors"
  '';

  cmakeFlags = [
    "-DBUILD_SHARED_LIBS=OFF"
    "-DCMAKE_POSITION_INDEPENDENT_CODE=ON"
  ];

  # Upstream builds with -Werror; a newer GCC than upstream tests against turns a
  # warning into a build failure that says nothing about our code. Dropping -Werror
  # here is the smallest change that keeps the compiler's warnings visible.
  env.NIX_CFLAGS_COMPILE = "-Wno-error";

  # The CMake project has no install target — it is consumed as a subdirectory. Take
  # the archives and the public header out by hand.
  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib $out/include
    find . -name 'libmoonlight-common-c.a' -exec cp {} $out/lib/ \;
    find . -name 'libenet.a' -exec cp {} $out/lib/ \;
    cp $NIX_BUILD_TOP/$sourceRoot/src/Limelight.h $out/include/
    runHook postInstall
  '';

  # A build that produced no archive would otherwise "succeed" and fail much later at
  # link time, in a crate that has nothing to do with the cause.
  postInstall = ''
    test -f $out/lib/libmoonlight-common-c.a || {
      echo "moonlight-common-c built no static archive — did BUILD_SHARED_LIBS stick?" >&2
      exit 1
    }
    test -f $out/lib/libenet.a || {
      echo "the bundled ENet fork built no static archive" >&2
      exit 1
    }
  '';

  meta = {
    description = "GameStream/Moonlight client core (RTSP, ENet control, FEC'd RTP video, Opus audio)";
    homepage = "https://github.com/moonlight-stream/moonlight-common-c";
    # GPL-3.0. Kept in its own derivation and linked, never vendored into this MIT
    # tree — the same quarantine crypto-playfair gets for the same reason.
    license = pkgs.lib.licenses.gpl3Only;
  };
}
