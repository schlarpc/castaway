#!/usr/bin/env bash
# VMP-sign the Windows artifact so Widevine will grant a verified media path.
#
# Deliberately outside `nix build`. EVS is a network service that signs *exact bytes*, so
# putting it inside a derivation would make the build non-reproducible and would fail
# closed on any machine without credentials — the same carve-out shape the unfree CDM
# already has (ground rule 6, D36).
#
# Two ordering rules, both load-bearing, both from castLabs' own documentation:
#
#   1. On Windows, VMP signing must come **after** Authenticode. Signing in the other
#      order invalidates the VMP signature, and the failure surfaces as a licence refusal
#      from the service rather than as an error here.
#   2. `sign-pkg` takes the directory *containing* the executable, not the executable.
#
# Linux needs none of this: the Linux CDM has no host verification at all, which G46
# established while the CEF path was still current.
#
#   vmp-sign.sh <dir containing electron.exe>            # sign
#   vmp-sign.sh --check <dir>                            # verify only, no account needed
#
# Requires the client: `pip install castlabs-evs`, then `python3 -m castlabs_evs.account
# signup` once. The service is free; the account is not optional.
set -euo pipefail

check_only=0
if [ "${1:-}" = "--check" ]; then
  check_only=1
  shift
fi

dir="${1:?usage: vmp-sign.sh [--check] <dir containing electron.exe>}"
[ -d "$dir" ] || { echo "not a directory: $dir" >&2; exit 1; }

# The signature covers the Electron binaries, not castaway.exe: Electron is the process
# that loads the CDM, and ours never does. That is a property of the subprocess model
# worth stating, because it means we sign a stock ECS tree we did not modify.
exe=$(find "$dir" -maxdepth 2 -iname 'electron.exe' -print -quit)
[ -n "$exe" ] || { echo "no electron.exe under $dir — is this the staged browser?" >&2; exit 1; }
target=$(dirname "$exe")
echo "browser at: $target"

# The client check comes *after* the structural ones on purpose: "is this the staged
# browser" is answerable with no account and no network, so CI can run it, and a
# packaging mistake should not hide behind a missing dependency.
if ! python3 -c 'import castlabs_evs' 2>/dev/null; then
  echo "castlabs-evs is not installed: pip install castlabs-evs" >&2
  echo "(structure is valid; signing needs the client and a free account)" >&2
  exit 2
fi


if [ "$check_only" = 1 ]; then
  # Verification reads the `.sig` files next to the binaries and needs no account, so it
  # is the half of this that CI can run.
  python3 -m castlabs_evs.vmp verify-pkg "$target"
  exit $?
fi

echo "VMP-signing $target (after Authenticode, per castLabs' ordering rule)"
python3 -m castlabs_evs.vmp sign-pkg "$target"

# Signing writes `<name>.sig` beside each signed binary. Their absence is exactly the
# state that plays fine against Widevine UAT and fails against production, so it is worth
# asserting rather than assuming.
if ! ls "$target"/*.sig >/dev/null 2>&1; then
  echo "sign-pkg reported success but produced no .sig files in $target" >&2
  exit 3
fi
echo "signed: $(ls "$target"/*.sig | wc -l) signature(s)"
