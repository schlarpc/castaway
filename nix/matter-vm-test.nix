# Tier-2 for Matter Casting (ground rule 6, issue #171): commissioning end to end,
# between two hosts, with no phone and no person.
#
# `crates/proto-matter/tests/udc_over_the_wire.rs` proves the UDC half against a socket
# and stops where the interesting part starts. Everything past "the user typed the
# passcode" — browsing `_matterc._udp`, PASE, ArmFailSafe → AddNOC, CASE,
# CommissioningComplete, and then the client opening CASE *back* to invoke a cluster —
# needs a peer that runs the Matter core. `examples/matter-peer` is that peer.
#
# Two nodes, for the usual reason (vm-test.nix) and one specific to this protocol.
# `rs-matter`'s own commissioning test skips mDNS *entirely* — its comment says device and
# controller share the host's `:5353`, whose multicast loopback is unreliable, so
# discovery is "covered by unit tests instead". Our `await_commissionable` browse
# therefore has no integration coverage anywhere, ours or upstream's, and a second host is
# the only place it gets any. It is also the step that fails first in the field: the
# instance label arrives in a UDC datagram and is matched against a label in a DNS record,
# and a mismatch presents as the panel waiting out its full sixty seconds on a node that
# is sitting right there.
#
# What is same-library-on-both-sides, said plainly: the Matter *core* (D54). TLV, MRP,
# PASE, CASE, the interaction model, the certificate format — `rs-matter` on the panel and
# `rs-matter` on the peer. That is agreement with ourselves and proves nothing about the
# core. Everything this project owns is on the path regardless, and all of it is what this
# test is for: the UDC exchange, the mDNS browse, our CA, our NOC generator wiring, the
# fabric we install, the ACLs we seed, and the endpoint tree a `LaunchURL` lands on.
#
# The passcode moves through a file because there is no wire path for it — the panel shows
# a number and a person reads it, and that gap *is* the security property. So the harness
# plays the person: it reads the number out of the panel's journal and writes it onto the
# phone, which is the closest a hands-free test can get to the real flow without inventing
# a channel the protocol does not have.
{ pkgs, self }:

let
  friendlyName = "castaway-matter";

  # The URL the phone casts. `example.invalid` on purpose: nothing fetches it — Matter
  # carries no media, so a LaunchURL is a sentence and the panel's whole job is to turn it
  # into a `SessionEvent::Play`. The null pipeline is what the assertions read.
  castUrl = "https://example.invalid/matter-vm.mp4";
  castTitle = "matter-vm launch";

  peer = pkgs.writeShellScriptBin "matter-peer-run" ''
    set -euo pipefail
    exec ${self.packages.${pkgs.stdenv.hostPlatform.system}.matter-peer}/bin/matter-peer "$@"
  '';
in
pkgs.testers.runNixOSTest {
  name = "castaway-matter";

  nodes = {
    # The panel, configured through the module so the module is under test too.
    panel = { config, ... }: {
      imports = [ self.nixosModules.castaway ];

      services.castaway = {
        enable = true;
        # The portable build, as in vm-test.nix: this proves the protocol stack, and the
        # null pipeline's log lines are what the media assertions grep for.
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.castaway-portable;
        logLevel = "info,castaway=debug,proto_matter=debug,rs_matter=info";
        settings = {
          friendly_name = friendlyName;
          uuid = "0f8c2e10-0000-4000-8000-0000000c0572";
          # The VM's default route is QEMU's NAT, so auto-detect would advertise the
          # wrong address and the phone would dial into the NAT. Pin it to the test LAN.
          interface = config.networking.primaryIPAddress;
          # Only Matter. Every other adapter is proven elsewhere and each one is a
          # socket, a responder registration and a log stream this test would have to
          # read past.
          enable = {
            dlna = false;
            spotify = false;
            dial = false;
            cast = false;
            airplay = false;
            bluetooth = false;
            gamestream = false;
            miracast = false;
            matter = true;
          };
        };
      };

      virtualisation.memorySize = 2048;
    };

    # The phone. Only the peer binary — this node is the Casting Client's half.
    #
    # Deliberately no avahi: `matter-peer` advertises `_matterc._udp` on this project's
    # own responder (substrate-mdns), which binds :5353 itself. A second responder on the
    # node would fight it for the port, and the failure would look like a discovery bug in
    # the code under test rather than a misconfigured guest.
    phone = { ... }: {
      environment.systemPackages = [ peer ];
      networking.firewall.enable = false;
      virtualisation.memorySize = 2048;
    };
  };

  testScript = { nodes, ... }: ''
    import re

    panel_ip = "${nodes.panel.networking.primaryIPAddress}"
    phone_ip = "${nodes.phone.networking.primaryIPAddress}"

    start_all()

    with subtest("the panel comes up as a commissioner"):
        panel.wait_for_unit("castaway.service")
        phone.wait_for_unit("multi-user.target")
        # Both Matter sockets are UDP, and the driver's `wait_for_open_port` is TCP only
        # — it shells out to `nc -z`. `ss -uln` is the check that matches the protocol,
        # and it names the bind address too, which is the part that actually goes wrong:
        # a socket on 127.0.0.1 would satisfy "is it listening" and be unreachable from
        # the phone.
        #
        # 5550 is the UDC socket — what a Casting Client talks to first, and the only
        # part of Matter that is entirely ours because `rs-matter` does not implement it.
        panel.wait_until_succeeds("ss -uln | grep -q ':5550'", timeout=90)
        # 5540 is the operational node: PASE, CASE, and the interaction model.
        panel.wait_until_succeeds("ss -uln | grep -q ':5540'", timeout=90)
        panel.succeed(
            "journalctl -u castaway --no-pager | grep -q 'matter: casting receiver up'"
        )
        # The `_matterd._udp` record is the panel's entire first impression — a client
        # browses for it before it will send anything at all.
        panel.succeed(
            "journalctl -u castaway --no-pager | grep -q 'mDNS advertised.*_matterd._udp'"
        )

    with subtest("the phone declares, and the panel puts a passcode on the glass"):
        # Backgrounded and left running: the whole exchange is one process, because the
        # second declaration has to come from the port the first one named.
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--passcode-file /tmp/passcode.txt --url '${castUrl}' "
            f"--display-string '${castTitle}' > /tmp/peer.log 2>&1 &"
        )
        phone.wait_until_succeeds(
            "grep -q 'passcode dialog is up' /tmp/peer.log", timeout=60
        )
        # Eight digits, and the panel is the one that chose them — the other UDC flow has
        # the *client* display a number, and answering that way would be a different bug.
        phone.succeed("grep -q 'passcode dialog is up, 8 digits' /tmp/peer.log")
        panel.succeed(
            "journalctl -u castaway --no-pager | grep -q 'matter: passcode on screen'"
        )
        # It reached the overlay, not just the log. On a real panel this is the only
        # place the number exists, so a passcode that never renders is a passcode nobody
        # can type.
        panel.succeed(
            "journalctl -u castaway --no-pager | grep -q 'OSD.*wants to cast — enter'"
        )

    with subtest("a person reads the number off the screen and types it"):
        shown = panel.succeed(
            "journalctl -u castaway --no-pager | grep -oE '[0-9]{4}-[0-9]{4}' | tail -1"
        ).strip()
        assert re.fullmatch(r"[0-9]{4}-[0-9]{4}", shown), f"no passcode on screen: {shown!r}"
        phone.succeed(f"echo {shown} > /tmp/passcode.txt")

    with subtest("the panel finds the phone on mDNS and commissions it"):
        try:
            phone.wait_until_succeeds(
                "grep -q 'commissioned onto the panel' /tmp/peer.log", timeout=180
            )
        except Exception:
            # Both halves, so a failure is readable without a second run.
            print(phone.succeed("cat /tmp/peer.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -80 || true"))
            raise

        peer_log = phone.succeed("cat /tmp/peer.log")
        print(peer_log)

        # The phone advertised, which is what the panel's browse had to resolve. The
        # instance label here and the one in the UDC declaration are the same string, and
        # that identity is the whole of `discovery::instance_matches`.
        assert "mDNS advertised" in peer_log and "_matterc._udp" in peer_log, peer_log

        journal = panel.succeed("journalctl -u castaway --no-pager")
        # The commissioning worker took the request off the channel...
        assert "matter: commissioning a casting client" in journal, journal
        # ...ran PASE against the node it found by browsing...
        assert "PASE session established" in journal, journal
        # ...and then CASE, which is `complete_via_case` — the step that turns AddNOC
        # into a fabric member rather than a half-commissioned node.
        assert "CASE session established" in journal, journal
        # The node id comes from our CA, not rs-matter's: 0x1000 is FIRST_CLIENT_NODE_ID.
        assert re.search(r"casting client commissioned.*node_id=4096", journal), journal

    with subtest("the phone casts, and the panel plays it"):
        try:
            phone.wait_until_succeeds(
                "grep -q 'matter-peer completed' /tmp/peer.log", timeout=120
            )
        except Exception:
            print(phone.succeed("cat /tmp/peer.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -80 || true"))
            raise

        peer_log = phone.succeed("cat /tmp/peer.log")
        # The direction that makes this Matter *Casting*: the panel commissioned the
        # phone, and the phone is now the one driving. The invoke rides the CASE session
        # established during commissioning — the panel advertises no operational
        # `_matter._tcp` record, so there is nothing for the client to resolve it by and
        # session reuse is the only path this can take.
        assert "LauncherResponse status=Success" in peer_log, peer_log

        journal = panel.succeed("journalctl -u castaway --no-pager")
        # NOTE (2026-08-05): this used to claim the invoke "landed on the content-app
        # endpoint rather than the bare player". It did not — `app=1` is PLAYER_ENDPOINT,
        # content apps start at 6, and `matter-peer` defaults to `--endpoint 1` with
        # nothing here overriding it. **No Content App endpoint is invoked anywhere**, and
        # a client's TargetApp match is therefore untested. See `docs/test-matrix.md` §4.7.
        assert re.search(r"matter: launching.*app=1", journal), journal
        assert "${castUrl}" in journal, journal
        # And it became a session, which is the only claim that matters to anyone
        # holding a phone: a LaunchURL is a sentence, and this is the panel acting on it.
        assert re.search(r"session: play.*source=matter/", journal), journal
        assert re.search(r"null pipeline: PLAY.*${castUrl}", journal), journal
        # The display string survived the round trip into the now-playing surface.
        assert "${castTitle}" in journal, journal

    with subtest("a passcode nobody types comes off the glass on its own"):
        # The other end of the flow, and the one that used to have no end at all: a phone
        # declares, the panel puts an eight-digit commissioning passcode on a wall-mounted
        # screen, and the phone walks away. `expire` ran only on the way past the *next*
        # inbound datagram, and a phone that walked away sends no more datagrams — so the
        # number stayed up indefinitely while the state machine considered it dead and
        # would have refused it (#197).
        #
        # `server.rs` proves the timer arms and emits `Prompt::Clear` over a real socket
        # with paused time. What only this can prove is the link past it: `pump_prompts`
        # turning that `Clear` into a takedown of the sticky OSD message, on the real
        # panel, through the module.
        #
        # A fresh instance name, so this is a new phone rather than a retransmit of the
        # one already commissioned.
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--instance 00112233445566AA --name 'a phone that leaves' "
            f"--declare-only --passcode-file /dev/null > /tmp/leaver.log 2>&1"
        )
        phone.succeed("grep -q 'matter-peer: declared and leaving' /tmp/leaver.log")

        panel.wait_until_succeeds(
            "journalctl -u castaway --no-pager "
            "| grep -q 'a phone that leaves wants to cast — enter'",
            timeout=60,
        )
        # Nothing else happens on the wire from here. The panel is on its own.
        panel.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep -q 'a displayed passcode expired'",
            # PASSCODE_LIFETIME is 180 s and is deliberately not shortened for the test:
            # what is being asserted is the constant the panel ships with.
            timeout=260,
        )
        journal = panel.succeed("journalctl -u castaway --no-pager")
        # …and the OSD, not just the log. The number lives on the overlay and nowhere
        # else, so a `Clear` that never reached it leaves the passcode exactly where it
        # was — which is the failure, unchanged.
        #
        # Read from *after* the expiry line rather than anywhere in the journal: the
        # commissioning flow above also clears the OSD when it finishes, and an assertion
        # that matched that one would pass with this path removed entirely.
        after_expiry = journal.split("a displayed passcode expired", 1)[1]
        assert "OSD clear" in after_expiry, after_expiry
  '';
}
