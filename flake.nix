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
  };

  outputs = { self, nixpkgs, systems, rust-overlay, crane, nix-direnv, ... }:
    let
      eachSystem = nixpkgs.lib.genAttrs (import systems);

      # Helper to get pkgs for a system with rust-overlay applied
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
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
          src = craneLib.cleanCargoSource ./.;
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
        });

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
        });

      # Development shell
      devShells = eachSystem (system:
        let
          pkgs = pkgsFor system;
          rustToolchain = rustToolchainFor system;
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

              # nix-direnv for this flake's shell
              nix-direnv.packages.${system}.default
            ];

            # Environment variables for development
            RUST_BACKTRACE = "1";
            RUST_LOG = "debug";
          };
        });

      # Expose nix-direnv for .envrc to use
      lib = {
        inherit nix-direnv;
      };
    };
}
