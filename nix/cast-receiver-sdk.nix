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
# on its own schedule is exactly what a pinned fetch is for. A bundle that moves fails
# the hash, which is the signal to go and re-read what changed — see `docs/` and #16.
#
# Both generations are pinned because both are in play and they are *identical* at the
# platform layer: same `ws://localhost:<port>/v2/ipc`, same `{namespace,senderId,data}`
# frame, same `port-for-web-server` default of 8008. v2 is what YouTube and Plex load;
# CAF v3 is what the Default Media Receiver loads. A test against only one would leave
# half the claim unmeasured.
{ pkgs }:

let
  v2 = pkgs.fetchurl {
    url = "https://www.gstatic.com/cast/sdk/libs/receiver/2.0.0/cast_receiver.js";
    hash = "sha256-R0JDwckB40gsLBGBTp8DOtonN0N43GkPclVnHsAExiY=";
  };
  caf = pkgs.fetchurl {
    url = "https://www.gstatic.com/cast/sdk/libs/caf_receiver/v3/cast_receiver_framework.js";
    hash = "sha256-RqeMsPGJV0J6c3zMEBXf0aWHx1WOxm3w/0d/VXnvjqA=";
  };
in
pkgs.runCommand "cast-receiver-sdk" { } ''
  mkdir -p $out
  cp ${v2} $out/cast_receiver.js
  cp ${caf} $out/cast_receiver_framework.js
''
