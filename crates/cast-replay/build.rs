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

    let out = Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("airserver_kek.rs");
    std::fs::write(
        &out,
        format!(
            "/// The carved constants, or `None` when this build was not given them.\n\
             pub(crate) const PROVISIONED: Option<Kek<'static>> = {provisioned};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}
