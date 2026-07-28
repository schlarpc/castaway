//! Emits link directives for the moonlight-common-c static library built by
//! `nix/moonlight-common-c.nix`. The devShell and the crane builds export
//! `MOONLIGHT_COMMON_C_LIB_DIR`; when it is absent nothing is emitted, so `cargo check`
//! and rlib builds of the workspace still work — only a final artifact that actually
//! calls into the library will fail to link, with an unresolved-symbol error naming
//! `LiStartConnection`, which is the honest failure.

fn main() {
    println!("cargo:rerun-if-env-changed=MOONLIGHT_COMMON_C_LIB_DIR");
    let Ok(dirs) = std::env::var("MOONLIGHT_COMMON_C_LIB_DIR") else {
        return;
    };
    for dir in std::env::split_paths(&dirs) {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    println!("cargo:rustc-link-lib=static=moonlight-common-c");
    // enet is moonlight's bundled fork, built as its own static archive; nanors is
    // compiled directly into libmoonlight-common-c.a.
    println!("cargo:rustc-link-lib=static=enet");
    // PlatformCrypto.c uses OpenSSL's libcrypto (AES-GCM/CBC for control, audio, and
    // the encrypted-RTSP variant).
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows") {
        // OpenSSL's MSVC-style import/static library name; ws2_32/winmm are what the
        // CMake build links on WIN32. Exercised by the cross-build, not the dev box.
        println!("cargo:rustc-link-lib=libcrypto");
        println!("cargo:rustc-link-lib=ws2_32");
        println!("cargo:rustc-link-lib=winmm");
    } else {
        println!("cargo:rustc-link-lib=crypto");
    }
}
