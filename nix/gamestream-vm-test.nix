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
# Three things the real program taught us that reading its source did not, all of which
# this file works around and none of which is obvious from the outside:
#
#   1. `sunshine -0` (PIN on stdin) looks like the hands-free path and is a trap — it
#      closes stdin once up, so a write into a FIFO blocks forever with nothing logged.
#      The PIN goes over its web API instead, which needs `--creds` and a JSON content
#      type.
#   2. Its main loop *is* the system-tray loop. Headless, it answers NVHTTP for about a
#      second and then exits, which presents as every later step hanging. Hence
#      `system_tray = disabled`.
#   3. `/api/pin` acts on whichever pairing session is currently in flight, so submitting
#      before the client has asked is a silent no-op. Hence the retry loop.
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

  # Credentials for Sunshine's own web API, which is how the PIN is delivered.
  webUser = "castaway";
  webPass = "castaway-test";

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
    host = { ... }: {
      environment.systemPackages = [ pkgs.sunshine ];
      # Sunshine binds a spread of ports and the client dials several of them; the
      # firewall is off rather than enumerated because a missed port here would look
      # exactly like a protocol bug.
      networking.firewall.enable = false;
      users.users.sunshine = {
        isNormalUser = true;
        home = "/var/lib/sunshine";
        createHome = true;
      };

      # Sunshine's main loop *is* the system-tray loop when the tray is built in, so a
      # headless host logs "System tray is not initialized" and exits about a second
      # after it starts answering — which presents as a FIFO write that blocks forever
      # and tells you nothing. `system_tray = disabled` keeps it alive.
      environment.etc."sunshine/sunshine.conf".text = ''
        system_tray = disabled
      '';

      # The PIN is delivered over Sunshine's own web API, which is the same path its
      # UI uses. `sunshine -0` (PIN on stdin) looks simpler and is a trap: Sunshine
      # closes stdin once it is up, so a write into the FIFO blocks forever and
      # presents as a hang with nothing in any log.
      #
      # The API needs credentials, and Sunshine writes them itself given `--creds`.
      systemd.services.sunshine = {
        description = "GameStream host under test";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        serviceConfig = {
          ExecStartPre =
            "${pkgs.sunshine}/bin/sunshine /etc/sunshine/sunshine.conf --creds ${webUser} ${webPass}";
          ExecStart = "${pkgs.sunshine}/bin/sunshine /etc/sunshine/sunshine.conf";
          User = "sunshine";
          WorkingDirectory = "/var/lib/sunshine";
          Restart = "no";
        };
      };

      # Avahi so the client's mDNS browse has something to find.
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
        # Bounded waits, so a Sunshine that failed to start surfaces in a minute
        # instead of after the driver's 15-minute default — the difference between
        # reading the cause and guessing at it.
        host.wait_for_unit("sunshine.service", timeout=90)
        host.wait_for_open_port(47989, timeout=90)
        # The host's own answer, not ours: proves it is healthy before we blame our
        # client for anything that follows.
        host.succeed(
            "curl -sf 'http://localhost:47989/serverinfo?uniqueid=test' | grep -q appversion"
        )

    with subtest("the client discovers, pairs, and is trusted over mutual TLS"):
        # The probe goes first and blocks in phase 1: Sunshine parks that response
        # until a PIN arrives. That ordering is the point — a client that put a timeout
        # on phase 1 would fail here, and a real pairing always waits on a human.
        client.succeed(
            f"gs-probe-run {host_ip} --pin ${pin} --state-dir /var/lib/gs "
            "--launch Desktop > /tmp/probe.log 2>&1 &"
        )
        # The PIN is submitted on a retry loop rather than once, because Sunshine's
        # pin handler acts on whatever pairing session is *currently* in flight — with
        # none, it is a silent no-op. There is no event to wait for that reliably means
        # "the request has arrived" (its log line for the parked request is not
        # distinctive), so this posts until the client says it worked.
        paired = False
        for _ in range(45):
            host.succeed(
                "curl -sk -u ${webUser}:${webPass} -X POST "
                "-H 'Content-Type: application/json' "
                "-d '{\"pin\":\"${pin}\",\"name\":\"castaway-test\"}' "
                "https://localhost:47990/api/pin > /tmp/pin-reply.json || true"
            )
            if "0" != client.succeed(
                "grep -c 'paired, and the host' /tmp/probe.log || true"
            ).strip():
                paired = True
                break
            client.sleep(2)
        if not paired:
            print(client.succeed("cat /tmp/probe.log || true"))
            print(host.succeed("cat /tmp/pin-reply.json || true"))
            print(host.succeed("journalctl -u sunshine --no-pager | tail -30 || true"))

        try:
            client.wait_until_succeeds(
                "grep -q 'gs-probe completed' /tmp/probe.log", timeout=120
            )
        except Exception:
            # Both halves of the conversation, so a failure here is readable without a
            # second run: what our client thought, and what the host thought.
            print(client.succeed("cat /tmp/probe.log || true"))
            print(host.succeed("journalctl -u sunshine --no-pager | tail -50 || true"))
            raise
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
