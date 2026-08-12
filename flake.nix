{
  description = "A Rust application built with Nix flakes using Crane";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";


    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    # nix-direnv for the development shell
    nix-direnv = {
      url = "github:nix-community/nix-direnv";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # Prebuilt third-party blobs. Inputs rather than in-tree `fetchurl` hashes so
    # `flake.lock` records every external artifact this repo pulls in — one place to
    # audit, one update story. `file+` keeps them as the raw archives: the nix/
    # derivations do the unpacking, so layout policy lives beside the build that uses it.
    #
    # Every URL is immutable by construction — and immutable is not durable, which the
    # Release workflow's first cold run paid to find out. BtbN replaces the assets under
    # its `latest` tag daily *and prunes the dated `autobuild-*` releases after a couple
    # of weeks*, so the dated pin this used kept 404ing on any machine without the blob
    # already in its store; a dev box's warm store masked it. The blob now lives as an
    # asset on this repo's own `vendor` release (provenance in its notes), which only has
    # to keep existing — the content itself is still pinned by narHash in flake.lock. The
    # rest are version-stamped release and CDN paths whose upstreams do not prune.
    ffmpeg-windows-src = {
      url = "file+https://github.com/schlarpc/castaway/releases/download/vendor/ffmpeg-n7.1.5-10-g2aefd64d48-win64-lgpl-shared-7.1.zip";
      flake = false;
    };

    # The Widevine CDM for the Windows artifact — the CRX3 Chrome's own component updater
    # installs, pinned to the version nixpkgs pins for Linux. See nix/widevine-windows.nix
    # for the query that regenerates this URL, and why we ship a CDM at all rather than
    # letting the component updater fetch one at runtime (offline first boot, one known
    # version, no five-minute window where casting a rental silently does nothing).
    #
    # Unfree, so unpacking it is gated by `allowUnfreePredicate` below; the *fetch* is not,
    # because a flake input is fetched whenever this flake is evaluated.
    widevine-windows-src = {
      url = "file+https://edgedl.me.gvt1.com/edgedl/release2/chrome_component/acddvywyhts76ngei465tcu7besa_4.10.3050.0/oimompecagnajdejgnnjijobebaeigek_4.10.3050.0_win64_adoev3c5ys462nbqhaead57zg2pa.crx3";
      flake = false;
    };

    # AirServer's Windows installer: the two BLAKE2b constants that open its Cast
    # credential database (PROVENANCE §3), and the identity fixtures `proto-cast`
    # presents. Pinned so both are carved at build time rather than living in this tree —
    # the constants used to be string literals in `src/airserver_db.rs`, and the identity
    # (a Google-issued device certificate and its RSA private key) used to be checked in
    # under `crates/cast-replay/fixtures/airserver/`.
    #
    # The classic MSI rather than the Store MSIX: its URL is version-stamped and stable,
    # whereas the `.appinstaller` line rolls forward on its own 48-hour schedule. Both
    # carry a byte-identical database. Note the *fetch* happens on every evaluation of
    # this flake, the same way the CDM's does.
    #
    # nix/airserver-carve.nix does the carving and is deliberately offset-free, so a
    # version bump here should not need a code change: verified against 5.7.0, 5.7.1 and
    # 5.7.2, whose databases sit at three different offsets under two different schemas.
    airserver-msi-src = {
      url = "file+https://dl.airserver.com/pc32/AirServer-5.7.2-x64.msi";
      flake = false;
    };

    # castLabs "Electron for Content Security" — the browser runtime (D36), pinned on
    # *both* platforms rather than taking nixpkgs' Electron on Linux. Same Chromium major
    # everywhere is the point: developing against one Chrome and shipping another means
    # every codec, DRM and offscreen behaviour verified in CI was verified against a
    # browser we do not ship.
    #
    # ECS rather than upstream Electron because it is the only route to a VMP-signable
    # Widevine host that does not require a Google licence agreement (GAPS G55/G56). Both
    # archives carry H.264/AAC — measured for linux-x64 with `browser-host/codec-probe.js`,
    # and inferred for win32-x64 from the same decoder long-names in `ffmpeg.dll`, which is
    # as far as a Linux builder can get.
    #
    # MIT-licensed, so unlike the CDM these need no unfree gate. Bump both together or the
    # platforms drift, which is the whole thing this pin exists to prevent.
    electron-linux-src = {
      url = "file+https://github.com/castlabs/electron-releases/releases/download/v43.0.0%2Bwvcus/electron-v43.0.0+wvcus-linux-x64.zip";
      flake = false;
    };

    electron-windows-src = {
      url = "file+https://github.com/castlabs/electron-releases/releases/download/v43.0.0%2Bwvcus/electron-v43.0.0+wvcus-win32-x64.zip";
      flake = false;
    };

    # Chromium's Open Screen Protocol library — the reference implementation of Cast
    # Streaming, and the only authoritative description of its RTP framing and crypto.
    #
    # A *test* dependency, never a runtime one (ground rule 9). The
    # `openscreen-rtp-fixtures` check compiles nine of its translation units to
    # regenerate the golden RTP stream in `crates/proto-cast/tests/fixtures/rtp-stream/`,
    # which is what proves our receiver agrees with real senders instead of only with
    # itself. Nothing here is linked into the binary.
    openscreen-src = {
      url = "git+https://chromium.googlesource.com/openscreen?rev=b13215d275c0c1661cf3d7c19f55ad7f59020938";
      flake = false;
    };

    # moonlight-common-c and its two submodules — the GameStream client core we link
    # instead of reimplementing (D37). Three inputs rather than one because the
    # upstream CMake build fetches the submodules from the network, which a Nix build
    # cannot do; nix/moonlight-common-c.nix grafts them in.
    #
    # Note the licence asymmetry: moonlight-common-c is GPL-3.0 while this workspace is
    # MIT. The `stream` feature that links it is on by default (D55), so the default
    # artifact is GPL-bound; the builds that must stay MIT-clean (`castaway-portable`,
    # `gs-probe`) opt *out* via `--no-default-features`. See D37.
    moonlight-common-c-src = {
      url = "github:moonlight-stream/moonlight-common-c/e41355ea01670fd4c830b384009d31dd0339a705";
      flake = false;
    };
    moonlight-enet-src = {
      url = "github:cgutman/enet/aca87840b57f045a1f7f9299e4b1b9b8e2a5e2f1";
      flake = false;
    };
    moonlight-nanors-src = {
      url = "github:sleepybishop/nanors/b1e3c22ca0cdc0bb83e3cd6ed1a2fc77869ed99a";
      flake = false;
    };

    # Sony's LDAC library, for the one A2DP codec ffmpeg has no decoder for (#14).
    #
    # Its own input rather than `pkgs.ldacbt` because under the nixpkgs pinned above that
    # attribute is EHfive/ldacBT 2.0.2.3, built `_ENCODE_ONLY` — a shared object with no
    # `ldacBT_decode` in it and a header that does not declare one. A newer nixpkgs
    # replaced it with this fork, but reaching that means bumping ffmpeg and Electron too.
    # See nix/ldacbt.nix.
    #
    # Unlike moonlight-common-c, the licence composes: Apache-2.0, so linking it does not
    # bind the artifact the way D37's GPL-3.0 does.
    external-libldac-src = {
      url = "github:open-vela/external_libldac/5b4bf66096ba0d69615efb2422ba3d023c34c2fd";
      flake = false;
    };
  };

  outputs =
    { self
    , nixpkgs
    , rust-overlay
    , crane
    , nix-direnv
    , ffmpeg-windows-src
    , widevine-windows-src
    , airserver-msi-src
    , electron-linux-src
    , electron-windows-src
    , openscreen-src
    , moonlight-common-c-src
    , moonlight-enet-src
    , moonlight-nanors-src
    , external-libldac-src
    , ...
    }:
    let
      # One system, and it is the one that gets built (#207).
      #
      # This was `nix-systems/default` — x86_64-linux, aarch64-linux, and the two Darwins.
      # Under the current nixpkgs pin **every Darwin check was unbuildable**: the
      # `commonArgs` Darwin branch referenced `darwin.apple_sdk.frameworks.Security`, a
      # removed compatibility stub, so forcing any `checks.aarch64-darwin.*` threw.
      # Nobody saw it because CI evaluates and builds only x86_64-linux.
      #
      # Forcing every attribute on the remaining systems found the same shape one over:
      # `checks.aarch64-linux.cast-app-hosting` cannot evaluate either, because the
      # vendored Electron (`nix/electron-linux.nix`) is `platforms = [ "x86_64-linux" ]`
      # by construction — as are the Widevine CDM and the MSVC sysroot the Windows
      # cross-build needs. So aarch64-linux was exactly as aspirational as Darwin, and the
      # issue's own reasoning applies to it verbatim: the deploy target is Windows
      # (cross-built from here, and that check is in this list), development is x86_64
      # Linux, and a platform nothing builds is not a supported platform — it is an
      # unproven claim in a `flake.nix`, which is the same shape as every other finding in
      # `docs/test-matrix.md`.
      #
      # Adding a system back is a real piece of work — the vendored blobs are the hard
      # part — and doing it means fixing what breaks, which is a truthful claim then
      # rather than a hopeful one now.
      eachSystem = nixpkgs.lib.genAttrs [ "x86_64-linux" ];

      # Helper to get pkgs for a system with rust-overlay applied
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
        config = {
          # Four derivations are whitelisted by name rather than flipping `allowUnfree`
          # wholesale, so anything else unfree still fails the evaluation loudly.
          #
          # - msvc-sysroot: repacks Microsoft's MSVC CRT + Windows SDK, redistributable
          #   for building but not free software.
          # - linux-firmware: carries `unfreeRedistributableFirmware`. Redistribution is
          #   permitted; the vendor licence text just has to travel with the blobs, which
          #   `bluetoothFirmwareFor` copies alongside them. Without this line the failure
          #   surfaces somewhere that looks unrelated (architecture §11.3b).
          # - widevine-cdm / widevine-cdm-windows: Google's content-decryption module, one
          #   per deploy platform. Marked non-redistributable, so they are fetched and used
          #   locally rather than shipped onward — which is what a receiver on a wall does
          #   anyway. Without one every EME-gated stream fails, and fails *quietly*: the
          #   page logs to its own console and the panel simply does not play, which looks
          #   like a network problem. Only the browser packaging touches them, and it
          #   degrades to no-DRM rather than failing if a downstream nixpkgs refuses
          #   them.
          allowUnfreePredicate = pkg:
            let name = nixpkgs.lib.getName pkg; in
            builtins.elem name [
              "msvc-sysroot"
              "linux-firmware"
              "widevine-cdm"
              "widevine-cdm-windows"
              "airserver-cast-carve"
              "libAirReceiver"
              "airreceiver-cast-carve"
              "airport-express-key"
            ]
            # The Android SDK slice `checks.android-bt` boots (#225): the emulator,
            # platform-tools, and the google_apis system image, pinned by hash from
            # Google's own repo XML via `androidenv`. Same precedent as the
            # Widevine/ffmpeg blobs — proprietary, but pinned and reproducible. A
            # prefix rather than a name list because androidenv names every component
            # derivation separately ("android-sdk-emulator", "platform-tools",
            # "system-image-35-google_apis-x86_64", the composed "androidsdk", …) and a
            # list that has to grow with each SDK layout change would fail the check on
            # every androidenv bump for no protective value — everything under these
            # prefixes is the same licence from the same fetcher.
            || builtins.any (p: nixpkgs.lib.hasPrefix p name) [
              "androidsdk"
              "android-sdk-"
              "system-image-"
              "emulator"
              "platform-tools"
              "platforms"
              "build-tools"
              "cmdline-tools"
              "cmake"
              "tools"
              "licenses"
            ];
          # androidenv refuses to evaluate without this; the licence text it stands for
          # is the SDK's, and the acceptance is scoped to this flake's builds.
          android_sdk.accept_license = true;
        };
      };

      # Just the Bluetooth firmware out of linux-firmware, plus the licence text each
      # vendor requires be redistributed with it. Carving out a subset keeps ~1 GB of
      # unrelated blobs out of the build closure, and makes what we ship auditable.
      bluetoothFirmwareFor = system:
        let pkgs = pkgsFor system;
        in pkgs.runCommand "castaway-bluetooth-firmware" { } ''
          mkdir -p $out/intel $out/rtl_bt $out/LICENSES

          # Intel AX200/AX201/AX210 — the dev box's own radio.
          cp ${pkgs.linux-firmware}/lib/firmware/intel/ibt-20-1-3.* $out/intel/
          cp ${pkgs.linux-firmware}/lib/firmware/intel/ibt-0041-0041.* $out/intel/
          # Realtek RTL8761B/BU — the deploy dongle.
          cp ${pkgs.linux-firmware}/lib/firmware/rtl_bt/rtl8761b*.bin $out/rtl_bt/

          # The licences are a *condition* of redistributing the blobs, so a missing one
          # fails the build rather than shipping a binary we are not allowed to hand out.
          # They live in the source tree, not the installed output — nixpkgs' install
          # phase drops them, which is precisely the sort of thing that goes unnoticed
          # until someone asks where the licence is.
          for licence in LICENCE.ibt_firmware LICENCE.rtlwifi_firmware.txt; do
            cp ${pkgs.linux-firmware.src}/LICENSES/"$licence" $out/LICENSES/
          done

          # build.rs skips anything with LICEN in its name, so these travel with the
          # binary's Nix closure without being embedded as firmware images.
          test -n "$(ls -A $out/intel)" || { echo "no intel firmware; layout changed?" >&2; exit 1; }
          test -n "$(ls -A $out/rtl_bt)" || { echo "no realtek firmware; layout changed?" >&2; exit 1; }
        '';

      # Rust toolchain - pinned via rust-toolchain.toml (single source of truth).
      # rust-overlay (locked in flake.lock) supplies the exact build, so this stays
      # reproducible; rustup users outside Nix get the same version from the file.
      rustToolchainFor = system:
        let pkgs = pkgsFor system;
        in pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      # Create crane lib for each system
      cranelibFor = system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
        in
        (crane.mkLib pkgs).overrideToolchain rustToolchain;

      # Common arguments for all crane builds
      commonArgsFor = system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
        in
        castCarveEnvFor system // {
          # `rs-matter`'s build.rs stamps the host wall clock into a constant, which is
          # its seed for "last known good UTC time" on a device with no real-time clock.
          # Left alone that makes every build of this tree a different derivation output.
          # Pinning it to the Matter epoch is safe here because nothing this project does
          # with Matter validates a certificate against a clock — the fabric's own
          # certificates are issued `VALID_FOREVER` (crates/proto-matter/src/fabric.rs).
          RS_MATTER_BUILD_MATTER_SECS = "0";

          # Keep Cargo sources plus non-Rust assets that crates `include_str!`/`include_bytes!`
          # (SCPD/description XML in proto-dlna; fonts, blue-noise dither and the default
          # adblock filter list in pipeline; the AirPort private key in crypto-raop; the Cast
          # signature table, peer certificate template and key in cast-replay). A missing suffix
          # here only shows up as an `include_str!` failure inside the sandbox, since a plain
          # `cargo build` reads the real tree — so add the extension when you add the asset.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              (craneLib.filterCargoSources path type)
              || (pkgs.lib.hasSuffix ".xml" path)
              || (pkgs.lib.hasSuffix ".ttf" path)
              # The CJK fallback subset (#88) is OpenType/CFF, not TrueType.
              || (pkgs.lib.hasSuffix ".otf" path)
              || (pkgs.lib.hasSuffix ".bin" path)
              || (pkgs.lib.hasSuffix ".der" path)
              || (pkgs.lib.hasSuffix ".pem" path)
              || (pkgs.lib.hasSuffix ".png" path)
              || (pkgs.lib.hasSuffix ".svg" path)
              || (pkgs.lib.hasSuffix ".txt" path)
              # The release signing key's public half (crates/update/release-key.pub),
              # which `castaway-update` `include_str!`s. Without this clause the sandbox
              # build fails outright rather than subtly, which is the good failure — but
              # it fails in a crate nobody was editing, so: it is here on purpose.
              || (pkgs.lib.hasSuffix ".pub" path)
              # The Windows resource script and the .exe icon it embeds (app/build.rs).
              || (pkgs.lib.hasSuffix ".rc" path)
              || (pkgs.lib.hasSuffix ".ico" path)
              # The patched third-party crates under `vendor/` (see vendor/README.md).
              # `[patch.crates-io]` points Cargo at them by path, so they are build
              # inputs exactly like a workspace member: without this the sandbox fails
              # at `cargo metadata` with "failed to read vendor/<crate>/Cargo.toml",
              # which is at least loud. By path rather than by suffix because a crate is
              # whatever files it ships — licences, a README, a build script.
              || (pkgs.lib.hasInfix "/vendor/" path)
              # Everything under a fixtures/ directory, wholesale: ground rule 9 lands
              # reverse engineering as checked-in fixtures, and several are files no
              # suffix rule can name — proto-cast's extensionless `expect`/`time`
              # vector files, cast-replay's trimmed .sqlite databases (D44). Enumerating
              # suffixes here made each new fixture kind a sandbox-only test failure.
              || (pkgs.lib.hasInfix "/fixtures/" path)
              # The network-surface artifacts (D45): the app's freshness tests read
              # them at runtime and fail on drift, which is what keeps the firewall
              # JSON in lock-step with the registry — so the sandbox must see them.
              || (pkgs.lib.hasSuffix "docs/network-surface.md" path)
              || (pkgs.lib.hasSuffix "nix/network-surface.json" path)
              # Everything under a `tests/fixtures` directory, whatever its extension.
              # By path rather than by suffix on purpose: the per-extension list above
              # fails *open* — a fixture whose type is not listed simply is not there,
              # and a test that reads it does not fail, it reads nothing. The captured
              # Cast registry responses (`.json`) were exactly that, and the rule that
              # would have caught them is "a fixture is a fixture".
              || (pkgs.lib.hasInfix "/tests/fixtures/" path)
              # The Electron host and its probes. Not Cargo sources, but tests drive them
              # as child processes, and without them those tests skip rather than fail —
              # which is how the receiver-SDK tests came to pass in the sandbox without
              # ever opening a browser (#16).
              || (pkgs.lib.hasInfix "/browser-host/" path)
              # nextest's own configuration, which `filterCargoSources` does not keep.
              # It declares which tests may not share a runner (`.config/nextest.toml`):
              # the mixer's and the output stream's assertions are about *rate*, and a
              # measurement starved of a core reads as a failure. Without it the file is
              # simply absent in the sandbox and every one of them runs against the full
              # parallel load — the same fails-open shape the fixture rule above was
              # written for, found the same way (D55).
              #
              # The directory needs its own clause because `cleanSourceWith` runs the
              # filter over directories too and never descends into a rejected one.
              #
              # Worth knowing if this ever looks like it is not working: in a flake `./.`
              # is the *git* source, so a file that is not tracked is not there at all and
              # no filter clause can reach it. Adding this rule while the file was still
              # untracked produced a byte-identical derivation, which reads exactly like
              # the filter being wrong. `git add` first, then look at the drv hash.
              || (type == "directory" && pkgs.lib.hasSuffix "/.config" path)
              || (pkgs.lib.hasSuffix "/.config/nextest.toml" path);
            name = "source";
          };
          strictDeps = true;

          buildInputs = [
            # Add additional build inputs here
          ];

          nativeBuildInputs = [
            # Add additional native build inputs here
          ];
        };

      # The revision the footer on the idle screen shows. Passed in rather than shelled
      # out for, because the build sandbox has no `.git` and no `git` — see the app's
      # `build.rs`, which falls back to asking git only for a plain `cargo build`.
      #
      # Set on the final package builds only, never in `commonArgs`: everything there is
      # inherited by every `buildDepsOnly`, and an env var that changes at each commit
      # invalidates all of the dependency trees each time — which is exactly the
      # source-insensitivity crane's dummy-src machinery exists to prevent. The checks
      # build with the "unknown" fallback instead; nothing shipped comes out of a check.
      gitRev = self.shortRev or self.dirtyShortRev or "unknown";

      # The build's position in the linear history of `main` — `git rev-list --count
      # HEAD`, which is what `self.revCount` is. Stamped in beside `gitRev` and passed
      # around with it, because it answers the one question the sha cannot: a sha
      # identifies a tree, it does not order two of them, and an updater offered a
      # release has to know whether it is *newer* (#343).
      #
      # `0` where nix has no answer — a dirty tree, or a source tree with no history.
      # A real build number starts at 1, so zero is unambiguous, and the receiver reads
      # it as "this build cannot order itself" and stands the updater down. That lines
      # up exactly with `dirtyShortRev` above: a hand-built receiver mid-bisect is
      # precisely the one that must not be replaced at 4 a.m.
      buildNumber = toString (self.revCount or 0);

      # Where `cast-replay`'s build.rs finds the carved Cast identities and the
      # constants that open them. Both offline identities — SoftMedia's and App
      # Dynamic's — are other companies' Google-issued device credentials, so they are
      # carved from pinned vendor artifacts rather than checked in
      # (nix/airserver-carve.nix, nix/airreceiver-carve.nix).
      #
      # In `commonArgs`, so *every* build gets them: checks included. A build without
      # them compiles and runs, but has no offline Cast identity at all and falls back
      # to a self-generated development credential that real senders refuse — so a
      # check that ran unprovisioned would be silently exercising a different receiver
      # from the one that ships. The cost is that `nix flake check` now depends on the
      # unfree carves; that is the honest trade, because the alternative is a green
      # check for an artifact nobody deploys.
      castCarveEnvFor = system:
        let
          airserver = airserverCarveFor system;
          airreceiver = airreceiverCarveFor system;
        in
        {
          CASTAWAY_AIRSERVER_CARVE = "${airserver}";
          CASTAWAY_AIRSERVER_KEK_PERSON_FILE = "${airserver}/kek_person.bin";
          CASTAWAY_AIRSERVER_KEK_PASS_FILE = "${airserver}/kek_pass.bin";
          CASTAWAY_AIRRECEIVER_CARVE = "${airreceiver}";
          CASTAWAY_AIRPORT_KEY_FILE = "${airportKeyFor system}/airport.pem";
        };

      # Build only dependencies (for caching)
      # The one dependency tree, built at the default feature set — which is now everything
      # (D55), so there is a single tree rather than the five near-identical ones the
      # per-feature checks used to need.
      #
      # Not folded into `commonArgs`: `nix/windows.nix` builds its `crossArgs` by appending
      # to `commonArgs`, so Linux `buildInputs` there would end up as inputs to a
      # cross-compile. `commonArgs` stays the platform-neutral base; this is where the
      # native deps land.
      cargoArtifactsFor = system:
        let craneLib = cranelibFor system;
        in craneLib.buildDepsOnly (fullArgsFor system);

      # `buildDepsOnly`, but extending the base artifacts above rather than starting
      # empty — see nix/deps-only-from.nix. The feature-set trees (kiosk, audio,
      # hwaccel) compile only what their features add on top of the portable tree.
      depsOnlyFromFor = system: import ./nix/deps-only-from.nix {
        craneLib = cranelibFor system;
        lib = nixpkgs.lib;
      };

      # The Widevine CDM, staged into the browser's profile so a panel that has never
      # been online can still play protected video (G46, re-proven under D36/#66).
      #
      # `tryEval` because the CDM is unfree: a nixpkgs without `allowUnfree` still builds
      # a working receiver, it just cannot play protected streams. A hard dependency would
      # make the package unbuildable for anyone who has not accepted Google's terms, over
      # a feature most casts never touch.
      widevineLinuxFor = system:
        let
          pkgs = pkgsFor system;
          attempt = builtins.tryEval (pkgs.widevine-cdm.outPath or null);
        in
        if attempt.success && attempt.value != null then
          "${attempt.value}/share/google/chrome/WidevineCdm"
        else
          "";

      # The browser runtime (D36). Same pinned ECS archive as the Windows artifact stages,
      # patchelf'd for NixOS.
      electronLinuxFor = system: import ./nix/electron-linux.nix {
        pkgs = pkgsFor system;
        src = electron-linux-src;
      };

      # The GameStream client core we link rather than reimplement (D37). Static
      # archives + the public header; `moonlight-sys/build.rs` finds them through
      # `MOONLIGHT_COMMON_C_LIB_DIR`.
      moonlightCommonCFor = system: import ./nix/moonlight-common-c.nix {
        pkgs = pkgsFor system;
        src = moonlight-common-c-src;
        enetSrc = moonlight-enet-src;
        nanorsSrc = moonlight-nanors-src;
      };

      # The AirPort Express key AirPlay 1 signs with, out of shairplay's source rather
      # than this tree. Not a secret in any meaningful sense — it is in every AirPlay
      # implementation there is, nixpkgs included — but there is no reason for us to be
      # one more copy of it. See nix/airport-key.nix.
      airportKeyFor = system:
        let pkgs = pkgsFor system; in
        import ./nix/airport-key.nix {
          inherit (pkgs) lib stdenvNoCC openssl;
          shairplaySrc = pkgs.shairplay.src;
        };

      # SoftMedia's `libAirReceiver.so`, for the CKS identity and backend credentials.
      # A fixed-output derivation rather than a flake input: there is no stable URL for
      # it, so nix/airreceiver-src.nix replays a download chain and the pin is the
      # library's own hash. See that file for why Play and APKPure do not work.
      airreceiverSrcFor = system: import ./nix/airreceiver-src.nix {
        inherit (pkgsFor system) lib stdenvNoCC curl unzip cacert;
      };

      # SoftMedia's CKS identity and backend credentials, out of the library above.
      airreceiverCarveFor = system: import ./nix/airreceiver-carve.nix {
        inherit (pkgsFor system) lib stdenvNoCC python3;
        airreceiverSrc = airreceiverSrcFor system;
      };

      # AirServer's Cast credential database and the two BLAKE2b constants that open it,
      # carved out of the pinned installer so neither lives in this tree. `cast-replay`
      # takes the constants through `CASTAWAY_AIRSERVER_KEK_{PERSON,PASS}_FILE`.
      airserverCarveFor = system: import ./nix/airserver-carve.nix {
        inherit (pkgsFor system) lib stdenvNoCC python3 p7zip;
        airserverMsi = airserver-msi-src;
      };

      # Sony's LDAC library, with the decoder in it — which `pkgs.ldacbt` under this pin
      # does not have. `ldac-sys/build.rs` finds it through `LDACBT_LIB_DIR` (#14).
      ldacbtFor = system: import ./nix/ldacbt.nix {
        pkgs = pkgsFor system;
        src = external-libldac-src;
      };

      # What the default feature set needs to *link*, on Linux (D55).
      #
      # Since every feature is on by default, this is no longer "the extras one check
      # wants" — it is the ordinary build environment, and `test`/`clippy`/`coverage`/
      # `build` all take it. That is the whole point of the inversion: the configuration
      # CI compiles is the configuration that ships, so there is no feature set left for a
      # test to rot behind.
      #
      # Linux only in the sense that the Windows cross-build names its own set in
      # nix/windows.nix. There is no third case: Darwin left `systems` in #207.
      fullArgsFor = system:
        let
          pkgs = pkgsFor system;
          commonArgs = commonArgsFor system;
        in
        # The `isLinux` guards here and below are always true since #207 took Darwin out
        # of `systems`. They stay because each one says *why* its contents are
        # Linux-shaped — nixosTest needs a Linux kernel, the Windows cross-build is
        # cross-built from Linux, pipewire and libldacBT are Linux libraries — and that is
        # a real distinction between the attributes they wrap and the ones they do not.
        commonArgs // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [
            pkgs.pkg-config
            # On `PATH`, not just linked: `strictDeps` means `buildInputs` binaries are not
            # reachable, and the decode tests shell out to make their clips. That gap is
            # what made `checks.audio` skip them and report ok (#182).
            pkgs.ffmpeg_7
          ];
          buildInputs = (commonArgs.buildInputs or [ ]) ++ [
            pkgs.ffmpeg_7
            pkgs.alsa-lib
            pkgs.pipewire
            (ldacbtFor system)
          ];
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
          MOONLIGHT_COMMON_C_LIB_DIR =
            "${moonlightCommonCFor system}/lib:${pkgs.openssl.out}/lib";
          LDACBT_LIB_DIR = "${ldacbtFor system}/lib";
          CASTAWAY_FIRMWARE_DIR = "${bluetoothFirmwareFor system}";
        };

      # The full kiosk build — renderer, browser, audio, Bluetooth. `packages.default` on
      # Linux, so it is what `nix run .` gives you.
      linuxKioskFor = system: import ./nix/linux-kiosk.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        baseCargoArtifacts = cargoArtifactsFor system;
        depsOnlyFrom = depsOnlyFromFor system;
        inherit gitRev buildNumber;
        electron = electronLinuxFor system;
        widevineCdm = widevineLinuxFor system;
        bluetoothFirmware = bluetoothFirmwareFor system;
        moonlightCommonC = moonlightCommonCFor system;
        ldacbt = ldacbtFor system;
      };

      # Linux → Windows cross-build (x86_64-pc-windows-msvc). Only meaningful from
      # Linux; the sysroot derivation is Linux-only.
      windowsFor = system: import ./nix/windows.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        rustToolchain = rustToolchainFor system;
        inherit gitRev buildNumber;
        # For the Authenticode check beside the DLL-closure one (#344). The artifact and
        # the signer have to meet somewhere, and the artifact is the expensive half.
        authenticode = (import ./nix/release-signing.nix { pkgs = pkgsFor system; }).authenticode;
        ffmpegSrc = ffmpeg-windows-src;
        electronSrc = electron-windows-src;
        widevineSrc = widevine-windows-src;
        # Controller firmware travels *inside* the binary on Windows, which has no
        # /lib/firmware. The blobs are platform-independent data, so the Linux-built
        # firmware tree is the right input for a cross build.
        bluetoothFirmware = bluetoothFirmwareFor system;
      };

    in
    {
      # The main package output
      packages = eachSystem (system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
          commonArgs = commonArgsFor system;
          cargoArtifacts = cargoArtifactsFor system;
          releaseSigning = import ./nix/release-signing.nix { inherit pkgs; };
        in
        {
          # No renderer, no browser, nothing platform-specific.
          #
          # This is no longer a *product* (D55) — it is the fixture the VM tests boot, and
          # the fallback `default` on Darwin. The VM tests want it because they assert on
          # the null pipeline's log lines (`null pipeline: PLAY …`), which is how a
          # protocol test says "the event crossed into the media plane" without needing a
          # GPU in a VM. That is a real need and it is why the build survives; it is not a
          # configuration anybody deploys, and `checks.build` no longer pretends otherwise.
          #
          # `--no-default-features` explicitly, because the default is now everything.
          castaway-portable = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--package castaway --no-default-features";
            CASTAWAY_GIT_REV = gitRev;
            CASTAWAY_BUILD = buildNumber;
            # Only run tests during the check phase, not during build
            doCheck = false;
          });

          default = self.packages.${system}.castaway-portable;

          castaway = self.packages.${system}.default;

          # Signing a release, and making the key that signs it (#343). Both halves of
          # one key, so they live in one file: `release-keygen` runs once by hand and
          # writes the public half into the tree; `release-manifest` runs in CI on every
          # push and refuses to produce an unsigned manifest.
          release-manifest = releaseSigning.manifest;
          release-keygen = releaseSigning.keygen;

          # Authenticode + castLabs VMP over the Windows artifact (#344), so the release
          # asset arrives complete and the panel holds no credentials. Without it the
          # first unattended update would replace a VMP-signed tree with an unsigned one
          # and kill DRM playback silently.
          sign-windows = releaseSigning.signWindows;
          windows-authenticode = releaseSigning.authenticode;
          windows-codesign-keygen = releaseSigning.codesignKeygen;

          # The stable binary the panel's scheduled task points at (#342). Exposed here
          # because the `update-vm` check runs it — the whole supervision loop is
          # platform-independent, and only the job object is not.
          launcher = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "castaway-launcher";
            cargoExtraArgs = "--package castaway-launcher --bins";
            doCheck = false;
          });

          # A scripted phone, for the one path no VM test can cover: YouTube's Lounge
          # servers are a third party to the session, so this needs the real internet
          # and a running receiver. `nix run .#yt-selfplay -- http://<receiver>:8080`.
          yt-selfplay = import ./nix/yt-selfplay.nix { inherit pkgs; };

          # The AirServer carve, exposed on its own so a version bump can be verified
          # before anything that consumes it is rebuilt: `nix build .#airserver-carve`
          # prints where the database was found and how the constants were confirmed.
          airserver-carve = airserverCarveFor system;

          # The AirReceiver library the CKS carve reads, exposed so a fetch failure can
          # be diagnosed on its own rather than inside a larger build.
          airreceiver-src = airreceiverSrcFor system;

          # Same idea as `airserver-carve`: buildable on its own so a version bump or
          # a fetch failure is diagnosable without a full rebuild.
          airreceiver-carve = airreceiverCarveFor system;

          airport-key = airportKeyFor system;

          # The linked GameStream core (D37), exposed on its own so it can be built and
          # cached independently — and so a bump can be checked before anything that
          # links it is rebuilt.
          moonlight-common-c = moonlightCommonCFor system;

          # The GameStream prober: discover, pair, list apps, launch. Its real job is
          # the `gamestream-vm` check, which points it at a real Sunshine — but it is
          # also the fastest way to find out why a panel will not pair with a host.
          # `nix run .#gs-probe -- <host> --pin 1234`.
          gs-probe = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "gs-probe";
            # `--no-default-features` since D55: `proto-gamestream`'s default is now
            # `stream`, which links moonlight-common-c and would need
            # `MOONLIGHT_COMMON_C_LIB_DIR` here. This prober only pairs and reads NVHTTP,
            # so it does not want the streaming core — and keeping it out is also what
            # keeps `nix run .#gs-probe` MIT-clean, which is the half of D37's licence
            # separation still worth having now the app itself links it.
            cargoExtraArgs = "-p proto-gamestream --example gs-probe --no-default-features";
            doCheck = false;
            # The example is not installed by crane's default install phase.
            postInstall = ''
              install -Dm755 \
                "$(find target -name gs-probe -type f -perm -u+x | head -1)" \
                "$out/bin/gs-probe"
            '';
          });

          # The stand-in Casting Client (#171): declare over UDC, become a commissionable
          # node with the passcode the panel chose, get commissioned, then cast back over
          # the CASE session that commissioning left open. Its real job is the
          # `matter-vm` check; pointed at a panel by hand with `--matter-port` it also
          # runs beside one on a single host, which is the quickest way to find out why a
          # phone will not pair.
          matter-peer = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "matter-peer";
            cargoExtraArgs = "-p proto-matter --example matter-peer";
            doCheck = false;
            postInstall = ''
              install -Dm755 \
                "$(find target -name matter-peer -type f -perm -u+x | head -1)" \
                "$out/bin/matter-peer"
            '';
          });
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
          let
            windows = windowsFor system;
            windowsDeploy = import ./nix/deploy-windows.nix { inherit pkgs; };
          in
          {
            # On Linux the default is the real receiver: every optional feature, `ldac`
            # included (nix/linux-kiosk.nix keeps the story of why that one was once the
            # exception). `nix run .` should hand you something that can actually display a
            # cast, not a build that discovers, accepts, and then has nowhere to put the
            # picture.
            default = linuxKioskFor system;

            # The browser runtime the port targets (D36). Exposed on its own so it can be
            # run against the probes in `browser-host/` — `nix run .#electron -- \
            # browser-host/codec-probe.js` is how a version bump gets checked before it
            # is trusted.
            electron = electronLinuxFor system;

            # Punch the receiver's whole network surface through this box's firewall, for
            # a native dev run (`cargo run` here, senders on the LAN). Same source of
            # truth as the NixOS module's holes — nix/network-surface.json at its
            # defaults, every gate open — so it cannot drift from the code either.
            # Transient by design: `sudo nixos-firewall-tool reset` (or a rebuild)
            # closes everything again. `nix run .#open-firewall`.
            open-firewall =
              let
                lib = pkgs.lib;
                surface = builtins.fromJSON (builtins.readFile ./nix/network-surface.json);
                portsOf = l:
                  if l.port ? fixed then
                    [ l.port.fixed ]
                  else if l.port ? config then
                    [ l.port.default ]
                  else
                    lib.range l.port.default_first l.port.default_last;
                # One hole per (transport, port), owners folded together: the RAOP and
                # Cast media ranges are the same 32 ports, and opening them twice would
                # double both the sudo calls and the noise.
                holes = lib.attrValues (lib.foldl'
                  (acc: h:
                    let k = "${h.transport}:${toString h.port}"; in
                    acc // {
                      ${k} = {
                        inherit (h) transport port;
                        owners = lib.unique ((acc.${k}.owners or [ ]) ++ [ h.owner ]);
                      };
                    })
                  { }
                  (lib.concatMap
                    (l: map (port: { inherit (l) transport owner; inherit port; })
                      (portsOf l))
                    surface.listeners));
                open = h:
                  "open ${h.transport} ${toString h.port} ${
                    lib.escapeShellArg (lib.concatStringsSep "+" h.owners)}\n";
              in
              pkgs.writeShellApplication {
                name = "castaway-open-firewall";
                text = ''
                  open() {
                    echo "open $1 $2  ($3)"
                    sudo ${pkgs.nixos-firewall-tool}/bin/nixos-firewall-tool open "$1" "$2"
                  }
                  ${lib.concatMapStrings open holes}
                  echo "castaway's surface (${toString (builtins.length holes)} holes) is open" \
                    "until 'sudo nixos-firewall-tool reset' or a rebuild"
                '';
              };

            # The Windows deploy artifact, cross-compiled from Linux. One now rather than
            # four (D55): the other three were subsets of this one — `electron` implies
            # `render` and `hwaccel` — kept around to bisect which layer broke, which is a
            # question the failing build's own log answers.
            castaway-windows-electron = windows.castaway-electron;

            # The MSVC CRT + Windows SDK sysroot they build against. Exposed on its own so
            # it can be built and cached independently of the Rust build.
            msvc-sysroot = windows.sysroot;

            # Getting the artifact onto the physical panel and watching it run:
            # `nix run .#deploy-windows`. Build, wipe, copy, verify the bits actually
            # landed, launch on the *console* session, stream the log back.
            deploy-windows = windowsDeploy.deploy;

            # The one-time move onto the versioned auto-update layout (#346):
            # `nix run .#windows-migrate`. Idempotent, so it is also how a launcher
            # change or a certificate rotation gets onto the box.
            windows-migrate = windowsDeploy.migrate;

            # The same hole-punching as `open-firewall`, from the same
            # nix/network-surface.json, against the Windows box's firewall:
            # `nix run .#windows-firewall` (`-- --close` to take it down again).
            windows-firewall = windowsDeploy.firewall;

            # Hand a USB device to castaway's own stack: `nix run .#windows-winusb`
            # (`-- --undo` gives it back to Windows). Defaults to the Intel radio.
            windows-winusb = windowsDeploy.winusb;
          }
        ));

      # Checks run by `nix flake check`
      checks = eachSystem (system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
          commonArgs = commonArgsFor system;
          cargoArtifacts = cargoArtifactsFor system;
          depsOnlyFrom = depsOnlyFromFor system;

          # The ordinary build environment now that every feature is on (D55): the native
          # deps the default set links.
          fullArgs = fullArgsFor system;

          # The phone, pinned once for both emulator checks (#225). `android-bt` puts it
          # on netsim's Bluetooth phy and `android-cast` on a TAP segment, but it is the
          # same image and the same emulator, and two copies of this expression would be
          # two things to bump and one to forget. `google_apis` rather than
          # `google_apis_playstore`: it ships Play Services — so the Cast framework and
          # GMS `MediaRouter` are real — while staying rootable and needing no store
          # sign-in, and apps arrive by `adb install` of pinned APKs.
          androidComposition = pkgs.androidenv.composeAndroidPackages {
            platformVersions = [ "35" ];
            includeEmulator = true;
            includeSystemImages = true;
            systemImageTypes = [ "google_apis" ];
            abiVersions = [ "x86_64" ];
            includeNDK = false;
          };

          # What the `hwaccel` feature needs on top of a default build: ffmpeg's headers
          # for `ffmpeg-sys-next` (7.x, matching the crate — nixpkgs defaults to 8.x) and
          # libclang, which its bindgen dlopens. `ash` and `wgpu-hal` need nothing at build
          # time; Vulkan is loaded at runtime, which is why this check stops at compiling.
          # The audio path: libav decoders for the A2DP codecs plus a real PCM device.
          # Kept as its own check because it is the one feature whose absence is
          # *silent* — a receiver with no decoder pairs, streams, and plays nothing.
          # `ldac` rides along here rather than getting a check of its own. It is part of
          # the same silent-failure story — a codec we advertise and cannot decode is a
          # session of silence (#14) — and its tests are the only ones that decode LDAC at
          # all, so leaving them out of `nix flake check` would mean the endpoint's
          # correctness rested on somebody remembering to pass a feature flag.
          # `pkgs.ffmpeg_7` is in *both* lists, for the reason spelled out at `mediaPlaneArgs`
          # below: `buildInputs` gets the libraries, `nativeBuildInputs` gets the **binary**
          # on `PATH`. `commonArgs` sets `strictDeps = true`, so without the second entry the
          # decode tests that shell out to make a clip take their skip branch and report
          # `ok` — which is what they did until 2026-08-05 (#182). That silently covered the
          # one feature this check exists to protect against failing silently.
          audioArgs = {
            cargoExtraArgs = "--package castaway --features audio-out,ldac";
            nativeBuildInputs = [ pkgs.pkg-config pkgs.ffmpeg_7 ];
            buildInputs = [ pkgs.ffmpeg_7 pkgs.alsa-lib (ldacbtFor system) ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
            LDACBT_LIB_DIR = "${ldacbtFor system}/lib";
          };

          hwaccelArgs = {
            cargoExtraArgs = "--package castaway --features hwaccel";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.ffmpeg_7 ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
          };

          # The media plane: `castaway` with `render` on, which is the feature the
          # end-to-end DLNA test is `cfg`'d behind (#98).
          #
          # `pkgs.ffmpeg_7` appears in *both* lists on purpose, and that is the whole
          # point of this check. As a `buildInput` it is the libraries `ffmpeg-sys-next`
          # links; as a `nativeBuildInput` it puts the `ffmpeg` **binary** on `PATH`,
          # which is what `dlna_media_plane.rs` shells out to for its clip. Turning the
          # feature on without the second one would compile the test, run it, and have it
          # do nothing — the same silent pass this check exists to end.
          mediaPlaneArgs = {
            cargoExtraArgs = "--package castaway --features render";
            nativeBuildInputs = [ pkgs.pkg-config pkgs.ffmpeg_7 ];
            buildInputs = [ pkgs.ffmpeg_7 ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
          };

          # A GPU in the sandbox, in software — the other half of #98.
          #
          # Everything that composites is written against an offscreen wgpu device, so what
          # stood between CI and a composited pixel was never a window: it was an adapter.
          # `nix build` has no render node, so `WgpuCompositor::new_offscreen` fails there
          # and two dozen tests take their skip branch and report `ok` having drawn
          # nothing. Mesa's lavapipe is a Vulkan ICD implemented on the CPU — no hardware,
          # no display, no `/dev/dri` — which is exactly the adapter those tests were
          # missing, and it is why this closes the entry without the Xvfb-plus-VM lift the
          # issue anticipated. Xvfb would only be needed for the *kiosk* window, and no
          # test asks for one.
          #
          # `VK_DRIVER_FILES` names the one ICD rather than pointing at Mesa's whole
          # `icd.d`: the directory also holds the hardware drivers, and on a machine that
          # has one, a check meant to prove the software path would quietly run on the GPU.
          # `LD_LIBRARY_PATH` is for the loader itself, which wgpu `dlopen`s by soname.
          # `WGPU_BACKEND` is belt and braces here — the sandbox has no GL to fall back to
          # — but it is what makes the same three variables reproduce the check on a
          # developer's box, where `Backends::all()` will otherwise pick the real GPU's GL
          # and fail on a downlevel flag rather than running what CI ran.
          #
          # `CASTAWAY_REQUIRE_GPU` is what keeps this honest. With it set, a test that
          # cannot open an adapter fails instead of skipping (`pipeline::test_gpu`), so if
          # a mesa bump moves that ICD path the check goes red — rather than reverting,
          # silently, to the green-and-empty state this exists to end.
          lavapipe = {
            LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib";
            WGPU_BACKEND = "vulkan";
            VK_DRIVER_FILES =
              "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
            CASTAWAY_REQUIRE_GPU = "1";
          };
        in
        {
          # Build the artifact that ships (D55).
          #
          # This used to be `castaway-portable`, on the reasoning that pulling Electron and
          # ffmpeg into `nix flake check` cost more than it proved. What it actually bought
          # was a gate on a build nobody deploys, while the one that does deploy set
          # `doCheck = false` and was not a check at all.
          build = self.packages.${system}.default;

          # Clippy over every target, at the default feature set — which is now everything,
          # so `media-plane-clippy` and `hwaccel-clippy` are subsumed and gone.
          clippy = craneLib.cargoClippy (fullArgs // {
            inherit cargoArtifacts;
            # `cargoExtraArgs` rides along from `fullArgs` (crane passes both), so this only
            # adds what clippy itself needs.
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          # Check formatting
          fmt = craneLib.cargoFmt {
            src = commonArgs.src;
          };

          # The whole auto-update loop, in a VM: a real launcher, two real receivers a
          # build number apart, a signed release, and a panel that ends up running newer
          # bits than it started with — with no Windows and no network (#345).
          update-vm = import ./nix/update-vm-test.nix {
            inherit pkgs;
            castaway = self.packages.${system}.castaway-portable;
            launcher = self.packages.${system}.launcher;
            releaseManifest = self.packages.${system}.release-manifest;
          };

          # The release manifest CI writes is the one the receiver reads (#343).
          #
          # Two halves that could drift apart without either side noticing: a shell script
          # in `nix/release-signing.nix` emits `manifest.json`, and `castaway-update`
          # parses it — with `deny_unknown_fields` and a typed schema, so a renamed key is
          # a refused release at 4 a.m. rather than a failing test. This check closes that
          # by regenerating the crate's fixtures with the real script and the checked-in
          # test key, and demanding they come back byte-identical.
          #
          # Byte-identical is available because Ed25519 signing is deterministic (RFC
          # 8032) and minisign does not salt: the same key over the same bytes always
          # produces the same signature. So this also pins the *signature format* — a
          # minisign that switched back from `ED` to `Ed`, or changed its base64 layout,
          # fails here rather than on the panel.
          #
          # If it fails because the format changed on purpose: run the same command the
          # check runs and copy the three outputs over `crates/update/fixtures/`.
          release-manifest = pkgs.runCommand "release-manifest-fixtures"
            {
              nativeBuildInputs = [ self.packages.${system}.release-manifest pkgs.diffutils ];
              meta.description = "The manifest release.yml writes round-trips through castaway-update's fixtures";
            } ''
            fixtures=${./crates/update/fixtures}
            export CASTAWAY_RELEASE_SECRET_KEY="$(cat "$fixtures/test-release.key")"
            # Point the script's own verification at the fixture's public half, so the
            # "does this signature check out against what the receiver trusts" branch is
            # exercised rather than skipped. Without it the check would silently take the
            # no-key path and assert nothing about the half that matters.
            export CASTAWAY_RELEASE_PUBKEY="$fixtures/test-release.pub"
            castaway-release-manifest \
              "$fixtures/castaway-windows-electron-ae2f19e.zip" \
              ae2f19ef1f9d9a2488008f1075b252178ae7ef85 935 out

            for f in manifest.json manifest.json.minisig; do
              diff -u "$fixtures/$f" "out/$f" || {
                echo "crates/update/fixtures/$f is not what the release script writes." >&2
                exit 1
              }
            done
            echo "the release script and crates/update agree byte for byte" > "$out"
          '';

          # Every test in the workspace, with every feature on, in a sandbox that can
          # actually run them: lavapipe supplies an adapter and ffmpeg is on `PATH`, and
          # both tripwires are set so a test that cannot get either fails rather than
          # skipping. This one check replaces `audio`, `render-pixels`, `media-plane` and
          # their two clippy siblings — they existed to reach feature sets the default
          # build could not, and there is no such set any more (D55).
          test = craneLib.cargoNextest (fullArgs // lavapipe // {
            inherit cargoArtifacts;
            CASTAWAY_REQUIRE_FFMPEG = "1";
            partitions = 1;
            partitionType = "count";
            # Report every failure rather than stopping at the second. The default costs
            # nothing when the check is green and hides siblings when it is not — and this
            # tree has at least two independent *intermittent* failure sources whose
            # treatments are opposite (#208): one is a starved measurement, fixed by runner
            # exclusivity, and one is a product defect that exclusivity would hide. Telling
            # them apart needs a run that names all of them, and a failing run that stopped
            # early was the missing evidence every time.
            cargoNextestExtraArgs = "--no-fail-fast";
          });

          # Run tests with coverage
          coverage = craneLib.cargoLlvmCov (fullArgs // lavapipe // {
            inherit cargoArtifacts;
            CASTAWAY_REQUIRE_FFMPEG = "1";
          });

          # Prove the checked-in Cast RTP fixtures still match what openscreen's own
          # packetizer emits. `tests/openscreen_stream.rs` tests our receiver against
          # those bytes; this is what keeps the bytes honest.
          openscreen-rtp-fixtures = import ./nix/openscreen-fixtures.nix {
            inherit pkgs;
            openscreenSrc = openscreen-src;
          };

          # The same idea pointed the other way: compile openscreen's *sender-side*
          # device-auth verifier and let it judge the auth responses this receiver
          # produces. It is what turns "an official sender would reject us, and here is
          # the line of C++ that says so" into an executed result — including which of
          # the sender's many checks we already pass, so a provisioned credential has
          # exactly one case left to flip.
          openscreen-device-auth = import ./nix/openscreen-device-auth.nix {
            inherit pkgs;
            openscreenSrc = openscreen-src;
          };
        }
        # Tier-2: whole adapters driven by scripted senders from a second VM over a real
        # LAN (ground rule 6). nixosTest needs KVM and a NixOS guest.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # The GameStream client against a real Sunshine — the only test here that
          # runs the *reference implementation* as the peer rather than a script of
          # ours (D37). Pairing is hands-free because `sunshine -0` takes the PIN on
          # stdin instead of its web UI.
          #
          # It was defined in the all-systems block above, between the comment for
          # `openscreen-device-auth` and the attribute that comment describes, so
          # `checks.aarch64-darwin.gamestream-vm` would have tried a nixosTest on Darwin.
          # Latent, because CI only ever built x86_64-linux — and moot since #207, but a
          # nixosTest belongs in the nixosTest block regardless.
          gamestream-vm = import ./nix/gamestream-vm-test.nix {
            inherit pkgs self;
          };

          integration-vm = import ./nix/vm-test.nix { inherit pkgs self; };

          # DIAL's positive discovery path (#202): the full kiosk — the one build that
          # advertises DIAL at all (D27/D55) — headless in a VM under Xvfb + lavapipe,
          # answering a targeted M-SEARCH from a second host, serving Application-URL,
          # and keeping its root USN distinct from the DLNA renderer beside it.
          # `integration-vm` holds the complementary property: the browser-less build
          # advertises no DIAL at all.
          dial-vm = import ./nix/dial-vm-test.nix { inherit pkgs self; };

          # The Miracast radio path end to end on mac80211_hwsim: real mac80211 radios,
          # real P2P group formation and WPS, DHCP across the group, and the sink
          # dialling out over it — the whole surface #45 said only hardware could touch,
          # minus the driver's own quirks (§7.6), which remain the hardware's to prove.
          miracast-vm = import ./nix/miracast-vm-test.nix { inherit pkgs self; };

          # FCast end to end between two hosts (#241), driven by the *real*
          # transmitters: the reference terminal sender (the fcast-sender-sdk stack
          # Grayjay embeds, Nix-pinned at the commit the wire fixtures were captured
          # from) and nixpkgs' 2024 pre-SDK client for the implicit-v1 path. Discovery
          # through the SDK's own mDNS browse, then URL casts, transport verbs,
          # playlists, and the version downgrade — none of it scripted by us.
          fcast-vm = import ./nix/fcast-vm-test.nix { inherit pkgs self; };

          # FCast protocol v4 (#248): the same real transmitters against a
          # receiver announcing v4 — the SDK pins the fp from mDNS and runs a
          # genuine TLS 1.3 session — then FUTO's own conformance driver sweeps
          # the 70-case green manifest, so a regression in any one case fails CI.
          fcast-v4-vm = import ./nix/fcast-v4-vm-test.nix { inherit pkgs self; };

          # Matter commissioning end to end between two hosts (#171). The half of
          # `proto-matter` a socket test cannot reach: the `_matterc._udp` browse, PASE,
          # AddNOC, CASE, and the client invoking `LaunchURL` back over the session
          # commissioning left open. Two nodes specifically because `rs-matter`'s own
          # commissioning test skips mDNS, so discovery has coverage nowhere else.
          matter-vm = import ./nix/matter-vm-test.nix { inherit pkgs self; };

          # A2DP up to a started stream, with no radio: btvirt's linked virtual
          # controllers, BlueZ on one, our receiver on the other. BlueZ browses our SDP
          # records, pairs over SSP, reads our endpoints and their codecs, and configures,
          # opens and starts a stream — all with its own tools. It then plays a real
          # waveform through bluetoothd's A2DP source, and the PCM we recorded is
          # correlated against what was sent (#186). Then, a layer down, BlueZ's `l2test`
          # drives our Enhanced Retransmission engine — segmentation, a rejected gap, a
          # bad checksum and a starved window (#210). See the note at the top of
          # nix/bluetooth-vm-test.nix and `docs/test-matrix.md` §4.3.
          bluetooth-vm = import ./nix/bluetooth-vm-test.nix {
            inherit pkgs;
            castaway = craneLib.buildPackage (fullArgs // {
              inherit cargoArtifacts;
              pname = "castaway-bluetooth";
              # Lean plus the two things this test needs. Without
              # `--no-default-features` D55's default would pull the whole kiosk — wgpu,
              # Electron, a browser — into a headless VM.
              #
              # `audio` is not optional here, and leaving it out is what kept this test at
              # discovery: the endpoints a sink advertises are *derived* from what the
              # build can decode (`app::bluetooth::decodable`), so a build without
              # decoders answers AVDTP DISCOVER with an empty list. Measured, in the trace
              # this test now takes: a two-byte `Discover Response Accept` and BlueZ
              # disconnecting immediately, having been told this device has no endpoints.
              # That is `fullArgs` rather than `commonArgs`, for the ffmpeg the decoders
              # link against.
              cargoExtraArgs =
                "--package castaway --no-default-features --features bluetooth-socket,audio";
              doCheck = false;
            });
            # The ERTM differential (#210). Not the receiver: it claims the same
            # controller and answers on a PSM of its own, because what is under test is
            # `substrate-l2cap`'s retransmission engine against the Linux kernel's, and
            # putting that behind AVDTP would only add a profile's opinions to it.
            #
            # `commonArgs` rather than `fullArgs`: nothing in `proto-bluetooth-audio`
            # links ffmpeg, and this example reaches no further than the HCI socket.
            ertmEcho = craneLib.buildPackage (commonArgs // {
              inherit cargoArtifacts;
              pname = "ertm-echo";
              cargoExtraArgs = "-p proto-bluetooth-audio --example ertm_echo";
              doCheck = false;
              postInstall = ''
                install -Dm755 \
                  "$(find target -name ertm_echo -type f -perm -u+x | head -1)" \
                  "$out/bin/ertm-echo"
              '';
            });
          };

          # A real phone stack as the sender (#225): the Android emulator, headless under
          # KVM, its virtual controller on netsim/rootcanal — and castaway on the same
          # phy over the H4-over-TCP transport. Android's Settings UI pairs with us (a
          # consent dialog tapped by uiautomator), the phone registers for volume
          # changes and hears its INTERIM (#211, as CI), negotiates aptX HD — the codec
          # no BlueZ path reaches — and VLC (pinned APK, #87's sender) plays a waveform
          # that is correlated per channel out of the mixer's recording. The netsim
          # pcaps land in the check's output: real Android A2DP/AVRCP transcripts,
          # regenerable on demand (rule 9). Not a nixosTest — the emulator inside a
          # NixOS guest would be nested KVM; see the note at the top of the file.
          android-bt = import ./nix/android-bt-test.nix {
            inherit pkgs;
            castaway = craneLib.buildPackage (fullArgs // {
              inherit cargoArtifacts;
              pname = "castaway-android-bt";
              # `audio` alone: decoders (so the endpoint table is real — see
              # bluetooth-vm's note) over the *null* output. Adding `audio-out` here
              # selects cpal, whose blocking writes backpressure the decode loop into
              # dropping media packets when the build sandbox has no sound server —
              # measured as 960 aptX HD sync errors and a silent recording. The null
              # sink accepts instantly and the recording taps the mix upstream of it.
              cargoExtraArgs = "--package castaway --no-default-features --features audio";
              doCheck = false;
            });
            inherit androidComposition;
            # The sender app, pinned the way rule 9 pins fixtures: F-Droid's own build
            # of VLC, fetched by hash. GPL, and a test-time dependency only.
            vlcApk = pkgs.fetchurl {
              url = "https://f-droid.org/repo/org.videolan.vlc_13070108.apk";
              sha256 = "06di28b5v5bpagdk05pbxyrbfij8s8pqknz9q32nqq6qvzx494aa";
            };
          };

          # The same phone, on a network instead of a radio (#225's second slice). The
          # emulator's NIC is a TAP that castaway also binds, so mDNS and CASTv2 cross a
          # real segment, and the sender is Android's own system Cast picker — Play
          # Services, a different implementation from the openscreen lineage every other
          # Cast check here uses. That difference is the whole value: it is the surface
          # #226's three defects were invisible on, and it is what proves device auth
          # survives a real sender (#40) and that a mirroring session actually completes.
          # The segment capture lands in the check's output, the network leg's half of
          # the fixture factory (rule 9). Needs `/dev/net/tun` in the sandbox; the file's
          # header says why and the check says so itself if it is missing.
          android-cast = import ./nix/android-cast-test.nix {
            inherit pkgs androidComposition;
            castaway = craneLib.buildPackage (fullArgs // {
              inherit cargoArtifacts;
              pname = "castaway-android-cast";
              # Nothing but the protocol: this check terminates a mirroring session and
              # counts what arrived, and the null pipeline receives RTP exactly as the
              # real one does. Pulling in `render` would put wgpu and a compositor in a
              # headless sandbox to decode frames nobody looks at.
              cargoExtraArgs = "--package castaway --no-default-features";
              doCheck = false;
            });
          };

          # The mixer against a sound card whose clock is not ours (#204). See the note at
          # the top of nix/mixer-vm-test.nix for why a dummy card is enough, and
          # `docs/test-matrix.md` §4.10.
          mixer-vm = import ./nix/mixer-vm-test.nix {
            inherit pkgs;
            mixerTest = craneLib.buildPackage (fullArgs // {
              inherit cargoArtifacts;
              pname = "mixer-real-device";
              # The test binary, not a package: `--test <name>` selects it the way
              # `--example` selects `ertm-echo` above, and the same `find` installs it,
              # because crane installs what cargo calls a binary and this is not one.
              #
              # `--no-default-features` and the two features the file's own `cfg` names.
              # D55's default would pull wgpu, Electron and a browser into a headless VM
              # to run two audio tests.
              cargoExtraArgs =
                "-p pipeline --test mixer_real_device "
                + "--no-default-features --features audio,audio-pipewire";
              doCheck = false;
              postInstall = ''
                install -Dm755 \
                  "$(find target -name 'mixer_real_device-*' -type f -perm -u+x | head -1)" \
                  "$out/bin/mixer-real-device"
              '';
            });
          };

          # The bindings for a library that ships no headers, regenerated and diffed.
          # Nothing at build time can catch a wrong FFI signature here, so this is the only
          # thing standing between a nixpkgs bump and a decoder that reads noise (#14).
          ldac-bindings = import ./nix/ldac-bindings.nix {
            inherit pkgs;
            rustToolchain = rustToolchainFor system;
            ldacbt = ldacbtFor system;
          };

          # The same guard for the other linked C library, and against a different
          # failure. moonlight-common-c *does* ship a header, and the generated file
          # carries bindgen's layout assertions — but bindgen wrote both the struct and
          # the assertion from the same header, so they are self-referential and cannot
          # see upstream drift. A revision bump that moved a field inside
          # `STREAM_CONFIGURATION` would compile, link, and hand a live session its
          # parameters from the wrong offsets (#191).
          moonlight-bindings = import ./nix/moonlight-bindings.nix {
            inherit pkgs;
            rustToolchain = rustToolchainFor system;
            moonlightCommonC = moonlightCommonCFor system;
          };




          # The Cast app-hosting path against Google's own receiver SDK, and a hosted
          # application playing real media (#16).
          #
          # A separate check because it needs a browser, which the ordinary `test`
          # derivation has no reason to carry. Without it those two test binaries skip —
          # audibly, on stderr, but they skip — and the only thing in the tree that can
          # say whether the platform protocol is actually right would never run in CI.
          #
          # The SDK bundles are fetched by hash, never at test time, so this measures our
          # implementation against a fixed oracle rather than against Google's uptime.
          cast-app-hosting = craneLib.cargoNextest (commonArgs // {
            cargoArtifacts = depsOnlyFrom cargoArtifacts (commonArgs // {
              pname = "castaway-cast-app-hosting";
              cargoExtraArgs = "--package proto-cast";
            });
            cargoExtraArgs = "--package proto-cast";
            cargoNextestExtraArgs = "-E 'binary(receiver_sdk) + binary(hosted_app_media)'";
            nativeBuildInputs = (commonArgs.nativeBuildInputs or [ ]) ++ [ pkgs.ffmpeg ];
            CASTAWAY_ELECTRON = "${electronLinuxFor system}/bin/electron";
            CASTAWAY_CAST_RECEIVER_SDK = import ./nix/cast-receiver-sdk.nix { inherit pkgs; };
            # Electron rasterises offscreen; the same software GPU the pixel tests use.
            inherit (lavapipe) LD_LIBRARY_PATH VK_DRIVER_FILES;
            # Chromium wants somewhere to put its user-data directory and its runtime
            # sockets. With neither it exits before `app.whenReady()` and prints nothing
            # at all, which reads as "the probe produced no report".
            preCheck = ''
              export HOME="$TMPDIR/home"
              export XDG_RUNTIME_DIR="$TMPDIR/xdg"
              export XDG_CACHE_HOME="$TMPDIR/cache"
              export XDG_CONFIG_HOME="$TMPDIR/config"
              mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME"
            '';
            partitions = 1;
            partitionType = "count";
          });

        }
        # Cross-build the Windows artifacts and verify each one's DLL closure. The Windows
        # binaries can't be executed on the builder, so a static check of what the loader
        # will look for is the closest thing to a smoke test we get without the hardware.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (windowsFor system).checks);

      # Development shell
      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
          # The same ECS distribution the Linux kiosk package stages; a devShell that
          # disagreed with the package would be a trap.
          electron = electronLinuxFor system;
          castReceiverSdk = import ./nix/cast-receiver-sdk.nix { inherit pkgs; };
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];

            nativeBuildInputs = [
              # Rust toolchain (includes rust-analyzer, rustfmt, clippy)
              rustToolchain

              # Fast test runner
              pkgs.cargo-nextest

              # Code coverage
              pkgs.cargo-llvm-cov

              # Watch mode for rapid development
              pkgs.bacon

              # Dependency management
              pkgs.cargo-edit

              # Security auditing
              pkgs.cargo-audit

              # Macro expansion (debugging)
              pkgs.cargo-expand

              # Native-dep build tooling for the render/decode features:
              # pkg-config + ffmpeg for `ffmpeg-sys-next`; the render stack links
              # against Vulkan/Wayland/X11 at runtime.
              pkgs.pkg-config

              # The scripted phone, on PATH: `yt-selfplay http://<receiver>:8080` while a
              # `--features electron` build runs, to check a YouTube cast really plays.
              self.packages.${system}.yt-selfplay

              # nix-direnv for this flake's shell
              nix-direnv.packages.${system}.default
            ];

            buildInputs = [
              # ffmpeg dev libs for the `ffmpeg` pipeline feature. Pin to 7.x to match
              # `ffmpeg-next`/`ffmpeg-sys-next` 7.1 (nixpkgs default is 8.x).
              pkgs.ffmpeg_7
              # ALSA dev libs for `cpal`, the PCM output behind the `audio-out` feature.
              # Linux-only: the Windows build reaches WASAPI through the OS.
              pkgs.alsa-lib
              # Sony's LDAC library, for the one A2DP codec ffmpeg cannot decode (#14).
              # Ours rather than `pkgs.ldacbt`, which under this nixpkgs pin is built
              # encoder-only — see nix/ldacbt.nix.
              (ldacbtFor system)
              # libcrypto, for the GameStream core's AES-GCM/CBC (D37).
              pkgs.openssl
              # Runtime libs for the `render`/`kiosk` pipeline features (wgpu + winit).
              pkgs.vulkan-loader
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxi
              pkgs.libxrandr
            ];

            # Where `hci-transport`'s build.rs finds controller firmware to embed.
            # Windows has no /lib/firmware, so blobs travel inside the binary.
            CASTAWAY_FIRMWARE_DIR = "${bluetoothFirmwareFor system}";

            # Where `moonlight-sys`'s build.rs finds the linked GameStream core (D37).
            # Two entries: the library's own archives, and OpenSSL, which its
            # PlatformCrypto.c needs and which nothing else in the link line provides.
            MOONLIGHT_COMMON_C_LIB_DIR =
              "${moonlightCommonCFor system}/lib:${pkgs.openssl.out}/lib";

            # Where `ldac-sys`'s build.rs finds `libldacBT` (#14). Set even though the
            # library is in `buildInputs` and the ld wrapper would find it anyway: the
            # build script emits no link directive at all without this, so that a build
            # without the `ldac` feature does not depend on the library being present.
            LDACBT_LIB_DIR = "${ldacbtFor system}/lib";

            # Environment variables for development
            RUST_BACKTRACE = "1";
            RUST_LOG = "debug";
            # `ffmpeg-sys-next` generates bindings with bindgen, which dlopens libclang
            # and needs the libc headers pointed out explicitly in a Nix env.
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
            # Point the receiver at the ECS runtime and our Electron host app.
            CASTAWAY_ELECTRON = "${electron}/bin/electron";
            CASTAWAY_BROWSER_APP = toString ./browser-host;
            # The pinned receiver SDKs the platform tests run against (#16). Set here
            # rather than fetched by the test, so the test never touches the network and
            # a bundle that moved fails a hash instead of changing a result.
            CASTAWAY_CAST_RECEIVER_SDK = castReceiverSdk;
            # Let winit/wgpu dlopen Vulkan/Wayland/X11.
            # libGL is needed because Electron's bundled libGLESv2.so links libGL.so.1;
            # without it the browser's GPU process dies and wgpu's GL-backend probe SIGSEGVs.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.vulkan-loader
              pkgs.libGL
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxi
              pkgs.libxrandr
            ];
          };
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # `nix develop .#windows` — cross shell where plain `cargo build` targets
          # Windows. Deliberately separate from the default shell: it exports
          # CARGO_BUILD_TARGET, which would silently hijack the native dev loop.
          windows = (windowsFor system).devShell;
        });

      # Expose nix-direnv for .envrc to use
      lib = {
        inherit nix-direnv;
      };

      # NixOS module: `services.castaway.enable = true` runs the receiver and opens the
      # LAN-discovery and HTTP ports it needs. This is also what the integration VMs
      # boot, so the deploy path and the tested path are the same path.
      nixosModules = rec {
        castaway = { config, lib, pkgs, ... }:
          let
            cfg = config.services.castaway;
            settingsFormat = pkgs.formats.toml { };
            configFile = settingsFormat.generate "castaway.toml" cfg.settings;

            # Miracast is the one protocol that needs a radio rather than a socket, and
            # the radio side is a *deployment*, not code (#45): a
            # wpa_supplicant castaway can command, and a DHCP server on the group
            # interface. Both are derived from the same settings the binary reads, so
            # the daemon castaway talks to and the daemon the module runs cannot drift.
            # Whether the selected package carries the Electron browser (#246). The
            # kiosk build declares it via `passthru.castawayBrowser`; the hardening
            # below narrows on it, because three of its switches kill Chromium
            # outright and a stock `enable = true` on Linux runs the kiosk.
            browserPackage = cfg.package.passthru.castawayBrowser or false;

            miracastEnabled = cfg.settings.enable.miracast or false;
            miracastInterface = cfg.settings.miracast.interface or "wlan0";
            # Our side of the group subnet, in the `address/prefix` form both networkd's
            # Address= and the binary's `[miracast] group_cidr` take — the same value
            # feeds both, so the DHCP pool the module serves and the range the backend
            # sweeps for peers cannot drift apart.
            miracastGroupCidr = cfg.settings.miracast.group_cidr or "192.168.77.1/24";

            miracastWpaConf = pkgs.writeText "castaway-wpa_supplicant.conf" ''
              # castaway owns this wpa_supplicant instance (miracast-protocol-notes §7.6:
              # NetworkManager structurally cannot host a Miracast sink). The control
              # socket is the API: castaway sets the WFD IE, brings up the autonomous
              # group and authorises WPS over it.
              ctrl_interface=DIR=/run/wpa_supplicant GROUP=castaway-p2p
              update_config=0
              # Placeholders; castaway SETs the advertised name and type at bring-up.
              device_name=castaway
              device_type=7-0050F204-1
            '';

            # The firewall's source of truth: generated by the network-surface registry
            # (crates/app/src/surface.rs, docs/network-surface.md), and `nix flake
            # check` runs the test that fails whenever it drifts from the code. This
            # module never names a port directly — a listener added to the registry
            # opens itself on deploy, and one added to the code without a registry
            # entry fails clippy first.
            networkSurface = builtins.fromJSON (builtins.readFile ./nix/network-surface.json);

            # An [enable] flag's value for firewall purposes. The binary defaults every
            # flag to true (an adapter missing its hardware logs and skips), so unset
            # means the port is live and must be open — the old `or false` here left a
            # stock deploy running Cast on 8009 with the firewall closed. The one
            # deliberate exception is miracast: this module only stands up its radio
            # units on an explicit opt-in, so its ports follow the same switch.
            enableFlag = {
              dlna = cfg.settings.enable.dlna or true;
              spotify = cfg.settings.enable.spotify or true;
              dial = cfg.settings.enable.dial or true;
              cast = cfg.settings.enable.cast or true;
              airplay = cfg.settings.enable.airplay or true;
              bluetooth = cfg.settings.enable.bluetooth or true;
              gamestream = cfg.settings.enable.gamestream or true;
              matter = cfg.settings.enable.matter or true;
              fcast = cfg.settings.enable.fcast or true;
              miracast = miracastEnabled;
              # Not under `[enable]`: the remote-control UI is the panel's own surface
              # served back out rather than a protocol anything casts to, so its switch
              # lives in its own section and the registry names the dotted path (#18).
              "remote.enable" = cfg.settings.remote.enable or true;
            };

            # Strict lookup on purpose: a gate flag the registry names and this set
            # does not know must fail evaluation, not silently stay closed.
            gateOpen = gate: gate == [ ] || lib.any (flag: enableFlag.${flag}) gate;

            # A registry entry's concrete ports under this configuration: fixed,
            # config-resolved single, or config-resolved inclusive range.
            listenerPorts = l:
              if l.port ? fixed then
                [ l.port.fixed ]
              else if l.port ? config then
                [ (lib.attrByPath l.port.config l.port.default cfg.settings) ]
              else
                lib.range
                  (lib.attrByPath (l.port.range_config ++ [ "first" ]) l.port.default_first
                    cfg.settings)
                  (lib.attrByPath (l.port.range_config ++ [ "last" ]) l.port.default_last
                    cfg.settings);

            surfacePortsFor = transport:
              lib.unique (lib.concatMap listenerPorts
                (lib.filter (l: l.transport == transport && gateOpen l.gate)
                  networkSurface.listeners));
          in
          {
            options.services.castaway = {
              enable = lib.mkEnableOption "the castaway universal cast receiver";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
                defaultText = lib.literalExpression "castaway.packages.\${system}.default";
                description = ''
                  The castaway package to run.

                  The default on Linux is the full kiosk: render pipeline, Electron browser,
                  audio output and Bluetooth. Every optional feature is on, `ldac`
                  included (the codec stays opt-in at runtime — see nix/linux-kiosk.nix).

                  For a headless box — one proving the protocol stack, or serving DLNA
                  with no display attached — use
                  `castaway.packages.''${system}.castaway-portable`, which has neither a
                  renderer nor a browser. It does not advertise DIAL at all: YouTube
                  casting is a web page, so a build with nowhere to put one declines to
                  offer it rather than accepting casts it can never play.

                  The unit's sandbox hardening follows the package: one carrying the
                  Electron browser (declared via `passthru.castawayBrowser`, which the
                  kiosk build sets) gets the named allowances Chromium's own sandbox
                  needs — user namespaces, `pkey_*`/`chroot`/`@sandbox` syscalls, a
                  real /tmp for the X socket (#246). A browser-less package keeps the
                  full hardening set.
                '';
              };

              httpPort = lib.mkOption {
                type = lib.types.port;
                default = 8080;
                description = ''
                  TCP port of castaway's shared HTTP host (DLNA description/SOAP,
                  Spotify onboarding, DIAL REST). Written into the generated config as
                  `http_port`, so the firewall hole and the listener can't drift apart.
                '';
              };

              settings = lib.mkOption {
                type = settingsFormat.type;
                default = { };
                example = lib.literalExpression ''
                  {
                    friendly_name = "hackerspace screen";
                    interface = "10.0.0.20";
                    enable.spotify = false;
                  }
                '';
                description = ''
                  Contents of `castaway.toml`, as a Nix attrset. See `crates/app/src/config.rs`
                  for the full schema; unset keys take the binary's own defaults.

                  This generates a file in the Nix store, which is world-readable. Secrets
                  belong outside it — `cast.credential` takes *paths* for exactly this
                  reason, so point them at files placed on the box (a Cast device
                  credential identifies one specific piece of hardware) rather than
                  inlining anything here:

                  ```nix
                  services.castaway.settings.cast.credential = {
                    key_file = "/var/lib/castaway/cast-device.pem";
                    certificate_file = "/var/lib/castaway/cast-device.der";
                  };
                  ```
                '';
              };

              logLevel = lib.mkOption {
                type = lib.types.str;
                default = "info";
                example = "info,castaway=debug";
                description = ''
                  `RUST_LOG` filter for the service — the *console* (journald) stream only.
                  The rotated files under /var/lib/castaway/logs keep their own filter, so
                  turning this up to `debug` to chase something does not also fill the
                  panel's disk. Set `settings.log.file_level` to move that one, and
                  `settings.log.to_file = false` to keep journald as the only sink.
                '';
              };

              openFirewall = lib.mkOption {
                type = lib.types.bool;
                default = true;
                description = ''
                  Open every port the network-surface registry declares for this
                  configuration (see docs/network-surface.md; the rules are derived
                  from nix/network-surface.json, which `nix flake check` keeps in
                  lock-step with the code). Discovery fails silently without these,
                  which is the failure mode this exists to prevent — turn it off only
                  if something else manages the rules, and derive that something from
                  `castaway --network-surface` rather than a hand-kept list.
                '';
              };
            };

            config = lib.mkIf cfg.enable {
              # One source of truth for the port: the option feeds the config file.
              services.castaway.settings.http_port = lib.mkDefault cfg.httpPort;

              # castaway runs its own mDNS responder on 5353 (#43). Both
              # can bind with SO_REUSEPORT, so this is a warning rather than an
              # assertion — but which one answers a given query becomes a race.
              warnings = lib.optional config.services.avahi.enable ''
                services.castaway: avahi is also enabled and will contend for UDP 5353
                with castaway's own mDNS responder. Disable services.avahi on the
                receiver so Cast/AirPlay/Spotify advertisements are answered by castaway.
              '';

              # castaway itself runs unprivileged; membership in this group is what lets
              # it reach the wpa_supplicant control sockets.
              users.groups.castaway-p2p = lib.mkIf miracastEnabled { };

              # The dedicated supplicant for the Miracast radio. Not networking.wireless
              # and not NetworkManager: a sink must create an autonomous P2P group, which
              # NM cannot do at all and the stock wireless module has no reason to allow.
              systemd.services.castaway-wpa = lib.mkIf miracastEnabled {
                description = "wpa_supplicant for castaway's Miracast radio";
                wantedBy = [ "multi-user.target" ];
                # The radio may appear after multi-user starts (USB, module load order),
                # so the unit follows the device when systemd knows it and otherwise
                # retries forever — an appliance's radio coming up late must not strand
                # the sink until a reboot.
                after = [ "sys-subsystem-net-devices-${miracastInterface}.device" ];
                wants = [ "sys-subsystem-net-devices-${miracastInterface}.device" ];
                unitConfig.StartLimitIntervalSec = 0;
                serviceConfig = {
                  ExecStart =
                    "${lib.getExe' pkgs.wpa_supplicant "wpa_supplicant"} "
                    + "-i ${miracastInterface} -D nl80211 -c ${miracastWpaConf}";
                  Restart = "on-failure";
                  RestartSec = 2;
                };
              };

              # As group owner we are expected to run the DHCP server (#45) — the peer's
              # address is how the backend finds who to dial, via the neighbour table.
              # networkd carries this whole obligation declaratively: the group interface
              # (`p2p-<parent>-N`) does not exist until the group forms and is named
              # unpredictably, and a match pattern handles exactly that.
              systemd.network = lib.mkIf miracastEnabled {
                enable = true;
                networks."40-castaway-p2p-group" = {
                  matchConfig.Name = "p2p-${miracastInterface}-*";
                  address = [ miracastGroupCidr ];
                  networkConfig = {
                    DHCPServer = true;
                    # Address the interface the moment it appears: the peer's DHCP
                    # DISCOVER can arrive within a second of association.
                    ConfigureWithoutCarrier = true;
                  };
                  dhcpServerConfig = {
                    PoolOffset = 100;
                    PoolSize = 50;
                    # A P2P group is a mirroring link, not a way to the internet. The
                    # default EmitRouter=yes makes the phone route everything at us and
                    # lose its own connectivity for the duration of the cast.
                    EmitRouter = false;
                    EmitDNS = false;
                    EmitNTP = false;
                  };
                  # A sometimes-existing interface must not hold network-online.target.
                  linkConfig.RequiredForOnline = false;
                };
              };

              # If NetworkManager is present it must keep its hands off both the parent
              # radio and the group interfaces wpa_supplicant creates on it; NM's P2P
              # support is source-only by design (protocol notes §7.6) and its touch here
              # is a torn-down group. Harmless to set when NM is disabled.
              networking.networkmanager.unmanaged = lib.optionals miracastEnabled [
                "interface-name:${miracastInterface}"
                "interface-name:p2p-${miracastInterface}-*"
              ];

              # Scripted networking's dhcpcd grabs every new interface by default, and
              # a DHCP *client* soliciting on the interface we serve DHCP on gets it an
              # IPv4LL address and a route it has no business having (observed in the
              # hwsim test). networkd owns this interface; nobody else touches it.
              networking.dhcpcd.denyInterfaces = lib.optionals miracastEnabled [
                "p2p-${miracastInterface}-*"
              ];

              systemd.services.castaway = {
                description = "castaway universal cast receiver";
                wantedBy = [ "multi-user.target" ];
                # Discovery joins multicast groups on a specific interface, so the
                # address has to be up before we bind.
                after = [ "network-online.target" ]
                  ++ lib.optional miracastEnabled "castaway-wpa.service";
                wants = [ "network-online.target" ]
                  ++ lib.optional miracastEnabled "castaway-wpa.service";

                environment = {
                  CASTAWAY_CONFIG = "${configFile}";
                  RUST_LOG = cfg.logLevel;
                  # Without this the filter-list cache lands somewhere unwritable and
                  # *silently* stops working. `cache_dir()` resolves XDG_CACHE_HOME, then
                  # HOME/.cache; under DynamicUser a dynamic user's home is `/`, so the
                  # path became /.cache/castaway with ProtectSystem=strict over it. Every
                  # failure there is swallowed by design (a missing list is not worth
                  # refusing to boot over), so the receiver looked healthy.
                  #
                  # The half that actually breaks is the render process: it loads the
                  # cache only, never fetches, so with nothing cached it injects no uBO
                  # scriptlets at all — while the browser process still blocks network
                  # requests from its in-memory engine. Exactly the silent failure #60 and
                  # #62 were written to prevent, reintroduced by the deployment.
                  #
                  # %C is systemd's CacheDirectory root, so this also gives the browser
                  # profile (cookies, "watch as guest") somewhere to persist.
                  XDG_CACHE_HOME = "%C";
                  # The same trap, on the state side, and it had the same shape: a
                  # dynamic user's home is `/`, so `$XDG_STATE_HOME` unset resolved to
                  # `/.local/state/castaway` under ProtectSystem=strict. Bluetooth link
                  # keys silently failed to persist there (every phone re-pairs after a
                  # restart) and it is now also where the rotated log files go.
                  #
                  # %S is the StateDirectory root, so with `StateDirectory=castaway`
                  # below this resolves to /var/lib/castaway — which is where the
                  # GameStream pairing store was hardcoded to anyway, so that credential
                  # keeps its existing path rather than moving under the deployment.
                  XDG_STATE_HOME = "%S";
                } // lib.optionalAttrs browserPackage {
                  # Chromium resolves its profile through HOME and a DynamicUser's
                  # home is `/` — the same trap as the two XDG variables above, but
                  # the browser reads HOME itself, so the XDG overrides don't reach
                  # it. %S/castaway is the state directory the unit already owns.
                  HOME = "%S/castaway";
                };

                serviceConfig = {
                  ExecStart = lib.getExe' cfg.package "castaway";
                  Restart = "on-failure";
                  RestartSec = 2;

                  # Everything it binds is above 1024 (HTTP, 1900, 5353), so it never
                  # needs root or CAP_NET_BIND_SERVICE.
                  DynamicUser = true;
                  # The wpa_supplicant control sockets are the one privileged thing the
                  # Miracast backend touches, and group membership is the whole grant.
                  SupplementaryGroups = lib.optional miracastEnabled "castaway-p2p";
                  StateDirectory = "castaway";
                  # Backs XDG_CACHE_HOME above: filter lists, uBO scriptlet bodies, and
                  # the browser profile. Losing it costs a refetch, not correctness, so it is
                  # a cache directory rather than state.
                  CacheDirectory = "castaway";
                  WorkingDirectory = "/var/lib/castaway";

                  NoNewPrivileges = true;
                  ProtectSystem = "strict";
                  ProtectHome = true;
                  ProtectKernelTunables = true;
                  ProtectKernelModules = true;
                  ProtectControlGroups = true;
                  RestrictRealtime = true;
                  RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
                  SystemCallArchitectures = "native";

                  # The three switches below narrow when the package carries the
                  # browser (#246): the kiosk exists to run Chromium, and Chromium's
                  # sandbox model is load-bearing — `--no-sandbox` was rejected
                  # deliberately (nix/electron-linux.nix, G86/D36), so the unit has
                  # to permit what the sandbox does. A browser-less package
                  # (`castaway-portable`) keeps the full set.
                  #
                  # An X-based deploy's kiosk finds its display through
                  # /tmp/.X11-unix; PrivateTmp shows it an empty /tmp and winit
                  # reports a display it cannot reach. Wayland hands the socket over
                  # XDG_RUNTIME_DIR and would tolerate PrivateTmp, but the module
                  # cannot see which server the box runs, and a kiosk that cannot
                  # find its display is the worse default.
                  PrivateTmp = !browserPackage;
                  # Chromium's namespace sandbox clones user namespaces (the setuid
                  # helper cannot exist in the store); RestrictNamespaces makes the
                  # zygote abort and the zygote host CHECK-crashes the whole browser
                  # (zygote_host_impl_linux.cc:221).
                  RestrictNamespaces = !browserPackage;
                  # `@system-service` alone SIGSYS-kills the browser twice over: V8
                  # allocates memory-protection keys (syscall 330, `pkey_alloc`) and
                  # the namespace sandbox chroots its zygote into an empty directory
                  # (syscall 161). The render sandbox then installs its own
                  # seccomp/Landlock filters (`@sandbox`). Observed directly on the
                  # dial-vm node and reproduced natively under `systemd-run` —
                  # with these lines electron runs, without any one of them it dies.
                  SystemCallFilter = [ "@system-service" ]
                    ++ lib.optionals browserPackage [
                    "@sandbox"
                    "pkey_alloc"
                    "pkey_free"
                    "pkey_mprotect"
                    "chroot"
                  ];
                };
              };

              # Entirely derived from the registry — see the `networkSurface` bindings
              # above. What the old hand-kept list had drifted into is the argument for
              # never keeping one again: it opened TCP 7011 (nothing has bound it since
              # the second AirPlay listener was removed), it gated Cast and AirPlay on
              # `or false` while the binary defaults every enable flag to true (a stock
              # deploy ran Cast on 8009 with the firewall closed), and it had no rules
              # at all for the mirroring media planes, which then bound ephemeral ports
              # no rule could have named — AirPlay/Cast mirroring onto a firewalled box
              # died silently while every control plane looked perfect. The media
              # planes now bind from `[media_ports]` and open here like anything else.
              #
              # Miracast keeps two quirks the registry records: nothing listens on TCP
              # 7236 (the sink is the RTSP client and dials the source), and its UDP
              # rules are deliberately not interface-scoped — the P2P group interface
              # (`p2p-wlan0-N`) does not exist until the group forms.
              networking.firewall = lib.mkIf cfg.openFirewall {
                allowedTCPPorts = surfacePortsFor "tcp";
                allowedUDPPorts = surfacePortsFor "udp";
              };
            };
          };
        default = castaway;
      };
    };
}
