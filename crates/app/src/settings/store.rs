//! Writing a setting back to the config file the operator wrote.
//!
//! The file is *theirs*: it carries comments, ordering and hand-formatting, and a
//! receiver that flattened all of that on every saved setting would teach people not to
//! comment their config. So this never round-trips through the `Config` structs —
//! `toml_edit` parses the file into a lossless document, one key is changed, and
//! everything else survives byte for byte.
//!
//! Writes are atomic (temp file + rename in the same directory), because the one moment
//! a panel is guaranteed to lose power is mid-write to the only file it boots from.

use std::path::PathBuf;

use crate::config::ConfigLocation;

/// Edits the receiver's own config file in place, preserving everything it does not
/// change.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

/// Why a setting could not be persisted.
///
/// These reach the panel — a settings screen that swallowed them would leave "why does
/// it forget my device on restart" with no answer — so the variants say which file and
/// what about it.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file exists but could not be read.
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file exists but is not valid TOML. Deliberately not "fixed" by rewriting:
    /// a file the operator broke is theirs to mend, not ours to clobber.
    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    /// A key on the way down is already something other than a table
    /// (`audio = true` when we need `[audio.output]`).
    #[error("config key `{key}` is not a table, so `{full}` cannot be written under it")]
    NotATable { key: String, full: String },
    /// The new file could not be written or moved into place.
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl ConfigStore {
    /// A store over an explicit file.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The store over the same file [`crate::config::Config::load_at`] loaded — the
    /// one resolved [`ConfigLocation`] both share. Neither the file nor the directory
    /// holding it needs to exist yet: the first saved setting creates both.
    #[must_use]
    pub fn at(location: &ConfigLocation) -> Self {
        Self::new(location.path())
    }

    /// The file being edited. (Error messages already name it; only tests need to ask.)
    #[cfg(test)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Set `path` (e.g. `["audio", "output", "pipewire"]`) to `value`, creating
    /// intermediate tables as needed, and rewrite the file atomically.
    ///
    /// Comments and formatting elsewhere are untouched. A comment sitting *above* the
    /// changed key survives too; an inline comment on the changed line itself belongs
    /// to the old value and goes with it.
    ///
    /// # Errors
    /// [`StoreError`] — unreadable, unparseable, a non-table in the way, or unwritable
    /// (a config generated into the read-only Nix store lands on that last one).
    pub fn set(&self, path: &[&str], value: impl Into<toml_edit::Value>) -> Result<(), StoreError> {
        let (last, parents) = match path {
            [] => return Ok(()),
            [parents @ .., last] => (*last, parents),
        };
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(StoreError::Read {
                    path: self.path.clone(),
                    source: e,
                })
            }
        };
        let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| StoreError::Parse {
            path: self.path.clone(),
            source: e,
        })?;

        let mut table = doc.as_table_mut();
        for key in parents {
            let created = !table.contains_key(key);
            let item = table.entry(key).or_insert(toml_edit::table());
            let full = path.join(".");
            let Some(inner) = item.as_table_mut() else {
                return Err(StoreError::NotATable {
                    key: (*key).to_owned(),
                    full,
                });
            };
            if created {
                // A table that exists only to hold deeper keys prints as
                // `[audio.output]`, not an empty `[audio]` header plus another.
                inner.set_implicit(true);
            }
            table = inner;
        }
        match table.get_mut(last) {
            // Replace the value in place: the key keeps its decor, which is where a
            // comment above the line lives.
            Some(item) => *item = toml_edit::value(value.into()),
            None => {
                table.insert(last, toml_edit::value(value.into()));
            }
        }

        self.write_atomically(&doc.to_string())
    }

    /// Write via a sibling temp file and rename, so the file is always either the old
    /// config or the new one, never a torn half.
    ///
    /// The directory is created first. On the deploy box it does not exist — the platform
    /// config location (`%LOCALAPPDATA%\castaway\config`) is somewhere nothing else in the
    /// app writes, so the very first saved setting is also what has to make the directory.
    /// Without this the temp write fails with "the system cannot find the path" and *every*
    /// setting changed at the panel reverts on the next restart (#179).
    fn write_atomically(&self, contents: &str) -> Result<(), StoreError> {
        let write_err = |source| StoreError::Write {
            path: self.path.clone(),
            source,
        };
        if let Some(dir) = self.path.parent() {
            // An empty parent is a bare filename in the working directory, which needs no
            // creating; `create_dir_all` is fine with a directory that already exists.
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(write_err)?;
            }
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, contents).map_err(write_err)?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            // Leave no droppings next to a config we failed to replace.
            let _ = std::fs::remove_file(&tmp);
            write_err(e)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A store over a fresh file in the OS temp dir, cleaned up by the guard.
    fn scratch(name: &str) -> (Cleanup, ConfigStore) {
        let path =
            std::env::temp_dir().join(format!("castaway-store-{}-{name}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Cleanup(path.clone()), ConfigStore::new(path))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_changed_key_leaves_every_comment_and_blank_line_alone() {
        // The property the whole module exists for. The fixture is the kind of file an
        // operator actually writes: comments above keys, inline comments elsewhere,
        // deliberate blank lines, their own ordering.
        let original = "\
# The name senders see.
friendly_name = \"Lab TV\"   # hand-tuned

http_port = 9090

[enable]
# Miracast takes the radio into group-owner mode; our uplink is Ethernet, so fine.
miracast = true

[audio.output]
# The USB DAC on the shelf.
pipewire = \"alsa_output.usb-old.analog-stereo\"
";
        let (_guard, store) = scratch("preserve");
        std::fs::write(store.path(), original).unwrap();

        store
            .set(
                &["audio", "output", "pipewire"],
                "alsa_output.usb-new.iec958-stereo",
            )
            .unwrap();

        let after = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(
            after,
            original.replace(
                "alsa_output.usb-old.analog-stereo",
                "alsa_output.usb-new.iec958-stereo"
            ),
            "everything but the one value must survive byte for byte"
        );
    }

    #[test]
    fn a_missing_section_is_created_without_reshaping_the_rest() {
        let original = "# Everything below is deliberate.\nfriendly_name = \"Lab TV\"\n";
        let (_guard, store) = scratch("create-section");
        std::fs::write(store.path(), original).unwrap();

        store
            .set(&["audio", "output", "windows"], "Speakers")
            .unwrap();

        let after = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            after.starts_with(original),
            "the existing file is a prefix, untouched"
        );
        assert!(
            after.contains("[audio.output]"),
            "the new table is explicit at its own depth, not an empty [audio] plus one:\n{after}"
        );
        // And what we wrote parses back into the real config type.
        let parsed: crate::config::Config = toml::from_str(&after).unwrap();
        assert_eq!(
            parsed.audio.output.windows,
            crate::config::OutputChoice::Device("Speakers".into())
        );
    }

    #[test]
    fn the_first_saved_setting_creates_the_file() {
        let (_guard, store) = scratch("fresh");
        store
            .set(&["audio", "output", "pipewire"], "default")
            .unwrap();
        let parsed: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(
            parsed.audio.output.pipewire,
            crate::config::OutputChoice::Default
        );
    }

    /// The panel's own case, and the one every fixture above misses by living in a
    /// directory that already exists: the platform config location is somewhere nothing
    /// else in the app writes, so the first saved setting has to make the directory as
    /// well as the file. Before this, the temp write failed with "the system cannot find
    /// the path" and the setting was applied at runtime and gone on the next start (#179).
    #[test]
    fn the_first_saved_setting_creates_the_directory_as_well_as_the_file() {
        let root =
            std::env::temp_dir().join(format!("castaway-store-{}-nodir", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let _guard = DirCleanup(root.clone());
        // Two levels deep, because the real path is `…\castaway\config\castaway.toml` and
        // neither component need exist.
        let store = ConfigStore::new(root.join("castaway").join("config").join("castaway.toml"));

        store
            .set(&["audio", "output", "windows"], "Speakers")
            .unwrap();

        let parsed: crate::config::Config =
            toml::from_str(&std::fs::read_to_string(store.path()).unwrap()).unwrap();
        assert_eq!(
            parsed.audio.output.windows,
            crate::config::OutputChoice::Device("Speakers".into())
        );
    }

    struct DirCleanup(PathBuf);
    impl Drop for DirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_key_that_is_not_a_table_is_an_error_not_a_clobber() {
        let (_guard, store) = scratch("not-a-table");
        std::fs::write(store.path(), "audio = true\n").unwrap();
        let err = store
            .set(&["audio", "output", "pipewire"], "x")
            .unwrap_err();
        assert!(matches!(err, StoreError::NotATable { .. }), "{err}");
        // The operator's file is exactly as they left it.
        assert_eq!(
            std::fs::read_to_string(store.path()).unwrap(),
            "audio = true\n"
        );
    }

    #[test]
    fn a_broken_file_is_reported_not_rewritten() {
        let (_guard, store) = scratch("broken");
        std::fs::write(store.path(), "this is not toml [").unwrap();
        let err = store
            .set(&["audio", "output", "pipewire"], "x")
            .unwrap_err();
        assert!(matches!(err, StoreError::Parse { .. }), "{err}");
        assert_eq!(
            std::fs::read_to_string(store.path()).unwrap(),
            "this is not toml ["
        );
    }

    #[test]
    fn rewriting_an_existing_key_twice_is_stable() {
        // The settings screen will do this all day; the file must not accrete.
        let (_guard, store) = scratch("stable");
        store.set(&["audio", "output", "alsa"], "front").unwrap();
        let once = std::fs::read_to_string(store.path()).unwrap();
        store.set(&["audio", "output", "alsa"], "front").unwrap();
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), once);
    }
}
