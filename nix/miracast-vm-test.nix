# Tier-2 for the radio itself (ground rule 6): mac80211_hwsim gives one kernel two real
# mac80211 radios sharing a virtual medium, so the one part of Miracast no loopback test
# can touch — Wi-Fi Direct group formation, WPS push-button enrolment, DHCP over the
# group, and the sink dialling out across it — runs in CI with no hardware.
#
# One VM rather than two on purpose, and not the usual discovery reason: hwsim's medium
# is per-kernel, so the second radio *cannot* live in a second VM. The honesty a second
# host would have provided comes from a network namespace instead — the sender's radio is
# moved into its own namespace, which gives it its own IP stack, so the DHCP exchange,
# the ARP resolution and the RTSP dial all genuinely cross the radio rather than being
# short-circuited through loopback by a kernel that can see both ends.
#
# The sender side is wpa_supplicant driven by wpa_cli — an implementation that has never
# seen our code — plus a scripted WFD source speaking M1→M7 (the same exchange as
# proto-miracast's loopback test, independently rewritten). Deliberately *not* scripted
# here: no ping from the sender towards the sink. The sink has to find the peer's address
# with its own neighbour-table sweep, because a real Windows client will not speak first.
{ pkgs, self }:

let
  # The sender's supplicant: a P2P source device that would rather the sink be the group
  # owner (go_intent=0, like Android) and enrols by push-button.
  senderWpaConf = pkgs.writeText "wfd-sender-wpa.conf" ''
    ctrl_interface=/run/wpa_supplicant-sender
    update_config=0
    device_name=hwsim-source
    device_type=1-0050F204-1
    config_methods=virtual_push_button
    p2p_go_intent=0
  '';

  # udhcpc's action script: apply the lease, nothing else. The mask arrives dotted
  # ($subnet), which iproute2 accepts.
  udhcpcApply = pkgs.writeShellScript "udhcpc-apply" ''
    case "$1" in
      bound|renew)
        ${pkgs.iproute2}/bin/ip addr replace "$ip/$subnet" dev "$interface"
        ;;
    esac
  '';

  # A scripted WFD *source*: accepts the sink's connection, walks M1→M7, streams real
  # MPEG2-TS-over-RTP at the negotiated port — mid-GOP, with datagrams dropped on a
  # schedule, answering the sink's M13s with keyframes — then triggers TEARDOWN so the
  # clean-shutdown path is what gets exercised. A second, independent reading of the
  # wire formats — TS packetisation included — so the assertions mirror what real
  # sources send, not what our sink implementation happens to accept.
  # W503: as in vm-test.nix — operators lead their continuation lines here.
  wfdSource = pkgs.writers.writePython3Bin "wfd-source" { flakeIgnore = [ "E501" "W503" ]; } ''
    import select
    import socket
    import struct
    import time

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", 7236))
    srv.listen(1)
    srv.settimeout(300)
    print("listening on 7236", flush=True)
    conn, peer = srv.accept()
    conn.settimeout(60)
    print("sink connected from {}:{}".format(*peer), flush=True)

    buffered = bytearray()
    mark = 0


    def send(text):
        conn.sendall(text.encode())


    def phase(*needles):
        """Read until every needle appears past the previous phase's high-water mark.

        The mark matters: the sink's OPTIONS Public header contains the words SETUP,
        PLAY and TEARDOWN, so searching the whole buffer would satisfy later phases
        with bytes from earlier ones.
        """
        global mark
        while True:
            text = buffered[mark:].decode("utf-8", "replace")
            if all(needle in text for needle in needles):
                mark = len(buffered)
                return text
            chunk = conn.recv(65536)
            if not chunk:
                raise SystemExit("sink closed the connection; buffered: " + text)
            buffered.extend(chunk)


    def take_messages():
        """Split whole RTSP messages out of the buffer, advancing the same high-water mark.

        `phase` is a substring search, which is all the negotiation needs; the media
        phase needs the CSeq of each request so it can answer it, so it needs framing.
        Both move `mark`, so the two can be interleaved — and they are: the media loop
        runs between M7 and the TEARDOWN phase.
        """
        global mark
        out = []
        while True:
            blob = bytes(buffered[mark:])
            head_end = blob.find(b"\r\n\r\n")
            if head_end < 0:
                return out
            head = blob[:head_end].decode("utf-8", "replace")
            length = 0
            for line in head.split("\r\n"):
                if line.lower().startswith("content-length:"):
                    length = int(line.split(":", 1)[1].strip() or 0)
            total = head_end + 4 + length
            if len(blob) < total:
                return out
            out.append(blob[:total].decode("utf-8", "replace"))
            mark += total


    # M1, with the Server: header a real Windows source sends. The sink must answer it
    # *and* open its own M2 OPTIONS — both arrive together.
    send("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nRequire: org.wfa.wfd1.0\r\nServer: MSMiracastSource/10.0\r\n\r\n")
    text = phase("RTSP/1.0 200", "OPTIONS")
    assert "Public: org.wfa.wfd1.0" in text, text
    send("RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER\r\n\r\n")

    # M3: the sink's capabilities, and the port it promises must be the advertised one.
    names = "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n"
    send("GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 2\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{}".format(len(names), names))
    text = phase("wfd_client_rtp_ports:")
    assert "unicast 1028 0 mode=play" in text, text

    # M4 and M5 coalesced into one segment, which real sources do and which only an
    # incremental parser survives. The presentation URL carries our real group address,
    # exactly as a source outside localhost would send it.
    url = "rtsp://{}/wfd1.0/streamid=0".format(conn.getsockname()[0])
    m4 = ("wfd_video_formats: 00 00 03 10 00000100 00000000 00000000 00 0000 0000 00 none none\r\n"
          "wfd_audio_codecs: AAC 00000001 00\r\n"
          "wfd_presentation_URL: {} none\r\n".format(url))
    m5 = "wfd_trigger_method: SETUP\r\n"
    send("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 3\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{}".format(len(m4), m4)
         + "SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 4\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{}".format(len(m5), m5))

    # M6: the transport request must name the port from M3.
    text = phase("SETUP")
    assert "client_port=1028" in text, text
    send("RTSP/1.0 200 OK\r\nCSeq: 2\r\nSession: 4242;timeout=30\r\nTransport: RTP/AVP/UDP;unicast;client_port=1028;server_port=19000\r\n\r\n")

    # M7.
    phase("PLAY")
    send("RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 4242\r\n\r\n")

    # --- The media plane: hand-rolled MPEG2-TS over RTP, sent to the port the sink
    # promised in M3, across the group interface. PAT, PMT (H.264 on PID 0x1011), and
    # a run of access units as unbounded PES packets.
    #
    # Two things the sink has to survive here, neither of which this source used to send
    # (#192):
    #
    # 1. **A mid-GOP join.** The stream opens on P-frames and stays that way until the
    #    sink asks for a keyframe. This source used to send an IDR first, with a comment
    #    saying it did so "so the sink never needs M13" — which meant WFD's only loss
    #    recovery primitive, and the entire justification for hand-rolling this demuxer
    #    rather than using ffmpeg's rtp_mpegts (D35), had never fired at any tier.
    # 2. **Loss.** The hwsim medium is lossless, jitter-free and instantaneous, so the
    #    reorder buffer, the continuity-counter resync and the drop-late-frames policy
    #    had never absorbed anything either. Datagrams are dropped on a schedule rather
    #    than with `tc`/netem, because a schedule can drop a *specific* datagram — which
    #    is what makes a continuity gap assertable rather than merely probable.
    #
    # An unbounded PES only completes at the next payload-unit start, so the last access
    # unit sent is still pending when TEARDOWN lands. That is fine and expected.


    def ts_packet(pid, cc, payload):
        """One 188-byte packet, payload-only (no adaptation field), stuffed with 0xff.

        The stuffing is safe in both uses: PSI readers stop at section_length, and in
        an unbounded PES the tail bytes ride inside the last NAL unit, where anything
        without a start code is legal.
        """
        assert len(payload) <= 184
        header = bytes([0x47, 0x40 | (pid >> 8), pid & 0xFF, 0x10 | (cc & 0x0F)])
        return header + payload + b"\xff" * (184 - len(payload))


    def psi(table_id, body):
        """pointer_field + section header + body + a CRC the demux ignores."""
        length = len(body) + 4
        return (b"\x00" + bytes([table_id, 0xB0 | (length >> 8), length & 0xFF])
                + body + b"\x00\x00\x00\x00")


    def pes(pts, payload):
        """An unbounded video PES (length 0) with a PTS, stream id 0xE0."""
        pts_bytes = bytes([
            0x21 | ((pts >> 29) & 0x0E),
            (pts >> 22) & 0xFF,
            0x01 | ((pts >> 14) & 0xFE),
            (pts >> 7) & 0xFF,
            0x01 | ((pts << 1) & 0xFE),
        ])
        return b"\x00\x00\x01\xe0\x00\x00" + bytes([0x80, 0x80, 5]) + pts_bytes + payload


    def annex_b(*nals):
        return b"".join(b"\x00\x00\x00\x01" + nal for nal in nals)


    VIDEO_PID = 0x1011
    PMT_PID = 0x1000
    # program 1 -> PMT; then program 1, PCR on the video PID, H.264 (0x1B) on it.
    pat = psi(0x00, bytes([0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xE0 | (PMT_PID >> 8), PMT_PID & 0xFF]))
    pmt = psi(0x02, bytes([0x00, 0x01, 0xC1, 0x00, 0x00, 0xE0 | (VIDEO_PID >> 8), VIDEO_PID & 0xFF, 0xF0, 0x00,
                           0x1B, 0xE0 | (VIDEO_PID >> 8), VIDEO_PID & 0xFF, 0xF0, 0x00]))
    keyframe = annex_b(b"\x67\x42\x00\x1f\xe9\x02\x80", b"\x68\xce\x06\xe2", b"\x65\x88\x80\x40" + b"\x10" * 24)
    delta = annex_b(b"\x41\x9a\x24\x6c" + b"\x10" * 24)

    rtp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sink = conn.getpeername()[0]


    def rtp_send(seq, pts, packets):
        header = struct.pack(">BBHII", 0x80, 33, seq, pts & 0xFFFFFFFF, 0x1234)
        rtp.sendto(header + b"".join(packets), (sink, 1028))


    # 25 fps: slow enough that a VM keeps up, fast enough that the sink's reorder depth
    # (8 packets) is crossed in well under half a second, so a dropped datagram becomes a
    # *known* loss inside one frame interval of the schedule rather than at teardown.
    FRAME_INTERVAL = 0.04

    seq = 0
    cc = 0
    pts = 90000
    sent = 0
    dropped = []
    idr_requests = []
    owed_idr = False
    started = 0.0


    def pump_control():
        """Answer whatever the sink says, without blocking, and note any M13.

        The only request it makes during playback is `wfd_idr_request`, and it makes it
        because it cannot decode what it is being sent — so a source that answered 200 OK
        and did not then send a keyframe would be agreeing to help and not helping.
        """
        global owed_idr
        while select.select([conn], [], [], 0)[0]:
            chunk = conn.recv(65536)
            if not chunk:
                raise SystemExit("sink closed the control connection mid-stream")
            buffered.extend(chunk)
        for message in take_messages():
            if message.startswith("RTSP/1.0"):
                continue  # a response to something we sent
            cseq = ""
            for line in message.split("\r\n"):
                if line.lower().startswith("cseq:"):
                    cseq = line.split(":", 1)[1].strip()
            if "wfd_idr_request" in message:
                idr_requests.append(time.monotonic() - started)
                owed_idr = True
                print("m13 #{} at {:.3f}s".format(len(idr_requests), idr_requests[-1]), flush=True)
            send("RTSP/1.0 200 OK\r\nCSeq: {}\r\n\r\n".format(cseq))


    def send_au(payload, drop=False):
        """One access unit: one PES, one TS packet, one RTP datagram.

        The RTP sequence number and the continuity counter advance whether or not the
        datagram goes out, which is the whole trick — the source produced it, the air
        lost it, and what the sink sees is a hole in one and a jump in the other.
        """
        global seq, cc, pts, sent
        packet = ts_packet(VIDEO_PID, cc, pes(pts, payload))
        if drop:
            dropped.append(seq)
            print("dropping datagram seq={}".format(seq), flush=True)
        else:
            rtp_send(seq, pts, [packet])
            sent += 1
        seq += 1
        cc = (cc + 1) & 0x0F
        pts += 3600


    def run(seconds, drop_at=()):
        """Send access units for `seconds`, dropping the ones whose index is in `drop_at`.

        A keyframe goes out in place of the next P-frame whenever the sink has asked for
        one. That is the source's entire half of M13, and it is what makes "decoding
        recovers" observable rather than assumed.
        """
        global owed_idr
        n = 0
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            pump_control()
            # The schedule wins ties. An owed keyframe waits a frame rather than taking a
            # scheduled drop's slot, because a drop that happened to land on the frame the
            # sink had just asked for would silently make this a one-loss test — and the
            # only symptom would be a count at the very end that no longer added up.
            drop = n in drop_at
            if owed_idr and not drop:
                send_au(keyframe)
                owed_idr = False
            else:
                send_au(delta, drop=drop)
            n += 1
            time.sleep(FRAME_INTERVAL)


    # The sink wires its media plane while handling our PLAY response; a datagram that
    # outruns that TCP segment is silently pre-session.
    time.sleep(0.5)
    started = time.monotonic()
    # The program tables get their own datagram, so nothing below can drop them.
    rtp_send(seq, pts, [ts_packet(0, 0, pat), ts_packet(PMT_PID, 0, pmt)])
    seq += 1

    # Mid-GOP. The sink has no keyframe and cannot get one by waiting, so it must ask.
    # Long enough that the limiter window from that first M13 has expired before the
    # first loss, or the two triggers would be indistinguishable.
    run(1.6)
    # Loss, twice, deliberately close together. The first is answered; the second lands
    # inside the limiter's window, which is the half that matters in the field — a real
    # capture shows a sink firing eight M13s back to back and turning a lossy link into
    # an unusable one. Eleven frames apart so each hole crosses the reorder depth on its own.
    run(2.4, drop_at=(5, 16))
    # And recovery: frames keep flowing, and the sink stops asking.
    run(1.2)
    print("rtp media sent: {} datagrams, dropped {}".format(sent, len(dropped)), flush=True)
    print("idr requests: {}".format(["{:.3f}".format(t) for t in idr_requests]), flush=True)
    gaps = [b - a for a, b in zip(idr_requests, idr_requests[1:])]
    print("idr gaps: {}".format(["{:.3f}".format(g) for g in gaps]), flush=True)
    # Reported rather than asserted here, deliberately: an assertion in this process
    # exits it before the TEARDOWN below, which would take the clean-shutdown subtest
    # down with it and make one failure look like two. The numbers are the source's; the
    # judgement is the test script's.
    assert len(dropped) == 2, "this schedule's own bookkeeping is wrong: {}".format(dropped)

    # Source-triggered teardown: the sink must come back with its own TEARDOWN rather
    # than just dropping the socket.
    trigger = "wfd_trigger_method: TEARDOWN\r\n"
    send("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 5\r\nContent-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{}".format(len(trigger), trigger))
    phase("TEARDOWN")
    send("RTSP/1.0 200 OK\r\nCSeq: 4\r\nSession: 4242\r\n\r\n")

    print("wfd source session completed", flush=True)
  '';
in
pkgs.testers.runNixOSTest {
  name = "castaway-miracast";

  nodes.machine = { config, ... }: {
    imports = [ self.nixosModules.castaway ];

    # Two virtual radios: wlan0 for the sink, wlan1 for the sender.
    boot.kernelModules = [ "mac80211_hwsim" ];

    services.castaway = {
      enable = true;
      # The protocol stack, not the kiosk — same reasoning as vm-test.nix, and the null
      # pipeline's log lines are what the mirror assertions grep for.
      package = self.packages.${pkgs.stdenv.hostPlatform.system}.castaway-portable;
      logLevel = "info,castaway=debug,proto_miracast=debug";
      settings = {
        friendly_name = "castaway-vm";
        uuid = "0f8c2e10-0000-4000-8000-0000000c0572";
        interface = config.networking.primaryIPAddress;
        enable.miracast = true;
        miracast = {
          interface = "wlan0";
          # Deterministic and social (channel 6): the sender's discovery scan must find
          # the group's beacons without a full sweep.
          freq_mhz = 2437;
        };
      };
    };

    environment.systemPackages = [ pkgs.iw pkgs.wpa_supplicant wfdSource ];
  };

  testScript = ''
    import re

    def one(pattern, text):
        """`re.search` that says what it was looking for when it finds nothing.

        Also what the driver's type checker wants: a bare `re.search(...).group(1)` is
        `None.group` on the failing path, and it refuses the script rather than the run.
        """
        found = re.search(pattern, text)
        assert found, f"nothing matched {pattern!r} in:\n{text}"
        return found

    # The sender's P2P management lives on wlan1's control socket; commands only touch
    # the socket, so they need no namespace.
    wcli = "wpa_cli -p /run/wpa_supplicant-sender -i wlan1"

    start_all()

    with subtest("the module brings up wpa_supplicant and castaway asks it for a group"):
        machine.wait_for_unit("castaway-wpa.service")
        machine.wait_for_unit("castaway.service")
        machine.succeed("iw dev wlan0 info")
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'autonomous P2P group requested'",
            timeout=90,
        )

    with subtest("the group forms, we are the owner, and networkd addresses it"):
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'P2P group up; we are the group owner'",
            timeout=90,
        )
        machine.wait_until_succeeds("ip -o link show p2p-wlan0-0", timeout=30)
        machine.wait_until_succeeds(
            "ip -4 -o addr show p2p-wlan0-0 | grep -q 192.168.77.1", timeout=60
        )

    with subtest("a second radio in its own namespace discovers the sink over the air"):
        machine.succeed("ip netns add wfdsource")
        phy = machine.succeed("iw dev wlan1 info | awk '/wiphy/ {print \"phy\" $2}'").strip()
        machine.succeed(f"iw phy {phy} set netns name wfdsource")
        machine.succeed("ip netns exec wfdsource ip link set lo up")
        machine.succeed(
            "ip netns exec wfdsource wpa_supplicant -B -i wlan1 -D nl80211 "
            "-c ${senderWpaConf} -f /tmp/sender-wpa.log"
        )
        # nixpkgs patches wpa_cli's *client*-socket directory to
        # /run/wpa_supplicant/client, which normally the NixOS wireless module
        # creates; nothing in this test does, and without it every wpa_cli call
        # dies before it ever connects to the -p path it was given.
        machine.succeed("mkdir -p /run/wpa_supplicant/client")
        machine.wait_until_succeeds("test -S /run/wpa_supplicant-sender/wlan1", timeout=30)
        machine.succeed(f"{wcli} set wifi_display 1")
        machine.succeed(f"{wcli} wfd_subelem_set 0 000600111c440032")
        machine.succeed(f"{wcli} p2p_find")
        peer = machine.wait_until_succeeds(
            f"{wcli} p2p_peers | grep -m1 :", timeout=90
        ).strip()
        info = machine.wait_until_succeeds(f"{wcli} p2p_peer {peer}", timeout=30)
        # The name Win+K would show, and the WFD IE that makes us a sink worth showing:
        # 1c44 is 7236, the RTSP control port in our device information subelement.
        assert "castaway-vm#miracast" in info, info
        assert "1c44" in info, info

    with subtest("the scripted WFD source is listening before anyone joins"):
        machine.succeed(
            "ip netns exec wfdsource sh -c 'wfd-source >/tmp/wfd-source.log 2>&1 &'"
        )
        machine.wait_until_succeeds("grep -q 'listening on 7236' /tmp/wfd-source.log", timeout=30)

    with subtest("push-button join forms the link and DHCP addresses the sender"):
        machine.succeed(f"{wcli} p2p_connect {peer} pbc join")
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'authorising push-button enrolment'",
            timeout=90,
        )
        group_iface = machine.wait_until_succeeds(
            "ip netns exec wfdsource ip -o link | grep -o 'p2p-wlan1-[0-9]*' | head -n1",
            timeout=60,
        ).strip()
        machine.wait_until_succeeds(
            f"wpa_cli -p /run/wpa_supplicant-sender -i {group_iface} status "
            "| grep -q wpa_state=COMPLETED",
            timeout=60,
        )
        machine.succeed(
            f"ip netns exec wfdsource ${pkgs.busybox}/bin/udhcpc "
            f"-i {group_iface} -n -q -t 20 -T 2 -s ${udhcpcApply}"
        )
        lease = machine.succeed(
            f"ip netns exec wfdsource ip -4 -o addr show {group_iface}"
        )
        assert "192.168.77." in lease, lease

    with subtest("the sink finds the peer itself and negotiates M1→M7 to a mirror"):
        # No help from the sender here: castaway's own subnet sweep has to resolve the
        # peer's address, dial its 7236, and negotiate.
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'WFD session opened'", timeout=60
        )
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'mirroring started'", timeout=60
        )
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'null pipeline: MIRROR begin'",
            timeout=30,
        )
        machine.wait_until_succeeds(
            "grep -q 'wfd source session completed' /tmp/wfd-source.log", timeout=60
        )

    with subtest("real TS-over-RTP frames crossed the radio into the pipeline"):
        # The source hand-rolled PAT/PMT/PES and sent them over the group interface;
        # the count is logged when teardown closes the plane. Zero here is exactly the
        # §7.2 failure — a sink that advertised a port it was not really reachable on —
        # so the assertion is a nonzero count.
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager "
            "| grep -E -q 'encoded video source ended frames=[1-9]'",
            timeout=30,
        )

    with subtest("the sink asks for an IDR, and keeps asking no faster than once a second"):
        # WFD's only loss-recovery primitive, and until #192 it had never fired at any
        # tier — this source used to send a keyframe first precisely so that it would not
        # have to. Now the stream opens mid-GOP and loses two datagrams on a schedule, so
        # the sink has to ask twice for two different reasons: once because it has no
        # keyframe at all, and once because the coded video it does have is missing bytes.
        #
        # The rate limiter is asserted by the *source*, from the arrival times of the
        # requests it received, because that is where the number matters: a sink that
        # asks on every damaged frame turns a lossy link into an unusable one, and both
        # halves of that are invisible from inside the sink.
        source_log = machine.succeed("cat /tmp/wfd-source.log")
        print(source_log)
        assert "m13 #2" in source_log, (
            f"the sink asked for an IDR at most once:\n{source_log}"
        )
        # `IDR_MIN_INTERVAL` is one second, and this is that number on a real wire. The
        # T1 test (#192, `45cbae2`) pins it against a clock the test controls; here it is
        # the source's own arrival times, over a radio, with two independent triggers
        # landing inside one window.
        gaps = [
            float(g)
            for g in re.findall(r"[\d.]+", one(r"idr gaps: \[(.*)\]", source_log).group(1))
        ]
        assert gaps, f"only one M13 ever arrived, so the limiter proves nothing:\n{source_log}"
        assert min(gaps) >= 0.95, (
            f"two M13s {min(gaps):.3f}s apart; the limiter is what keeps a lossy link "
            f"usable, and a sink without one sends eight in a row"
        )
        # Our own side of the same events, so a request the source imagined is caught.
        asked = int(machine.succeed(
            "journalctl -u castaway --no-pager | grep -c 'asking the source for an IDR'"
        ).strip())
        received = source_log.count("m13 #")
        assert asked == received, (
            f"the sink logged {asked} IDR requests and the source received {received}"
        )

    with subtest("the loss the source injected is the loss the sink reports"):
        # The media plane's counters, said out loud once at teardown. Every one of these
        # existed before #192 and none of them was ever logged, so a bad mirror in the
        # field left nothing behind to tell a lossy radio from a slow decoder from a
        # source sending to the wrong port.
        closed = machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -m1 'miracast: media plane closed'",
            timeout=30,
        )
        print(closed)
        # Exactly the two datagrams the source withheld, found by the holes they left —
        # and *only* those, on a medium with no loss of its own. `foreign` and `late`
        # stay zero for the same reason, and a nonzero one of those would mean something
        # other than this source is reaching our media port.
        assert "lost=2" in closed, closed
        assert "late=0" in closed, closed
        assert "foreign=0" in closed, closed
        # Each hole reaching the demuxer as a continuity-counter jump on the video PID:
        # this is the signal the IDR request is derived from, and asserting the two
        # numbers together is what says the chain from a missing datagram to an M13 is
        # whole rather than two coincidences.
        assert "video_gaps=2" in closed, closed

    with subtest("the mirror recovers rather than freezing until the next natural IDR"):
        # What the loss would cost without M13: an AOSP source puts IDRs fifteen seconds
        # apart, so a single lost reference is a fifteen-second frozen screen. Here the
        # source answers each request with a keyframe, and the assertion is that frames
        # kept arriving after the last one — the count is a large fraction of what was
        # sent, rather than the handful that would arrive if the plane had stalled.
        sent = int(one(r"rtp media sent: (\d+) datagrams", source_log).group(1))
        delivered = int(one(
            r"encoded video source ended frames=(\d+)",
            machine.succeed(
                "journalctl -u castaway --no-pager | grep -m1 'encoded video source ended'"
            ),
        ).group(1))
        print(f"{delivered} frames delivered of {sent} datagrams sent")
        # Not equality: the two datagrams dropped cost the access units they carried *and*
        # the ones the gaps broke, an unbounded PES only completes at the next one's
        # start, and the tables' datagram carries no frame at all.
        assert delivered >= sent * 0.8, (
            f"only {delivered} of {sent} access units survived; the plane stalled"
        )

    with subtest("the triggered teardown ended the session cleanly"):
        machine.fail(
            "journalctl -u castaway --no-pager | grep -q 'the WFD session ended in error'"
        )
        print(machine.succeed("cat /tmp/wfd-source.log"))
  '';
}
