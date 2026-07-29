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

  # A scripted WFD *source*: accepts the sink's connection, walks M1→M7, streams a few
  # real MPEG2-TS-over-RTP datagrams at the negotiated port, then triggers TEARDOWN so
  # the clean-shutdown path is what gets exercised. A second, independent reading of the
  # wire formats — TS packetisation included — so the assertions mirror what real
  # sources send, not what our sink implementation happens to accept.
  # W503: as in vm-test.nix — operators lead their continuation lines here.
  wfdSource = pkgs.writers.writePython3Bin "wfd-source" { flakeIgnore = [ "E501" "W503" ]; } ''
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
    # three access units as unbounded PES packets — the first carries SPS/PPS/IDR so
    # the very first completed frame is a keyframe and the sink never needs M13. An
    # unbounded PES only completes at the next payload-unit start, so three sent means
    # two delivered; the third is still pending when TEARDOWN lands, and that is fine.


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


    # The sink wires its media plane while handling our PLAY response; a datagram that
    # outruns that TCP segment is silently pre-session.
    time.sleep(0.5)
    rtp_send(0, 90000, [ts_packet(0, 0, pat), ts_packet(PMT_PID, 0, pmt),
                        ts_packet(VIDEO_PID, 0, pes(90000, keyframe))])
    rtp_send(1, 91500, [ts_packet(VIDEO_PID, 1, pes(91500, delta))])
    rtp_send(2, 93000, [ts_packet(VIDEO_PID, 2, pes(93000, delta))])
    print("rtp media sent", flush=True)
    time.sleep(1)

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
        # so the assertion is a nonzero count, keyframe first.
        machine.wait_until_succeeds(
            "journalctl -u castaway --no-pager "
            "| grep -E -q 'encoded video source ended frames=[1-9]'",
            timeout=30,
        )

    with subtest("the triggered teardown ended the session cleanly"):
        machine.fail(
            "journalctl -u castaway --no-pager | grep -q 'the WFD session ended in error'"
        )
        print(machine.succeed("cat /tmp/wfd-source.log"))
  '';
}
