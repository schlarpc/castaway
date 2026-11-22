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
{ lib, stdenvNoCC, xwin, cacert, fetchFromGitHub, rustPlatform }:

let
  # xwin 0.10.0, ahead of nixpkgs (0.9.0). Microsoft's VS 17 manifest switched its payload
  # paths from `Installers\…` to `Installers/…` in November 2026, and 0.9.0 found the
  # per-arch SDK headers by stripping the literal backslash prefix — so every cold build
  # of this derivation failed with "unable to find x86_64 headers for Win11SDK_10.0.26100".
  # 0.10.0 matches separator-agnostically (upstream PR #179, found first on v18 manifests).
  # Drop this override once nixpkgs carries 0.10.0 or later.
  xwin' = xwin.overrideAttrs (finalAttrs: old: {
    version = "0.10.0";
    src = fetchFromGitHub {
      owner = "Jake-Shadle";
      repo = "xwin";
      tag = finalAttrs.version;
      hash = "sha256-kUbRHrM88x3ezy0+GIpbGQfEROn16dSwPtobjdYauRM=";
    };
    # `cargoHash` alone does not reach the vendor derivation through overrideAttrs.
    cargoDeps = rustPlatform.fetchCargoVendor {
      inherit (finalAttrs) src;
      name = "xwin-${finalAttrs.version}-vendor";
      hash = "sha256-34v/YDT0Juwjv8EbwsOsmbp/Q89Z4c+BYxcA60X7Q3A=";
    };
    # 0.10.0 adds a third network-bound test beside the two nixpkgs already skips.
    checkFlags = old.checkFlags ++ [ "--skip=verify_aarch64_compiles" ];
  });

  # `xwin --arch x86_64 list` prints the versions available in the current manifest.
  crtVersion = "14.44.17.14";
  sdkVersion = "10.0.26100";
in
stdenvNoCC.mkDerivation {
  pname = "msvc-sysroot";
  version = "crt-${crtVersion}-sdk-${sdkVersion}";

  dontUnpack = true;
  nativeBuildInputs = [ xwin' ];

  # The MSVC CRT and Windows SDK are redistributable for build purposes but are not
  # free software; this derivation only repacks Microsoft's own payloads.
  SSL_CERT_FILE = "${cacert}/etc/ssl/certs/ca-bundle.crt";

  buildPhase = ''
    runHook preBuild

    # xwin keeps downloads + intermediate unpacking here; only the splat lands in $out.
    # `--copy` rather than the default move: $TMPDIR and the store are usually different
    # filesystems in the build sandbox, and a cross-device rename(2) fails with EXDEV.
    export HOME="$TMPDIR"

    # `--http-retry` defaults to 0, so a single dropped connection to Microsoft's CDN
    # fails the whole derivation — and it is half a gigabyte in, having already fetched
    # everything else. The observed failure is "failed to retrieve
    # Microsoft.VC.14.44.17.14.CRT.x64.Desktop.base.vsix after 1 tries due to I/O failures
    # reading the response body".
    #
    # `--timeout` is raised with it because the default 60s is per *download*, not per
    # stall, and the payload that fails is the largest one in the set at 205 MiB — which
    # that budget only covers above about 3.5 MB/s sustained. That makes the timeout a
    # likely cause of the truncated body rather than merely a second thing to harden;
    # retries alone would then just spend five attempts hitting the same wall.
    xwin \
      --accept-license \
      --http-retry 5 \
      --timeout 600s \
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
  outputHash = "sha256-Pr66nX8amUCdh/7oKmlIWbvz+8S5WQ8NiI7QwckKRO8=";

  meta = {
    description = "MSVC CRT ${crtVersion} + Windows SDK ${sdkVersion} repacked for cross-compilation";
    license = lib.licenses.unfree;
    platforms = lib.platforms.linux;
  };
}
