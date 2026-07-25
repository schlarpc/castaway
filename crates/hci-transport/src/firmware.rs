//! Firmware images: where they come from and how they reach the loader.
//!
//! Embedded at build time rather than read from `/lib/firmware`, because the deploy
//! target is Windows and there is no such path there. `build.rs` copies whatever
//! directory Nix points it at into `OUT_DIR`; a build with no firmware directory still
//! compiles, and the loader says exactly which image was missing rather than failing
//! somewhere downstream (architecture-substrate.md §11.3b).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::TransportError;

/// One firmware image.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Firmware {
    /// Baked in at build time. What ships.
    Embedded(&'static [u8]),
    /// Read at runtime — for trying a newer blob without a rebuild.
    File(PathBuf),
}

impl Firmware {
    /// The image bytes.
    ///
    /// # Errors
    /// [`TransportError::Firmware`] if a file-backed image cannot be read.
    pub fn load(&self, name: &str) -> Result<std::borrow::Cow<'_, [u8]>, TransportError> {
        match self {
            Self::Embedded(bytes) => Ok(std::borrow::Cow::Borrowed(bytes)),
            Self::File(path) => std::fs::read(path)
                .map(std::borrow::Cow::Owned)
                .map_err(|e| TransportError::Firmware {
                    name: name.to_owned(),
                    detail: format!("reading {}: {e}", path.display()),
                }),
        }
    }
}

/// The images available to the loaders, by filename.
///
/// Keyed by the same names `linux-firmware` uses (`intel/ibt-20-1-3.sfi`,
/// `rtl_bt/rtl8761bu_fw.bin`), so a blob can be dropped in from a distribution tree
/// unchanged and the kernel's own naming stays the reference.
#[derive(Debug, Clone, Default)]
pub struct FirmwareSet {
    images: BTreeMap<String, Firmware>,
}

impl FirmwareSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The images `build.rs` embedded, if any.
    ///
    /// A build without a firmware directory yields an empty set rather than failing to
    /// compile — the failure belongs at the point someone plugs in a controller that
    /// needs an image we do not have, where it can name the missing file.
    #[must_use]
    pub fn embedded() -> Self {
        let mut set = Self::new();
        for (name, bytes) in crate::embedded::IMAGES {
            set.insert(*name, Firmware::Embedded(bytes));
        }
        set
    }

    /// Add an image.
    pub fn insert(&mut self, name: impl Into<String>, image: Firmware) {
        self.images.insert(name.into(), image);
    }

    /// Add an image, builder-style.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, image: Firmware) -> Self {
        self.insert(name, image);
        self
    }

    /// Load every file in `dir` whose name matches a firmware image, recursively.
    ///
    /// # Errors
    /// [`TransportError::Firmware`] if the directory cannot be walked.
    pub fn from_dir(dir: &Path) -> Result<Self, TransportError> {
        let mut set = Self::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let entries = std::fs::read_dir(&current).map_err(|e| TransportError::Firmware {
                name: current.display().to_string(),
                detail: e.to_string(),
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(rel) = path.strip_prefix(dir) {
                    set.insert(
                        rel.to_string_lossy().replace('\\', "/"),
                        Firmware::File(path),
                    );
                }
            }
        }
        Ok(set)
    }

    /// Fetch an image by name.
    ///
    /// # Errors
    /// [`TransportError::Firmware`] naming the missing file, so the message says what to
    /// go and find rather than that something went wrong.
    pub fn get(&self, name: &str) -> Result<std::borrow::Cow<'_, [u8]>, TransportError> {
        self.images
            .get(name)
            .ok_or_else(|| TransportError::Firmware {
                name: name.to_owned(),
                detail: "not present in this build; see architecture §11.3b".to_owned(),
            })?
            .load(name)
    }

    /// Whether an image is available.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.images.contains_key(name)
    }

    /// Every image name available.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.images.keys().map(String::as_str)
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_missing_image_names_the_file_it_wanted() {
        // The failure mode this exists to improve: "firmware missing" is useless,
        // "intel/ibt-20-1-3.sfi missing" tells you what to go and fetch.
        let set = FirmwareSet::new();
        let err = set.get("intel/ibt-20-1-3.sfi").unwrap_err();
        assert!(
            format!("{err}").contains("intel/ibt-20-1-3.sfi"),
            "got: {err}"
        );
    }

    #[test]
    fn an_embedded_image_round_trips() {
        let set = FirmwareSet::new().with(
            "rtl_bt/rtl8761bu_fw.bin",
            Firmware::Embedded(&[0xDE, 0xAD, 0xBE, 0xEF]),
        );
        assert!(set.has("rtl_bt/rtl8761bu_fw.bin"));
        assert_eq!(
            &*set.get("rtl_bt/rtl8761bu_fw.bin").unwrap(),
            &[0xDE, 0xAD, 0xBE, 0xEF]
        );
    }

    #[test]
    fn a_build_with_no_firmware_directory_yields_an_empty_set_not_a_panic() {
        // Compiling without blobs must work; the failure belongs at the point someone
        // plugs in a controller needing one.
        let _ = FirmwareSet::embedded();
    }

    #[test]
    fn a_directory_is_walked_with_linux_firmware_style_names() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("intel")).unwrap();
        std::fs::write(dir.join("intel/ibt-20-1-3.sfi"), b"fw").unwrap();
        std::fs::write(dir.join("intel/ibt-20-1-3.ddc"), b"ddc").unwrap();

        let set = FirmwareSet::from_dir(&dir).unwrap();
        let mut names: Vec<&str> = set.names().collect();
        names.sort_unstable();
        assert_eq!(names, ["intel/ibt-20-1-3.ddc", "intel/ibt-20-1-3.sfi"]);
        assert_eq!(&*set.get("intel/ibt-20-1-3.sfi").unwrap(), b"fw");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("castaway-fw-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
