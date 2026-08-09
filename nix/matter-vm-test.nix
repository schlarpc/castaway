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

  # The second cast, after the panel restarts (#173). A different URL so the assertion
  # that it played cannot be satisfied by the first cast's journal lines.
  castUrlAgain = "https://example.invalid/matter-vm-again.mp4";
  castTitleAgain = "matter-vm cast again";

  # The overlap pair (#209): whichever racer wins the pairing slot casts the first URL;
  # the refused one comes back later and casts the second. Distinct from each other and
  # from every other cast in this file, so neither assertion can ride an earlier journal.
  raceUrl = "https://example.invalid/matter-vm-race.mp4";
  raceTitle = "matter-vm race winner";
  raceRetryUrl = "https://example.invalid/matter-vm-race-retry.mp4";
  raceRetryTitle = "matter-vm race retry";

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
          # Two content apps, not the default one (#196). `TargetNavigator`'s identifiers
          # are one-based and dense while endpoints start at 6, so with a single app every
          # off-by-one in that mapping still lands on the right place. Two is the smallest
          # catalogue where target 2 and endpoint 7 are different numbers, and it is what
          # a `NavigateTarget` below actually distinguishes.
          #
          # Both `media-url`: a `browser` app is dropped at startup on a build with no
          # browser, and `castaway-portable` is one, so a browser app here would be an
          # endpoint that silently is not there.
          matter.apps = [
            {
              name = "castaway";
              vendor_name = "castaway";
              vendor_id = 65521; # 0xFFF1, the test range
              product_id = 32769; # 0x8001
              application_id = "castaway";
              surface = "media-url";
            }
            {
              name = "castaway two";
              vendor_name = "castaway";
              vendor_id = 65521;
              product_id = 32770; # 0x8002
              application_id = "castaway.two";
              surface = "media-url";
            }
          ];
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

    def one(pattern, text):
        """`re.search` that says what it was looking for when it finds nothing.

        Also what the driver's type checker wants: a bare `re.search(...).group(1)` is
        `None.group` on the failing path, and it refuses the script rather than the run.
        """
        found = re.search(pattern, text)
        assert found, f"nothing matched {pattern!r} in:\n{text}"
        return found

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
            f"--display-string '${castTitle}' "
            # Persist the fabric this run is commissioned onto, so the cast-again
            # scenario at the bottom can be this phone coming back (#173).
            f"--state-dir /tmp/peer-state "
            # Everything past the LaunchURL (#196). One commissioning, then every cluster
            # handler this node serves, driven through a real interaction model by a
            # client. `--transport play,pause` in that order because the panel is playing
            # by then — the LaunchURL above is what put it there — so `drive`'s
            # `NotActive` guard and its success path are both on the same run.
            f"--read-descriptor --app-basic --app-endpoint 7 "
            f"--transport play,pause,stop,play --navigate 2 "
            f"--launch-content 'a search nobody can serve' --read-acl "
            f"> /tmp/peer.log 2>&1 &"
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

        # The fabric just gained its first member, which is the moment the operational
        # `_matter._tcp` record goes up (#173) — before that there was nobody entitled
        # to resolve it, and without it a phone whose CASE session dies can never come
        # back. Instance name per the spec: <compressed-fabric-id>-<node-id>, uppercase
        # hex, the node id being the panel's own (1).
        panel.wait_until_succeeds(
            "journalctl -u castaway --no-pager | grep 'mDNS advertised' "
            "| grep -q '_matter\\._tcp'",
            timeout=30,
        )
        journal = panel.succeed("journalctl -u castaway --no-pager")
        assert re.search(
            r"mDNS advertised.*_matter\._tcp.*[0-9A-F]{16}-0000000000000001", journal
        ), journal

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
        # phone, and the phone is now the one driving. Seconds after commissioning, the
        # invoke rides the CASE session `complete_via_case` established — the reuse
        # branch. The other branch, resolving the operational record when no session
        # survives, is what the restart scenario at the bottom exercises (#173).
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

    with subtest("a client reads the endpoint tree the panel actually serves"):
        # `node.rs` had zero tests until `4cd7373`, and what it grew then was a
        # constructor asserting against itself. This is the same tree read *by a client*,
        # through the interaction model, which is the only form a phone ever sees (#196).
        descriptors = dict(
            re.findall(r"descriptor endpoint=(\d+) (.*)", peer_log)
        )
        print(descriptors)
        assert set(descriptors) == {"0", "1", "7"}, descriptors

        # Our clusters stay off the root. A Descriptor on endpoint 0 listing ContentLauncher
        # is a client being told the root can launch content, and `AppCluster` exists to
        # keep that from happening.
        assert "0x050a" not in descriptors["0"], descriptors["0"]
        # …and the root lists the rest of the tree, which is how a client finds any of it.
        parts = one(r"parts=\[(.*)\]", descriptors["0"]).group(1)
        assert parts.split(", ") == ['"1"', '"6"', '"7"'], parts

        # The player: the casting video-player device type a client matches on before it
        # will cast at all, and all four of its clusters.
        assert "0x00000023" in descriptors["1"], descriptors["1"]
        for cluster in ["0x001d", "0x050a", "0x0506", "0x0505"]:
            assert cluster in descriptors["1"], (cluster, descriptors["1"])
        # A content app: the content-app device type, ApplicationBasic and ContentLauncher —
        # and **not** MediaPlayback, which would give a client two places to send Play and
        # no rule for which.
        assert "0x00000024" in descriptors["7"], descriptors["7"]
        assert "0x050d" in descriptors["7"], descriptors["7"]
        assert "0x050a" in descriptors["7"], descriptors["7"]
        assert "0x0506" not in descriptors["7"], descriptors["7"]

    with subtest("the content app answers for itself, on the endpoint it occupies"):
        # `ApplicationBasicHandler`'s seven attributes, none of which had ever been read.
        # Every one comes out of the catalogue entry rather than a constant, so reading
        # them is what says the endpoint a client picked is the app it thought it picked —
        # endpoint 7 is the *second* app, and answering with the first one's name would be
        # a client casting into the wrong place with no way to tell.
        app = one(r"application_basic endpoint=7 (.*)", peer_log).group(1)
        print(app)
        assert 'name="castaway two"' in app, app
        assert 'vendor_name="castaway"' in app, app
        assert "vendor_id=0xfff1" in app, app
        assert "product_id=0x8002" in app, app
        assert "app_id=castaway.two" in app, app
        # Not playing *this* app's media, so `Stopped` — the LaunchURL above went to the
        # player endpoint, and this attribute is what distinguishes the two.
        assert "status=Stopped" in app, app
        # And the one attribute a commissioned phone may *not* read. The spec gives
        # `AllowedVendorList` Administer privilege — it is the list a content app would
        # refuse a casting client by — and this client holds `Operate`, so `rs-matter`'s
        # access control answers `UnsupportedAccess` before our handler is reached. That is
        # the ACL grant asserted on a cluster a phone actually touches, which is stronger
        # evidence than the Access Control read below. (Our handler returns an empty list
        # when it *is* reached, and says why: a non-empty one is an access-control claim
        # backed by attestation this panel does not verify.)
        assert "allowed_vendors=refused" in app, app

    with subtest("the transport works, and stops working when there is nothing to drive"):
        # `MediaPlaybackHandler::drive` and its `NotActive` guard: no Play, Pause or Stop
        # invoke had ever run at any tier. The sequence is what makes the guard reachable —
        # `stop` is the only thing that can put the panel back to `NotPlaying`, and the
        # `play` after it is the one that has to be refused.
        verbs = re.findall(r"media_playback (\w[\w-]*) status=(\w+)", peer_log)
        states = re.findall(r"media_playback current_state=(\w+)", peer_log)
        print(verbs, states)
        assert verbs == [
            ("play", "Success"),
            ("pause", "Success"),
            ("stop", "Success"),
            ("play", "NotActive"),
        ], verbs
        # And the state a phone would read back after each one. A transport command the
        # panel accepted and then denied having accepted is what a paused phone showing
        # "playing" looks like.
        assert states == ["Playing", "Paused", "NotPlaying", "NotPlaying"], states

    with subtest("a client can list the panel's apps and select one"):
        nav = one(r"target_navigator targets=(.*) current=(\d+)", peer_log)
        targets, current = nav.group(1), nav.group(2)
        print(targets, current)
        # One-based and dense, in catalogue order, against endpoints that start at 6.
        assert '"1:castaway"' in targets, targets
        assert '"2:castaway two"' in targets, targets
        # The launch went to the player endpoint, which no content app owns — and 0 is the
        # spec's reserved "no target", which is the honest answer rather than target 1.
        assert current == "0", current
        assert "navigate_target target=2 status=Success" in peer_log, peer_log
        # Target 2 is endpoint 7, and `endpoint_for_target` is four lines with two ways to
        # be silently wrong — `saturating_sub` in place of `checked_sub` selects the first
        # app for the reserved target 0, which launches something rather than nothing. The
        # only place the selection is observable is the next read of this attribute, which
        # is also the only place a phone would look.
        assert "current after navigate=2" in peer_log, peer_log
        journal = panel.succeed("journalctl -u castaway --no-pager")
        assert "matter: a client selected a content app" in journal, journal

    with subtest("a search a media-URL app cannot serve is refused, not faked"):
        # `handle_launch_content`'s `parameterList` walk, and the refusal underneath it.
        # A media-URL app has no notion of "find me something called X" — the alternative
        # to saying so is opening a home page and calling it a result.
        # `AuthFailed`, not something more descriptive, because the cluster's whole
        # vocabulary is Success / URLNotAvailable / AuthFailed / {Text,Audio}TrackNotAvailable
        # — `NotAllowed` and `NoAppFound` both land on it, which is lossy and is the
        # spec's doing rather than ours. The panel's own log is where the reason survives.
        assert re.search(
            r"launch_content query=.*endpoint=7 status=AuthFailed", peer_log
        ), peer_log
        assert "matter: declining a LaunchContent" in journal, journal

    with subtest("a commissioned phone gets Operate, and cannot rewrite the access list"):
        # `seed_acls` grants `Operate` with a comment saying why not `Administer`:
        # "handing out Administer because it is simpler would let any commissioned phone
        # evict every other one". Nothing tested it, so a regression to `ADMINISTER`
        # passed every check in the repo — and the phone that noticed would be the second
        # one, when the first one removed its fabric.
        acl = one(r"access_control acl (.*)", peer_log).group(1)
        print(acl)
        assert "refused" in acl, f"an Operate client read the ACL: {acl}"

    with subtest("a phone that mistypes the passcode is told so, not left guessing"):
        # `commission_loop`'s `Err` arm logged, put a banner on the panel, and sent the
        # client nothing — so all ten of the spec's commissioning error codes had no
        # producer anywhere in the tree. In the room: somebody misreads a digit across
        # it, and their phone gets silence rather than "wrong code". The phone's UI then
        # has to guess, and what it usually guesses is a timeout (#198).
        #
        # PASE, not discovery: the peer advertises correctly and is found, and the panel
        # runs the handshake against a verifier built from a number one digit out.
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--instance 00112233445566BB --name 'a phone that mistypes' "
            f"--matter-port 5541 --discriminator 3841 --wrong-passcode "
            f"--passcode-file /tmp/wrong.txt > /tmp/wrong.log 2>&1 &"
        )
        phone.wait_until_succeeds(
            "grep -q 'passcode dialog is up' /tmp/wrong.log", timeout=60
        )
        shown = panel.succeed(
            "journalctl -u castaway --no-pager "
            "| grep 'a phone that mistypes wants to cast' "
            "| grep -oE '[0-9]{4}-[0-9]{4}' | tail -1"
        ).strip()
        assert re.fullmatch(r"[0-9]{4}-[0-9]{4}", shown), f"no passcode on screen: {shown!r}"
        phone.succeed(f"echo {shown} > /tmp/wrong.txt")

        try:
            phone.wait_until_succeeds(
                "grep -q 'the panel refused with' /tmp/wrong.log", timeout=180
            )
        except Exception:
            print(phone.succeed("cat /tmp/wrong.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -80 || true"))
            raise

        wrong_log = phone.succeed("cat /tmp/wrong.log")
        # Code 2, `PaseConnectionFailed`. Not 3 (`PaseAuthFailed`), and deliberately so:
        # rs-matter 0.2 does not distinguish a wrong passcode from an unreachable peer at
        # this boundary, so emitting 3 would be a guess. The reasoning, and the other five
        # codes with no producer, are on `CommissionStage::cd_error`.
        assert "PaseConnectionFailed (2)" in wrong_log, wrong_log
        panel.succeed(
            "journalctl -u castaway --no-pager "
            "| grep -q 'telling the client why commissioning stopped'"
        )

    with subtest("a phone the panel cannot find is told that, and not something else"):
        # The other stage, isolated: everything is correct except the label the peer
        # advertises under, so the panel's browse waits out its full 60 s on a node that
        # is right there. The distinction matters to the client — a discovery failure is
        # worth retrying, a PASE failure is worth re-reading the number for.
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--instance 00112233445566CC --name 'a phone in disguise' "
            f"--matter-port 5542 --discriminator 3842 --wrong-instance "
            f"--passcode-file /tmp/disguise.txt > /tmp/disguise.log 2>&1 &"
        )
        phone.wait_until_succeeds(
            "grep -q 'passcode dialog is up' /tmp/disguise.log", timeout=60
        )
        shown = panel.succeed(
            "journalctl -u castaway --no-pager "
            "| grep 'a phone in disguise wants to cast' "
            "| grep -oE '[0-9]{4}-[0-9]{4}' | tail -1"
        ).strip()
        assert re.fullmatch(r"[0-9]{4}-[0-9]{4}", shown), f"no passcode on screen: {shown!r}"
        phone.succeed(f"echo {shown} > /tmp/disguise.txt")

        try:
            phone.wait_until_succeeds(
                "grep -q 'the panel refused with' /tmp/disguise.log", timeout=200
            )
        except Exception:
            print(phone.succeed("cat /tmp/disguise.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -80 || true"))
            raise

        disguise_log = phone.succeed("cat /tmp/disguise.log")
        assert "CommissionableDiscoveryFailed (1)" in disguise_log, disguise_log

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
        # Read from *after* the expiry line rather than anywhere in the journal, so an
        # OSD clear from any earlier flow cannot satisfy this assertion with the expiry
        # path removed entirely. (Since #209 the expiry clear is keyed to the instance
        # that lapsed; a finished commissioning replaces its prompt with a banner
        # instead of clearing.)
        after_expiry = journal.split("a displayed passcode expired", 1)[1]
        assert "OSD clear" in after_expiry, after_expiry

    # ---- Two phones at once (#209). --------------------------------------------------
    #
    # #198's failure phones each had the panel to themselves; the accident under test
    # here is overlap. The decided policy: the pairing slot is single-occupancy, the
    # second declaration is refused with CommissionerPasscodeDisabled — temporary
    # unavailability, since the spec's CdError list has no "busy" code — and prompts are
    # keyed by instance so neither phone's cancel or expiry can touch the other's number.

    race_instances = {"one": "00112233445566DD", "two": "00112233445566EE"}
    race_ports = {"one": (5543, 3843), "two": (5544, 3844)}

    with subtest("two phones declare at once; one gets the glass, one a clean refusal (#209)"):
        for racer in ("one", "two"):
            port, disc = race_ports[racer]
            phone.succeed(
                f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
                f"--instance {race_instances[racer]} --name 'racer {racer}' "
                f"--matter-port {port} --discriminator {disc} "
                f"--passcode-file /tmp/race-{racer}.txt --url '${raceUrl}' "
                f"--display-string '${raceTitle}' > /tmp/race-{racer}.log 2>&1 &"
            )

        # Exactly one wins the slot — the UDC socket loop is serial, so which one is a
        # race, and the script has to be ready for either answer.
        try:
            phone.wait_until_succeeds(
                "grep -q 'passcode dialog is up' /tmp/race-one.log /tmp/race-two.log",
                timeout=60,
            )
            phone.wait_until_succeeds(
                "grep -q 'the panel refused: CommissionerPasscodeDisabled' "
                "/tmp/race-one.log /tmp/race-two.log",
                timeout=60,
            )
        except Exception:
            print(phone.succeed("cat /tmp/race-one.log || true"))
            print(phone.succeed("cat /tmp/race-two.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -40 || true"))
            raise

        winner = (
            "one"
            if "passcode dialog is up" in phone.succeed("cat /tmp/race-one.log")
            else "two"
        )
        loser = "two" if winner == "one" else "one"
        print(f"race winner: racer {winner}")

        # One phone, one outcome: the winner was not also refused, and the loser never
        # saw a dialog.
        assert "the panel refused" not in phone.succeed(f"cat /tmp/race-{winner}.log")
        assert "passcode dialog is up" not in phone.succeed(f"cat /tmp/race-{loser}.log")

        # And the panel put up exactly one prompt — the winner's. Under the old accident
        # both phones were issued passcodes and the second prompt overwrote the first,
        # so whichever user looked at the screen saw at most one of the two numbers.
        journal = panel.succeed("journalctl -u castaway --no-pager")
        assert f"racer {winner} wants to cast" in journal, journal
        assert f"racer {loser} wants to cast" not in journal, journal
        assert re.search(
            r"declining a UDC declaration.*CommissionerPasscodeDisabled", journal
        ), journal

    with subtest("the refused phone's cancel does not clobber the winner's prompt (#209)"):
        # The refused user backs out on their phone, which is what a person does with a
        # refusal. Before prompts carried instance identity, this cancel took the
        # *winner's* passcode off the glass mid-pairing.
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--instance {race_instances[loser]} --name 'racer {loser}' "
            f"--cancel-only > /tmp/race-cancel.log 2>&1"
        )
        phone.succeed("grep -q 'matter-peer: cancel acknowledged' /tmp/race-cancel.log")

        # The winner's number is still up: nothing has cleared the OSD since the
        # winner's prompt went on it. The slice from the last prompt line is what makes
        # this an assertion about *this* prompt rather than about the whole run.
        journal = panel.succeed("journalctl -u castaway --no-pager")
        since_prompt = journal.rsplit("wants to cast — enter", 1)[1]
        assert "OSD clear" not in since_prompt, since_prompt

    with subtest("the surviving phone commissions and casts through the noise (#209)"):
        shown = panel.succeed(
            "journalctl -u castaway --no-pager "
            f"| grep 'racer {winner} wants to cast' "
            "| grep -oE '[0-9]{4}-[0-9]{4}' | tail -1"
        ).strip()
        assert re.fullmatch(r"[0-9]{4}-[0-9]{4}", shown), f"no passcode on screen: {shown!r}"
        phone.succeed(f"echo {shown} > /tmp/race-{winner}.txt")

        try:
            phone.wait_until_succeeds(
                f"grep -q 'matter-peer completed' /tmp/race-{winner}.log", timeout=180
            )
        except Exception:
            print(phone.succeed(f"cat /tmp/race-{winner}.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -80 || true"))
            raise

        journal = panel.succeed("journalctl -u castaway --no-pager")
        assert re.search(r"null pipeline: PLAY.*${raceUrl}", journal), journal

    with subtest("the refusal is temporary: the refused phone pairs once the slot is free (#209)"):
        port, disc = race_ports[loser]
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--instance {race_instances[loser]} --name 'racer {loser}' "
            f"--matter-port {port} --discriminator {disc} "
            f"--passcode-file /tmp/race-retry.txt --url '${raceRetryUrl}' "
            f"--display-string '${raceRetryTitle}' > /tmp/race-retry.log 2>&1 &"
        )
        phone.wait_until_succeeds(
            "grep -q 'passcode dialog is up' /tmp/race-retry.log", timeout=60
        )
        shown = panel.succeed(
            "journalctl -u castaway --no-pager "
            f"| grep 'racer {loser} wants to cast' "
            "| grep -oE '[0-9]{4}-[0-9]{4}' | tail -1"
        ).strip()
        assert re.fullmatch(r"[0-9]{4}-[0-9]{4}", shown), f"no passcode on screen: {shown!r}"
        phone.succeed(f"echo {shown} > /tmp/race-retry.txt")

        try:
            phone.wait_until_succeeds(
                "grep -q 'matter-peer completed' /tmp/race-retry.log", timeout=180
            )
        except Exception:
            print(phone.succeed("cat /tmp/race-retry.log || true"))
            print(panel.succeed("journalctl -u castaway --no-pager | tail -80 || true"))
            raise

        journal = panel.succeed("journalctl -u castaway --no-pager")
        assert re.search(r"null pipeline: PLAY.*${raceRetryUrl}", journal), journal
        assert "${raceRetryTitle}" in journal, journal

    with subtest("the panel restarts, and a commissioned phone casts again (#173)"):
        # Restarting the service is the bluntest of the ordinary ways every CASE session
        # dies at once — an idle timeout, a phone asleep overnight, and eviction from
        # rs-matter's fixed-size session table are the same failure, slower. "Cast again
        # tomorrow morning" is the normal use of a panel on a wall.
        panel.succeed("systemctl restart castaway.service")
        panel.wait_until_succeeds("ss -uln | grep -q ':5540'", timeout=90)

        # Everything below asserts against this invocation of the service, not the
        # journal the whole run has accumulated.
        invocation = panel.succeed(
            "systemctl show -p InvocationID --value castaway.service"
        ).strip()
        journal_now = f"journalctl --no-pager _SYSTEMD_INVOCATION_ID={invocation}"
        panel.wait_until_succeeds(
            f"{journal_now} | grep -q 'matter: casting receiver up'", timeout=90
        )

        # The phone comes back: same fabric off /tmp/peer-state, no UDC, no passcode, no
        # commissioning. Its CASE session died with the panel, so the only way back in
        # is resolving `<compressed-fabric-id>-<node-id>._matter._tcp` and establishing
        # a fresh one — rs-matter's `Transport::initiate`, second branch.
        phone.succeed(
            f"matter-peer-run --player {panel_ip} --bind {phone_ip} "
            f"--cast-again --state-dir /tmp/peer-state "
            f"--url '${castUrlAgain}' --display-string '${castTitleAgain}' "
            f"> /tmp/again.log 2>&1 &"
        )
        try:
            phone.wait_until_succeeds(
                "grep -q 'matter-peer completed' /tmp/again.log", timeout=180
            )
        except Exception:
            print(phone.succeed("cat /tmp/again.log || true"))
            print(panel.succeed(f"{journal_now} | tail -80 || true"))
            raise

        again_log = phone.succeed("cat /tmp/again.log")
        print(again_log)
        assert "loaded the fabric a previous run was commissioned onto" in again_log
        assert "cast again on attempt" in again_log, again_log
        assert "LauncherResponse status=Success" in again_log, again_log

        journal = panel.succeed(journal_now)
        # The record is up on the *new* invocation, rebuilt from persisted fabric state
        # alone — no commissioning happened on this boot, and a panel that restarted
        # without re-publishing it would strand every phone it ever paired.
        assert re.search(
            r"mDNS advertised.*_matter\._tcp.*[0-9A-F]{16}-0000000000000001", journal
        ), journal
        # And the second cast became a session on the restarted panel.
        assert re.search(r"matter: launching.*app=1", journal), journal
        assert re.search(r"null pipeline: PLAY.*${castUrlAgain}", journal), journal
        assert "${castTitleAgain}" in journal, journal
  '';
}
