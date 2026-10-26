# The two Google receiver SDK bundles a hosted Cast application loads, pinned.
#
# These are **test oracles, not runtime dependencies** — nothing in the receiver links
# them or ships them. They are here because `proto-cast::platform` is a reimplementation
# of a protocol whose only specification is these files, and the honest way to test a
# reimplementation is against the thing it was read off (ground rule 6, and the same
# posture as `nix/openscreen-fixtures.nix`). `crates/proto-cast/tests/receiver_sdk.rs`
# loads them in the real browser runtime and points them at our platform server.
#
# Not checked into the tree: half a megabyte of minified JavaScript that Google rewrites
# on its own schedule is exactly what a pinned fetch is for.
#
# Fetched from a Wayback Machine snapshot, not from gstatic. Google publishes no
# versioned URLs for either bundle — CAF is only ever `v3/`, and v2's `2.0.0/` is a
# label, not a release: its contents changed at least twice between August and October
# 2026 — so pinning the live URL meant every rewrite broke every dev shell. A snapshot's
# `id_` URL serves the original bytes and never changes. Noticing that Google *has*
# moved is still wanted, because it is the signal to go and re-read the platform layer
# (#16); that is `sdk-drift` in .github/workflows/live-endpoints.yml, which compares
# the live bundles against these pins and goes red on its own instead of on a `direnv`
# load. To re-pin: find the new capture in the Wayback CDX index for the upstream URL,
# check its `id_` bytes against a fresh gstatic fetch, and update cast-receiver-sdk.json.
#
# Both generations are pinned because both are in play and they are *identical* at the
# platform layer: same `ws://localhost:<port>/v2/ipc`, same `{namespace,senderId,data}`
# frame, same `port-for-web-server` default of 8008. v2 is what YouTube and Plex load;
# CAF v3 is what the Default Media Receiver loads. A test against only one would leave
# half the claim unmeasured.
{ pkgs }:

let
  pins = builtins.fromJSON (builtins.readFile ./cast-receiver-sdk.json);
  fetch = name: pin: pkgs.fetchurl {
    inherit name;
    url = "https://web.archive.org/web/${pin.snapshot}id_/${pin.upstream}";
    inherit (pin) hash;
  };
in
pkgs.runCommand "cast-receiver-sdk" { } ''
  mkdir -p $out
  ${pkgs.lib.concatStrings (pkgs.lib.mapAttrsToList (name: pin: ''
    cp ${fetch name pin} $out/${name}
  '') pins)}
''
