//! Stamps the build's identity into the binary: which commit it is, and where that
//! commit sits in the history.
//!
//! Two ways in, because there are two ways this gets built. Under Nix the sandbox has
//! neither `.git` nor `git`, so the flake passes its own `self.shortRev`/`self.revCount`
//! through `CASTAWAY_GIT_REV` and `CASTAWAY_BUILD`. For a plain `cargo build` on a
//! checkout, ask git.
//!
//! The revision is for the idle screen's footer and is a diagnostic. The build number is
//! not: it is what the auto-updater orders itself against (#343), because a sha
//! identifies a tree and does not order two of them. Both are `unknown`/`0` on a dirty
//! tree, and that is the same statement twice — a build that is not the commit it names
//! must not be replaced at 4 a.m. by something claiming to be newer.

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

    println!("cargo:rerun-if-env-changed=CASTAWAY_BUILD");
    let build = std::env::var("CASTAWAY_BUILD")
        .ok()
        .filter(|b| !b.is_empty())
        .or_else(git_build_number)
        // Zero is the "this build cannot order itself" sentinel, and a real build number
        // starts at one — so a tarball with no history is representable rather than a
        // build failure, and the updater stands down on it by construction.
        .unwrap_or_else(|| "0".to_string());
    println!("cargo:rustc-env=CASTAWAY_BUILD={build}");

    // Only for the git path — under Nix the env var is the input and the flake tracks it.
    if let Some(dir) = git_dir() {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }

    embed_windows_icon();
}

/// Compile assets/castaway.rc — the `.exe` icon, and nothing else (see the comment
/// in that file about the external manifest) — into the binary when the *target* is
/// Windows. Keyed on `CARGO_CFG_TARGET_OS` rather than `cfg(windows)` because the
/// Windows binary is cross-built from Linux (docs/cross-build.md), where build.rs
/// itself runs as a Linux program; `embed-resource` finds `llvm-rc`, which
/// nix/windows.nix puts on the PATH for exactly this.
///
/// A warning rather than a failure if no resource compiler turns up: an icon-less
/// .exe is cosmetically wrong, an unbuildable one is broken.
fn embed_windows_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/castaway.rc");
    println!("cargo:rerun-if-changed=assets/castaway.ico");
    if let Err(e) =
        embed_resource::compile("assets/castaway.rc", embed_resource::NONE).manifest_optional()
    {
        println!("cargo:warning=castaway.exe ships without its icon: {e}");
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

/// `git rev-list --count HEAD` — the same number `self.revCount` is, and sound as an
/// ordering only because this repository commits linearly to `main`.
///
/// `None` on a dirty tree, matching `git_rev`'s `-dirty` suffix: the working copy is not
/// the commit it is counting, so the count is not a claim about these bytes. Also `None`
/// on a *shallow* clone, which answers with a small number that would look like an
/// ancient build — the release workflow checks out with `fetch-depth: 0` for exactly this
/// reason, and detecting it here costs one extra git call.
fn git_build_number() -> Option<String> {
    if Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .is_some_and(|o| !o.stdout.is_empty())
    {
        return None;
    }
    let shallow = Command::new("git")
        .args(["rev-parse", "--is-shallow-repository"])
        .output()
        .ok()?;
    if String::from_utf8_lossy(&shallow.stdout).trim() != "false" {
        return None;
    }
    let out = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let count = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!count.is_empty() && count != "0").then_some(count)
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
