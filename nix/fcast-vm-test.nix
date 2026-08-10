# FCast end-to-end (#241), tier 2 (ground rule 6): the receiver in one VM, driven from
# a second VM by the *real* transmitters — not scripted lookalikes:
#
#  1. the reference terminal sender from FUTO's repository, pinned at the same commit
#     the checked-in wire fixtures were captured from. It links `fcast-sender-sdk`,
#     which is the exact stack Grayjay embeds, so what passes here is what a phone
#     running Grayjay does — including the SDK's own mDNS discovery, its `Version {4}`
#     hello downgrading to our v3, and its automatic `MediaItemEnd` subscription.
#  2. nixpkgs' `fcast-client`, the 2024 pre-SDK client: no `Version` frame at all, so
#     it exercises the implicit-v1 path with a second independent implementation.
#
# Rule 9's memory note applies: reference implementations are banned as *runtime*
# dependencies, not as Nix-pinned test oracles. Neither package ships in any output.
{ pkgs, self }:

let
  httpPort = 8080;
  friendlyName = "castaway-vm";
  # `advertised_name(FCast)` = "<friendly_name>#fcast" — what pickers actually show.
  advertised = "${friendlyName}#fcast";

  # The reference sender, pinned at the commit the fixtures in
  # crates/proto-fcast/tests/fixtures were captured from (see its README). Only the
  # terminal sender package is built — the workspace's receiver crates want GStreamer
  # and friends, and `-p fcast` never compiles them.
  fcastSender = pkgs.rustPlatform.buildRustPackage {
    pname = "fcast-terminal-sender";
    version = "0-unstable-2026-08-03";
    src = pkgs.fetchgit {
      url = "https://gitlab.futo.org/videostreaming/fcast.git";
      rev = "f22f72dcd62dbe7de401c6ddf1a0a3c2e1f11c37";
      hash = "sha256-MUCrrnd9jPtYZ98GEsCdGwg54HjR72IEESL5p/OV+PM=";
    };
    cargoHash = "sha256-xMyVUb6cYvbUFfjLYu/YRTGGWQLXb8MQXVulhUF+FLQ=";
    buildAndTestSubdir = "senders/terminal";
    doCheck = false;
    meta.description = "FCast reference terminal sender (test oracle only, never shipped)";
  };

  # The 2024 pre-SDK client, renamed so both `fcast` binaries can coexist on the
  # sender node.
  fcastOldClient = pkgs.runCommand "fcast-2024" { } ''
    mkdir -p $out/bin
    ln -s ${pkgs.fcast-client}/bin/fcast $out/bin/fcast-2024
  '';

  # A two-item playlist for the queue-owning half of the adapter. Inline JSON (v3
  # `PlaylistContent`), the same shape the repository's own example file uses.
  playlist = builtins.toJSON {
    contentType = 0;
    items = [
      {
        container = "video/mp4";
        url = "http://example.invalid/first.mp4";
        metadata = {
          type = 0;
          title = "First Clip";
        };
      }
      {
        container = "video/mp4";
        url = "http://example.invalid/second.mp4";
        metadata = {
          type = 0;
          title = "Second Clip";
        };
      }
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "castaway-fcast";

  nodes = {
    receiver = { config, ... }: {
      imports = [ self.nixosModules.castaway ];
      services.castaway = {
        enable = true;
        inherit httpPort;
        # The portable build: the null pipeline's journal lines are what the media
        # assertions grep for (same reasoning as nix/vm-test.nix).
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.castaway-portable;
        logLevel = "info,castaway=debug,proto_fcast=debug";
        settings = {
          friendly_name = friendlyName;
          uuid = "0f8c2e10-0000-4000-8000-0000000c0571";
          interface = config.networking.primaryIPAddress;
          # Pinned, not defaulted, so a defaults change cannot hollow this test out.
          enable.fcast = true;
        };
      };
    };

    sender = { ... }: {
      networking.firewall.enable = false;
      # avahi for the TXT-record assertion; the SDK sender does its own mDNS.
      services.avahi = {
        enable = true;
        openFirewall = false;
      };
      environment.systemPackages = [ fcastSender fcastOldClient ];
    };
  };

  testScript = { nodes, ... }: ''
    import shlex

    kiosk = "${nodes.receiver.networking.primaryIPAddress}"

    playlist = ${builtins.toJSON playlist}

    def journal(pattern):
        receiver.succeed(
            "journalctl -u castaway --no-pager | grep -q " + shlex.quote(pattern)
        )

    def send(args):
        # One connection per invocation, exactly how the CLI is built. The SDK holds
        # the connection through its ~2s "connected event deadline" before the verb,
        # so these are not instant — but they are bounded.
        return sender.succeed(f"fcast -H {kiosk} {args}")

    start_all()

    with subtest("the service comes up and the FCast port is listening"):
        receiver.wait_for_unit("castaway.service")
        receiver.wait_for_open_port(46899)
        sender.wait_for_unit("multi-user.target")

    with subtest("the advertisement is discoverable and states protocol v3"):
        # Third-party view first: avahi sees the record, its TXT v, and the port.
        # The instance is `${advertised}`, but avahi-browse's parseable output
        # escapes `#` as `\035` (measured on the bench), so the grep matches the
        # unescaped prefix and the port+TXT that share the resolved line.
        sender.wait_until_succeeds(
            "avahi-browse -rpt _fcast._tcp | grep '${friendlyName}' | grep ';46899;' | grep -q 'v=3'",
            timeout=60,
        )
        # Then the view that matters: the sender SDK's own discovery finds us by name.
        scan = sender.succeed("fcast scan --timeout 10")
        assert "${advertised}" in scan, scan

    with subtest("the reference sender casts a URL and it reaches the pipeline"):
        send("play --mime-type video/mp4 --url http://example.invalid/clip.mp4 -t 5")
        journal("session: play")
        journal("null pipeline: PLAY")
        journal("http://example.invalid/clip.mp4")

    with subtest("transport verbs from the real sender reach the pipeline as absolute controls"):
        send("pause")
        journal("null pipeline: CONTROL txn=Pause")
        send("resume")
        journal("null pipeline: CONTROL txn=Play")
        send("seek -t 42")
        journal("null pipeline: CONTROL txn=Seek(42s)")
        send("set-volume -v 0.5")
        journal("null pipeline: CONTROL txn=Volume")

    with subtest("a listening sender is told what is playing"):
        # `listen` prints what the SDK parsed out of our frames: the session's
        # `Initial.playData` arrives as "Source changed" and the 1 Hz PlaybackUpdate
        # ticker as "Playback state changed" / "Time changed". The sender exits on
        # the timeout signal, so the exit code is the timeout's — the transcript is
        # the assertion, and it proves the *real SDK* accepted our bytes.
        # `|| true`, not `; true`: the driver's shell runs `set -e`, which aborts the
        # list at timeout's SIGTERM exit before a `;`-joined true can run.
        out = sender.succeed(
            f"timeout 8 fcast -H {kiosk} listen 2>&1 || true"
        )
        assert "Source changed" in out, out
        assert "Playback state changed" in out, out

    with subtest("a playlist loads, and the receiver owns the queue"):
        send(f"play --mime-type application/json --content {shlex.quote(playlist)}")
        journal("http://example.invalid/first.mp4")
        # UP NEXT names the second item: the queue arrived, not just item one.
        journal("Second Clip")
        # A sender-driven jump plays item two.
        send("set-playlist-item -i 1")
        journal("http://example.invalid/second.mp4")

    with subtest("stop clears the session"):
        send("stop")
        journal("null pipeline: CONTROL txn=Stop")

    with subtest("the 2024 pre-SDK client still casts (implicit v1)"):
        sender.succeed(
            f"fcast-2024 -h {kiosk} play --mime_type video/mp4 --url http://example.invalid/legacy.mp4"
        )
        journal("http://example.invalid/legacy.mp4")
  '';
}
