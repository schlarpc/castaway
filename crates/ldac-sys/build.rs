//! Emits link directives for `libldacBT`, the LDAC codec library from nixpkgs' `ldacbt`.
//! The devShell and the crane builds export `LDACBT_LIB_DIR`; when it is absent nothing
//! is emitted, so `cargo check` and rlib builds of the workspace still work — only a
//! final artifact that actually calls into the library will fail to link, with an
//! unresolved-symbol error naming `ldacBT_decode`, which is the honest failure. Same
//! shape as `moonlight-sys/build.rs`, for the same reason.
//!
//! Dynamic, not static: `ldacbt` ships `libldacBT.so` and no archive.

fn main() {
    println!("cargo:rerun-if-env-changed=LDACBT_LIB_DIR");
    let Ok(dirs) = std::env::var("LDACBT_LIB_DIR") else {
        return;
    };
    for dir in std::env::split_paths(&dirs) {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    println!("cargo:rustc-link-lib=dylib=ldacBT");
}
