# The Windows sysroot: Microsoft's MSVC CRT + Windows SDK, repacked for cross-compilation.
#
# `xwin` downloads the same VS installer payloads Visual Studio uses, prunes them, and
# fixes the file-casing problem (the SDK ships `Windows.h`, everyone's code includes
# `windows.h`) by adding symlinks. Because that means talking to Microsoft's CDN, this is
# a *fixed-output derivation*: network access is allowed, and the result is pinned by
# `outputHash` so the sysroot is byte-identical on every machine and in CI.
#
# The CRT/SDK versions are pinned explicitly rather than tracking "latest in the
# manifest" — otherwise Microsoft publishing a new SDK would silently change the output
# and break the hash. To upgrade: bump `crtVersion`/`sdkVersion`, set `hash` to
# `lib.fakeHash`, rebuild, and paste in the hash Nix reports.
#
# Layout produced (this is what `cargo-xwin` also generates, so its flag conventions
# apply verbatim — see nix/windows.nix):
#
#   crt/include/            crt/lib/x86_64/
#   sdk/include/{ucrt,um,shared,winrt}/
#   sdk/lib/{ucrt,um}/x86_64/
{ lib, stdenvNoCC, xwin, cacert }:

let
  # `xwin --arch x86_64 list` prints the versions available in the current manifest.
  crtVersion = "14.44.17.14";
  sdkVersion = "10.0.26100";
in
stdenvNoCC.mkDerivation {
  pname = "msvc-sysroot";
  version = "crt-${crtVersion}-sdk-${sdkVersion}";

  dontUnpack = true;
  nativeBuildInputs = [ xwin ];

  # The MSVC CRT and Windows SDK are redistributable for build purposes but are not
  # free software; this derivation only repacks Microsoft's own payloads.
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  buildPhase = ''
    runHook preBuild

    # xwin keeps downloads + intermediate unpacking here; only the splat lands in $out.
    # `--copy` rather than the default move: $TMPDIR and the store are usually different
    # filesystems in the build sandbox, and a cross-device rename(2) fails with EXDEV.
    export HOME="$TMPDIR"

    xwin \
      --accept-license \
      --cache-dir "$TMPDIR/xwin-cache" \
      --manifest-version 17 \
      --channel release \
      --arch x86_64 \
      --variant desktop \
      --crt-version ${crtVersion} \
      --sdk-version ${sdkVersion} \
      splat --copy --output "$out"

    runHook postBuild
  '';

  # `cargo-xwin` treats a cache directory as already-populated when it finds a `DONE`
  # file whose first line lists the architectures present. Writing it means this sysroot
  # can be handed to `cargo xwin build` via `XWIN_CACHE_DIR` as an escape hatch, without
  # it trying to re-download anything.
  installPhase = ''
    runHook preInstall
    echo "x86_64" > "$out/DONE"
    runHook postInstall
  '';

  outputHashMode = "recursive";
  outputHashAlgo = "sha256";
  outputHash = "sha256-o825/2nx69+2Tt0l5wnD0NzGDhcqUF/ocrgf5SDladY=";

  meta = {
    description = "MSVC CRT ${crtVersion} + Windows SDK ${sdkVersion} repacked for cross-compilation";
    license = lib.licenses.unfree;
    platforms = lib.platforms.linux;
  };
}
