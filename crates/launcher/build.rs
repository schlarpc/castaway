//! Embeds assets/launcher.rc — the `.exe` icon, and nothing else — when the *target*
//! is Windows. The launcher is what a desktop shortcut on the box points at, so it is
//! the binary whose icon a person actually sees; without this it wears the generic
//! executable glyph while `castaway.exe` beside it carries the brand.
//!
//! Keyed on `CARGO_CFG_TARGET_OS` rather than `cfg(windows)` because the Windows binary
//! is cross-built from Linux (docs/cross-build.md), where build.rs itself runs as a
//! Linux program; `embed-resource` finds `llvm-rc`, which nix/windows.nix puts on the
//! PATH for exactly this. A warning rather than a failure if no resource compiler
//! turns up: an icon-less .exe is cosmetically wrong, an unbuildable one is broken.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=assets/launcher.rc");
    println!("cargo:rerun-if-changed=../app/assets/castaway.ico");
    if let Err(e) =
        embed_resource::compile("assets/launcher.rc", embed_resource::NONE).manifest_optional()
    {
        println!("cargo:warning=launcher.exe ships without its icon: {e}");
    }
}
