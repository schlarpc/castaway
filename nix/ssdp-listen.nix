# A passive SSDP listener: join the multicast group and print every NOTIFY that lands,
# verbatim, for a test script to assert on.
#
# The counterpart of `ssdp-search.nix`, and it exists for the announcements a search
# cannot draw out. `ssdp:alive` is unsolicited and `ssdp:byebye` only ever arrives once,
# on the way out — a control point that is not already listening never hears either, and
# a test that searches after the receiver has gone learns only that it is gone.
#
# Writes each message as it arrives and flushes, so a caller can start this in the
# background, stop the receiver, and read the file.
#
# The local address is a required argument for the reason it is in `ssdp-search`: a test
# VM has two interfaces and 239.255.255.250 matches no route, so the group has to be
# joined on the LAN one by name rather than by whichever the default route picks.
{ pkgs }:

pkgs.writers.writePython3Bin "ssdp-listen" { flakeIgnore = [ "E501" ]; } ''
  import socket
  import struct
  import sys
  import time

  window = float(sys.argv[1]) if len(sys.argv) > 1 else 30.0
  local = sys.argv[2] if len(sys.argv) > 2 else "0.0.0.0"

  sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
  sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
  # Bind to the wildcard rather than the group: on Linux a socket bound to the multicast
  # address receives only datagrams sent *to* it, which is what we want here, but binding
  # the group address is not portable and the membership below is what filters anyway.
  sock.bind(("", 1900))
  sock.setsockopt(
      socket.IPPROTO_IP,
      socket.IP_ADD_MEMBERSHIP,
      struct.pack("4s4s", socket.inet_aton("239.255.255.250"), socket.inet_aton(local)),
  )
  sock.settimeout(0.5)
  # Announce readiness, because a caller that starts this and immediately does the thing
  # it wants to observe has a race it cannot see: `ssdp:byebye` arrives once, and a
  # membership that is a few milliseconds late misses it and reports silence.
  print("listening on 239.255.255.250:1900 via " + local, flush=True)

  deadline = time.monotonic() + window
  while time.monotonic() < deadline:
      try:
          data, addr = sock.recvfrom(4096)
      except socket.timeout:
          continue
      print("--- from {}:{}".format(*addr), flush=True)
      print(data.decode("utf-8", "replace").strip(), flush=True)
''
