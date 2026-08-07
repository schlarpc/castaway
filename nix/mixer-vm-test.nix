# The mixer against a sound card whose clock is not ours (#204).
#
# Every pacing guarantee the mixer makes — a source drains in real time, a live sender is
# never parked, a ramp is deferred rather than perforated — is asserted in `mixer::tests`
# against a `Recorder` whose `frames_played` is *derived from the wall clock*. The device
# counter is therefore correct by construction, and the device counter is the whole risk:
# the three live regressions in exactly that blind spot (#174, #175, #177) were every one
# of them found by a person listening to the panel, not by those tests.
#
# `crates/pipeline/tests/mixer_real_device.rs` exists to remove that assumption and was
# `#[ignore]`d for hardware, so it ran nowhere. It does not need hardware. `snd-dummy` is
# a kernel sound card driven by an hrtimer, PipeWire runs its graph off that timer's
# wakeups, and `PipeWireAudioOut` counts frames in the process callback — so `frames_played`
# here comes from the kernel's clock and the assertions are measured against the wall.
# Nothing in the loop under test supplies both terms.
#
# The `#[ignore]` stays and this test passes `--include-ignored`. Removing it would mean
# `checks.test` — which builds in a sandbox with no session bus, no PipeWire and no card —
# running two tests that can only fail there.
#
# ## What a dummy card does and does not buy
#
# It buys the independent clock, which is the property under test. It does not buy an
# analogue output, so nothing here proves the samples were audible; that is what the
# `MixTap` in the second test is for, and it is on our side of the device either way.
#
# It also does not buy hardware's *misbehaviour* — a card whose clock drifts against the
# system's, an HDMI sink that vanishes when the panel sleeps (#55). snd-dummy's timer is
# as well-behaved as a timer gets. So this is the floor, not the ceiling: it catches a
# mixer that cannot keep up with an honest device, which is every defect on #174/#175/#177,
# and it does not retire the panel-side reading those issues close on.
{ pkgs, mixerTest }:

pkgs.testers.runNixOSTest {
  name = "castaway-mixer-real-device";

  nodes.machine = { ... }: {
    # The whole point: a card with a clock of its own. A VM has no sound hardware, and
    # `snd-dummy` is the kernel's own stand-in for one — a real ALSA device whose period
    # wakeups come from an hrtimer rather than from anything in this process.
    boot.kernelModules = [ "snd-dummy" ];

    environment.systemPackages = [
      mixerTest
      # `pactl`, to ask the graph whether it has a sink before the test asks it for one.
      pkgs.pulseaudio
    ];

    security.rtkit.enable = true;
    services.pipewire = {
      enable = true;
      alsa.enable = true;
      pulse.enable = true;
      wireplumber.enable = true;
    };

    users.users.tester = {
      isNormalUser = true;
      extraGroups = [ "audio" ];
    };

    # Four, and the count is load-bearing rather than generous. The assertions are rate
    # bands: two mixer threads, a producer on its own deadline clock, and the PipeWire
    # daemon all have to run when they mean to, and a measurement starved of a core reads
    # as a mixer that missed real time. `.config/nextest.toml` records what that costs
    # when it is got wrong — three tests that pass alone and fail in company, and the
    # temptation to widen the band until it stops meaning anything (#156).
    virtualisation.cores = 4;
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("multi-user.target")

    # The card first, because everything below is downstream of it and its absence would
    # otherwise surface as PipeWire simply having no sink to pick as the default.
    print(machine.succeed("cat /proc/asound/cards"))

    # PipeWire is a *user* service and nothing is logged in. Lingering rather than a login
    # shell: `su` gives a session whose runtime dir is torn down when it exits, which
    # would take the audio graph with it between two `machine.succeed` calls.
    machine.succeed("loginctl enable-linger tester")
    uid = machine.succeed("id -u tester").strip()

    def as_tester(cmd, **kw):
        return machine.succeed(
            f"su tester -c 'export XDG_RUNTIME_DIR=/run/user/{uid}; {cmd}'", **kw
        )

    # Started head-on rather than waited for: PipeWire is socket-activated, so
    # `is-active pipewire.service` reports `inactive` until something connects, which is
    # not the same as "not running" and turns a ready graph into a 90 s timeout.
    machine.wait_until_succeeds(
        "systemctl --user --machine=tester@.host start "
        "pipewire.service wireplumber.service",
        timeout=120,
    )
    # A client reaching the graph over the socket the test will use, rather than a unit's
    # own opinion of itself.
    machine.wait_until_succeeds(
        f"su tester -c 'export XDG_RUNTIME_DIR=/run/user/{uid}; pactl info'", timeout=60
    )
    # And then the thing the test actually needs: `OutputSelection::SystemDefault` asks
    # for the default sink, and wireplumber registers the ALSA node seconds after PipeWire
    # will answer `pactl info`. Opening in that window is a device that is not there yet.
    machine.wait_until_succeeds(
        f"su tester -c 'export XDG_RUNTIME_DIR=/run/user/{uid}; "
        f"pactl list short sinks | grep -q alsa_output'",
        timeout=60,
    )
    print(as_tester("pactl list short sinks"))

    # `--test-threads=1` is not tidiness: both tests open the one device and measure
    # against it in real time, so running them together would have each one's assertion
    # reading the other's traffic.
    print(
        as_tester(
            "mixer-real-device --include-ignored --test-threads=1 --nocapture",
            timeout=300,
        )
    )
  '';
}
