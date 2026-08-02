//! Hand the AirServer KEK constants to the crate without writing them down.
//!
//! The two BLAKE2b constants that open an AirServer credential database are App
//! Dynamic's, not ours, so they are not literals in this tree. `nix/airserver-carve.nix`
//! recovers them from a pinned installer at build time and points these two variables at
//! the result; a build without them still compiles and fails closed at the point of use
//! (`ReplayError::NoKek`), which is what keeps `cargo build` working on a machine that
//! has never seen the installer.
//!
//! Files rather than inline values so the constants never appear in a process listing,
//! a build log, or `ps` output on a shared builder.

use std::path::{Path, PathBuf};

const PERSON_VAR: &str = "CASTAWAY_AIRSERVER_KEK_PERSON_FILE";
const PASS_VAR: &str = "CASTAWAY_AIRSERVER_KEK_PASS_FILE";

/// BLAKE2b's personalisation and key limits. A carve that violates either would
/// otherwise surface as a panic deep inside the hasher at runtime.
const MAX_PERSON: usize = 16;
const MAX_KEY: usize = 64;

fn read_bounded(var: &str, limit: usize) -> Option<Vec<u8>> {
    let path = std::env::var_os(var)?;
    let path = PathBuf::from(path);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{var} points at {}, which could not be read: {e}",
            path.display()
        )
    });
    assert!(
        !bytes.is_empty() && bytes.len() <= limit,
        "{var} holds {} bytes; BLAKE2b takes 1..={limit}",
        bytes.len()
    );
    Some(bytes)
}

fn render(bytes: &[u8]) -> String {
    let body = bytes.iter().map(|b| format!("{b}, ")).collect::<String>();
    format!("&[{body}]")
}

fn main() {
    println!("cargo:rerun-if-env-changed={PERSON_VAR}");
    println!("cargo:rerun-if-env-changed={PASS_VAR}");

    let person = read_bounded(PERSON_VAR, MAX_PERSON);
    let pass = read_bounded(PASS_VAR, MAX_KEY);

    let provisioned = match (person, pass) {
        (Some(person), Some(pass)) => format!(
            "Some(Kek {{ person: {}, pass: {} }})",
            render(&person),
            render(&pass)
        ),
        (None, None) => "None".to_owned(),
        // Half a carve is a broken build environment, not a reason to fall back: a
        // receiver that silently loses its AirServer identity looks like a protocol
        // bug months later.
        _ => panic!("exactly one of {PERSON_VAR} and {PASS_VAR} is set; set both or neither"),
    };

    let outdir = Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR")).to_path_buf();
    let out = outdir.join("airserver_kek.rs");
    std::fs::write(
        &out,
        format!(
            "/// The carved constants, or `None` when this build was not given them.\n\
             pub(crate) const PROVISIONED: Option<Kek<'static>> = {provisioned};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));

    airserver_identity(&outdir);
}

/// The AirServer identity fixtures, `include_bytes!`'d from wherever the carve put them.
///
/// Absolute paths rather than copies: the bytes are App Dynamic's, and the one place
/// they are allowed to exist is the carve derivation's output. Emitting `None` when the
/// variable is unset is what lets a plain `cargo build` still compile.
fn airserver_identity(outdir: &Path) {
    const VAR: &str = "CASTAWAY_AIRSERVER_CARVE";
    println!("cargo:rerun-if-env-changed={VAR}");

    // A cfg as well as the constant, so the tests that need a real identity can be
    // gated on it rather than failing on a build that legitimately has none.
    println!("cargo:rustc-check-cfg=cfg(airserver_identity)");

    let body = match std::env::var_os(VAR) {
        None => "None".to_owned(),
        Some(dir) => {
            println!("cargo:rustc-cfg=airserver_identity");
            let dir = PathBuf::from(dir);
            let f = |name: &str| {
                let p = dir.join(name);
                assert!(p.is_file(), "{VAR} is set but {} is missing", p.display());
                println!("cargo:rerun-if-changed={}", p.display());
                format!("include_bytes!(r\"{}\")", p.display())
            };
            format!(
                "Some(BundledIdentity {{\n    \
                   device_cert_der: {},\n    \
                   chain_der: [{}, {}],\n    \
                   peer_key_der: {},\n    \
                   peer_certs: {},\n    \
                   signatures_sha1: {},\n    \
                   signatures_sha256: {},\n}})",
                f("airserver_device_crt.der"),
                f("airserver_chain0.der"),
                f("airserver_chain1.der"),
                f("airserver_peer_key.der"),
                f("airserver_peer_certs.bin"),
                f("airserver_sha1.bin"),
                f("airserver_sha256.bin"),
            )
        }
    };

    let out = outdir.join("airserver_identity.rs");
    std::fs::write(
        &out,
        format!(
            "/// The carved AirServer identity, or `None` on a build without the carve.\n\
             pub(crate) const BUNDLED: Option<BundledIdentity> = {body};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}
