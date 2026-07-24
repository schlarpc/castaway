//! Fetch + cache EasyList for the ad blocker. On startup we try to download the current
//! EasyList, cache it to disk, and build an [`AdBlocker`] from it. If the network is down
//! we fall back to the cached copy, and if there's no cache, to the compact built-in list
//! ([`AdBlocker::with_defaults`]). This keeps the kiosk ad-blocking even offline while
//! staying current when online (chosen policy — OPEN-QUESTIONS Q17).

use std::path::Path;
use std::time::Duration;

use tracing::{info, warn};

use crate::cef_adblock::AdBlocker;

/// The canonical EasyList URL.
pub const EASYLIST_URL: &str = "https://easylist.to/easylist/easylist.txt";

/// Build an [`AdBlocker`], preferring a fresh fetch of `url` (cached to `cache_path`),
/// then the cached copy, then the compact built-in list.
#[must_use]
pub fn load_or_fetch(url: &str, cache_path: &Path) -> AdBlocker {
    match fetch(url) {
        Ok(text) if text.len() > 1024 => {
            if let Some(dir) = cache_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(cache_path, &text) {
                warn!(target: "castaway::adblock", error = %e, "could not cache EasyList");
            }
            info!(target: "castaway::adblock", bytes = text.len(), "fetched EasyList");
            return AdBlocker::from_list_text(&text);
        }
        Ok(_) => warn!(target: "castaway::adblock", "EasyList fetch returned too little data"),
        Err(e) => warn!(target: "castaway::adblock", error = %e, "EasyList fetch failed"),
    }
    if let Ok(text) = std::fs::read_to_string(cache_path) {
        info!(target: "castaway::adblock", path = %cache_path.display(), "using cached EasyList");
        return AdBlocker::from_list_text(&text);
    }
    warn!(target: "castaway::adblock", "no EasyList available; using compact built-in list");
    AdBlocker::with_defaults()
}

/// The default cache location under the temp dir.
#[must_use]
pub fn default_cache_path() -> std::path::PathBuf {
    std::env::temp_dir().join("castaway-easylist.txt")
}

fn fetch(url: &str) -> Result<String, String> {
    let body = ureq::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn falls_back_to_cache_when_fetch_fails() {
        // Point at an unreachable URL but provide a cache file → uses the cache.
        let dir = std::env::temp_dir().join("castaway-easylist-test");
        std::fs::create_dir_all(&dir).unwrap();
        let cache = dir.join("list.txt");
        std::fs::write(&cache, "||tracker.example^\n").unwrap();
        let ab = load_or_fetch("http://127.0.0.1:1/nope", &cache);
        assert!(ab.should_block(
            "https://tracker.example/x.js",
            "https://site.test/",
            "script"
        ));
    }

    #[test]
    fn falls_back_to_defaults_when_no_cache() {
        let missing = std::env::temp_dir().join("castaway-easylist-does-not-exist-xyz.txt");
        let _ = std::fs::remove_file(&missing);
        let ab = load_or_fetch("http://127.0.0.1:1/nope", &missing);
        // The compact built-in list blocks googletagmanager.
        assert!(ab.should_block(
            "https://www.googletagmanager.com/gtm.js",
            "https://site.test/",
            "script"
        ));
    }
}
