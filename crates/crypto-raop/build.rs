//! Point the crate at the AirPort Express key without keeping a copy of it.
//!
//! AirPlay 1 cannot be spoken without this key, and every implementation carries it —
//! but there is no reason for this repository to be one more copy. `nix/airport-key.nix`
//! takes it out of shairplay's source, which nixpkgs already pins, and names the file
//! here. A build without it compiles and fails closed at the point of use, the same way
//! the Cast carves do.

use std::path::{Path, PathBuf};

const VAR: &str = "CASTAWAY_AIRPORT_KEY_FILE";

fn main() {
    println!("cargo:rerun-if-env-changed={VAR}");
    println!("cargo:rustc-check-cfg=cfg(airport_key)");

    let body = match std::env::var_os(VAR) {
        None => "None".to_owned(),
        Some(p) => {
            let p = PathBuf::from(p);
            assert!(
                p.is_file(),
                "{VAR} points at {}, which is not a file",
                p.display()
            );
            println!("cargo:rustc-cfg=airport_key");
            println!("cargo:rerun-if-changed={}", p.display());
            format!("Some(include_str!(r\"{}\"))", p.display())
        }
    };

    let out = Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("airport_key.rs");
    std::fs::write(
        &out,
        format!(
            "/// The AirPort Express key, or `None` on a build that was not given it.\n\
             const AIRPORT_PEM: Option<&str> = {body};\n"
        ),
    )
    .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}
