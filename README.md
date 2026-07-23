# castaway

A Rust application with a reproducible [Nix]-based development environment.

## Development

This project uses [rust-flake] as its foundation, providing a pinned Rust toolchain and
reproducible builds via [Nix] and [Crane].

### Setting up the development environment

The project uses [direnv] to automatically load the development environment. When you enter
the project directory, direnv activates a shell with the pinned toolchain and dev tools
available, refreshing automatically when you change the flake or `Cargo.toml`.

```shell
$ direnv allow
```

Without direnv, enter the shell manually:

```shell
$ nix develop
```

### Building and running

```shell
$ cargo run                                      # debug build + run
$ cargo build --release                          # optimized build
$ nix run                                        # build and run via Nix
$ ./result/bin/castaway   # the nix-built binary
```

### Testing, linting, and formatting

```shell
$ cargo nextest run          # fast parallel test runner
$ cargo llvm-cov nextest     # tests with coverage
$ cargo clippy --all-targets # lint
$ cargo fmt                  # format
```

### Working with Nix

```shell
$ nix build          # build the package
$ nix flake check    # run all checks (build, clippy, fmt, test, coverage)
$ nix flake update   # update flake inputs
```

The Rust toolchain is pinned in `rust-toolchain.toml` (single source of truth); Nix reads it
via `rust-bin.fromRustupToolchainFile`, so builds stay reproducible. To upgrade Rust, bump
`channel` there.

## Keeping in sync with the base template

This project was generated from [rust-flake] and can receive updates from the upstream
template using [cruft]:

```shell
$ cruft update --checkout template
```

[Crane]: https://crane.dev/
[cruft]: https://cruft.github.io/cruft/
[direnv]: https://direnv.net/
[Nix]: https://nixos.org/
[rust-flake]: https://github.com/schlarpc/rust-flake
