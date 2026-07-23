# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

This is a Rust project using Nix flakes with a pinned toolchain. First load the environment:

- `direnv allow` — load the development environment (or `nix develop`)

### Build & run

- `cargo run` — build (debug) and run
- `cargo build --release` — optimized build
- `nix run` — build and run via Nix
- `./result/bin/castaway` — the nix-built binary

### Test, lint, format

- `cargo nextest run` — fast parallel test runner
- `cargo llvm-cov nextest` — tests with coverage
- `cargo clippy --all-targets` — lint
- `cargo fmt` — format

### Nix

- `nix build` — build the package
- `nix flake check` — run all checks (build, clippy, fmt, test, coverage)
- `nix flake update` — update flake inputs

## Architecture

Built from the [rust-flake](https://github.com/schlarpc/rust-flake) template.

- **src/main.rs** — application entry point
- **Cargo.toml** — package manifest; lints configured under `[lints.rust]` and `[lints.clippy]`
- **flake.nix** — Nix build (Crane), dev shell, and CI checks
- **rust-toolchain.toml** — single source of truth for the Rust version; Nix reads it via
  `rust-bin.fromRustupToolchainFile`, so builds stay reproducible. Bump `channel` to upgrade.

## Keeping in sync with the base template

Pull upstream template updates with [cruft](https://cruft.github.io/cruft/):

- `cruft update --checkout template`
