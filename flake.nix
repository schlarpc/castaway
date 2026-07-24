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
    , ...
    }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);

      # Helper to get pkgs for a system with rust-overlay applied
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
        config = {
          # The Windows cross-build sysroot repacks Microsoft's MSVC CRT + Windows SDK,
          # which are redistributable-for-building but not free software. Whitelist that
          # one derivation by name rather than flipping `allowUnfree` wholesale.
          allowUnfreePredicate = pkg: nixpkgs.lib.getName pkg == "msvc-sysroot";
        };
      };

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

      # Linux → Windows cross-build (x86_64-pc-windows-msvc). Only meaningful from
      # Linux; the sysroot derivation is Linux-only.
      windowsFor = system: import ./nix/windows.nix {
        pkgs = pkgsFor system;
        craneLib = cranelibFor system;
        commonArgs = commonArgsFor system;
        rustToolchain = rustToolchainFor system;
        ffmpegSrc = ffmpeg-windows-src;
        cefSrc = cef-windows-src;
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
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
          let windows = windowsFor system; in {
            # The Windows deploy artifacts, cross-compiled from Linux. `-cef` is the one
            # that ships; `-render` drops the browser, and the bare build is the toolchain
            # canary — if it stops linking, the toolchain broke, not the media stack.
            castaway-windows = windows.castaway;
            castaway-windows-render = windows.castaway-render;
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
          # The `cef`/`cef-dll-sys` crates expect a *flattened* CEF distribution
          # (libcef.so + .pak resources at the root, not the Release/Resources split
          # nixpkgs ships) plus an `archive.json` to pass their version check. We match
          # the `cef` crate 147.1.0 to nixpkgs `cef-binary` (147.0.10) so the crates use
          # the already-NixOS-linked libcef.so instead of downloading their own.
          cefDist = pkgs.runCommand "cef-dist-${pkgs.cef-binary.version}" { } ''
            mkdir -p $out
            ln -s ${pkgs.cef-binary}/Release/* $out/
            ln -s ${pkgs.cef-binary}/Resources/* $out/
            printf '%s' '{"type":"minimal","name":"cef_binary_${pkgs.cef-binary.version}+chromium_linux64","sha1":"0000000000000000000000000000000000000000"}' > $out/archive.json
          '';
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

              # nix-direnv for this flake's shell
              nix-direnv.packages.${system}.default
            ];

            buildInputs = [
              # ffmpeg dev libs for the `ffmpeg` pipeline feature. Pin to 7.x to match
              # `ffmpeg-next`/`ffmpeg-sys-next` 7.1 (nixpkgs default is 8.x).
              pkgs.ffmpeg_7
              # Runtime libs for the `render`/`kiosk` pipeline features (wgpu + winit).
              pkgs.vulkan-loader
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libx11
              pkgs.libxcursor
              pkgs.libxi
              pkgs.libxrandr
            ];

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

      # NixOS module: `services.castaway.enable = true` opens the LAN-discovery and
      # HTTP ports the receiver needs. (Running castaway as a systemd/kiosk service is
      # a follow-up; the firewall is the part that silently breaks discovery today.)
      nixosModules = rec {
        castaway = { config, lib, ... }:
          let
            cfg = config.services.castaway;
          in
          {
            options.services.castaway = {
              enable = lib.mkEnableOption
                "the castaway universal cast receiver (currently: open its firewall ports)";

              httpPort = lib.mkOption {
                type = lib.types.port;
                default = 8080;
                description = ''
                  TCP port of castaway's shared HTTP host (DLNA description/SOAP,
                  Spotify onboarding, DIAL REST). Must match `http_port` in
                  castaway.toml.
                '';
              };
            };

            config = lib.mkIf cfg.enable {
              networking.firewall = {
                # The shared HTTP host (dd.xml fetches, DIAL launch, SOAP, Spotify).
                allowedTCPPorts = [ cfg.httpPort ];
                allowedUDPPorts = [
                  # SSDP: DIAL/DLNA senders discover us via M-SEARCH on 1900.
                  1900
                  # mDNS: Spotify Connect (and later Cast/AirPlay) advertisements.
                  5353
                ];
              };
            };
          };
        default = castaway;
      };
    };
}
