# Fetch `libAirReceiver.so` (arm64-v8a) out of AirReceiverLite 5.1.7.
#
# A fixed-output derivation, so it gets network access and its result is pinned by
# hash. That hash is the **library's**, not the container's, which is deliberate: the
# same library reaches you inside several different wrappers (a Play split bundle, an
# APKPure XAPK, an APKMirror `.apkm`) and only the innermost bytes are stable. The
# APKMirror repack of `base.apk`, for instance, differs from the one PROVENANCE
# recorded while the library inside is byte-identical.
#
# ## Why this route
#
# There is no vendor download for this app, and no anonymous stable URL:
#
# * **Google Play** serves it, and `apkeep -d google-play` can fetch the arm64 split
#   given a device profile — but it requires `--email` and an `--aas-token`, i.e. a
#   Google account in the build, and Play's terms disallow unofficial clients. Not
#   something to automate in a public repository.
# * **APKPure** needs no credentials and `apkeep -o 'arch=arm64-v8a'` does select an
#   architecture — but for 5.1.7 and 5.1.6 it publishes only `armeabi-v7a` bundles.
#   The arm64 builds it does carry (5.1.5, 4.9.7) predate the CKS client entirely:
#   no `cast.remotetogo.com`, no `x-api-key`, nothing to carve.
# * **APKMirror** publishes a universal `.apkm` for 5.1.7 that contains the
#   `config.arm64_v8a` split, which is what this fetches.
#
# ## The fragile part, and what happens when it breaks
#
# The download is gated behind a short page chain that sets a cookie and mints a
# per-file key; requesting the final URL cold returns 403. This replays the chain with
# a cookie jar. The key is stable per file rather than per session, but nothing about
# the markup is a contract, so treat this as a best-effort convenience: when it breaks
# the derivation fails loudly with instructions, and `CASTAWAY_AIRRECEIVER_SO` takes a
# path to a library you obtained yourself. The hash below is the check either way.
{
  lib,
  stdenvNoCC,
  curl,
  unzip,
  cacert,
}:

stdenvNoCC.mkDerivation {
  pname = "libAirReceiver";
  version = "5.1.7";

  nativeBuildInputs = [ curl unzip ];

  # The library, not the container. `nix-prefetch` will not help you regenerate this:
  # it is the sha256 of lib/arm64-v8a/libAirReceiver.so inside the arm64 split, and it
  # is also the value PROVENANCE §1 records for the binary the RE was done against.
  outputHashAlgo = "sha256";
  outputHashMode = "flat";
  outputHash = "sha256-cXAfT8M14UpQRnRFRYC6qObkUorQ3ijxz7xco0w5iXQ=";

  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  buildCommand = ''
    set -euo pipefail

    if [ -n "''${CASTAWAY_AIRRECEIVER_SO:-}" ]; then
      echo "using CASTAWAY_AIRRECEIVER_SO=$CASTAWAY_AIRRECEIVER_SO" >&2
      cp "$CASTAWAY_AIRRECEIVER_SO" "$out"
      exit 0
    fi

    base=https://www.apkmirror.com
    rel="$base/apk/devsoftmedia/airreceiverlite/airreceiverlite-5-1-7-release/"
    dl="$base/apk/softmedia/airreceiverlite/airreceiverlite-5-1-7-release/airreceiverlite-5-1-7-android-apk-download/"
    ua='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36'
    jar=$PWD/jar.txt
    get() { curl -sSL --retry 5 --retry-all-errors --retry-delay 3 -c "$jar" -b "$jar" -A "$ua" "$@"; }

    fail() {
      echo "" >&2
      echo "$1" >&2
      cat >&2 <<'EOF'

    The download chain at apkmirror.com has changed, or is refusing this build.
    Nothing here is a contract, so this is expected to happen eventually.

    Obtain lib/arm64-v8a/libAirReceiver.so from AirReceiverLite 5.1.7 by any route
    (its sha256 is the outputHash in nix/airreceiver-src.nix), then either:

      export CASTAWAY_AIRRECEIVER_SO=/path/to/libAirReceiver.so

    or add it to the store directly:

      nix-store --add-fixed sha256 /path/to/libAirReceiver.so
    EOF
      exit 1
    }

    get -e "$base/" "$rel" -o rel.html || fail "could not fetch the release page"
    get -e "$rel" "$dl" -o dl.html     || fail "could not fetch the download page"

    key=$(grep -oE 'download/\?key=[0-9a-f]+' dl.html | head -1) \
      || fail "no download key on the download page"
    [ -n "$key" ] || fail "no download key on the download page"

    # The final link is not always present on the first request; the page mints it a
    # beat later. Retry rather than treating a race as a breakage.
    final=""
    for _ in 1 2 3 4 5; do
      get -e "$dl" "$dl$key" -o key.html || true
      final=$(grep -oE 'download\.php\?id=[0-9]+&key=[0-9a-f]+' key.html | head -1 || true)
      [ -n "$final" ] && break
      sleep 5
    done
    [ -n "$final" ] || fail "no final download link after 5 attempts"

    get -e "$dl$key" "$base/wp-content/themes/APKMirror/$final" -o bundle.apkm \
      || fail "could not download the bundle"

    # An .apkm is a zip of split APKs; the arm64 split holds the library.
    unzip -q -o bundle.apkm 'split_config.arm64_v8a.apk' \
      || fail "the bundle has no arm64-v8a split"
    unzip -q -o split_config.arm64_v8a.apk 'lib/arm64-v8a/libAirReceiver.so' \
      || fail "the arm64 split has no libAirReceiver.so"

    cp lib/arm64-v8a/libAirReceiver.so "$out"
  '';

  meta = {
    description = "libAirReceiver.so (arm64-v8a) from AirReceiverLite 5.1.7";
    # SoftMedia's, and not ours to redistribute — which is the whole point of fetching
    # it rather than vendoring it.
    license = lib.licenses.unfree;
    platforms = lib.platforms.all;
  };
}
