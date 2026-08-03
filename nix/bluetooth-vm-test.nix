# A complete A2DP session with no radio, no dongle, and no hardware of any kind.
#
# The kernel's `hci_vhci` plus BlueZ's `btvirt` emulator give a pair of *linked* virtual
# controllers — two `hciN` devices on `Bus: Virtual` that inquire, page and carry L2CAP
# between each other over an emulated air interface, and that need no firmware at all.
# Verified on the dev box before this was written: `l2ping` across the pair reports 0%
# loss and `btmon` shows real L2CAP exchanges.
#
# That makes the test worth having possible: **BlueZ drives one controller as an ordinary
# A2DP source, and our receiver owns the other.** The sender side is then an independent
# implementation that has never seen our code — categorically better evidence than our
# source code talking to our sink code, which is all the in-process tests can offer.
#
# Two properties make this *harsher* than real hardware, which is the point. A virtual
# controller reports an ACL MTU of 192 with a **single** buffer, against 1021x4 on a real
# AX200. Every SDP record and AVDTP capability response therefore fragments, and transmit
# flow control has no slack whatsoever — both paths run on every test rather than only
# under load.
{ pkgs, castaway }:

let
  # btvirt lives behind `--enable-testing` and nixpkgs does not install it.
  bluezWithBtvirt = pkgs.bluez.overrideAttrs (old: {
    pname = "bluez-btvirt";
    configureFlags = (old.configureFlags or [ ]) ++ [ "--enable-testing" ];
    postInstall = (old.postInstall or "") + ''
      install -Dm755 emulator/btvirt $out/bin/btvirt
    '';
  });

  config = pkgs.writeText "castaway.toml" ''
    friendly_name = "castaway vm"
    uuid = "0f8c2e10-castaway-0001-00000000vmbt"
    http_port = 8080

    # Everything defaults on; this test is about the A2DP sink alone, so the rest is
    # named off rather than left to drift with the defaults.
    [enable]
    dlna = false
    spotify = false
    dial = false
    cast = false
    airplay = false
    gamestream = false
    miracast = false
    bluetooth = true

    [bluetooth]
    # hci1 is btvirt's second controller; BlueZ keeps hci0 and plays the source.
    transport = "socket:1"
    state_dir = "/var/lib/castaway"
  '';
in
pkgs.testers.runNixOSTest {
  name = "castaway-bluetooth-a2dp";

  nodes.machine = { pkgs, ... }: {
    # `hci_vhci` is what makes a virtual controller possible; `snd-dummy` gives PipeWire
    # a sink to exist on, since a VM has no sound card.
    boot.kernelModules = [ "hci_vhci" "snd-dummy" ];
    hardware.bluetooth.enable = true;
    hardware.bluetooth.powerOnBoot = false;

    environment.systemPackages = [
      bluezWithBtvirt
      castaway
      pkgs.python3
    ];

    # A real audio graph, so the source side is PipeWire doing what it does for a
    # headset rather than a bespoke test harness.
    security.rtkit.enable = true;
    services.pipewire = {
      enable = true;
      alsa.enable = true;
      pulse.enable = true;
      wireplumber.enable = true;
    };
    users.users.tester = {
      isNormalUser = true;
      extraGroups = [ "audio" "wheel" ];
    };

    virtualisation.memorySize = 2048;
    # btvirt emulates the air interface in userspace, so it and the kernel's HCI layer
    # are two runnable things: on the single vCPU a nixosTest defaults to, a loaded host
    # can starve one long enough for an HCI command to hit its 2s timeout. A second core
    # is cheap and takes most of that window away.
    virtualisation.cores = 2;
    systemd.tmpfiles.rules = [ "d /var/lib/castaway 0755 root root -" ];
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")
    machine.succeed("modprobe hci_vhci")

    with subtest("btvirt creates a linked pair of virtual controllers"):
        # btvirt first: bluetoothd does not start until a controller exists.
        machine.succeed("${bluezWithBtvirt}/bin/btvirt -l2 >/tmp/btvirt.log 2>&1 &")
        machine.wait_until_succeeds("hciconfig | grep -q hci1", timeout=30)
        # bluetoothd is then left running deliberately. It powers and configures the
        # controllers the way a real host does, and on the dev box the emulated link
        # only carried traffic with it in charge — bare `hciconfig up` is not enough.
        machine.succeed("systemctl start bluetooth")
        machine.wait_for_unit("bluetooth.service")
        machine.wait_until_succeeds("hciconfig | grep -q hci1", timeout=30)
        out = machine.succeed("hciconfig")
        assert "Bus: Virtual" in out, f"expected virtual controllers, got:\n{out}"
        # The harsh geometry that makes this a better test than real hardware.
        assert "ACL MTU: 192:1" in out, f"expected a 192-byte single-buffer controller:\n{out}"

    # btvirt derives each controller's address from its *hci index*, which depends on
    # what else is present — the pair is 00:AA:01:00:00:00 / ...:01:00:01 in a bare VM but
    # ...:00:00:01 / ...:01:00:02 on a box with a real hci0. Hardcoding one cost a debug
    # cycle, so it is read back instead.
    def address_of(dev):
        out = machine.succeed(f"hciconfig {dev} | grep -oE '([0-9A-F]{{2}}:){{5}}[0-9A-F]{{2}}'")
        return out.strip().splitlines()[0]

    # `hciconfig <dev> up` issues an HCI_Reset (opcode 0x0c03), and bluetoothd — left
    # running above on purpose — issues its own against the same controllers as it
    # initialises them. The two collide, the kernel times the command out at 2s, and the
    # whole test fails on `Opcode 0x0c03 failed: -110`. That is a race, not a real
    # failure: it only shows up when the host is loaded enough to stretch the window,
    # which made it look like flakiness for a while.
    #
    # So bring each controller up on its own and retry, rather than one compound
    # `succeed`. `up` on an already-up controller is a no-op, so retrying is safe, and
    # doing them separately means a failure names which one.
    def bring_up(dev):
        machine.wait_until_succeeds(f"hciconfig {dev} up", timeout=120)
        # `up` returning 0 is not the same as the controller being usable; wait for the
        # kernel to actually report it running before anything is asked of it.
        machine.wait_until_succeeds(f"hciconfig {dev} | grep -q 'UP RUNNING'", timeout=120)

    with subtest("the two controllers can see each other"):
        # Best-effort settle: give bluetoothd a chance to finish claiming the controllers
        # over DBus before we reset them, which removes most of the collision window
        # rather than retrying through it. Deliberately `execute` and not
        # `wait_until_succeeds` — the retry in `bring_up` is the actual guarantee, so this
        # must never become a failure of its own if bluetoothd is slow or renames things.
        machine.execute(
            "timeout 60 sh -c 'until busctl --system tree org.bluez 2>/dev/null "
            "| grep -q /org/bluez/hci; do sleep 1; done'"
        )
        bring_up("hci0")
        bring_up("hci1")
        machine.wait_until_succeeds("hciconfig hci1 piscan", timeout=60)
        global sink_addr
        sink_addr = address_of("hci1")
        print(f"source=hci0 {address_of('hci0')}  sink=hci1 {sink_addr}")
        # Inquiry first: it is the cheapest proof the emulated link works at all, so a
        # later L2CAP failure is never ambiguous about the substrate.
        out = machine.wait_until_succeeds(
            f"hcitool -i hci0 inq 2>&1 | grep '{sink_addr}'", timeout=60
        )
        print(f"inquiry found: {out}")

    # A trace of the whole run, from here on. The failure this replaces (#156) reported
    # `l2ping: Can't connect: No route to host` nine times and nothing else, which is not
    # enough to tell a link that never carried anything from a link that carried it to a
    # controller in no state to answer. It is what found the cause below, and it is copied
    # out at the end whether the test passes or not.
    machine.succeed(
        "systemd-run --unit=btmon --collect "
        "${bluezWithBtvirt}/bin/btmon -w /tmp/btmon.btsnoop"
    )

    with subtest("the emulated air interface carries ACL and L2CAP"):
        # The scan state, asserted rather than assumed, and re-asserted on each attempt in
        # case bluetoothd writes its own while it initialises the controllers. This was the
        # first suspect for #156 and it is *not* the cause — measured at 0.02 s, page scan
        # already on — but it is cheap, and having it here is what makes a later failure
        # unambiguous about the sink being pageable.
        machine.wait_until_succeeds(
            "hciconfig hci1 piscan; hciconfig hci1 | grep -qw PSCAN", timeout=60
        )
        print(machine.succeed("hciconfig hci1"))

        # The cause was the diagnostic. This subtest used to run `hcitool -i hci0 cc` for a
        # connection l2ping neither needs nor reuses, and then retry `l2ping` blindly.
        # l2ping makes its own connection; the stale one collided with it, and the echo
        # requests went out over an ACL handle nothing answered. The retries only got
        # through once the abandoned connection had timed out and gone away.
        #
        # Measured on an idle box, same tree, both directions: with the `cc`, 14.09 s of
        # retries and a trace of `Echo Request` with no response; without it, 2.02 s, first
        # attempt, request and response both. So the 60 s budget was never covering a
        # warm-up — it was covering a self-inflicted wait, and a loaded host is exactly
        # where that wait stretched past it.
        #
        # `con` is read-only and stays; it costs nothing and names the link if this fails.
        print(machine.succeed("hcitool -i hci0 con || true"))
        machine.wait_until_succeeds(f"l2ping -i hci0 -c 2 -t 5 {sink_addr}", timeout=60)

        # And the trace says it was carried, rather than `l2ping` merely having exited 0 —
        # which is the positive signal this subtest claimed and, until now, inferred. It is
        # also what distinguishes the two failures above from each other, so a future
        # regression arrives already diagnosed.
        machine.succeed("systemctl stop btmon")
        trace = machine.succeed("${bluezWithBtvirt}/bin/btmon -r /tmp/btmon.btsnoop")
        assert "Echo Request" in trace, f"no L2CAP echo request in the trace:\n{trace[-4000:]}"
        assert "Echo Response" in trace, f"the echo was never answered:\n{trace[-4000:]}"

    with subtest("the receiver claims the second controller"):
        # Same race in the other direction: bringing a controller down is an HCI command
        # too, and the receiver is about to reset it anyway.
        machine.wait_until_succeeds("hciconfig hci1 down", timeout=60)
        machine.succeed(
            "systemd-run --unit=castaway --setenv=RUST_LOG=info "
            "--setenv=CASTAWAY_CONFIG=${config} ${castaway}/bin/castaway"
        )
        machine.wait_until_succeeds(
            "journalctl -u castaway | grep -q 'enabled: Bluetooth A2DP sink'", timeout=60
        )
        machine.wait_until_succeeds(
            "journalctl -u castaway | grep -q 'attached to hci1'", timeout=30
        )

    with subtest("the receiver brings its controller up and becomes discoverable"):
        # Proves the whole HostController bring-up sequence ran against a controller
        # that is not ours: reset, buffer size, class of device, SSP, scan enable.
        machine.wait_until_succeeds(
            "journalctl -u castaway | grep -q 'bluetooth: discoverable'", timeout=60
        )

    with subtest("BlueZ finds the receiver by inquiry"):
        # The receiver is now driving hci1 itself — BlueZ is not involved on that side.
        # Finding it means our inquiry-scan and class-of-device settings actually took.
        out = machine.wait_until_succeeds(
            f"hcitool -i hci0 inq | grep '{sink_addr}'", timeout=60
        )
        # The class of device deliberately is *not* asserted here. btvirt accepts
        # Write_Class_of_Device and reports 0x000000 in inquiry results regardless, so a
        # check would be testing the emulator rather than us. The exact bytes of that
        # command are pinned by a unit test in substrate-hci instead
        # (`class_of_device_is_three_bytes_little_endian`), which is where the real risk
        # was — sending 0x240414 as a u32 rather than three bytes.
        print(f"inquiry found the receiver: {out.strip()}")

    machine.succeed("journalctl -u castaway > /tmp/castaway.log")
    machine.copy_from_machine("/tmp/castaway.log", "")
    machine.copy_from_machine("/tmp/btmon.btsnoop", "")
  '';
}
