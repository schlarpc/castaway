# Tier-2 integration test (ground rule 6): boot the receiver in a VM and drive it from a
# *second* VM over a real LAN, with scripted senders. Everything the manual `curl` notes
# in docs/STATUS.md claimed, asserted by CI with no hardware and no human.
#
# Two nodes rather than one on purpose: discovery is the part that breaks in the field,
# and loopback hides all of it. A second host makes the multicast join, the unicast
# M-SEARCH reply, and the advertised LOCATION address real — bind to the wrong interface
# or advertise 127.0.0.1 and this test fails, where a localhost curl would pass.
{ pkgs, self }:

let
  httpPort = 8080;
  friendlyName = "castaway-vm";

  # A scripted SSDP control point: multicast the M-SEARCH, collect the unicast replies,
  # print them verbatim for the test script to assert on. Exits non-zero on silence, so
  # "discovery is broken" surfaces as a failing command rather than an empty match.
  #
  # The local address is a required argument, not a convenience: a test VM has two
  # interfaces (QEMU's NAT eth0 and the test VLAN eth1), and 239.255.255.250 matches no
  # route, so an unbound socket sends the M-SEARCH out the default route — into the NAT,
  # where the receiver never sees it. Pin the multicast egress to the LAN.
  ssdpSearch = pkgs.writers.writePython3Bin "ssdp-search" { flakeIgnore = [ "E501" ]; } ''
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
  '';

  # A scripted UPnP/DLNA control point — the "cast a video from VLC" flow, one action
  # per invocation so the test can assert the transport state after each step.
  dlnaCtl = pkgs.writers.writePython3Bin "dlna-ctl" { flakeIgnore = [ "E501" ]; } ''
    import re
    import sys
    import urllib.request

    SERVICE = "urn:schemas-upnp-org:service:AVTransport:1"

    base = sys.argv[1].rstrip("/")
    command = sys.argv[2]
    argument = sys.argv[3] if len(sys.argv) > 3 else ""


    def escape(text):
        return (text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


    def soap(action, body=""):
        envelope = (
            '<?xml version="1.0" encoding="utf-8"?>'
            '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"'
            ' s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
            "<s:Body>"
            '<u:{action} xmlns:u="{service}"><InstanceID>0</InstanceID>{body}</u:{action}>'
            "</s:Body></s:Envelope>"
        ).format(action=action, service=SERVICE, body=body)
        request = urllib.request.Request(
            base + "/dlna/control/AVTransport",
            data=envelope.encode(),
            headers={
                "Content-Type": 'text/xml; charset="utf-8"',
                "SOAPAction": '"{}#{}"'.format(SERVICE, action),
            },
        )
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.read().decode()


    if command == "set":
        soap(
            "SetAVTransportURI",
            "<CurrentURI>{}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>".format(
                escape(argument)
            ),
        )
    elif command == "play":
        soap("Play", "<Speed>1</Speed>")
    elif command in ("pause", "stop"):
        soap(command.capitalize())
    elif command == "state":
        pass
    else:
        print("unknown command " + command, file=sys.stderr)
        sys.exit(2)

    info = soap("GetTransportInfo")
    match = re.search(r"<CurrentTransportState>([^<]*)</CurrentTransportState>", info)
    if not match:
        print("no CurrentTransportState in " + info, file=sys.stderr)
        sys.exit(1)
    print(match.group(1))
  '';
in
pkgs.testers.runNixOSTest {
  name = "castaway-integration";

  nodes = {
    # The kiosk, configured exactly as a deploy would be — through the module, so the
    # module itself is under test too.
    receiver = { config, ... }: {
      imports = [ self.nixosModules.castaway ];

      services.castaway = {
        enable = true;
        inherit httpPort;
        # Debug for castaway so the assertions can read the session manager's decisions
        # out of the journal.
        logLevel = "info,castaway=debug";
        settings = {
          friendly_name = friendlyName;
          uuid = "0f8c2e10-0000-4000-8000-0000000c0571";
          # The VM's default route is the NAT interface, so the auto-detect would pick
          # the wrong address; pin discovery to the test LAN.
          interface = config.networking.primaryIPAddress;
        };
      };
    };

    # The sender: a plain host on the same LAN with control points installed.
    sender = { ... }: {
      # SSDP replies come back as unicast from :1900 to our ephemeral port, which
      # conntrack can't associate with a datagram sent to 239.255.255.250 — the default
      # firewall would drop every reply and fail the test for the wrong reason.
      networking.firewall.enable = false;
      services.avahi = {
        enable = true;
        openFirewall = false;
      };
      environment.systemPackages = [ pkgs.curl ssdpSearch dlnaCtl ];
    };
  };

  testScript = { nodes, ... }: ''
    import json

    base = "http://${nodes.receiver.networking.primaryIPAddress}:${toString httpPort}"
    lan = "${nodes.sender.networking.primaryIPAddress}"

    start_all()

    with subtest("the service comes up under the module"):
        receiver.wait_for_unit("castaway.service")
        receiver.wait_for_open_port(${toString httpPort})
        sender.wait_for_unit("multi-user.target")

    with subtest("SSDP M-SEARCH is answered, and every LOCATION it hands out resolves"):
        replies = sender.succeed(f"ssdp-search ssdp:all 5 {lan}")
        assert "urn:dial-multiscreen-org:service:dial:1" in replies, replies
        assert "urn:schemas-upnp-org:device:MediaRenderer:1" in replies, replies

        locations = sorted({
            line.split(":", 1)[1].strip()
            for line in replies.splitlines()
            if line.lower().startswith("location:")
        })
        assert locations, replies
        for location in locations:
            # Advertising an address the sender can't reach is the classic discovery bug,
            # so fetch each one from the other host rather than trusting the string.
            assert location.startswith(base), f"LOCATION {location} is not on {base}"
            sender.succeed(f"curl -sSf -o /dev/null {location}")

    with subtest("DIAL launches and stops YouTube"):
        headers = sender.succeed(f"curl -sSf -D- -o /dev/null {base}/dial/dd.xml").lower()
        assert f"application-url: {base}/dial/apps/" in headers, headers

        assert "<state>stopped</state>" in sender.succeed(f"curl -sSf {base}/dial/apps/YouTube")
        launch = sender.succeed(
            "curl -sSf -D- -o /dev/null -X POST "
            f"-d 'pairingCode=zt7bq2&theme=cl' {base}/dial/apps/YouTube"
        ).lower()
        assert "201 created" in launch, launch
        assert f"location: {base}/dial/apps/YouTube/run".lower() in launch, launch

        assert "<state>running</state>" in sender.succeed(f"curl -sSf {base}/dial/apps/YouTube")
        # allowStop="true" promises this works; a sender that disconnects must dismiss it.
        sender.succeed(f"curl -sSf -X DELETE {base}/dial/apps/YouTube/run")
        assert "<state>stopped</state>" in sender.succeed(f"curl -sSf {base}/dial/apps/YouTube")

        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'DIAL launched YouTube'")

    with subtest("DLNA cast-a-video drives the transport and reaches the pipeline"):
        assert sender.succeed(f"dlna-ctl {base} state").strip() == "NO_MEDIA_PRESENT"
        sender.succeed(f"dlna-ctl {base} set http://example.invalid/clip.mp4")
        assert sender.succeed(f"dlna-ctl {base} play").strip() == "PLAYING"
        assert sender.succeed(f"dlna-ctl {base} pause").strip() == "PAUSED_PLAYBACK"
        assert sender.succeed(f"dlna-ctl {base} stop").strip() == "STOPPED"

        # The SOAP response alone only proves the state machine moved. These two lines
        # prove the event crossed the whole stack: adapter → session manager → pipeline.
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'session: play'")
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'null pipeline: PLAY'")

    with subtest("Spotify Connect onboarding answers getInfo"):
        info = json.loads(sender.succeed(f"curl -sSf '{base}/spotify?action=getInfo'"))
        assert info["remoteName"] == "${friendlyName}", info
        assert info["publicKey"], info

    with subtest("the receiver is discoverable over mDNS from another host"):
        sender.wait_until_succeeds(
            "avahi-browse -rpt _spotify-connect._tcp | grep -q '${friendlyName}'",
            timeout=60,
        )
  '';
}
