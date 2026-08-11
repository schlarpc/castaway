# The namespace operations `checks.android-cast` needs, and nothing else, so the answer
# arrives in a minute instead of at the end of an emulator boot. Driven by
# `.github/workflows/ns-probe.yml`; both are temporary and go away with #338.
#
# Run on the bench with the same sandbox path CI configures, to compare like with like:
#
#   nix build --impure --no-link -f .github/scripts/ns-probe.nix \
#     --option extra-sandbox-paths /dev/net/tun
#
# On the dev box (NixOS, Determinate Nix 3.17.3) that prints: no capabilities, a private
# netns holding only `lo`, `/dev/net/tun` present, nested `unshare --user --map-root-user
# --net` OK, bare `unshare --net` refused, direct tap creation refused. So the check's
# unshare is what buys CAP_NET_ADMIN, and any fix has to keep it or replace it.
{ pkgs ? import <nixpkgs> { } }:
pkgs.runCommand "ns-probe"
{
  nativeBuildInputs = [ pkgs.util-linux pkgs.iproute2 ];
} ''
  echo "== identity and capabilities inside the sandbox"
  id
  grep -E 'CapPrm|CapEff|CapBnd|NoNewPrivs|Seccomp' /proc/self/status || true

  echo "== the sandbox's own network namespace"
  ip -o link show || echo "  ip link failed"
  readlink /proc/self/ns/net /proc/self/ns/user || true

  echo "== userns policy as seen from in here"
  for f in /proc/sys/user/max_user_namespaces /proc/sys/kernel/unprivileged_userns_clone; do
    printf '%-50s %s\n' "$f" "$(cat "$f" 2>/dev/null || echo '(unreadable)')"
  done
  cat /proc/self/uid_map || true

  echo "== /dev/net/tun"
  ls -l /dev/net/tun 2>&1 || echo "  absent"

  echo "== nested unshare --user --map-root-user --net (what android-cast does)"
  unshare --user --map-root-user --net -- true \
    && echo "  UNSHARE OK" || echo "  UNSHARE FAILED rc=$?"

  echo "== nested unshare --user alone"
  unshare --user --map-root-user -- true \
    && echo "  USERNS OK" || echo "  USERNS FAILED rc=$?"

  echo "== nested unshare --net alone (needs CAP_SYS_ADMIN, expected to fail)"
  unshare --net -- true \
    && echo "  NETNS OK" || echo "  NETNS FAILED rc=$?"

  echo "== tap creation directly in the sandbox's netns"
  ip tuntap add dev tap0 mode tap \
    && echo "  TAP OK" || echo "  TAP FAILED rc=$?"

  echo "== tap creation inside the nested namespace"
  unshare --user --map-root-user --net -- \
    ${pkgs.writeShellScript "tap-inside" ''
      export PATH=${pkgs.lib.makeBinPath [ pkgs.iproute2 ]}:$PATH
      ip tuntap add dev tap0 mode tap && echo "  TAP-IN-NS OK" || echo "  TAP-IN-NS FAILED rc=$?"
    ''} || echo "  (could not enter the namespace at all)"

  mkdir -p $out; echo probed > $out/result
''
