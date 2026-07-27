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

    # Prebuilt third-party blobs for the Windows cross-build. Inputs rather than in-tree
    # `fetchurl` hashes so `flake.lock` records every external artifact this repo pulls in
    # — one place to audit, one update story. `file+` keeps them as the raw archives:
    # nix/{ffmpeg,cef}-windows.nix do the unpacking, because both need layout fixups
    # afterwards that a bare tarball input can't express.
    #
    # Both URLs are immutable by construction. BtbN replaces the assets under its `latest`
    # tag daily, so this pins the dated `autobuild-*` release instead; the CEF CDN keys on
    # the full version+commit+chromium triple.
    ffmpeg-windows-src = {
      url = "file+https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-24-13-32/ffmpeg-n7.1.5-10-g2aefd64d48-win64-lgpl-shared-7.1.zip";
      flake = false;
    };

    cef-windows-src = {
      # `+` has to stay percent-encoded or the CDN reads it as a space.
      url = "file+https://cef-builds.spotifycdn.com/cef_binary_147.0.10%2Bgd58e84d%2Bchromium-147.0.7727.118_windows64_minimal.tar.bz2";
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
  };

  outputs =
    { self
    , nixpkgs
    , systems
    , rust-overlay
    , crane
    , nix-direnv
    , ffmpeg-windows-src
    , cef-windows-src
    , widevine-windows-src
    , openscreen-src
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
          #   like a network problem. Only the `cef` packages touch them, and both
          #   `cefDistFor` and `windows.nix` degrade to no-DRM rather than failing if a
          #   downstream nixpkgs refuses them.
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
          # adblock filter list in pipeline). A missing suffix here only shows up as an
          # `include_str!` failure inside the sandbox, since a plain `cargo build` reads the
          # real tree — so add the extension when you add the asset.
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              (craneLib.filterCargoSources path type)
              || (pkgs.lib.hasSuffix ".xml" path)
              || (pkgs.lib.hasSuffix ".ttf" path)
              || (pkgs.lib.hasSuffix ".bin" path)
              || (pkgs.lib.hasSuffix ".txt" path);
            name = "source";
          };
          strictDeps = true;

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

      # The `cef`/`cef-dll-sys` crates expect a *flattened* CEF distribution (libcef.so +
      # .pak resources at the root, not the Release/Resources split nixpkgs ships) plus an
      # `archive.json` to pass their version check. We match the `cef` crate 147.1.0 to
      # nixpkgs `cef-binary` (147.0.10) so the crates use the already-NixOS-linked
      # libcef.so instead of downloading their own. Shared by the devShell and the
      # `castaway-cef` package, which must agree on it.
      # The Widevine CDM belongs *here*, beside libcef, and not next to our own binary:
      # Chromium scans `DIR_COMPONENT_PREINSTALLED` for `WidevineCdm/`, which resolves to
      # `base::DIR_ASSETS` → `DIR_MODULE` → the directory of the module holding Chromium's
      # code. On Linux that is libcef.so's directory, i.e. this distribution — on Windows
      # it is the folder holding both, which is why `windows.nix` stages it beside the .exe.
      #
      # `tryEval` because the CDM is unfree: a nixpkgs without `allowUnfree` still builds a
      # working receiver, it just cannot play protected streams. A hard dependency would
      # make the package unbuildable for anyone who has not accepted Google's terms, over a
      # feature most casts never touch.
      widevineLinuxFor = system:
        let
          pkgs = pkgsFor system;
          attempt = builtins.tryEval (pkgs.widevine-cdm.outPath or null);
        in
        if attempt.success && attempt.value != null then
          "${attempt.value}/share/google/chrome/WidevineCdm"
        else
          null;

      cefDistFor = system:
        let
          pkgs = pkgsFor system;
          widevine = widevineLinuxFor system;
        in
        pkgs.runCommand "cef-dist-${pkgs.cef-binary.version}" { } ''
          mkdir -p $out
          ln -s ${pkgs.cef-binary}/Release/* $out/
          ln -s ${pkgs.cef-binary}/Resources/* $out/
          ${pkgs.lib.optionalString (widevine != null) "ln -s ${widevine} $out/WidevineCdm"}
          printf '%s' '{"type":"minimal","name":"cef_binary_${pkgs.cef-binary.version}+chromium_linux64","sha1":"0000000000000000000000000000000000000000"}' > $out/archive.json
        '';

      # The kiosk build with the browser in it — what actually plays YouTube.
      linuxCefFor = system: import ./nix/linux-cef.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        cefDist = cefDistFor system;
      };

      # Linux → Windows cross-build (x86_64-pc-windows-msvc). Only meaningful from
      # Linux; the sysroot derivation is Linux-only.
      windowsFor = system: import ./nix/windows.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        rustToolchain = rustToolchainFor system;
        ffmpegSrc = ffmpeg-windows-src;
        cefSrc = cef-windows-src;
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
          default = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            # Only run tests during the check phase, not during build
            doCheck = false;
          });

          castaway = self.packages.${system}.default;

          # A scripted phone, for the one path no VM test can cover: YouTube's Lounge
          # servers are a third party to the session, so this needs the real internet
          # and a running receiver. `nix run .#yt-selfplay -- http://<receiver>:8080`.
          yt-selfplay = import ./nix/yt-selfplay.nix { inherit pkgs; };
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
          let windows = windowsFor system; in {
            # The Linux kiosk build with the browser in it. `default` above has neither a
            # renderer nor a browser, so it cannot play YouTube at all — it does not even
            # advertise DIAL (D27). This is the one to deploy on Linux.
            castaway-cef = linuxCefFor system;

            # The Windows deploy artifacts, cross-compiled from Linux. `-cef` is the one
            # that ships; `-render` drops the browser, and the bare build is the toolchain
            # canary — if it stops linking, the toolchain broke, not the media stack.
            castaway-windows = windows.castaway;
            castaway-windows-render = windows.castaway-render;
            castaway-windows-hwaccel = windows.castaway-hwaccel;
            castaway-windows-cef = windows.castaway-cef;

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
          # Build the crate as part of checks
          build = self.packages.${system}.default;

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
          openscreen-device-auth = import ./nix/openscreen-device-auth.nix {
            inherit pkgs;
            openscreenSrc = openscreen-src;
          };
        }
        # Tier-2: whole adapters driven by scripted senders from a second VM over a real
        # LAN (ground rule 6). Linux-only — nixosTest needs KVM and a NixOS guest.
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          integration-vm = import ./nix/vm-test.nix { inherit pkgs self; };

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
          # The same flattened CEF distribution the `castaway-cef` package builds against;
          # a devShell that disagreed with the package would be a trap.
          cefDist = cefDistFor system;
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

              # For the `cef` feature (cef-dll-sys constructs a cmake::Config; the C++
              # wrapper is only *built* on Windows/macOS, but keep the tools available).
              pkgs.cmake
              pkgs.ninja

              # The scripted phone, on PATH: `yt-selfplay http://<receiver>:8080` while a
              # `--features cef` build runs, to check a YouTube cast really plays.
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

            # Environment variables for development
            RUST_BACKTRACE = "1";
            RUST_LOG = "debug";
            # `ffmpeg-sys-next` generates bindings with bindgen, which dlopens libclang
            # and needs the libc headers pointed out explicitly in a Nix env.
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.glibc.dev}/include";
            # Point the `cef` crates at the flattened, NixOS-linked CEF distribution.
            CEF_PATH = "${cefDist}";
            # Let winit/wgpu dlopen Vulkan/Wayland/X11, and the loader find libcef.so.
            # libGL is needed because cefDist's bundled libGLESv2.so links libGL.so.1;
            # without it CEF's GPU process dies and wgpu's GL-backend probe SIGSEGVs.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.vulkan-loader
              pkgs.libGL
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxi
              pkgs.libxrandr
            ] + ":${cefDist}";
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

                  The default has no renderer and no browser: it serves and discovers, but
                  it cannot display anything, and it does not advertise DIAL at all —
                  YouTube casting is a web page, so a build with nowhere to put one
                  declines to offer it rather than accepting casts it can never play.

                  For a kiosk on a real display, use
                  `castaway.packages.''${system}.castaway-cef`, which carries the CEF
                  browser and the render pipeline.
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
                description = "`RUST_LOG` filter for the service.";
              };

              openFirewall = lib.mkOption {
                type = lib.types.bool;
                default = true;
                description = ''
                  Open the HTTP port plus SSDP (1900/udp) and mDNS (5353/udp). Discovery
                  fails silently without these, which is the failure mode this exists to
                  prevent — turn it off only if something else manages the rules.
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

              systemd.services.castaway = {
                description = "castaway universal cast receiver";
                wantedBy = [ "multi-user.target" ];
                # Discovery joins multicast groups on a specific interface, so the
                # address has to be up before we bind.
                after = [ "network-online.target" ];
                wants = [ "network-online.target" ];

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
                  # %C is systemd's CacheDirectory root, so this also gives the CEF
                  # profile (cookies, "watch as guest") somewhere to persist.
                  XDG_CACHE_HOME = "%C";
                };

                serviceConfig = {
                  ExecStart = lib.getExe' cfg.package "castaway";
                  Restart = "on-failure";
                  RestartSec = 2;

                  # Everything it binds is above 1024 (HTTP, 1900, 5353), so it never
                  # needs root or CAP_NET_BIND_SERVICE.
                  DynamicUser = true;
                  StateDirectory = "castaway";
                  # Backs XDG_CACHE_HOME above: filter lists, uBO scriptlet bodies, and
                  # the CEF profile. Losing it costs a refetch, not correctness, so it is
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

              networking.firewall = lib.mkIf cfg.openFirewall {
                # The shared HTTP host (dd.xml fetches, DIAL launch, SOAP, Spotify),
                # plus a socket protocol's own listener when it's switched on.
                allowedTCPPorts = [ cfg.httpPort ]
                  ++ lib.optional (cfg.settings.enable.cast or false) 8009
                  # AirPlay control (7000) and RAOP audio (7011). The mirroring media
                  # plane needs UDP ports too, but it can't start until pairing and
                  # FairPlay land (Q1) — no point opening what nothing listens on.
                  ++ lib.optionals (cfg.settings.enable.airplay or false) [ 7000 7011 ];
                allowedUDPPorts = [
                  # SSDP: DIAL/DLNA senders discover us via M-SEARCH on 1900.
                  1900
                  # mDNS: Spotify Connect, Cast, and AirPlay/RAOP advertisements.
                  5353
                ]
                # Miracast's RTP port. Note there is no TCP rule to add: the sink is the
                # RTSP *client* and dials out to the source's 7236, so nothing listens on
                # 7236 here. The rule is per-interface-less on purpose — the P2P group
                # interface does not exist until the group starts and is named
                # unpredictably (`p2p-wlan0-N`).
                ++ lib.optional (cfg.settings.enable.miracast or false)
                  (cfg.settings.miracast.rtp_port or 1028);
              };
            };
          };
        default = castaway;
      };
    };
}
