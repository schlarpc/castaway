#!/usr/bin/env bash
# Pre-stage a Widevine CDM into an Electron profile, so first launch needs no network.
#
# NOTE: the shipping receiver no longer needs this — `stageWidevine()` in main.js does it
# at startup, on both platforms, which is where it belongs (the marker's absolute path
# means only a running receiver knows what to write). This is kept as the standalone
# reproducer for Q42's table: it stages a profile *without* launching castaway, which is
# what lets `widevine-probe.js` judge a pre-staged profile against an empty one. Keep the
# two in step, or the reproducer stops reproducing what ships.
#
# This is G46's property under D36's mechanism. Left alone, ECS fetches the CDM from
# Google's component updater on first run — measured, and it *blocks* while doing so. A
# panel that has never been online would then have no DRM, and would wait on the fetch
# before it got there.
#
# The layout is ECS's own, reproduced from what a fetching run actually wrote:
#
#   <profile>/WidevineCdm/latest-component-updated-widevine-cdm   {"Path":"<abs>/<version>"}
#   <profile>/WidevineCdm/<version>/manifest.json
#   <profile>/WidevineCdm/<version>/_platform_specific/<plat>/libwidevinecdm.so
#
# Note the marker holds an **absolute** path, which is why this is a startup step rather
# than something the Nix store can contain: the profile directory is not known until the
# receiver knows where it is running. That mirrors the hint file the CEF path had to write
# for the same reason (G46).
#
#   stage-widevine.sh <profile-dir> <cdm-dir> [platform]
#
# <cdm-dir> is a directory holding `manifest.json` and `_platform_specific/`, which is
# exactly the shape of `pkgs.widevine-cdm`'s share/google/chrome/WidevineCdm — the same
# derivation the flake already pins, and byte-identical to what ECS would have fetched.
set -euo pipefail

profile="${1:?usage: stage-widevine.sh <profile-dir> <cdm-dir> [platform]}"
cdm="${2:?usage: stage-widevine.sh <profile-dir> <cdm-dir> [platform]}"
platform="${3:-linux_x64}"

[ -f "$cdm/manifest.json" ] || { echo "no manifest.json in $cdm" >&2; exit 1; }
version=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$cdm/manifest.json" | head -1)
[ -n "$version" ] || { echo "could not read version from $cdm/manifest.json" >&2; exit 1; }

lib="$cdm/_platform_specific/$platform/libwidevinecdm.so"
[ -f "$lib" ] || lib="$cdm/_platform_specific/$platform/widevinecdm.dll"
[ -f "$lib" ] || { echo "no CDM binary for $platform under $cdm" >&2; exit 1; }

dest="$profile/WidevineCdm/$version"
mkdir -p "$dest/_platform_specific/$platform"
install -m 644 "$cdm/manifest.json" "$dest/manifest.json"
install -m 755 "$lib" "$dest/_platform_specific/$platform/$(basename "$lib")"

# Absolute, because that is what the component updater writes and what Chromium reads
# back. A relative path here silently yields no CDM.
printf '{"Path":"%s"}' "$(cd "$dest" && pwd)" \
  > "$profile/WidevineCdm/latest-component-updated-widevine-cdm"

echo "staged Widevine $version for $platform into $profile"
