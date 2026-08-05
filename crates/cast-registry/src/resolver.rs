//! The resolver: the one module here that touches a socket or the filesystem
//! (ground rule 3). What it *decides* is decided by [`crate::entry`] and
//! [`crate::cache`], which are pure.
//!
//! ## Resolution order, and why stale beats failed
//!
//! memory → disk → network, and on a network failure, **back to disk even when the entry
//! is old**. A receiver page's URL changes at Google's pace, which is years; the uplink
//! on a hackerspace panel drops at the pace of somebody unplugging a switch. Preferring a
//! stale answer to no answer is therefore right nearly always, and the failure it risks —
//! loading a URL that has moved — is visible on the panel, where a launch that refuses
//! with no explanation is not.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::cache::Cache;
use crate::{entry, AppSurface, RegistryError, DEFAULT_ENDPOINT};

/// How long a single lookup may take. A sender is waiting on the `LAUNCH` this resolves,
/// so the bound is short: a slow answer and no answer look the same from the room, and
/// the cache is usually holding the answer anyway.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on a registry response. Real entries are a few KB; the largest observed
/// (YouTube's, with its promo artwork in a dozen locales) is 4 KB.
const MAX_BODY: u64 = 256 * 1024;

/// Resolves `appId`s to receiver pages, and remembers what it learned.
pub struct Registry {
    endpoint: String,
    cache_path: Option<PathBuf>,
    /// Everything resolved this run, plus whatever was on disk at startup.
    memory: Mutex<Cache>,
    /// Whether a lookup may go to the network at all. Off in tests that must not
    /// depend on the internet, and off on a panel configured for no uplink.
    network: bool,
    timeout: Duration,
}

impl Registry {
    /// A registry against the real endpoint, caching under the platform cache directory.
    #[must_use]
    pub fn new() -> Self {
        let path = crate::cache::default_path();
        Self::with_cache_path(Some(path))
    }

    /// A registry caching at `path` (or nowhere, with [`None`]).
    #[must_use]
    pub fn with_cache_path(cache_path: Option<PathBuf>) -> Self {
        let memory = cache_path.as_deref().map_or_else(Cache::new, Cache::load);
        if !memory.is_empty() {
            debug!(
                known = memory.len(),
                "cast registry: resolutions from cache"
            );
        }
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            cache_path,
            memory: Mutex::new(memory),
            network: true,
            timeout: LOOKUP_TIMEOUT,
        }
    }

    /// Point lookups at a different endpoint — a local server standing in for Google's.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Allow or forbid network lookups. Forbidden means cache-only.
    #[must_use]
    pub const fn with_network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    /// Set the per-lookup timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Seed a resolution without asking anybody. For tests, and for the two ids this
    /// receiver serves natively.
    pub fn preload(&self, app_id: &str, surface: &AppSurface) {
        if let Ok(mut cache) = self.memory.lock() {
            cache.put(app_id, surface);
        }
    }

    /// Every resolution held, as `(app_id, is_a_page)`.
    ///
    /// Exists for callers on a message path, which must not make a lookup: this is the
    /// whole of what is knowable for free, and it is small — a panel resolves a handful
    /// of applications in its life.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, bool)> {
        self.memory.lock().map(|c| c.snapshot()).unwrap_or_default()
    }

    /// What is known about `app_id` without a lookup.
    #[must_use]
    pub fn cached(&self, app_id: &str) -> Option<AppSurface> {
        self.memory.lock().ok().and_then(|c| c.get(app_id))
    }

    /// Resolve `app_id` to the surface that serves it.
    ///
    /// # Errors
    /// [`RegistryError::NotAnAppId`] for a malformed id (no request is made);
    /// [`RegistryError::Lookup`] when the network failed *and* nothing was cached;
    /// [`RegistryError::NotRegistryJson`] when the registry does not have the app.
    pub async fn resolve(&self, app_id: &str) -> Result<AppSurface, RegistryError> {
        if !crate::is_app_id(app_id) {
            return Err(RegistryError::NotAnAppId(app_id.to_owned()));
        }
        if let Some(hit) = self.cached(app_id) {
            debug!(%app_id, "cast registry: resolved from cache");
            return Ok(hit);
        }
        if !self.network {
            return Err(RegistryError::Lookup {
                app_id: app_id.to_owned(),
                reason: "network lookups are disabled and nothing is cached".to_owned(),
            });
        }

        let url = format!("{}?a={app_id}", self.endpoint);
        let timeout = self.timeout;
        // ureq is blocking, so it does not belong on the runtime (ground rule 4).
        let fetched = tokio::task::spawn_blocking(move || fetch_blocking(&url, timeout))
            .await
            .map_err(|e| RegistryError::Lookup {
                app_id: app_id.to_owned(),
                reason: format!("joining the lookup: {e}"),
            })?;

        let body = match fetched {
            Ok(body) => body,
            Err(reason) => {
                // Nothing cached, or we would have returned above. This is the honest
                // failure: the panel has never seen this app and cannot reach anyone
                // who knows about it.
                return Err(RegistryError::Lookup {
                    app_id: app_id.to_owned(),
                    reason,
                });
            }
        };

        let surface = entry::parse(&body)?;
        info!(
            %app_id,
            name = surface.display_name().unwrap_or("?"),
            page = surface.page_url().unwrap_or("<native>"),
            "cast registry: resolved"
        );
        self.remember(app_id, &surface).await;
        Ok(surface)
    }

    /// Store a resolution in memory, and on disk if it changed anything.
    async fn remember(&self, app_id: &str, surface: &AppSurface) {
        let snapshot = {
            let Ok(mut cache) = self.memory.lock() else {
                return;
            };
            if !cache.put(app_id, surface) {
                return;
            }
            cache.clone()
        };
        let Some(path) = self.cache_path.clone() else {
            return;
        };
        // Writing is blocking and nobody is waiting on it: the resolution is already
        // returned by the time this runs.
        let written = tokio::task::spawn_blocking(move || snapshot.store(&path)).await;
        match written {
            Ok(Err(e)) => warn!(error = %e, "cast registry: could not cache the resolution"),
            Err(e) => warn!(error = %e, "cast registry: joining the cache write"),
            Ok(Ok(())) => {}
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// One lookup. Blocking; belongs on `spawn_blocking` (ground rule 4).
///
/// Errors come back as strings rather than as `ureq::Error`, which is large and would
/// have to cross a thread boundary to be of any use.
fn fetch_blocking(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read as _;

    let agent = ureq::builder().timeout(timeout).build();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    Ok(body)
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

    #[tokio::test]
    async fn a_malformed_app_id_never_reaches_the_network() {
        // The endpoint is deliberately unroutable: if the guard failed to short-circuit,
        // this would hang for the timeout and then fail with a different error.
        let registry = Registry::with_cache_path(None).with_endpoint("http://127.0.0.1:1/nope");
        let err = registry.resolve("../../etc/passwd").await.unwrap_err();
        assert!(matches!(err, RegistryError::NotAnAppId(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_cached_resolution_is_served_without_a_lookup() {
        let registry = Registry::with_cache_path(None)
            .with_endpoint("http://127.0.0.1:1/nope")
            .with_timeout(Duration::from_millis(50));
        registry.preload("CC1AD845", &web("https://receiver.example/app.html"));
        let surface = registry.resolve("cc1ad845").await.unwrap();
        assert_eq!(
            surface.page_url(),
            Some("https://receiver.example/app.html")
        );
    }

    #[tokio::test]
    async fn with_no_network_and_no_cache_the_failure_says_so() {
        let registry = Registry::with_cache_path(None).with_network(false);
        let err = registry.resolve("233637DE").await.unwrap_err();
        assert!(matches!(err, RegistryError::Lookup { .. }), "{err:?}");
    }

    /// The property the cache exists for: a panel that resolved an app yesterday and
    /// lost its uplink overnight still launches that app.
    #[tokio::test]
    async fn a_resolution_outlives_the_uplink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");

        // Yesterday: resolved and cached. `remember` is what a successful lookup
        // calls, so this is that path and not a shortcut around it.
        {
            let registry = Registry::with_cache_path(Some(path.clone()));
            registry
                .remember("233637DE", &web("https://www.youtube.com/tv?castv=2.0"))
                .await;
        }

        // Today: no uplink at all, and the endpoint would refuse instantly.
        let registry = Registry::with_cache_path(Some(path))
            .with_endpoint("http://127.0.0.1:1/nope")
            .with_timeout(Duration::from_millis(50));
        let surface = registry.resolve("233637DE").await.unwrap();
        assert_eq!(
            surface.page_url(),
            Some("https://www.youtube.com/tv?castv=2.0")
        );
    }

    #[tokio::test]
    async fn a_lookup_that_cannot_connect_reports_the_app_id_it_failed_on() {
        let registry = Registry::with_cache_path(None)
            .with_endpoint("http://127.0.0.1:1/nope")
            .with_timeout(Duration::from_millis(100));
        let err = registry.resolve("9AC194DC").await.unwrap_err();
        match err {
            RegistryError::Lookup { app_id, .. } => assert_eq!(app_id, "9AC194DC"),
            other => panic!("{other:?}"),
        }
    }
}
