# A real phone stack as the scripted sender: the Android emulator on netsim's phy (#225).
#
# Every other Bluetooth check talks to BlueZ — a Linux stack that has never shipped in a
# phone. This one boots Android itself, headless under KVM, with its virtual controller
# on netsim/rootcanal — and castaway joins the same phy as one more controller, over the
# H4-over-TCP transport, exactly the way a fourth `HciTransport` backend should: nothing
# above `substrate-hci` knows the radio is simulated.
#
# What it asserts, in order:
#  1. Android's inquiry finds us, and its Settings UI pairs with us — the consent tap is
#     a real dialog found by uiautomator, because Android has no shell command that
#     pairs, and that is the point: this is the flow a person drives.
#  2. The #211 exchange: the phone registers REGISTER_NOTIFICATION(VOLUME_CHANGED) and
#     hears its INTERIM — asserted from our journal, upgraded from an iPhone observation
#     to CI.
#  3. The stream negotiates aptX HD — the *best* codec we advertise, which is the one a
#     real phone reaches for and the one no BlueZ default path exercises. Pinned, so an
#     image bump that changes Android's preference order is a red check to read, not a
#     silent downgrade.
#  4. The signal survives: VLC (pinned APK — the same sender #87 names) plays a known
#     waveform, the mixer's recording is correlated against it per channel, same
#     correlator and thresholds as `checks.bluetooth-vm`. A real Android encoder's
#     aptX HD, decoded bit-true, is what retires the "aptX HD never met a phone" caveat.
#
# Not a nixosTest on purpose: the emulator inside a NixOS guest would be nested KVM.
# This is a plain derivation that runs QEMU itself, so it sits in the same
# `requiredSystemFeatures = ["kvm"]` class as the VM checks without the second layer.
#
# netsim wrinkles this file works around, each found on the bench:
#  - netsimd exits when its last device disconnects, so the emulator (whose gRPC stream
#    is a device) starts before castaway and outlives it.
#  - a chip that vanishes mid-link leaves the *peer* holding a stale ACL — rootcanal
#    emits no Disconnection Complete for it. One castaway process for the whole check,
#    started before Bluetooth comes up, so the situation cannot arise.
#  - `--pcap` records every chip's HCI transcript; they land in $out as the fixture
#    factory rule 9 was promised (#225): real Android A2DP/AVRCP/AVDTP exchanges,
#    regenerable by rebuilding this check.
{ pkgs, castaway, androidComposition, vlcApk }:

let
  sdkRoot = "${androidComposition.androidsdk}/libexec/android-sdk";
  python = pkgs.python3.withPackages (ps: [ ps.numpy ]);
  signal = ./bluetooth-a2dp-signal.py;
  drive = ./android-bt-drive.py;

  config = pkgs.writeText "castaway-android-bt.toml" ''
    friendly_name = "castaway"
    uuid = "0f8c2e10-castaway-0001-0000androidbt"
    http_port = 8080

    [enable]
    dlna = false
    spotify = false
    dial = false
    cast = false
    airplay = false
    gamestream = false
    miracast = false
    matter = false
    bluetooth = true

    [bluetooth]
    transport = "tcp:127.0.0.1:6402"
    state_dir = "/build/castaway-state"

    [audio]
    record = "/build/castaway-state/mix.wav"
  '';
in
pkgs.runCommand "castaway-android-bt"
{
  nativeBuildInputs = [ python pkgs.jdk_headless ];
  requiredSystemFeatures = [ "kvm" ];
  # Boot plus pairing plus playback is minutes; a wedged emulator should not hold the
  # builder for the global default. Generous, then dead.
  meta.timeout = 3600;
} ''
  set -uo pipefail
  export HOME=$TMPDIR USER=builder
  export ANDROID_SDK_ROOT=${sdkRoot}
  export ANDROID_AVD_HOME=$TMPDIR/avd ANDROID_USER_HOME=$TMPDIR/android-home
  mkdir -p $ANDROID_AVD_HOME $ANDROID_USER_HOME /build/castaway-state $out

  cleanup() {
    pkill -P $$ 2>/dev/null || true
    pkill -f qemu-system 2>/dev/null || true
    pkill -f netsimd 2>/dev/null || true
  }
  trap cleanup EXIT

  echo "== creating the AVD"
  echo no | ${androidComposition.androidsdk}/bin/avdmanager create avd --force \
    -n bench -k "system-images;android-35;google_apis;x86_64"

  echo "== netsimd (hci port 6402, pcap on)"
  ${sdkRoot}/emulator/netsimd --hci-port 6402 --no-web-ui --no-cli-ui --pcap -l \
    > netsimd.log 2>&1 &

  for i in $(seq 60); do
    grep -q "Hci socket server is listening" netsimd.log && break
    sleep 1
  done
  grep -q "Hci socket server is listening" netsimd.log || {
    echo "netsimd never opened its HCI port:"; cat netsimd.log; exit 1
  }

  echo "== emulator (headless, KVM, netsim phy)"
  ${sdkRoot}/emulator/emulator -avd bench \
    -no-window -no-audio -no-boot-anim -no-snapshot -gpu swiftshader_indirect \
    -memory 3072 -cores 2 -packet-streamer-endpoint default \
    > emulator.log 2>&1 &

  echo "== castaway joins the phy"
  RUST_LOG=info,proto_bluetooth_audio=debug CASTAWAY_CONFIG=${config} \
    ${castaway}/bin/castaway > castaway.log 2>&1 &
  CASTAWAY_PID=$!

  echo "== reference waveform"
  ${python}/bin/python3 ${signal} make ref.wav

  echo "== driving the phone"
  ADB=${sdkRoot}/platform-tools/adb \
  CASTAWAY_LOG=$PWD/castaway.log REF_WAV=$PWD/ref.wav VLC_APK=${vlcApk} \
    ${python}/bin/python3 ${drive} || {
      echo "== drive failed; the tails that matter:"
      tail -50 emulator.log; tail -60 castaway.log; tail -30 netsimd.log
      cp castaway.log netsimd.log emulator.log $out/ 2>/dev/null || true
      exit 1
    }

  # Stopped before the recording is read: the WAV lengths are patched per batch, so
  # this is tidiness, but it also ends the session cleanly in the pcap.
  kill $CASTAWAY_PID 2>/dev/null || true; sleep 2

  echo "== the codec a real phone picked"
  grep "bluetooth: stream configured" castaway.log
  grep -q "stream configured.*AptXHd" castaway.log || {
    echo "FAIL: the phone did not negotiate aptX HD — read the log above; if the image"
    echo "changed its preference order this pin is doing its job."
    cp castaway.log $out/; exit 1
  }

  echo "== the signal, per channel"
  ${python}/bin/python3 ${signal} check ref.wav /build/castaway-state/mix.wav 0.9 0.6 || {
    echo "FAIL: the audio did not survive the path"
    cp castaway.log /build/castaway-state/mix.wav $out/ 2>/dev/null || true
    exit 1
  }

  echo "== keeping the artifacts"
  cp castaway.log netsimd.log $out/
  cp /build/castaway-state/mix.wav $out/
  cp -r $TMPDIR/android-$USER/netsimd/pcaps $out/pcaps 2>/dev/null || \
    cp -r /tmp/android-$USER/netsimd/pcaps $out/pcaps 2>/dev/null || \
    echo "warning: no pcaps found to keep"
  echo ok > $out/result
''
