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
    CONNECTION_MANAGER = "urn:schemas-upnp-org:service:ConnectionManager:1"

    base = sys.argv[1].rstrip("/")
    command = sys.argv[2]
    argument = sys.argv[3] if len(sys.argv) > 3 else ""


    def escape(text):
        return (text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


    def soap(action, body="", service=SERVICE, path="/dlna/control/AVTransport",
             instance=True):
        # `InstanceID` belongs to AVTransport and RenderingControl; ConnectionManager
        # actions take no instance, and sending one is how this script first got a fault
        # back and blamed the receiver for it.
        head = "<InstanceID>0</InstanceID>" if instance else ""
        envelope = (
            '<?xml version="1.0" encoding="utf-8"?>'
            '<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"'
            ' s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">'
            "<s:Body>"
            '<u:{action} xmlns:u="{service}">{head}{body}</u:{action}>'
            "</s:Body></s:Envelope>"
        ).format(action=action, service=service, head=head, body=body)
        request = urllib.request.Request(
            base + path,
            data=envelope.encode(),
            headers={
                "Content-Type": 'text/xml; charset="utf-8"',
                "SOAPAction": '"{}#{}"'.format(service, action),
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
    elif command == "set-with-metadata":
        # What a real control point sends: the URI *and* a DIDL-Lite blob describing it.
        # Escaped once here because it travels as text inside the SOAP body — which is
        # exactly the nesting a receiver has to get right to show a title at all.
        didl = (
            '<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"'
            ' xmlns:dc="http://purl.org/dc/elements/1.1/"'
            ' xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">'
            '<item id="1" parentID="0" restricted="1">'
            "<dc:title>Windowlicker</dc:title>"
            "<upnp:artist>Aphex Twin</upnp:artist>"
            "<upnp:album>Windowlicker</upnp:album>"
            "<upnp:class>object.item.audioItem.musicTrack</upnp:class>"
            '<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:06:06.000">{}'
            "</res></item></DIDL-Lite>"
        ).format(escape(argument))
        soap(
            "SetAVTransportURI",
            "<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>".format(
                escape(argument), escape(didl)
            ),
        )
    elif command == "protocol-info":
        info = soap(
            "GetProtocolInfo",
            service=CONNECTION_MANAGER,
            path="/dlna/control/ConnectionManager",
            instance=False,
        )
        match = re.search(r"<Sink>([^<]*)</Sink>", info)
        print(match.group(1) if match else "")
        sys.exit(0)
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
  # A scripted CASTv2 sender: TLS to :8009, then length-prefixed protobuf CastMessages.
  #
  # The protobuf is hand-rolled rather than generated. CastMessage is seven scalar
  # fields, so an encoder is ~20 lines — and a hand-written one is a *second*, independent
  # reading of the wire format. Generating from the same .proto the receiver uses would
  # make the test agree with the implementation by construction, which is exactly the
  # agreement worth not assuming.
  # W503: flake8's own docs call it the non-PEP8 half of a mutually exclusive pair —
  # the operators lead their continuation lines here, which is the readable half.
  castSender = pkgs.writers.writePython3Bin "cast-send" { flakeIgnore = [ "E501" "W503" ]; } ''
    import json
    import socket
    import ssl
    import struct
    import sys

    CONNECTION = "urn:x-cast:com.google.cast.tp.connection"
    HEARTBEAT = "urn:x-cast:com.google.cast.tp.heartbeat"
    RECEIVER = "urn:x-cast:com.google.cast.receiver"
    MEDIA = "urn:x-cast:com.google.cast.media"

    host = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8009
    media_url = sys.argv[3] if len(sys.argv) > 3 else "http://example.invalid/cast.mp4"


    def varint(value):
        out = bytearray()
        while True:
            byte = value & 0x7F
            value >>= 7
            out.append(byte | (0x80 if value else 0))
            if not value:
                return bytes(out)


    def tag(field, wire):
        return varint((field << 3) | wire)


    def field_varint(field, value):
        return tag(field, 0) + varint(value)


    def field_bytes(field, value):
        raw = value.encode() if isinstance(value, str) else value
        return tag(field, 2) + varint(len(raw)) + raw


    def encode(source, destination, namespace, payload):
        return (
            field_varint(1, 0)          # protocol_version = CASTV2_1_0
            + field_bytes(2, source)
            + field_bytes(3, destination)
            + field_bytes(4, namespace)
            + field_varint(5, 0)        # payload_type = STRING
            + field_bytes(6, payload)
        )


    def decode(body):
        """Pull namespace and the utf8 payload back out of a CastMessage."""
        fields, i = {}, 0
        while i < len(body):
            key, i = read_varint(body, i)
            field, wire = key >> 3, key & 7
            if wire == 0:
                fields[field], i = read_varint(body, i)
            elif wire == 2:
                length, i = read_varint(body, i)
                fields[field] = body[i:i + length]
                i += length
            else:
                raise ValueError("unexpected wire type {}".format(wire))
        return {
            "namespace": fields.get(4, b"").decode("utf-8", "replace"),
            "payload": fields.get(6, b"").decode("utf-8", "replace"),
        }


    def read_varint(buf, i):
        value, shift = 0, 0
        while True:
            byte = buf[i]
            i += 1
            value |= (byte & 0x7F) << shift
            if not byte & 0x80:
                return value, i
            shift += 7


    # Senders never validate the receiver's certificate — CASTv2 authenticates the
    # *device* (device-auth over the cert), not the transport. Matching that here is
    # accuracy, not laziness.
    context = ssl._create_unverified_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE

    raw = socket.create_connection((host, port), timeout=15)
    tls = context.wrap_socket(raw)
    tls.settimeout(10)

    buffered = bytearray()


    def send(destination, namespace, payload):
        body = encode("sender-0", destination, namespace, json.dumps(payload))
        tls.sendall(struct.pack(">I", len(body)) + body)


    def recv():
        while True:
            if len(buffered) >= 4:
                (length,) = struct.unpack(">I", buffered[:4])
                if len(buffered) >= 4 + length:
                    frame = bytes(buffered[4:4 + length])
                    del buffered[:4 + length]
                    return decode(frame)
            chunk = tls.recv(65536)
            if not chunk:
                raise SystemExit("receiver closed the connection")
            buffered.extend(chunk)


    def expect(namespace, kind):
        """Read until the expected message arrives; unrelated traffic is not an error."""
        for _ in range(10):
            message = recv()
            payload = json.loads(message["payload"]) if message["payload"] else {}
            print("<- {} {}".format(message["namespace"], payload.get("type")))
            if message["namespace"] == namespace and payload.get("type") == kind:
                return payload
        raise SystemExit("never saw {} on {}".format(kind, namespace))


    send("receiver-0", CONNECTION, {"type": "CONNECT"})

    send("receiver-0", HEARTBEAT, {"type": "PING"})
    expect(HEARTBEAT, "PONG")

    send("receiver-0", RECEIVER, {"type": "GET_STATUS", "requestId": 1})
    status = expect(RECEIVER, "RECEIVER_STATUS")
    assert not status["status"].get("applications"), status

    # What a sender asks before it will offer this device as somewhere to cast to. The
    # reply keys off "responseType", not "type", so it needs its own matcher — and a
    # receiver that never answers is one that never appears in the picker.
    send("receiver-0", RECEIVER, {
        "type": "GET_APP_AVAILABILITY",
        "requestId": 10,
        "appId": ["CC1AD845", "0F5096E8", "CA5E8412"],
    })
    for _ in range(10):
        message = recv()
        payload = json.loads(message["payload"]) if message["payload"] else {}
        if payload.get("responseType") == "GET_APP_AVAILABILITY":
            break
    else:
        raise SystemExit("never saw a GET_APP_AVAILABILITY response")
    availability = payload["availability"]
    print("<- availability {}".format(availability))
    assert availability["CC1AD845"] == "APP_AVAILABLE", availability
    assert availability["0F5096E8"] == "APP_AVAILABLE", availability
    # Somebody else's web receiver. We cannot host it and must not claim otherwise.
    assert availability["CA5E8412"] == "APP_UNAVAILABLE", availability

    # And the same honesty on launch. Answering RECEIVER_STATUS here is the G56 failure:
    # the sender opens a connection to a transport id nothing is listening on, and the
    # room gets a connected phone and a black panel.
    send("receiver-0", RECEIVER, {"type": "LAUNCH", "requestId": 11, "appId": "CA5E8412"})
    refusal = expect(RECEIVER, "LAUNCH_ERROR")
    assert refusal["reason"] == "NOT_FOUND", refusal

    send("receiver-0", RECEIVER, {"type": "GET_STATUS", "requestId": 12})
    status = expect(RECEIVER, "RECEIVER_STATUS")
    assert not status["status"].get("applications"), status

    send("receiver-0", RECEIVER, {"type": "LAUNCH", "requestId": 2, "appId": "CC1AD845"})
    status = expect(RECEIVER, "RECEIVER_STATUS")
    apps = status["status"]["applications"]
    assert apps[0]["appId"] == "CC1AD845", status

    send("receiver-0", MEDIA, {
        "type": "LOAD",
        "requestId": 3,
        "media": {"contentId": media_url, "contentType": "video/mp4", "streamType": "BUFFERED"},
        "autoplay": True,
    })
    media = expect(MEDIA, "MEDIA_STATUS")
    assert media["status"][0]["playerState"] == "PLAYING", media

    send("receiver-0", MEDIA, {"type": "PAUSE", "requestId": 4})
    media = expect(MEDIA, "MEDIA_STATUS")
    assert media["status"][0]["playerState"] == "PAUSED", media

    # Having paused, asking must not be told playback resumed. The status a sender reads
    # back is what its transport bar draws itself from.
    send("receiver-0", MEDIA, {"type": "GET_STATUS", "requestId": 5})
    media = expect(MEDIA, "MEDIA_STATUS")
    assert media["status"][0]["playerState"] == "PAUSED", media

    # The volume slider in a sender's cast dialog. Unanswered, it moves and nothing
    # happens; answered with a stale level, it snaps back and looks broken as well.
    send("receiver-0", RECEIVER, {
        "type": "SET_VOLUME",
        "requestId": 6,
        "volume": {"level": 0.25},
    })
    status = expect(RECEIVER, "RECEIVER_STATUS")
    assert abs(status["status"]["volume"]["level"] - 0.25) < 1e-6, status

    # CLOSE to the receiver ends the session; the actor must emit End for it.
    send("receiver-0", CONNECTION, {"type": "CLOSE"})
    tls.close()
    print("cast session completed")
  '';
  # A scripted AirPlay sender: RTSP over a plain TCP socket, bare-path request-URIs and
  # all. It sends two requests in a single write on purpose — a receiver that framed by
  # "one read == one message" passes every other test and fails this one.
  airplaySender = pkgs.writers.writePython3Bin "airplay-send" { flakeIgnore = [ "E501" "W503" ]; } ''
    import plistlib
    import socket
    import sys

    host = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 7000

    sock = socket.create_connection((host, port), timeout=15)
    sock.settimeout(10)
    buffered = bytearray()


    def request(method, path, cseq, flush=True):
        """Queue a request; send it unless the caller is batching."""
        raw = "{} {} RTSP/1.0\r\nCSeq: {}\r\n\r\n".format(method, path, cseq).encode()
        if flush:
            sock.sendall(raw)
        return raw


    def response():
        """Read one response, honoring Content-Length for the body."""
        while b"\r\n\r\n" not in buffered:
            chunk = sock.recv(65536)
            if not chunk:
                raise SystemExit("receiver closed the connection mid-response")
            buffered.extend(chunk)
        head, _, rest = bytes(buffered).partition(b"\r\n\r\n")
        del buffered[:len(head) + 4]
        lines = head.decode("utf-8", "replace").split("\r\n")
        status = int(lines[0].split()[1])
        headers = {}
        for line in lines[1:]:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        length = int(headers.get("content-length", "0"))
        body = bytearray(rest)
        del buffered[:len(rest)]
        while len(body) < length:
            chunk = sock.recv(65536)
            if not chunk:
                raise SystemExit("receiver closed the connection mid-body")
            body.extend(chunk)
        # Anything past this message belongs to the next one.
        buffered[:0] = body[length:]
        print("<- {} CSeq {}".format(status, headers.get("cseq")))
        return status, headers, bytes(body[:length])


    # Two requests, one write: this is the pipelining a real sender does, and it only
    # works if the actor drains its buffer by the parser's consumed count.
    sock.sendall(request("OPTIONS", "*", 1, flush=False) + request("GET", "/info", 2, flush=False))

    status, headers, _ = response()
    assert status == 200, status
    assert headers["cseq"] == "1", headers
    assert "SETUP" in headers.get("public", ""), headers

    status, headers, body = response()
    assert status == 200, status
    # Echoing the *right* CSeq per request is what pipelining depends on.
    assert headers["cseq"] == "2", headers
    info = plistlib.loads(body)
    print("info: " + repr(sorted(info)))
    assert info["name"] == sys.argv[3], info

    # Refused, not faked: pairing isn't implemented, and the receiver must say so rather
    # than 200 its way into a sender that then waits forever for a media plane.
    request("POST", "/pair-setup", 3)
    status, _, _ = response()
    assert status == 501, status

    request("TEARDOWN", "/stream", 4)
    status, _, _ = response()
    assert status == 200, status

    sock.close()
    print("airplay session completed")
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
          # Both off in the shipped defaults — Cast's device auth is still a dev key
          # (Q2/Q11) and AirPlay can't pair (Q1) — and both on here, because their
          # socket actors are exactly what this test exists to exercise.
          enable.cast = true;
          enable.airplay = true;
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
      environment.systemPackages = [ pkgs.curl ssdpSearch dlnaCtl castSender airplaySender ];
    };
  };

  testScript = { nodes, ... }: ''
    import json

    kiosk = "${nodes.receiver.networking.primaryIPAddress}"
    base = f"http://{kiosk}:${toString httpPort}"
    lan = "${nodes.sender.networking.primaryIPAddress}"

    start_all()

    with subtest("the service comes up under the module"):
        receiver.wait_for_unit("castaway.service")
        receiver.wait_for_open_port(${toString httpPort})
        sender.wait_for_unit("multi-user.target")

    with subtest("SSDP M-SEARCH is answered, and every LOCATION it hands out resolves"):
        replies = sender.succeed(f"ssdp-search ssdp:all 5 {lan}")
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

    with subtest("a browser-less build does not offer YouTube at all"):
        # This build has no `cef` feature, so there is no page to launch — and DIAL is
        # launch-only, so every part of a YouTube cast after the launch happens between
        # the phone, YouTube's servers, and a page that would never exist. Advertising it
        # anyway is what D16 forbids: the sender gets a cast target that accepts the
        # launch, reports `running`, and can never play. So: nothing advertised, nothing
        # mounted, and a log line saying why.
        #
        # The launch/stop semantics themselves are covered by proto-dial's own tests, and
        # the whole path (launch → the page binds a Lounge session → the screen actually
        # plays) by `nix run .#yt-selfplay`, which needs the real internet and so cannot
        # live in here.
        assert "urn:dial-multiscreen-org:service:dial:1" not in replies, replies
        sender.succeed(f"curl -sS -o /dev/null -w '%{{http_code}}' {base}/dial/dd.xml | grep -q 404")
        sender.succeed(f"curl -sS -o /dev/null -w '%{{http_code}}' {base}/dial/apps/YouTube | grep -q 404")
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'DIAL disabled'")

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

    with subtest("DLNA metadata and panel controls reach the pipeline"):
        # A control point sends DIDL-Lite alongside the URI, and the receiver stored it
        # and never looked inside — so a DLNA cast put a title on nobody's screen while
        # Bluetooth and Spotify both drew a full card. The blob travels escaped inside the
        # SOAP body, so this also exercises that nesting rather than a bare document.
        sender.succeed(f"dlna-ctl {base} set-with-metadata http://example.invalid/song.mp3")
        assert sender.succeed(f"dlna-ctl {base} play").strip() == "PLAYING"
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q 'null pipeline: NOW PLAYING.*Windowlicker'"
        )
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q 'null pipeline: NOW PLAYING.*Aphex Twin'"
        )

        # DLNA is renderer-is-player, so the panel's transport strip has to be able to
        # drive it. The control surface is what decides whether those buttons are drawn
        # at all, and it is published behind the play that takes the screen.
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q 'session: control surface up.*dlna'"
        )
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'null pipeline: CONTROLS'")
        sender.succeed(f"dlna-ctl {base} stop")

    with subtest("the sink advertises only what this receiver can render"):
        # A control point reads GetProtocolInfo to decide what it may send. image/* was
        # claimed with nothing to render a still, which gets a photo pushed to the panel
        # and a blank screen back.
        sink = sender.succeed(f"dlna-ctl {base} protocol-info").strip()
        assert "video/*" in sink, sink
        assert "audio/*" in sink, sink
        assert "image/" not in sink, sink

    with subtest("Spotify Connect onboarding answers getInfo"):
        info = json.loads(sender.succeed(f"curl -sSf '{base}/spotify?action=getInfo'"))
        # `#spotify`, not the bare name: one box shows up in several pickers at once, and
        # every advertised surface says which one it is. This assertion predates that and
        # has been failing ever since — the suffix is the correct expectation.
        assert info["remoteName"] == "${friendlyName}#spotify", info
        assert info["publicKey"], info

    with subtest("a CASTv2 sender launches the media receiver and LOADs a video"):
        receiver.wait_for_open_port(8009)
        session = sender.succeed(
            f"cast-send {kiosk} 8009 http://example.invalid/cast.mp4"
        )
        assert "cast session completed" in session, session

        # As with DLNA: the sender's own assertions prove the state machine answered.
        # These prove the LOAD crossed the actor into the session manager and pipeline.
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q 'source=cast/.*example.invalid/cast.mp4'"
        )
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'CASTv2 sender connected'")
        # The CLOSE must land as a session End, not a leaked session holding the screen.
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'CASTv2 sender disconnected'")
        # The sender asserted it got LAUNCH_ERROR for an app we cannot host; this proves
        # the refusal was a decision the session took and said out loud, rather than a
        # message that happened to have the right shape.
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q "
            "'declining a LAUNCH for an app this receiver cannot host'"
        )
        # The sender saw its own volume echoed; this proves the change also crossed into
        # the pipeline rather than being applied to session state and stopping there.
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q 'null pipeline: CONTROL.*Volume'"
        )

    with subtest("an AirPlay sender gets OPTIONS, /info, and an honest 501 for pairing"):
        receiver.wait_for_open_port(7000)
        # `#airplay`, not the bare name: every advertised surface says which one it is,
        # and `/info` reports the same name the picker shows.
        session = sender.succeed(f"airplay-send {kiosk} 7000 ${friendlyName}#airplay")
        assert "airplay session completed" in session, session
        # `wait_for_open_port` also produces a connect/disconnect pair, so this next line
        # alone is weak. The pairing refusal is the one only a real request can log — it
        # proves the sender's 501 came from the state machine, not from a closed socket.
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'AirPlay pairing not implemented (Q1) path=/pair-setup'")
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'AirPlay sender connected'")
        # The connection must be *closed*, not leaked: the actor emits End and logs this
        # on the way out. It can't be asserted as 'session: end' the way Cast is — the
        # manager only logs that for the active source, and AirPlay never becomes active
        # while there is no media plane.
        receiver.succeed("journalctl -u castaway --no-pager | grep -q 'AirPlay sender disconnected'")

    with subtest("AirPlay and RAOP are advertised, on the one port that answers"):
        airplay = sender.succeed("avahi-browse -rpt _airplay._tcp")
        assert "${friendlyName}" in airplay, airplay
        assert ";7000;" in airplay, airplay
        raop = sender.succeed("avahi-browse -rpt _raop._tcp")
        # RAOP's instance convention is <deviceid>@<name>, and senders rely on it.
        # avahi's parsable output escapes the '@' as its octal code, so match that —
        # written \\064 because a bare \064 is a Python octal escape and means "4".
        assert "\\064${friendlyName}" in raop, raop
        # Both services on 7000. This used to say 7011, which is not a control port at
        # all — it is the AirPlay 1 UDP timing port — so a sender that believed the
        # advertisement and dialled RAOP's "own" port was reaching a listener that only
        # existed because we had bound one there to match the lie.
        assert ";7000;" in raop, raop

    with subtest("the advertisement promises nothing the receiver cannot serve"):
        # The failure this catches is silent and total: a feature bit or an `et` value
        # we do not implement sends a real iPhone down a flow that ends in a 501, and
        # the only symptom is a device that appears in the picker and then does nothing.
        # These records were, verbatim, a Denon AVR-X3500H's.
        raop = sender.succeed("avahi-browse -rpt _raop._tcp")
        assert "et=0,1" in raop and "et=0,3,5" not in raop, raop  # RSA yes, FairPlay no
        assert "cn=0,1" in raop and "cn=0,1,2,3" not in raop, raop  # no AAC we don't offer
        airplay = sender.succeed("avahi-browse -rpt _airplay._tcp")
        # No `pk`: an empty one publishes an identity a sender cannot verify against.
        assert "pk=" not in airplay, airplay
        # `pi` is a UUID, not the device id echoed back.
        assert "pi=" in airplay, airplay

    with subtest("Cast is advertised over mDNS with the port that answered"):
        txt = sender.succeed("avahi-browse -rpt _googlecast._tcp")
        assert "${friendlyName}" in txt, txt
        assert ";8009;" in txt, txt

    with subtest("the receiver is discoverable over mDNS from another host"):
        sender.wait_until_succeeds(
            "avahi-browse -rpt _spotify-connect._tcp | grep -q '${friendlyName}'",
            timeout=60,
        )
  '';
}
