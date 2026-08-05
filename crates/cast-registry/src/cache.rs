//! The on-disk resolution cache (pure apart from the two file calls at the bottom).
//!
//! One JSON file holding every app id the panel has ever resolved. Small — a resolution
//! is a name and a URL — and worth keeping indefinitely rather than expiring, because an
//! entry too old to trust is still better than nothing when the uplink is down.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{AppSurface, RegistryError};

/// Filename under the cache directory.
pub const CACHE_FILE: &str = "cast-app-registry.json";

/// The default path: `<cache>/cast-app-registry.json`.
#[must_use]
pub fn default_path() -> PathBuf {
    castaway_paths::host().cache().join(CACHE_FILE)
}

/// Bumped if the stored shape changes, so an old file is ignored rather than misread.
const VERSION: u32 = 1;

/// How a resolution is stored. Flattened deliberately: the file is meant to be readable
/// by whoever is standing in front of a panel that launched the wrong thing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredSurface {
    /// A web receiver.
    Web {
        /// Where the page lives.
        url: String,
        /// The registry's name for it.
        display_name: String,
    },
    /// A native application — hostable, but not by a browser.
    Native {
        /// The registry's name for it.
        display_name: String,
    },
}

impl From<&AppSurface> for Option<StoredSurface> {
    fn from(surface: &AppSurface) -> Self {
        match surface {
            AppSurface::Web { url, display_name } => Some(StoredSurface::Web {
                url: url.clone(),
                display_name: display_name.clone(),
            }),
            AppSurface::Native { display_name } => Some(StoredSurface::Native {
                display_name: display_name.clone(),
            }),
            // Absence is not cached. The registry gaining an app is the *expected*
            // direction of change, and a cached "no" would outlive it silently.
            AppSurface::Absent => None,
        }
    }
}

impl From<StoredSurface> for AppSurface {
    fn from(stored: StoredSurface) -> Self {
        match stored {
            StoredSurface::Web { url, display_name } => Self::Web { url, display_name },
            StoredSurface::Native { display_name } => Self::Native { display_name },
        }
    }
}

/// The file's contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    version: u32,
    /// Keyed by uppercase app id, ordered so the file diffs cleanly.
    entries: BTreeMap<String, StoredSurface>,
}

impl Cache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: VERSION,
            entries: BTreeMap::new(),
        }
    }

    /// What this cache knows about `app_id`, if anything.
    #[must_use]
    pub fn get(&self, app_id: &str) -> Option<AppSurface> {
        self.entries
            .get(&app_id.to_ascii_uppercase())
            .cloned()
            .map(Into::into)
    }

    /// Record a resolution. Returns whether anything changed, so a resolution that
    /// merely confirmed what was already stored does not rewrite the file.
    pub fn put(&mut self, app_id: &str, surface: &AppSurface) -> bool {
        let Some(stored) = Option::<StoredSurface>::from(surface) else {
            return false;
        };
        let key = app_id.to_ascii_uppercase();
        if self.entries.get(&key) == Some(&stored) {
            return false;
        }
        self.entries.insert(key, stored);
        true
    }

    /// How many resolutions are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read a cache from `path`. A missing file is an empty cache, not an error — the
    /// first run of a new panel is the common case, not a fault.
    ///
    /// A file that exists and does not parse, or that carries a version this build does
    /// not know, is also an empty cache: the alternative is a panel that will not launch
    /// anything because of a corrupt convenience file.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::new();
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(cache) if cache.version == VERSION => cache,
            Ok(cache) => {
                tracing::debug!(
                    path = %path.display(),
                    found = cache.version,
                    expected = VERSION,
                    "ignoring a registry cache written by another version"
                );
                Self::new()
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "ignoring an unreadable registry cache");
                Self::new()
            }
        }
    }

    /// Write the cache to `path`, creating the directory if it is not there.
    ///
    /// # Errors
    /// [`RegistryError::Cache`] if the directory or the file cannot be written.
    pub fn store(&self, path: &Path) -> Result<(), RegistryError> {
        let err = |source| RegistryError::Cache {
            path: path.to_path_buf(),
            source,
        };
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).map_err(err)?;
            }
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| err(std::io::Error::other(e.to_string())))?;
        std::fs::write(path, text).map_err(err)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn web(url: &str) -> AppSurface {
        AppSurface::Web {
            url: url.into(),
            display_name: "App".into(),
        }
    }

    #[test]
    fn a_resolution_survives_a_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(CACHE_FILE);
        let mut cache = Cache::new();
        assert!(cache.put("233637de", &web("https://www.youtube.com/tv?castv=2.0")));
        cache.store(&path).unwrap();

        // Looked up in the other case to the one it was stored in, because senders are
        // not consistent about it and the file is keyed by one of them.
        let read = Cache::load(&path);
        assert_eq!(
            read.get("233637DE").unwrap().page_url(),
            Some("https://www.youtube.com/tv?castv=2.0")
        );
    }

    #[test]
    fn storing_the_same_resolution_twice_reports_no_change() {
        let mut cache = Cache::new();
        assert!(cache.put("CC1AD845", &web("https://x/")));
        assert!(!cache.put("CC1AD845", &web("https://x/")));
        assert!(
            cache.put("CC1AD845", &web("https://y/")),
            "a moved url is a change"
        );
    }

    /// The registry gaining an app is the expected direction of change. A cached "this
    /// app does not exist" would outlive that and there would be no way to notice.
    #[test]
    fn absence_is_never_cached() {
        let mut cache = Cache::new();
        assert!(!cache.put("DEADBEEF", &AppSurface::Absent));
        assert!(cache.is_empty());
    }

    #[test]
    fn a_corrupt_or_foreign_cache_is_ignored_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);

        std::fs::write(&path, "{ this is not json").unwrap();
        assert!(Cache::load(&path).is_empty());

        std::fs::write(
            &path,
            r#"{"version":99,"entries":{"CC1AD845":{"kind":"native","display_name":"X"}}}"#,
        )
        .unwrap();
        assert!(
            Cache::load(&path).is_empty(),
            "a future version's file must not be half-read"
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_cache_not_an_error() {
        assert!(Cache::load(Path::new("/nonexistent/nowhere/x.json")).is_empty());
    }

    /// The distinction the browser depends on has to survive storage: a native app read
    /// back as a web one would send a mirroring session to a page.
    #[test]
    fn native_and_web_stay_distinct_across_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let mut cache = Cache::new();
        cache.put(
            "0F5096E8",
            &AppSurface::Native {
                display_name: "Chrome Mirroring".into(),
            },
        );
        cache.put("CC1AD845", &web("https://x/"));
        cache.store(&path).unwrap();

        let read = Cache::load(&path);
        assert_eq!(read.get("0F5096E8").unwrap().page_url(), None);
        assert_eq!(read.get("CC1AD845").unwrap().page_url(), Some("https://x/"));
        assert_eq!(read.len(), 2);
    }
}
