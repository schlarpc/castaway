# A real Play Services Cast sender, on a real L2 segment (#225, second slice).
#
# `checks.android-bt` put the Android emulator on netsim's Bluetooth phy. This one puts
# it on an Ethernet segment: a TAP interface is the whole LAN, castaway binds it, and the
# sender is Android's own system Cast picker — Play Services' implementation, which is a
# different lineage from the openscreen one every other Cast check here uses.
#
# That difference is the point. Both Cast defects that reached users (#184) slipped past
# openscreen; so did all three that #226 found, because openscreen's sender does not ask
# the questions Play Services asks. This check is the one that does.
#
# What it asserts, in order:
#  1. The guest is on the segment — checked head-on, because everything below fails in
#     confusing ways if it is not.
#  2. **The system Cast picker lists us.** That is #226's whole subject: GMS filters that
#     picker by the DNS-SD sub-types in the mDNS answer, so a row appearing means the
#     advertisement is right in the one way our own tests could not see.
#  3. The sender connects on 8009 and **device auth against real Play Services passes**.
#     #40 said a borrowed CKS credential could not survive a real sender's trust check.
#     It does, and this is the check that keeps saying so.
#  4. A mirroring OFFER/ANSWER completes, and then — from the segment capture rather than
#     from our own journal — the phone's RTP actually lands. "Negotiated and silent" is a
#     real failure mode and a log line cannot rule it out.
#
# ## Why the TAP, and what it costs
#
# The emulator's default networking is SLIRP NAT, which multicast crosses in neither
# direction — so mDNS discovery is structurally impossible there and the product is
# discovery. `-net-tap` puts Android's NIC on a host TAP instead, and the TAP *is* the
# segment: one netns holding tap0, castaway and the emulator is a complete two-node LAN.
# No bridge to a real network, and no DHCP server either — the goldfish image hardcodes
# eth0 to SLIRP's 10.0.2.15 and never DHCPs, so putting the host end on 10.0.2.2/24 is
# enough. (dnsmasq was the plan; it turned out unnecessary.)
#
# The netns itself is unprivileged (`unshare --user --map-root-user --net`) and works in
# the build sandbox. Creating a TAP in it does not, by default: the sandbox's /dev has no
# `net/tun` and a userns cannot mknod one. That is the one thing this check needs from the
# host — `extra-sandbox-paths = /dev/net/tun` in nix.conf, which CI sets in the installer's
# extra-conf. The check says so itself rather than failing as "open: No such file".
#
# Not a nixosTest, for the same reason `android-bt` is not: the emulator inside a NixOS
# guest would be nested KVM. This is a plain derivation in the `requiredSystemFeatures =
# ["kvm"]` class.
{ pkgs, castaway, androidComposition }:

let
  sdkRoot = "${androidComposition.androidsdk}/libexec/android-sdk";
  python = pkgs.python3;
  drive = ./android-cast-drive.py;
  rtpCount = ./android-cast-rtp.py;

  # The receiver's address on the segment, and the guest's. Both are fixed: ours by this
  # config, the guest's by the image (see the header).
  receiverIp = "10.0.2.2";
  guestIp = "10.0.2.15";
  # The address the emulator's *own* Wi-Fi SLIRP hands wlan0 — on this same subnet, which
  # is the collision the driver switches Wi-Fi off to avoid. Named here so the three
  # addresses that describe the segment live in one place.
  guestWifiIp = "10.0.2.16";

  config = pkgs.writeText "castaway-android-cast.toml" ''
    friendly_name = "castaway"
    uuid = "0f8c2e10-castaway-0002-0000androidnet"
    http_port = 8080
    # Pinned rather than detected: in the netns tap0 is the only interface, but a wrong
    # advertised address is exactly the kind of thing this check exists to catch, and
    # detection succeeding by having no alternative would hide it.
    interface = "${receiverIp}"

    [enable]
    dlna = false
    spotify = false
    dial = false
    cast = true
    airplay = false
    gamestream = false
    miracast = false
    matter = false
    bluetooth = false

    [cast.replay]
    # The sandbox has no network. The identity the carve derivations bundle is what
    # device auth uses here, which is also what makes assertion 3 mean anything.
    network = false

    # Assertions are read from the app's *own* journal rather than from a redirect of its
    # stdout, because that file is written without ANSI on purpose — "escape codes make
    # grep miss lines whose level marker they wrap", as `logging.rs` puts it, and this
    # check went green on a needle that a coloured `peer=` field had silently defeated.
    # `never` keeps it at one predictable name instead of a dated one.
    [log]
    directory = "/build/castaway-journal"
    rotation = "never"
    file_level = "info"
  '';

  journal = "/build/castaway-journal/castaway.log";

  # Everything network-y, inside the namespace. A separate script because the whole body
  # runs under `unshare` as one command, and inlining it into the builder would mean
  # escaping a shell inside a shell inside a nix string.
  inner = pkgs.writeShellScript "castaway-android-cast-inner" ''
    set -uo pipefail
    export PATH=${
      pkgs.lib.makeBinPath [ pkgs.iproute2 pkgs.tcpdump pkgs.procps pkgs.coreutils pkgs.gnugrep ]
    }:$PATH

    echo "== the segment"
    ip link set lo up
    ip tuntap add dev tap0 mode tap || {
      echo "FAIL: could not create tap0 in the sandbox's network namespace."
      echo "This check needs /dev/net/tun inside the build sandbox, which nix does not"
      echo "bind-mount by default. Add to nix.conf (or the installer's extra-conf):"
      echo "    extra-sandbox-paths = /dev/net/tun"
      exit 1
    }
    ip addr add ${receiverIp}/24 dev tap0
    ip link set tap0 up
    ip -o addr show

    # The whole run, on the wire. This is the fixture rule 9 was promised for the network
    # leg, and assertion 4 is read back out of it.
    tcpdump -i tap0 -s0 -U -w "$WORK/segment.pcap" > "$WORK/tcpdump.log" 2>&1 &
    TCPDUMP_PID=$!

    echo "== emulator on tap0"
    # `-net-tap-script-up/down no`: the emulator otherwise runs /etc/qemu-ifup, which does
    # not exist here and does not need to — the interface is already configured above.
    ${sdkRoot}/emulator/emulator -avd cast \
      -no-window -no-audio -no-boot-anim -no-snapshot -no-metrics \
      -gpu swiftshader_indirect -memory 3072 -cores 2 \
      -net-tap tap0 -net-tap-script-up no -net-tap-script-down no \
      > "$WORK/emulator.log" 2>&1 &

    echo "== castaway on the same segment"
    # stdout is kept too, but only for diagnosis: anything that goes wrong before the
    # journal exists — a config that will not parse, a credential that will not load —
    # is reported there and nowhere else.
    CASTAWAY_CONFIG=${config} \
      ${castaway}/bin/castaway > "$WORK/castaway-stdout.log" 2>&1 &
    CASTAWAY_PID=$!

    echo "== driving the phone"
    ADB=${sdkRoot}/platform-tools/adb \
    CASTAWAY_LOG=${journal} \
    RECEIVER_IP=${receiverIp} GUEST_IP=${guestIp} GUEST_WIFI_IP=${guestWifiIp} \
      ${python}/bin/python3 ${drive}
    DRIVE_RC=$?

    ${sdkRoot}/platform-tools/adb logcat -d > "$WORK/logcat.txt" 2>&1 || true

    # Stop the capture before it is read, and *wait for it to be gone* rather than
    # sleeping at it: the file is complete when the writer has exited, and nothing else
    # is a guarantee.
    kill $TCPDUMP_PID 2>/dev/null || true
    wait $TCPDUMP_PID 2>/dev/null || true
    kill $CASTAWAY_PID 2>/dev/null || true

    if [ $DRIVE_RC -ne 0 ]; then
      echo "== drive failed; the tails that matter:"
      tail -40 "$WORK/emulator.log"
      tail -40 "$WORK/castaway-stdout.log"
      tail -80 ${journal} 2>/dev/null
      exit 1
    fi

    echo "== what the phone actually sent"
    # The port the ANSWER told the sender to use, taken from our own journal so the
    # capture is judged against the session that really happened.
    PORT=$(grep -o "udp_port=[0-9]*" ${journal} | tail -1 | cut -d= -f2)
    if [ -z "$PORT" ]; then
      echo "FAIL: no mirroring port in the journal, so nothing to count"
      exit 1
    fi
    echo "negotiated media port: $PORT"
    # Twenty seconds of a phone's screen is thousands of packets; the floors are set low
    # enough that only "it sent essentially nothing" trips them.
    ${python}/bin/python3 ${rtpCount} \
      "$WORK/segment.pcap" ${guestIp} ${receiverIp} "$PORT" 200 100000
  '';
in
pkgs.runCommand "castaway-android-cast"
{
  nativeBuildInputs = [ python pkgs.jdk_headless pkgs.util-linux ];
  requiredSystemFeatures = [ "kvm" ];
  # Boot, a GMS scan, a mirroring session. Minutes; a wedged emulator should not hold the
  # builder for the global default. Generous, then dead.
  meta.timeout = 3600;
} ''
  set -uo pipefail
  export HOME=$TMPDIR USER=builder WORK=$TMPDIR
  export ANDROID_SDK_ROOT=${sdkRoot}
  export ANDROID_AVD_HOME=$TMPDIR/avd ANDROID_USER_HOME=$TMPDIR/android-home
  mkdir -p $ANDROID_AVD_HOME $ANDROID_USER_HOME $out $(dirname ${journal})

  # The artifacts, kept whether the check passed or failed — the segment capture is the
  # network leg's half of the fixture factory #225 promised (rule 9): a real Play
  # Services sender's mDNS, CASTv2 and mirroring RTP, regenerable by rebuilding this.
  keep() {
    cp "$WORK"/castaway-stdout.log "$WORK"/emulator.log "$WORK"/logcat.txt \
       "$WORK"/segment.pcap ${journal} $out/ 2>/dev/null || true
  }
  cleanup() {
    pkill -P $$ 2>/dev/null || true
    pkill -f qemu-system 2>/dev/null || true
  }
  trap cleanup EXIT

  echo "== creating the AVD"
  echo no | ${androidComposition.androidsdk}/bin/avdmanager create avd --force \
    -n cast -k "system-images;android-35;google_apis;x86_64"

  # One `unshare` for the whole run: the tap, the emulator, the receiver and adb all have
  # to be in the same network namespace, and joining one from outside would need the same
  # user namespace anyway.
  if unshare --user --map-root-user --net -- ${inner}; then
    keep
    echo ok > $out/result
  else
    keep
    exit 1
  fi
''
