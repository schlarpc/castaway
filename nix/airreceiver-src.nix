# Fetch `libAirReceiver.so` (arm64-v8a) out of AirReceiverLite 5.1.8.
#
# A fixed-output derivation, so it gets network access and its result is pinned by
# hash. That hash is the **library's**, not the container's, which is deliberate: the
# same library reaches you inside several different wrappers (a Play split bundle, an
# APKPure XAPK, an APKMirror `.apkm`) and only the innermost bytes are stable. The
# APKMirror repack of `base.apk`, for instance, differs from the one PROVENANCE
# recorded while the library inside is byte-identical.
#
# ## Where from
#
# The APKMirror `.apkm` for 5.1.8 — the universal bundle that carries the
# `config.arm64_v8a` split — as mirrored on the Internet Archive, which serves a stable
# anonymous URL. That is what this route was missing everywhere else:
#
# * **Google Play** needs a Google account and an AAS token in the build, against Play's
#   terms. Not something to automate in a public repository.
# * **APKPure** publishes only `armeabi-v7a` bundles for 5.1.7 and 5.1.6; its arm64
#   builds (5.1.5, 4.9.7) predate the CKS client entirely, so there is nothing to carve.
# * **APKMirror** itself publishes the bundle, and this derivation used to walk its
#   download chain with `curl`. Since 2026-10-08 every hop of that chain is behind
#   Cloudflare's JS challenge (#356), which no `curl` gets past.
#
# The hash pins the library, so the mirror is trusted for availability only. If it
# goes away, any route to the same bytes works: `nix-store --add-fixed sha256` on the
# extracted library, as the failure says.
{
  lib,
  stdenvNoCC,
  curl,
  unzip,
  cacert,
}:

stdenvNoCC.mkDerivation {
  pname = "libAirReceiver";
  version = "5.1.8";

  nativeBuildInputs = [ curl unzip ];

  # The library, not the container. `nix-prefetch` will not help you regenerate this:
  # it is the sha256 of lib/arm64-v8a/libAirReceiver.so inside the arm64 split, and it
  # is also the value PROVENANCE §1 records for the binary the RE was done against.
  outputHashAlgo = "sha256";
  outputHashMode = "flat";
  outputHash = "sha256-JUhStadrP2LTv7eeeCK5CgnwFhD9t1e/lrCPWpxq7OU=";

  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  buildCommand = ''
    set -euo pipefail

    fail() {
      echo "" >&2
      echo "$1" >&2
      cat >&2 <<'EOF'

    The Internet Archive mirror of AirReceiverLite 5.1.8 is unreachable, or no longer
    holds the bundle. See issue #356.

    Obtain lib/arm64-v8a/libAirReceiver.so from AirReceiverLite 5.1.8 by any route
    (its sha256 is the outputHash in nix/airreceiver-src.nix), then add it to the
    store:

      unzip bundle.apkm split_config.arm64_v8a.apk
      unzip split_config.arm64_v8a.apk lib/arm64-v8a/libAirReceiver.so
      nix-store --add-fixed sha256 lib/arm64-v8a/libAirReceiver.so
    EOF
      exit 1
    }

    curl -sSfL --retry 5 --retry-all-errors --retry-delay 3 -o bundle.apkm \
      'https://archive.org/download/com.softmedia.receiver.lite/com.softmedia.receiver.lite_5.1.8-2020164766_4arch_7dpi_24lang_a8ddb5eb459ae731a6e7015d9a9ae025_apkmirror.com.apkm' \
      || fail "could not download the bundle from the Internet Archive mirror"

    # An .apkm is a zip of split APKs; the arm64 split holds the library.
    unzip -q -o bundle.apkm 'split_config.arm64_v8a.apk' \
      || fail "the bundle has no arm64-v8a split"
    unzip -q -o split_config.arm64_v8a.apk 'lib/arm64-v8a/libAirReceiver.so' \
      || fail "the arm64 split has no libAirReceiver.so"

    cp lib/arm64-v8a/libAirReceiver.so "$out"
  '';

  meta = {
    description = "libAirReceiver.so (arm64-v8a) from AirReceiverLite 5.1.8";
    # SoftMedia's, and not ours to redistribute — which is the whole point of fetching
    # it rather than vendoring it.
    license = lib.licenses.unfree;
    platforms = lib.platforms.all;
  };
}
