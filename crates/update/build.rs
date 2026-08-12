//! One line, and it is the line `option_env!` does not write for you.
//!
//! `CASTAWAY_RELEASE_PUBKEY` decides which key this build trusts (see `lib.rs`). Without
//! this, changing it and rebuilding in place would silently produce a binary carrying the
//! old key — under Nix every build is fresh so it would never show up there, and on a
//! developer's machine it would show up as an update that mysteriously will not verify.

fn main() {
    println!("cargo:rerun-if-env-changed=CASTAWAY_RELEASE_PUBKEY");
    println!("cargo:rerun-if-changed=release-key.pub");
}
