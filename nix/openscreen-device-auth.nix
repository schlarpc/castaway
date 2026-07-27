# Runs openscreen's own sender-side device-auth verifier over the device-auth vectors in
# `crates/proto-cast/tests/fixtures/device-auth/`, and asserts each recorded verdict.
#
# This is the sibling of `openscreen-fixtures.nix`, inverted. There, openscreen produces
# bytes and our receiver consumes them; here our receiver produces bytes and openscreen
# judges them. Both exist for the same reason: a reimplementation that is only tested
# against itself is self-consistent, not compatible.
#
# What it buys specifically. "An official sender will not accept this receiver" was, until
# this check, a conclusion drawn from reading `cast_auth_util.cc` — correct, but the kind
# of correct that quietly rots and cannot distinguish one reason from five. Running the
# real verifier separates them: the chain is refused for its root and *only* its root, the
# sender's nonce check is genuinely a no-op, a peer certificate valid for too long is
# rejected before any of that, and the signed blob's layout is checked (there is a
# deliberately wrong vector to prove the check has teeth). The day a real device credential
# is provisioned, `dev-chain-google-roots` is the case that flips, and nothing else has to.
#
# `cast/sender/channel/cast_auth_util.cc` plus the certificate path builder under
# `cast/common/certificate/` is the same code Chrome runs. openscreen's full GN build wants
# `gclient sync`, a CIPD toolchain and a stack of vendored third-party libraries; none of
# that is needed to compile the sixteen translation units that make the decision, so none
# of it happens.
{ pkgs, openscreenSrc }:

let
  # Paths inside the openscreen checkout. Each is here because the compiler or linker
  # asked for it.
  translationUnits = [
    "cast/sender/channel/cast_auth_util.cc"
    "cast/common/certificate/cast_cert_validator.cc"
    "cast/common/certificate/cast_crl.cc"
    "cast/common/certificate/date_time.cc"
    "cast/common/certificate/boringssl_parsed_certificate.cc"
    "cast/common/certificate/boringssl_trust_store.cc"
    "cast/common/certificate/boringssl_util.cc"
    "platform/base/error.cc"
    "platform/impl/time.cc"
    "util/crypto/openssl_util.cc"
    "util/crypto/certificate_utils.cc"
    "util/crypto/pem_helpers.cc"
    "util/crypto/sha2.cc"
    "util/crypto/random_bytes.cc"
    "util/string_util.cc"
  ];

  # Both protos the verifier needs: the channel messages it parses, and the revocation
  # list format `cast_crl.cc` is written against.
  protos = [
    "cast/common/channel/proto/cast_channel.proto"
    "cast/common/certificate/proto/revocation.proto"
  ];

  vectors = ../crates/proto-cast/tests/fixtures/device-auth;

  # openscreen's logging API is three functions, and its own POSIX implementation pulls in
  # Chromium's `build/` repository for `build_config.h` — a gclient dependency we
  # deliberately do not fetch. Shared with the RTP fixture generator for the same reason.
  loggingStub = ../crates/proto-cast/tests/fixtures/rtp-stream/generator/logging_stub.cc;
in
pkgs.runCommandCC "openscreen-device-auth"
{
  # openscreen is written against BoringSSL: the certificate code uses APIs
  # (`bssl::UniquePtr`, `X509_ALGOR_cmp` semantics, the pathlen handling) that OpenSSL 3
  # does not present the same way.
  buildInputs = [ pkgs.boringssl pkgs.protobuf ];
  nativeBuildInputs = [ pkgs.protobuf pkgs.pkg-config ];
  inherit vectors loggingStub;
  oracle = ./openscreen-device-auth/oracle.cc;
} ''
  set -euo pipefail

  echo "building the sender-side verifier against openscreen ${openscreenSrc.shortRev or "pinned"}"
  mkdir -p gen
  protoc --proto_path=${openscreenSrc} --cpp_out=gen \
    ${pkgs.lib.concatStringsSep " " protos}

  c++ -std=c++20 -O1 \
    -I${openscreenSrc} -Igen \
    "$oracle" "$loggingStub" \
    gen/cast/common/channel/proto/cast_channel.pb.cc \
    gen/cast/common/certificate/proto/revocation.pb.cc \
    ${pkgs.lib.concatMapStringsSep " \\\n    " (f: "${openscreenSrc}/${f}") translationUnits} \
    $(pkg-config --cflags --libs protobuf) -lcrypto -lssl \
    -o verify

  failed=0
  cases=0
  for dir in "$vectors"/*/; do
    name=$(basename "$dir")
    # A directory without a verdict is not a vector. Skipping silently would let one be
    # added and never judged, which is the shape of failure this whole check exists for,
    # so say so and fail.
    if [ ! -f "$dir/expect" ]; then
      echo "ERROR: $name has no 'expect' file; it would never be judged." >&2
      exit 1
    fi
    want=$(cat "$dir/expect")
    got=$(./verify "$dir")
    cases=$((cases + 1))
    if [ "$got" = "$want" ]; then
      echo "  ok    $name -> $got"
    else
      echo "  FAIL  $name" >&2
      echo "          expected: $want" >&2
      echo "          got:      $got" >&2
      failed=$((failed + 1))
    fi
  done

  if [ "$cases" -eq 0 ]; then
    echo "ERROR: no vectors found under $vectors." >&2
    echo "An empty run is not a passing run — it is a check that stopped checking." >&2
    exit 1
  fi

  if [ "$failed" -ne 0 ]; then
    echo "" >&2
    echo "$failed of $cases device-auth vectors were judged differently by openscreen." >&2
    echo "Either our device-auth response changed, or openscreen's sender-side rules did." >&2
    echo "Both matter: this is the only thing standing between 'a sender would accept" >&2
    echo "this' being knowledge and being an opinion." >&2
    exit 1
  fi

  echo "$cases device-auth vectors judged as recorded"
  touch $out
''
