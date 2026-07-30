{
  description = "A Rust application built with Nix flakes using Crane";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

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
    # Every URL is immutable by construction. BtbN replaces the assets under its `latest`
    # tag daily, so this pins the dated `autobuild-*` release instead; the rest are
    # version-stamped release and CDN paths.
    ffmpeg-windows-src = {
      url = "file+https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-24-13-32/ffmpeg-n7.1.5-10-g2aefd64d48-win64-lgpl-shared-7.1.zip";
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
    # MIT, so the `stream` feature that links it is opt-in and off in the portable
    # build. See D37.
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
  };

  outputs =
    { self
    , nixpkgs
    , systems
    , rust-overlay
    , crane
    , nix-direnv
    , ffmpeg-windows-src
    , widevine-windows-src
    , electron-linux-src
    , electron-windows-src
    , openscreen-src
    , moonlight-common-c-src
    , moonlight-enet-src
    , moonlight-nanors-src
    , ...
    }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);

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
            builtins.elem (nixpkgs.lib.getName pkg) [
              "msvc-sysroot"
              "linux-firmware"
              "widevine-cdm"
              "widevine-cdm-windows"
            ];
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
        {
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
              || (pkgs.lib.hasSuffix ".bin" path)
              || (pkgs.lib.hasSuffix ".der" path)
              || (pkgs.lib.hasSuffix ".pem" path)
              || (pkgs.lib.hasSuffix ".png" path)
              || (pkgs.lib.hasSuffix ".svg" path)
              || (pkgs.lib.hasSuffix ".txt" path)
              # cast-replay's trimmed AirServer databases (D44) — include_bytes!'d by
              # its tests, so their absence is a sandbox-only compile failure.
              || (pkgs.lib.hasSuffix ".sqlite" path)
              # The network-surface artifacts (D45): the app's freshness tests read
              # them at runtime and fail on drift, which is what keeps the firewall
              # JSON in lock-step with the registry — so the sandbox must see them.
              || (pkgs.lib.hasSuffix "docs/network-surface.md" path)
              || (pkgs.lib.hasSuffix "nix/network-surface.json" path);
            name = "source";
          };
          strictDeps = true;

          # The revision the footer on the idle screen shows. Passed in rather than shelled
          # out for, because the build sandbox has no `.git` and no `git` — see the app's
          # `build.rs`, which falls back to asking git only for a plain `cargo build`.
          CASTAWAY_GIT_REV = self.shortRev or self.dirtyShortRev or "unknown";

          buildInputs = [
            # Add additional build inputs here
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
            pkgs.darwin.apple_sdk.frameworks.Security
          ];

          nativeBuildInputs = [
            # Add additional native build inputs here
          ];
        };

      # Build only dependencies (for caching)
      cargoArtifactsFor = system:
        let craneLib = cranelibFor system;
        in craneLib.buildDepsOnly (commonArgsFor system);

      # The Widevine CDM, staged into the browser's profile so a panel that has never
      # been online can still play protected video (G46, re-proven under D36/Q42).
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

      # The full kiosk build — renderer, browser, audio, Bluetooth. `packages.default` on
      # Linux, so it is what `nix run .` gives you.
      linuxKioskFor = system: import ./nix/linux-kiosk.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        electron = electronLinuxFor system;
        widevineCdm = widevineLinuxFor system;
        moonlightCommonC = moonlightCommonCFor system;
      };

      # Linux → Windows cross-build (x86_64-pc-windows-msvc). Only meaningful from
      # Linux; the sysroot derivation is Linux-only.
      windowsFor = system: import ./nix/windows.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        rustToolchain = rustToolchainFor system;
        ffmpegSrc = ffmpeg-windows-src;
        electronSrc = electron-windows-src;
        widevineSrc = widevine-windows-src;
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
        in
        {
          # No renderer, no browser, nothing platform-specific: the build that proves the
          # protocol stack. It is `default` everywhere the full kiosk cannot be built (i.e.
          # Darwin), and what `checks.build` compiles so `nix flake check` stays cheap.
          castaway-portable = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            # Only run tests during the check phase, not during build
            doCheck = false;
          });

          default = self.packages.${system}.castaway-portable;

          castaway = self.packages.${system}.default;

          # A scripted phone, for the one path no VM test can cover: YouTube's Lounge
          # servers are a third party to the session, so this needs the real internet
          # and a running receiver. `nix run .#yt-selfplay -- http://<receiver>:8080`.
          yt-selfplay = import ./nix/yt-selfplay.nix { inherit pkgs; };

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
            cargoExtraArgs = "-p proto-gamestream --example gs-probe";
            doCheck = false;
            # The example is not installed by crane's default install phase.
            postInstall = ''
              install -Dm755 \
                "$(find target -name gs-probe -type f -perm -u+x | head -1)" \
                "$out/bin/gs-probe"
            '';
          });
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
          let windows = windowsFor system; in {
            # On Linux the default is the real receiver: every optional feature except
            # `ldac` (see nix/linux-kiosk.nix for why that one stays off). `nix run .`
            # should hand you something that can actually display a cast, not a build that
            # discovers, accepts, and then has nowhere to put the picture.
            default = linuxKioskFor system;

            # The browser runtime the port targets (D36). Exposed on its own so it can be
            # run against the probes in `browser-host/` — `nix run .#electron -- \
            # browser-host/codec-probe.js` is how a version bump gets checked before it
            # is trusted.
            electron = electronLinuxFor system;

            # The Windows deploy artifacts, cross-compiled from Linux. `-electron` is the one
            # that ships; `-render` drops the browser, and the bare build is the toolchain
            # canary — if it stops linking, the toolchain broke, not the media stack.
            castaway-windows = windows.castaway;
            castaway-windows-render = windows.castaway-render;
            castaway-windows-hwaccel = windows.castaway-hwaccel;
            castaway-windows-electron = windows.castaway-electron;

            # The MSVC CRT + Windows SDK sysroot they build against. Exposed on its own so
            # it can be built and cached independently of the Rust build.
            msvc-sysroot = windows.sysroot;
          }
        ));

      # Checks run by `nix flake check`
      checks = eachSystem (system:
        let
          pkgs = pkgsFor system;
          craneLib = cranelibFor system;
          commonArgs = commonArgsFor system;
          cargoArtifacts = cargoArtifactsFor system;

          # What the `hwaccel` feature needs on top of a default build: ffmpeg's headers
          # for `ffmpeg-sys-next` (7.x, matching the crate — nixpkgs defaults to 8.x) and
          # libclang, which its bindgen dlopens. `ash` and `wgpu-hal` need nothing at build
          # time; Vulkan is loaded at runtime, which is why this check stops at compiling.
          # The audio path: libav decoders for the A2DP codecs plus a real PCM device.
          # Kept as its own check because it is the one feature whose absence is
          # *silent* — a receiver with no decoder pairs, streams, and plays nothing.
          audioArgs = {
            cargoExtraArgs = "--package castaway --features audio-out";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.ffmpeg_7 pkgs.alsa-lib ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
          };

          hwaccelArgs = {
            cargoExtraArgs = "--package castaway --features hwaccel";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.ffmpeg_7 ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
          };
        in
        {
          # Build the crate as part of checks. Deliberately the *portable* build and not
          # `default`: on Linux `default` is now the kiosk, and pulling Electron, ffmpeg
          # and a second dependency tree into `nix flake check` would cost far more than it
          # proves. The feature sets that build adds are covered by `audio`/`hwaccel`
          # below; what is left uncovered is the `electron` build, and that is a
          # deploy-time concern.
          build = self.packages.${system}.castaway-portable;

          # Run clippy
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          # Check formatting
          fmt = craneLib.cargoFmt {
            src = commonArgs.src;
          };

          # Run tests
          test = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
          });

          # Run tests with coverage
          coverage = craneLib.cargoLlvmCov (commonArgs // {
            inherit cargoArtifacts;
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
          # The GameStream client against a real Sunshine — the only test here that
          # runs the *reference implementation* as the peer rather than a script of
          # ours (D37). Pairing is hands-free because `sunshine -0` takes the PIN on
          # stdin instead of its web UI.
          gamestream-vm = import ./nix/gamestream-vm-test.nix {
            inherit pkgs self;
          };

          openscreen-device-auth = import ./nix/openscreen-device-auth.nix {
            inherit pkgs;
            openscreenSrc = openscreen-src;
          };
        }
        # Tier-2: whole adapters driven by scripted senders from a second VM over a real
        # LAN (ground rule 6). Linux-only — nixosTest needs KVM and a NixOS guest.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          integration-vm = import ./nix/vm-test.nix { inherit pkgs self; };

          # The Miracast radio path end to end on mac80211_hwsim: real mac80211 radios,
          # real P2P group formation and WPS, DHCP across the group, and the sink
          # dialling out over it — the whole surface Q7 said only hardware could touch,
          # minus the driver's own quirks (§7.6), which remain the hardware's to prove.
          miracast-vm = import ./nix/miracast-vm-test.nix { inherit pkgs self; };

          # A complete A2DP session with no radio: btvirt's linked virtual controllers,
          # BlueZ as an independent A2DP source on one, our receiver on the other. The
          # sender side is then an implementation that has never seen our code, which is
          # categorically better evidence than our source talking to our sink.
          bluetooth-vm = import ./nix/bluetooth-vm-test.nix {
            inherit pkgs;
            castaway = craneLib.buildPackage (commonArgs // {
              inherit cargoArtifacts;
              pname = "castaway-bluetooth";
              cargoExtraArgs = "--package castaway --features bluetooth-socket";
              doCheck = false;
            });
          };

          # Compile-check the Linux hardware-decode backend (VA-API → DMA-BUF → Vulkan).
          #
          # Only clippy, not the tests: the sandbox has no render node, so the zero-copy
          # readback test would skip and prove nothing. What this *does* catch is the
          # backend rotting behind its feature flag — raw libav and `wgpu-hal` are exactly
          # the kind of unsafe FFI that stops compiling when a dependency moves, and the
          # default build never touches it. Running the real thing needs the dev box's GPU.
          # The audio path must keep compiling *and* keep passing its round-trip tests:
          # this is the one feature whose failure is silent, since a receiver with no
          # decoder still pairs and still streams — it just plays nothing. The decode
          # tests assert on the decoded audio's level, so a path that produces silence
          # fails here rather than in the room.
          audio = craneLib.cargoNextest (commonArgs // {
            cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
              pname = "castaway-audio-deps";
              inherit (audioArgs) nativeBuildInputs buildInputs LIBCLANG_PATH BINDGEN_EXTRA_CLANG_ARGS;
              cargoExtraArgs = audioArgs.cargoExtraArgs;
            });
            inherit (audioArgs) nativeBuildInputs buildInputs LIBCLANG_PATH BINDGEN_EXTRA_CLANG_ARGS;
            cargoExtraArgs = "--package pipeline --features audio";
          });

          hwaccel-clippy = craneLib.cargoClippy (commonArgs // {
            cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
              pname = "castaway-hwaccel-deps";
              inherit (hwaccelArgs) nativeBuildInputs buildInputs LIBCLANG_PATH BINDGEN_EXTRA_CLANG_ARGS;
              cargoExtraArgs = hwaccelArgs.cargoExtraArgs;
            });
            inherit (hwaccelArgs) nativeBuildInputs buildInputs LIBCLANG_PATH BINDGEN_EXTRA_CLANG_ARGS;
            cargoClippyExtraArgs = "${hwaccelArgs.cargoExtraArgs} --all-targets -- --deny warnings";
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
            # the radio side is a *deployment*, not code (OPEN-QUESTIONS Q7c/Q7d): a
            # wpa_supplicant castaway can command, and a DHCP server on the group
            # interface. Both are derived from the same settings the binary reads, so
            # the daemon castaway talks to and the daemon the module runs cannot drift.
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
              miracast = miracastEnabled;
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
                  audio output and Bluetooth. Every optional feature is on except `ldac`,
                  which advertises a codec the build cannot decode.

                  For a headless box — one proving the protocol stack, or serving DLNA
                  with no display attached — use
                  `castaway.packages.''${system}.castaway-portable`, which has neither a
                  renderer nor a browser. It does not advertise DIAL at all: YouTube
                  casting is a web page, so a build with nowhere to put one declines to
                  offer it rather than accepting casts it can never play.
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

              # castaway runs its own mDNS responder on 5353 (OPEN-QUESTIONS Q5). Both
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

              # As group owner we are expected to run the DHCP server (Q7c) — the peer's
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
                  # requests from its in-memory engine. Exactly the silent failure Q17 and
                  # Q36 were written to prevent, reintroduced by the deployment.
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
                  PrivateTmp = true;
                  ProtectSystem = "strict";
                  ProtectHome = true;
                  ProtectKernelTunables = true;
                  ProtectKernelModules = true;
                  ProtectControlGroups = true;
                  RestrictNamespaces = true;
                  RestrictRealtime = true;
                  RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" "AF_NETLINK" ];
                  SystemCallArchitectures = "native";
                  SystemCallFilter = [ "@system-service" ];
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
