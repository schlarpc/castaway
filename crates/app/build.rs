//! Stamps the build's git revision into the binary, for the idle screen's footer.
//!
//! Two ways in, because there are two ways this gets built. Under Nix the sandbox has
//! neither `.git` nor `git`, so the flake passes the flake's own revision through
//! `CASTAWAY_GIT_REV`. For a plain `cargo build` on a checkout, ask git.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CASTAWAY_GIT_REV");

    let rev = std::env::var("CASTAWAY_GIT_REV")
        .ok()
        .filter(|r| !r.is_empty())
        .or_else(git_rev)
        // Not a build failure: a tarball with no history still has to build, and the
        // footer is a diagnostic rather than something correctness depends on.
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CASTAWAY_GIT_REV={rev}");

    // Only for the git path — under Nix the env var is the input and the flake tracks it.
    if let Some(dir) = git_dir() {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }
}

fn git_rev() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let rev = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if rev.is_empty() {
        return None;
    }
    // A dirty tree is not the revision it claims to be, and the panel runs unattended for
    // weeks — "which build is this" is the only question the footer exists to answer.
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|o| !o.stdout.is_empty());
    Some(if dirty { format!("{rev}-dirty") } else { rev })
}

fn git_dir() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
