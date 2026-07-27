# The Widevine CDM for the Windows artifact, in the layout Chromium's component updater
# scans on startup.
#
# nixpkgs' `widevine-cdm` is Linux-only (`meta.platforms`), and there is no Windows
# equivalent, so this unpacks the same artifact Chrome itself installs: the CRX3 Google's
# component update service serves for `oimompecagnajdejgnnjijobebaeigek` (the Widevine CDM
# component) on win/x64. The URL is the immutable, version-stamped one the update server
# hands out — resolvable again with:
#
#   curl -H 'Content-Type: application/json' -X POST \
#     https://update.googleapis.com/service/update2/json --data '{"request":{
#       "@os":"win","@updater":"chrome","acceptformat":"crx3","protocol":"3.1",
#       "arch":"x64","nacl_arch":"x86-64","prodversion":"147.0.7727.118",
#       "os":{"arch":"x86_64","platform":"Windows","version":"10.0"},
#       "app":[{"appid":"oimompecagnajdejgnnjijobebaeigek","updatecheck":{},
#               "version":"0.0.0.0","enabled":true}]}}'
#
# Pinned to the same 4.10.3050.0 nixpkgs pins for Linux, so both artifacts run one CDM
# version and a DRM bug is not "which box was it on?".
#
# Unfree and non-redistributable, exactly like the Linux CDM: fetched and used locally
# rather than shipped onward, which is what a receiver on a wall does anyway. The
# `allowUnfreePredicate` in flake.nix names it, and `windows.nix` degrades to a no-DRM
# artifact rather than failing if a downstream nixpkgs refuses it.
{ lib, stdenvNoCC, unzip, src }:

let
  version = "4.10.3050.0";
in
stdenvNoCC.mkDerivation {
  pname = "widevine-cdm-windows";
  inherit version src;

  nativeBuildInputs = [ unzip ];

  # A CRX3 is a signed header followed by a plain zip. `unzip` reports the header as
  # "extra bytes at the beginning" and reads the central directory anyway, which is enough
  # here — Nix already verified the whole file against the locked hash, so re-checking
  # Google's signature would only be proving that hash to itself.
  unpackPhase = ''
    runHook preUnpack
    unzip -qq "$src" -d crx || true
    runHook postUnpack
  '';

  # The layout is Chromium's, not ours: `ComponentInstaller::StartRegistration` looks for
  # `WidevineCdm/manifest.json` under `DIR_COMPONENT_PREINSTALLED`, and
  # `GetCdmPathFromInstallDir` appends `_platform_specific/win_x64/widevinecdm.dll`. Both
  # halves must be present or `VerifyInstallation` rejects the directory and nothing is
  # registered — silently, so the check below is the difference between a build error and
  # a panel that will not play rentals.
  #
  # `widevinecdm.dll.sig` travels with the library because it is the CDM's own signature
  # file, which `CdmHostFiles::OpenCdmFile` looks for beside it. (Our *host* binaries have
  # no `.sig` — CEF is not a Google-signed build — so verification fails and, per
  # `cdm_module.cc`, is recorded to UMA and otherwise ignored. The practical consequence
  # is VMP: services that demand a verified media path will refuse licences. YouTube's
  # software-secure path does not.)
  installPhase = ''
    runHook preInstall

    payload=crx/_platform_specific/win_x64
    for want in crx/manifest.json "$payload/widevinecdm.dll"; do
      if [ ! -f "$want" ]; then
        echo "the Widevine CRX did not contain $want; its layout has changed" >&2
        echo "contents: $(cd crx 2>/dev/null && find . -maxdepth 3 || echo '<nothing unpacked>')" >&2
        exit 1
      fi
    done

    got=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' crx/manifest.json | head -1)
    if [ "$got" != "${version}" ]; then
      echo "the Widevine CRX declares version $got but this derivation pins ${version}." >&2
      echo "Update both, or the panel and the Linux build run different CDMs." >&2
      exit 1
    fi

    install -Dm644 crx/manifest.json "$out/WidevineCdm/manifest.json"
    install -Dm644 crx/LICENSE "$out/WidevineCdm/LICENSE.txt"
    install -Dm644 "$payload/widevinecdm.dll" \
      "$out/WidevineCdm/_platform_specific/win_x64/widevinecdm.dll"
    install -Dm644 "$payload/widevinecdm.dll.sig" \
      "$out/WidevineCdm/_platform_specific/win_x64/widevinecdm.dll.sig"

    runHook postInstall
  '';

  dontFixup = true;

  meta = {
    description = "Widevine Content Decryption Module ${version} for Windows x64";
    homepage = "https://www.widevine.com/";
    license = lib.licenses.unfree;
    # The *payload* is for Windows; the derivation is a Linux-side unzip, so this is
    # `all` like cef-windows.nix. Naming the target platform here would make `checkMeta`
    # refuse it on the cross-builder — and because the caller wraps this in `tryEval`,
    # the refusal would land as a silently DRM-less artifact rather than an error.
    platforms = lib.platforms.all;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
