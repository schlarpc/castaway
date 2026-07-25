# Regenerates the Cast RTP fixtures with openscreen's own packetizer and proves the
# bytes checked into `crates/proto-cast/tests/fixtures/rtp-stream/` still match.
#
# Those fixtures are the oracle for `proto-cast`'s receiver: they are what makes the
# reimplementation demonstrably compatible rather than merely self-consistent. A stale
# fixture would quietly turn that guarantee off, so this check regenerates them from a
# pinned openscreen and fails if a single byte moved.
#
# Only nine openscreen translation units are needed — the RTP packetizer, the frame
# crypto, and their support code. openscreen's full GN build wants `gclient sync`, a
# CIPD-fetched toolchain, and a stack of vendored third-party libraries; none of that
# is required to exercise the two files we actually care about, so none of it happens.
{ pkgs, openscreenSrc }:

let
  # Paths inside the openscreen checkout. Each is here because the linker asked for it.
  translationUnits = [
    "cast/streaming/impl/rtp_packetizer.cc"
    "cast/streaming/impl/frame_crypto.cc"
    "cast/streaming/impl/rtp_defines.cc"
    "cast/streaming/public/encoded_frame.cc"
    "util/crypto/openssl_util.cc"
    "util/crypto/random_bytes.cc"
    "platform/impl/time.cc"
    "platform/base/error.cc"
    "platform/base/location.cc"
  ];

  fixtures = ../crates/proto-cast/tests/fixtures/rtp-stream;
in
pkgs.runCommandCC "openscreen-rtp-fixtures"
{
  # openscreen is written against BoringSSL, not OpenSSL: frame_crypto.cc calls
  # AES_ctr128_encrypt and CRYPTO_library_init, which OpenSSL 3 does not have.
  buildInputs = [ pkgs.boringssl ];
  inherit fixtures;
} ''
  set -euo pipefail

  echo "building the fixture generator against openscreen ${openscreenSrc.shortRev or "pinned"}"
  c++ -std=c++20 -O1 \
    -I${openscreenSrc} \
    "$fixtures/generator/gen_rtp_fixtures.cc" \
    "$fixtures/generator/logging_stub.cc" \
    ${pkgs.lib.concatMapStringsSep " \\\n    " (f: "${openscreenSrc}/${f}") translationUnits} \
    -lcrypto -lssl \
    -o generate

  ./generate

  for f in packets.bin frames.bin; do
    if ! cmp -s "$f" "$fixtures/$f"; then
      echo "ERROR: regenerated $f differs from the checked-in fixture." >&2
      echo "openscreen changed its wire behaviour, or the generator was edited." >&2
      echo "Re-read the diff before accepting it — the receiver's compatibility" >&2
      echo "guarantee is only as good as these bytes." >&2
      cmp "$f" "$fixtures/$f" >&2 || true
      exit 1
    fi
  done

  echo "fixtures match openscreen's packetizer"
  touch $out
''
