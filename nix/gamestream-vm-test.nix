# Tier-2 integration test for the GameStream client (D37), against a **real Sunshine**.
#
# Every other protocol here is tested by scripting the *sender* against our receiver. This
# one is the other way round, and that inversion is the whole reason this test exists:
# our pairing implementation is a reading of Sunshine's source, and the unit tests check
# it against Sunshine's own checked-in vectors — which proves we agree with the vectors,
# not that we agree with the program. A scripted host cannot fail in the one way that
# matters, because we would be scripting it from the same reading. So this boots the
# actual `sunshine` binary from nixpkgs and pairs with it.
#
# Two nodes, per the convention in vm-test.nix: discovery is what breaks in the field and
# loopback hides all of it.
#
# What is deliberately *not* asserted: a launched session. Sunshine probes for an encoder
# at `/launch` and answers 503 when it cannot find a display, which in a headless VM says
# nothing about the protocol. `gs-probe --launch` is run anyway and prints the host's own
# refusal, so the request shape is exercised and the answer is on the record; the test
# passes on either outcome and the reason is in the log.
{ pkgs, self }:

let
  # The PIN the client will send and the host will read from stdin. `sunshine -0` puts
  # PIN entry on stdin instead of the web UI, which is what makes this hands-free: the
  # real flow has a person typing into a browser.
  pin = "4213";

  probe = pkgs.writeShellScriptBin "gs-probe-run" ''
    set -euo pipefail
    exec ${self.packages.${pkgs.system}.gs-probe}/bin/gs-probe "$@"
  '';
in
pkgs.testers.runNixOSTest {
  name = "castaway-gamestream";

  nodes = {
    # The GameStream host. Plain nixpkgs Sunshine — the point is that nothing about it
    # is ours.
    host = { config, ... }: {
      environment.systemPackages = [ pkgs.sunshine ];
      # Sunshine binds a spread of ports and the client dials several of them; the
      # firewall is off rather than enumerated because a missed port here would look
      # exactly like a protocol bug.
      networking.firewall.enable = false;
      # Its state file doubles as the web-UI credentials store, so it needs a writable
      # home. `-0` keeps pairing off the web UI entirely, so no credentials are set.
      users.users.sunshine = {
        isNormalUser = true;
        home = "/var/lib/sunshine";
        createHome = true;
      };
      # Avahi so the client's mDNS browse has something to find. Sunshine publishes
      # `_nvstream._tcp` through the system responder.
      services.avahi = {
        enable = true;
        publish.enable = true;
        publish.userServices = true;
      };
      virtualisation.memorySize = 2048;
    };

    # The client. Only our probe binary — this node is castaway's half.
    client = { ... }: {
      environment.systemPackages = [ probe pkgs.curl ];
      networking.firewall.enable = false;
      services.avahi.enable = true;
      virtualisation.memorySize = 2048;
    };
  };

  testScript = { nodes, ... }: ''
    import re

    start_all()
    host.wait_for_unit("multi-user.target")
    client.wait_for_unit("multi-user.target")

    host_ip = "${nodes.host.networking.primaryIPAddress}"

    with subtest("sunshine is up and answering NVHTTP"):
        # `-0` = PIN from stdin. The FIFO is what lets the test type the PIN at the
        # moment the client asks for it — Sunshine parks the phase-1 response until
        # then, which is the behaviour the client's "no timeout on phase 1" exists for.
        host.succeed("mkfifo /tmp/pin-fifo")
        host.succeed(
            "su sunshine -c 'cd /var/lib/sunshine && "
            "(sleep infinity > /tmp/pin-fifo &) && "
            "sunshine -0 < /tmp/pin-fifo > /tmp/sunshine.log 2>&1 &' "
        )
        host.wait_for_open_port(47989)
        # Its own answer, not ours: proves the host is healthy before we blame our client.
        host.succeed("curl -sf http://localhost:47989/serverinfo?uniqueid=test | grep -q appversion")

    with subtest("the client discovers, pairs, and is trusted over mutual TLS"):
        # The probe blocks in phase 1 until the PIN is typed, so it runs in the
        # background and the PIN goes in behind it. This ordering *is* the test of the
        # unbounded phase-1 request: a client that timed out would fail here.
        client.succeed(
            f"gs-probe-run {host_ip} --pin ${pin} --state-dir /var/lib/gs "
            "--launch Desktop > /tmp/probe.log 2>&1 &"
        )
        # Wait until Sunshine is actually asking, then answer.
        host.wait_until_succeeds("grep -q -i 'pin' /tmp/sunshine.log", timeout=60)
        host.succeed("echo '${pin}' > /tmp/pin-fifo")

        client.wait_until_succeeds("grep -q 'gs-probe completed' /tmp/probe.log", timeout=120)
        probe_log = client.succeed("cat /tmp/probe.log")
        print(probe_log)

        # Each of these is a distinct claim, and each fails differently:
        #  - the handshake completed and the certificate persisted
        #  - the *host* agrees, over TLS, that it trusts that certificate
        #  - an HTTPS-only endpoint answered, which nothing unpaired can reach
        assert "paired, and the host certificate is persisted" in probe_log, probe_log
        assert "mutual TLS works and the host considers us paired" in probe_log, probe_log
        assert re.search(r"applist: [1-9][0-9]* app", probe_log), probe_log
        # Sunshine identifies itself with a negative fourth version component, and every
        # GFE-only workaround in the client hangs off getting that right.
        assert "sunshine=true" in probe_log, probe_log

    with subtest("the launch request reaches the host and is answered"):
        probe_log = client.succeed("cat /tmp/probe.log")
        # Either outcome is a pass: a headless VM has no encoder, so 503 is the honest
        # answer and it still proves the request was well-formed enough to be judged.
        # What would fail is a 400 ("missing a required launch parameter"), which is the
        # failure mode a wrong query encoding produces.
        assert ("launched: sessionUrl0=" in probe_log) or (
            "launch refused by the host" in probe_log
        ), probe_log
        assert "(400)" not in probe_log, "the host rejected our /launch parameters:\n" + probe_log

    with subtest("the pairing survives a restart, without the PIN"):
        # The credential is the certificate on disk. A client that regenerated its
        # identity per run would be unpaired on every boot, and the only symptom would be
        # a 401 that says nothing.
        client.succeed(
            f"gs-probe-run {host_ip} --state-dir /var/lib/gs > /tmp/probe2.log 2>&1"
        )
        probe_log2 = client.succeed("cat /tmp/probe2.log")
        print(probe_log2)
        assert "restoring the pairing" in probe_log2, probe_log2
        assert "mutual TLS works and the host considers us paired" in probe_log2, probe_log2
        assert "gs-probe completed" in probe_log2, probe_log2
  '';
}
