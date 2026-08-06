# A2DP up to a started stream, driven by BlueZ, with no radio and no hardware.
#
# The kernel's `hci_vhci` plus BlueZ's `btvirt` emulator give a pair of *linked* virtual
# controllers — two `hciN` devices on `Bus: Virtual` that inquire, page and carry L2CAP
# between each other over an emulated air interface, and that need no firmware at all.
# BlueZ drives one as an ordinary source; our receiver owns the other. Everything on the
# far side is an implementation that has never seen our code, which is categorically
# better evidence than our source code talking to our sink code — all the in-process
# tests can offer (#186).
#
# What that peer now does, in order: browses our SDP records with `sdptool`, pairs over
# SSP, enumerates our stream endpoints and decodes every capability record with `avinfo`,
# refuses to be given a codec an endpoint does not offer, and configures, opens and starts
# a stream with `avtest`. Four of those had never happened here.
#
# Two properties make this *harsher* than real hardware, which is the point. A virtual
# controller reports an ACL MTU of 192 with a **single** buffer, against 1021x4 on a real
# AX200. So every SDP browse response fragments — a browse returns all three records at
# once and the largest is 335 bytes — and transmit flow control has no slack whatsoever.
#
# Three things the emulator gets wrong for us, each found the hard way and each worked
# around at its subtest rather than here: a controller reset leaves BlueZ holding a stale
# ACL handle it will reuse forever, SSP is *off* by default on the BlueZ-side controller
# (so pairing falls back to legacy PIN, which this receiver refuses by design), and
# `avtest` configures the first endpoint it finds rather than a matching one.
#
# **Still not covered, and it is the audio.** No media packet is sent and nothing is
# decoded, so the `services.pipewire`/`tester` block below remains configured and unused.
# That wants bluetoothd's own A2DP source with a PipeWire endpoint behind it — which now
# has a fair chance of working, because a *trusted* device makes bluetoothd connect
# profiles on its own (this test can already see it driving AVRCP), and a receiver that
# writes decoded PCM somewhere a test can cross-correlate it. See `docs/test-matrix.md`
# §4.3 and #186.
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

  # The same receiver, advertising only the mandatory codec.
  #
  # `avtest` is BlueZ's AVDTP tester rather than its A2DP source: it configures the
  # *first* endpoint it discovers with whichever codec it was built to send, and does not
  # look for a matching one. Against the full table that is an SBC configuration aimed at
  # our aptX HD endpoint, which we refuse with `UNSUPPORTED_CONFIGURATION` — correctly,
  # and the refusal is asserted below on the full table before this narrowing.
  #
  # `codecs` exists for exactly this, and says so: a source takes the first endpoint it
  # supports, so the only way to exercise a *particular* codec is to stop offering the
  # ones it would otherwise prefer.
  sbcConfig = pkgs.writeText "castaway-sbc.toml" ''
    friendly_name = "castaway vm"
    uuid = "0f8c2e10-castaway-0001-00000000vmbt"
    http_port = 8080

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
    transport = "socket:1"
    state_dir = "/var/lib/castaway"
    codecs = ["sbc"]
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

    def drop_stale_links():
        """Make BlueZ page afresh instead of reusing a handle to a controller that reset.

        The same trap the `l2ping` comment above records, arriving from the other side.
        `l2ping` left an ACL up; our receiver then claimed hci1 and reset it, so the link
        is gone at the far end while BlueZ's kernel still holds a handle for it. Every
        client after that — `sdptool`, `avinfo`, `avtest` — opens an L2CAP socket, the
        kernel reuses that handle rather than paging, and the request goes out over a
        connection nothing is listening on. Measured: `sdptool browse` sat there until its
        30 s timeout with the receiver reporting `RX bytes:0 acl:0`, i.e. not one byte
        ever reached it.
        """
        # `bluetoothctl` first, and only once there is something to tell: after pairing,
        # bluetoothd knows this address is an audio sink and reconnects it the moment the
        # link drops, so `hcitool dc` alone races its own reconnect and the link is never
        # observed down. That cost a cycle — a 30 s wait that never came true.
        machine.execute(f"bluetoothctl --timeout 5 disconnect {sink_addr} 2>&1")
        machine.execute(f"hcitool -i hci0 dc {sink_addr} 2>&1")
        machine.wait_until_succeeds(
            f"! hcitool -i hci0 con | grep -q '{sink_addr}'", timeout=60
        )

    with subtest("BlueZ's own SDP client reads the records we serve"):
        drop_stale_links()
        # `substrate-sdp/src/server.rs` had no inline tests and no third-party client had
        # ever parsed what it serves — the only such parse on record was a manual bench
        # run (#74, #186). `sdptool browse` is BlueZ's, and it is the harsher of the two
        # ways to ask: a browse is a `ServiceSearchAttribute` for the public browse group
        # that returns *every* record at once, which over this controller's 192-byte MTU
        # cannot fit in one response. So this also exercises the continuation path the
        # file's header claims and nothing had run — `chunk`'s offset token, and a client
        # that is not ours reassembling from it.
        # Traced and time-boxed rather than retried. `sdptool` blocks on its own SDP
        # connect, so a `wait_until_succeeds` around it waits out the *client's* timeout
        # several times over and then reports nothing about why — which is how the first
        # attempt at this subtest spent minutes saying only "waiting for success".
        machine.succeed(
            "systemd-run --unit=btmon-sdp --collect "
            "${bluezWithBtvirt}/bin/btmon -w /tmp/sdp.btsnoop"
        )
        status, records = machine.execute(
            f"timeout 30 sdptool -i hci0 browse {sink_addr} 2>&1"
        )
        machine.succeed("systemctl stop btmon-sdp")
        print(records)
        if status != 0:
            trace = machine.succeed("${bluezWithBtvirt}/bin/btmon -r /tmp/sdp.btsnoop")
            print(machine.succeed("journalctl -u castaway | tail -40"))
            raise Exception(
                f"sdptool browse exited {status}:\n{records}\n\n{trace[-8000:]}"
            )
        # The A2DP half. `Audio Sink` is the class a source searches for before it will
        # offer anything, and PSM 25 is where it then connects AVDTP.
        assert "Audio Sink" in records, records
        assert "PSM: 25" in records, records
        # Both AVRCP roles, in `sdptool`'s own spelling. The controller record is the one
        # that gets us metadata at all — a sink publishing only Target gets none — and a
        # browse that returned one of the three records would be caught here rather than
        # by a phone showing no track title.
        assert '"AV Remote Controller" (0x110f)' in records, records
        assert '"AV Remote Target" (0x110c)' in records, records
        assert "PSM: 23" in records, records
        # The generic A/V Remote Control class beside the role-specific one: a peer that
        # searches for 0x110E, which is what the profile says to search for, finds nothing
        # in a record listing only 0x110F.
        assert '"AV Remote" (0x110e)' in records, records
        # All three, and their handles, so a record silently dropped from the server's
        # list is a failure rather than a shorter browse nobody reads.
        for handle in ["0x10000", "0x10001", "0x10002"]:
            assert handle in records, records

    with subtest("BlueZ pairs with the receiver over Secure Simple Pairing"):
        # Everything past SDP is behind authentication: BlueZ opens its AVDTP sockets at
        # security MEDIUM, so the link is authenticated before a byte of AVDTP flows.
        #
        # And the emulator decides *which* pairing happens. `btdev.c`'s `use_ssp()` is
        # `!auth_enable && both simple_pairing_mode`, so if either controller has SSP off
        # the link falls back to legacy PIN — which this receiver refuses by design
        # (`host.rs`: a panel with no keypad cannot be asked for a number), and the
        # result was BlueZ retrying forever: 54 `link up` / `link down` cycles with
        # `No agent available for request type 0` beside each one. That is the emulator's
        # default, not ours; our side sets SSP in its bring-up sequence.
        print(machine.succeed("hciconfig hci0 sspmode"))
        machine.succeed("hciconfig hci0 sspmode 1")
        print(machine.succeed("hciconfig hci0 sspmode"))

        drop_stale_links()
        # `NoInputNoOutput` on both ends is the just-works case, which is what a panel and
        # a phone actually negotiate: no number to compare, no keypad on either side.
        status, out = machine.execute(
            f"timeout 40 bluetoothctl --agent NoInputNoOutput pair {sink_addr} 2>&1"
        )
        print(out)
        # Our side's own account of it, which is the one that matters: the link key
        # arrived and was stored, so the next connection skips pairing entirely.
        machine.wait_until_succeeds(
            "journalctl -u castaway | grep -q 'bluetooth: paired'", timeout=60
        )
        assert status == 0, f"bluetoothctl pair exited {status}:\n{out}"

        # Trust it, or every later service connection is refused with
        # `security block` and `Authentication attempt without agent` in the log: the
        # agent that paired exits with the command, and an *untrusted* paired device
        # needs one again to authorise each profile. A phone marks a speaker trusted
        # when the person taps "pair"; this is that tap.
        machine.succeed(f"bluetoothctl --timeout 5 trust {sink_addr}")

    with subtest("BlueZ's own AVDTP client discovers the endpoint and reads its codecs"):
        # `avinfo` is the other end of `avdtp.rs`: DISCOVER, then GET_CAPABILITIES on what
        # comes back, decoded by BlueZ's parser rather than by ours. A capability response
        # with a wrong length byte, a codec element in the wrong order, or a service
        # category we invented would be read here by something with no reason to be
        # forgiving — and `adapter_end_to_end.rs`, which is our state machine against our
        # own scripted bytes, would still be green.
        machine.succeed(
            "systemd-run --unit=btmon-avdtp --collect "
            "${bluezWithBtvirt}/bin/btmon -w /tmp/avdtp.btsnoop"
        )
        status, info = machine.execute(f"timeout 30 avinfo -i hci0 {sink_addr} 2>&1")
        machine.succeed("systemctl stop btmon-avdtp")
        print(info)
        if status != 0 or "Audio Sink" not in info:
            trace = machine.succeed("${bluezWithBtvirt}/bin/btmon -r /tmp/avdtp.btsnoop")
            print(machine.succeed("journalctl -u castaway | tail -30"))
            raise Exception(f"avinfo exited {status}:\n{info}\n\n{trace[-9000:]}")
        # SBC is mandatory for every A2DP sink and is what a source falls back to; if the
        # only thing it could read were our optional codecs, a phone would have nothing to
        # negotiate with.
        assert "SBC" in info, info

    with subtest("a configuration naming a codec the endpoint does not offer is refused"):
        # `avtest` configures the *first* endpoint it discovers, whatever codec it is
        # sending — so against the full table it aims an SBC configuration at our aptX HD
        # endpoint. Refusing that is the correct answer and worth pinning: a sink that
        # accepted it would go on to decode aptX HD frames as SBC, which is noise at full
        # scale on a PA rather than an error anybody can act on.
        machine.succeed(
            f"avtest --device hci0 --send start --preconf --wait 1 {sink_addr} "
            "> /tmp/avtest-mismatch.log 2>&1 || true"
        )
        mismatch = machine.succeed("cat /tmp/avtest-mismatch.log")
        print(mismatch)
        # AVDTP message type 3 is a reject, and 0x29 is UNSUPPORTED_CONFIGURATION. The
        # 0x31 (BAD_STATE) that follows on OPEN and START is the other half of the same
        # answer: nothing was configured, so there is nothing to open or start.
        assert "MT 3 SI 3" in mismatch, mismatch
        assert "29" in mismatch, mismatch

    with subtest("an implementation that is not ours configures and starts a stream"):
        # The step every previous version of this file stopped short of. Restarted with a
        # single-codec table so `avtest`'s naive endpoint choice lands on an endpoint that
        # accepts what it is sending — see `sbcConfig` above for why that is the shipped
        # mechanism rather than a fudge.
        machine.succeed("systemctl stop castaway")
        machine.wait_until_succeeds("hciconfig hci1 down", timeout=60)
        machine.succeed(
            "systemd-run --unit=castaway-sbc --setenv=RUST_LOG=info "
            "--setenv=CASTAWAY_CONFIG=${sbcConfig} ${castaway}/bin/castaway"
        )
        machine.wait_until_succeeds(
            "journalctl -u castaway-sbc | grep -q 'bluetooth: discoverable'", timeout=60
        )
        drop_stale_links()

        machine.succeed(
            "systemd-run --unit=btmon2 --collect "
            "${bluezWithBtvirt}/bin/btmon -w /tmp/avdtp.btsnoop"
        )
        # `systemd-run` returns when the unit *starts*, not when btmon has its socket
        # open, and the difference is enough to lose the first exchange — a trace that
        # began at `Start` with no `Discover` before it cost a cycle here.
        machine.wait_until_succeeds("test -s /tmp/avdtp.btsnoop", timeout=30)
        machine.succeed(
            f"avtest --device hci0 --send start --preconf --wait 3 {sink_addr} "
            "> /tmp/avtest.log 2>&1 || true"
        )
        avtest_log = machine.succeed("cat /tmp/avtest.log")
        print(avtest_log)
        machine.succeed("systemctl stop btmon2")

        # What the *other end* saw, in its own words. `MT 2` is an AVDTP response-accept
        # and `MT 3` a reject, so this is BlueZ reporting that our responder agreed to
        # each step: SI 3 SET_CONFIGURATION, SI 6 OPEN, SI 7 START.
        #
        # Read from `avtest`'s log rather than from the trace because btmon can only
        # decode a channel whose connection it observed, and it cannot be started early
        # enough here without also capturing the previous subtest's teardown — a trace
        # that shows the bytes and calls them raw ACL proves nothing this can rely on.
        for step in ["MT 2 SI 3", "MT 2 SI 6", "MT 2 SI 7"]:
            assert step in avtest_log, f"AVDTP {step} was not accepted:\n{avtest_log}"

        # Our side of the same event. `stream configured` carries the codec and the format
        # it resolved to, which is the thing a wrong capability response gets wrong
        # silently — a sink that agreed to 44.1 kHz mono when the source asked for 48 kHz
        # stereo plays, and plays wrong.
        machine.wait_until_succeeds(
            "journalctl -u castaway-sbc | grep -q 'bluetooth: stream configured'", timeout=60
        )
        print(machine.succeed(
            "journalctl -u castaway-sbc | grep -E "
            "'bluetooth: (link up|stream configured|sbc bitpool)'"
        ))

    machine.succeed(
        "journalctl -u castaway -u castaway-sbc > /tmp/castaway.log"
    )
    machine.copy_from_machine("/tmp/castaway.log", "")
    machine.copy_from_machine("/tmp/btmon.btsnoop", "")
  '';
}
