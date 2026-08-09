# A scripted SSDP control point: multicast the M-SEARCH, collect the unicast replies,
# print them verbatim for a test script to assert on. Exits non-zero on silence, so
# "discovery is broken" surfaces as a failing command rather than an empty match.
#
# Shared by `integration-vm` (nix/vm-test.nix) and `dial-vm` (nix/dial-vm-test.nix) so the
# two checks cannot drift into searching differently.
#
# The local address is a required argument, not a convenience: a test VM has two
# interfaces (QEMU's NAT eth0 and the test VLAN eth1), and 239.255.255.250 matches no
# route, so an unbound socket sends the M-SEARCH out the default route — into the NAT,
# where the receiver never sees it. Pin the multicast egress to the LAN.
{ pkgs }:

pkgs.writers.writePython3Bin "ssdp-search" { flakeIgnore = [ "E501" ]; } ''
  import socket
  import sys
  import time

  st = sys.argv[1] if len(sys.argv) > 1 else "ssdp:all"
  window = float(sys.argv[2]) if len(sys.argv) > 2 else 4.0
  local = sys.argv[3] if len(sys.argv) > 3 else "0.0.0.0"

  request = "\r\n".join([
      "M-SEARCH * HTTP/1.1",
      "HOST: 239.255.255.250:1900",
      'MAN: "ssdp:discover"',
      "MX: 1",
      "ST: " + st,
      "",
      "",
  ]).encode()

  sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
  sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 2)
  sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(local))
  # Bind too, so the unicast reply comes back to the LAN address the receiver saw.
  sock.bind((local, 0))
  sock.settimeout(0.5)
  sock.sendto(request, ("239.255.255.250", 1900))

  replies = []
  deadline = time.monotonic() + window
  while time.monotonic() < deadline:
      try:
          data, addr = sock.recvfrom(4096)
      except socket.timeout:
          continue
      replies.append((addr, data.decode("utf-8", "replace")))

  for addr, text in replies:
      print("--- reply from {}:{}".format(*addr))
      print(text.strip())

  if not replies:
      print("no SSDP replies for ST " + st, file=sys.stderr)
      sys.exit(1)
''
