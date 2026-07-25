//! Embeds controller firmware at build time.
//!
//! Windows has no `/lib/firmware`, so blobs travel inside the binary. Nix points
//! `CASTAWAY_FIRMWARE_DIR` at a tree laid out the way `linux-firmware` is; a build
//! without it produces an empty table and compiles fine, because the right place to fail
//! is when someone plugs in a controller that needs an image we do not have — where the
//! error can name the file.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CASTAWAY_FIRMWARE_DIR");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is always set"));
    let dest = out.join("firmware.rs");

    let Some(dir) = std::env::var_os("CASTAWAY_FIRMWARE_DIR").map(PathBuf::from) else {
        std::fs::write(&dest, "pub const IMAGES: &[(&str, &[u8])] = &[];\n")
            .expect("writing the empty firmware table");
        return;
    };
    println!("cargo:rerun-if-changed={}", dir.display());

    let mut entries = Vec::new();
    collect(&dir, &dir, &mut entries);
    entries.sort();

    let mut src = String::from("pub const IMAGES: &[(&str, &[u8])] = &[\n");
    for (name, path) in &entries {
        src.push_str(&format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            path.display().to_string()
        ));
    }
    src.push_str("];\n");
    std::fs::write(&dest, src).expect("writing the firmware table");
}

/// Walk `dir`, recording each file under its `linux-firmware`-style relative name.
fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            // Licence files travel with the blobs but are not images.
            let name = rel.to_string_lossy().replace('\\', "/");
            if !name.to_ascii_uppercase().contains("LICEN") {
                out.push((name, path));
            }
        }
    }
}
