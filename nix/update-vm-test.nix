# The whole auto-update loop, end to end, with no Windows and no network (#345).
#
# What a nixosTest can carry here is more than it looks like, because the parts that are
# Windows-specific are small and well fenced: the job object, `GetLastInputInfo`, and
# schtasks. Everything else — the launcher's supervision, the pointer files, the release
# API call, signature verification, the build-number ordering, staging, the atomic rename,
# the handshake exit, and the launcher restarting into the new tree — is the same code on
# both platforms, and this runs all of it.
#
# The one thing that could not be faked is the *key*. A receiver that trusts nothing
# refuses every release, so the two builds here are compiled against the checked-in test
# key (`CASTAWAY_RELEASE_PUBKEY`, the same compile-time knob an operator running their own
# fork would use), and the release is signed with its secret half by `release-manifest` —
# the very script `release.yml` calls. So this check also fails if the release workflow and
# the receiver ever stop agreeing about the format.
#
# Two builds, not one, and that is what makes the ordering real: the running receiver is
# stamped build 100 and the release is build 101, so `Offer::Newer` is a fact about the
# binaries rather than something the test asserted into existence. It is also why the loop
# terminates — after activating, the new receiver reports 101, reads the same manifest, and
# concludes there is nothing to do.
{ pkgs, castaway, launcher, releaseManifest }:

let
  # Two versions. Forty hex characters because that is what a `VersionId` is, and visibly
  # patterned because a failure message full of shas is unreadable otherwise.
  shaA = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
  shaB = "b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2";
  shortB = builtins.substring 0 7 shaB;
  buildA = 100;
  buildB = 101;

  testKey = ../crates/update/fixtures/test-release.key;
  testPub = ../crates/update/fixtures/test-release.pub;

  # The port the fake release API answers on, and the address it answers at. Loopback on
  # the receiver itself rather than a second node: the asset URLs have to be baked into
  # the release JSON at build time, and localhost is the one address a build can know.
  apiPort = 8099;
  apiBase = "http://127.0.0.1:${toString apiPort}";

  # A receiver that knows which build it is and which key it trusts. Overriding the
  # package rather than adding a feature: both are already build-time inputs of every real
  # artifact, so this configures the same knobs the release build uses (D55 — nothing here
  # is a compile-time gate on behaviour).
  receiver = { build, rev }: castaway.overrideAttrs (_: {
    CASTAWAY_BUILD = toString build;
    CASTAWAY_GIT_REV = rev;
    CASTAWAY_RELEASE_PUBKEY = builtins.readFile testPub;
  });
  receiverA = receiver { build = buildA; rev = builtins.substring 0 7 shaA; };
  receiverB = receiver { build = buildB; rev = shortB; };

  artifact = "castaway-portable-${shortB}.zip";

  # The release, as GitHub would serve it: a zip with one wrapping directory (exactly what
  # `nix/windows.nix`'s `mkArchive` produces), the signed manifest beside it, and the API
  # response that points at both.
  release = pkgs.runCommand "castaway-test-release"
    {
      nativeBuildInputs = [ pkgs.zip pkgs.jq releaseManifest pkgs.coreutils ];
    } ''
    mkdir -p tree/castaway-portable
    install -m755 ${receiverB}/bin/castaway tree/castaway-portable/castaway
    ( cd tree \
      && find castaway-portable -exec touch -d '1980-01-01 00:00:00 UTC' {} + \
      && find castaway-portable | sort | zip -qX ../${artifact} -@ )

    export CASTAWAY_RELEASE_SECRET_KEY="$(cat ${testKey})"
    # The script checks its own output against the key the receiver carries. Pointing it at
    # the fixture's public half is what makes that check run here rather than be skipped.
    export CASTAWAY_RELEASE_PUBKEY=${testPub}
    castaway-release-manifest ${artifact} ${shaB} ${toString buildB} .

    mkdir -p "$out/assets" "$out/repos/schlarpc/castaway/releases"
    cp ${artifact} manifest.json manifest.json.minisig "$out/assets/"
    # The two fields the receiver reads, and nothing else — it takes the asset URLs from
    # here and the *truth* from the signature.
    jq -n --arg base '${apiBase}' --arg artifact '${artifact}' '{
      tag_name: "build-${shortB}",
      assets: [
        { name: "manifest.json", browser_download_url: ($base + "/assets/manifest.json") },
        { name: "manifest.json.minisig", browser_download_url: ($base + "/assets/manifest.json.minisig") },
        { name: $artifact, browser_download_url: ($base + "/assets/" + $artifact) }
      ]
    }' > "$out/repos/schlarpc/castaway/releases/latest"
  '';

  # The panel's config. The window is the *shipped* one — the test moves the guest's clock
  # into it rather than widening it, so what runs here is the schedule that ships.
  config = pkgs.writeText "castaway.toml" ''
    friendly_name = "castaway-update-vm"
    http_port = 8080

    [log]
    # The updater's "Latest is not newer" line is `debug` — it is the ordinary answer on
    # every night after the first, and an unattended panel should not narrate those at
    # `info`. The test asserts on it, so the console filter has to carry it.
    level = "info,castaway_update=debug"
    to_file = false

    [update]
    enable = true
    base_url = "${apiBase}"
    repository = "schlarpc/castaway"
    window_start = "03:30"
    window_end = "05:00"
    # Zero, because the panel in this VM has no input device at all and the Linux build
    # reports itself permanently idle (crates/app/src/update.rs says why). What is being
    # tested here is the *loop*; the idle rule itself is asserted in virtual time in
    # `castaway_update::policy`.
    idle_minutes = 0
    # Immediately, so the health marker and the version GC are observable inside a test
    # rather than five minutes into one.
    healthy_after_minutes = 0
  '';

  root = "/var/lib/castaway";
in
pkgs.testers.runNixOSTest {
  name = "castaway-update";

  nodes.receiver = { ... }: {
    virtualisation.memorySize = 2048;

    # UTC and no time sync, because the test moves the clock into the update window and a
    # daemon putting it back would be a flake nobody could reproduce.
    time.timeZone = "UTC";
    services.timesyncd.enable = false;

    environment.systemPackages = [ pkgs.python3 pkgs.jq ];

    # GitHub, for the purposes of one panel.
    systemd.services.release-api = {
      description = "a release API with one release in it";
      wantedBy = [ "multi-user.target" ];
      serviceConfig.ExecStart =
        "${pkgs.python3}/bin/python3 -m http.server ${toString apiPort} "
        + "--bind 127.0.0.1 --directory ${release}";
    };

    # The launcher, exactly as the box will run it: one binary, one argument, and
    # everything else read off the tree beneath it.
    systemd.services.castaway-launcher = {
      description = "the castaway launcher";
      serviceConfig = {
        ExecStart = "${launcher}/bin/launcher --root ${root}";
        Environment = [
          "CASTAWAY_CONFIG=${config}"
          "HOME=/root"
        ];
        Restart = "no";
      };
    };
  };

  testScript = ''
    start_all()
    receiver.wait_for_unit("multi-user.target")
    receiver.wait_for_unit("release-api.service")
    receiver.wait_for_open_port(${toString apiPort})

    with subtest("the install tree, as the box's one-time migration leaves it"):
        receiver.succeed("mkdir -p ${root}/versions/${shaA}")
        receiver.succeed(
            "install -m755 ${receiverA}/bin/castaway ${root}/versions/${shaA}/castaway"
        )
        receiver.succeed("printf '${shaA}\\n' > ${root}/current.txt")

    with subtest("the clock is inside the shipped update window"):
        # 04:00, in the middle of the 03:30–05:00 window `Policy::default` ships. Moving
        # the panel to the window rather than the window to the panel is what makes this a
        # test of the constant that ships.
        receiver.succeed("date -s '04:00:00'")

    receiver.succeed("systemctl start castaway-launcher")
    log = "${root}/castaway.log"

    with subtest("the launcher starts the version current.txt names, and it arms"):
        receiver.wait_until_succeeds(f"grep -q 'auto-update is armed' {log}", timeout=120)
        # The build number reached the binary: without it the updater stands down as
        # `UnknownBuild` and nothing below could happen.
        receiver.succeed(f"grep -q 'build=${toString buildA}' {log}")

    with subtest("this version reports itself healthy, which is what stops a rollback"):
        receiver.wait_until_succeeds(
            "test -f ${root}/versions/${shaA}/.healthy", timeout=120
        )

    with subtest("the signed release is verified, ordered, and staged"):
        receiver.wait_until_succeeds(
            f"grep -q 'the release is signed' {log}", timeout=180
        )
        receiver.wait_until_succeeds(
            f"grep -q 'staged and waiting for the panel' {log}", timeout=300
        )
        # Named only once it was complete: the staging directory is gone and the tree is
        # under a name `VersionId::parse` accepts.
        receiver.succeed("test -x ${root}/versions/${shaB}/castaway")
        receiver.succeed("test ! -e ${root}/versions/.staging-${shaB}")

    with subtest("the panel is quiet, so it hands over to the launcher"):
        receiver.wait_until_succeeds(
            f"grep -q 'restarting into the new version' {log}", timeout=180
        )
        # The pointers moved together and in the right order: the new version is current,
        # and the one that was running is the rollback target.
        receiver.wait_until_succeeds(
            "test \"$(cat ${root}/current.txt)\" = ${shaB}", timeout=60
        )
        receiver.succeed("test \"$(cat ${root}/previous.txt)\" = ${shaA}")

    with subtest("the launcher notices the handshake and starts the new tree"):
        receiver.wait_until_succeeds(
            f"grep -q 'asked to be reloaded' {log}", timeout=120
        )
        receiver.wait_until_succeeds(
            f"grep -q 'now running version ${shortB}' {log}", timeout=120
        )
        # And the new one comes all the way up, which is the claim the whole exercise is
        # for: an unattended panel is running newer bits than it was, by itself.
        receiver.wait_until_succeeds(
            "test -f ${root}/versions/${shaB}/.healthy", timeout=180
        )

    with subtest("and it stops: the same release is no longer newer than what is running"):
        receiver.wait_until_succeeds(
            f"grep -q 'Latest is not newer' {log}", timeout=300
        )
        # The rollback target is kept, not collected — a panel with nothing to fall back to
        # is the state this whole design exists to avoid.
        receiver.succeed("test -d ${root}/versions/${shaA}")
  '';
}
