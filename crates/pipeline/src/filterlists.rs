//! Subscribing to filter lists, and to the scriptlet bundle that makes half of them work.
//!
//! Two things are fetched, and both are needed for uBlock Origin's rules to mean anything:
//!
//! - **Filter lists** — EasyList (network rules) and uBO's own list, which is mostly
//!   *cosmetic* rules including `##+js(...)` scriptlet injections. Concatenated into one
//!   engine, because that is how a browser with several subscriptions behaves.
//! - **The scriptlet bundle** — uBO's `scriptlets.js`. A `##+js(name, args)` rule names
//!   code and its arguments; the code itself lives here. Without the bundle those rules
//!   parse and then do nothing, which is the quietest possible failure.
//!
//! Same policy as before for each (OPEN-QUESTIONS Q17): fetch → cache → fall back to the
//! cache → fall back to the compact built-in list. A kiosk with no network still blocks
//! ads; a kiosk with network stays current without a redeploy.
//!
//! **Licensing.** uBO's scriptlet bodies are GPLv3. They are fetched at runtime into a
//! cache directory, never vendored into this repository or linked into the binary, which
//! keeps them a thing the operator's machine downloads rather than a thing we distribute.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use adblock::resources::Resource;
use tracing::{info, warn};

use crate::cef_adblock::AdBlocker;

/// The canonical EasyList URL — network-level rules.
pub const EASYLIST_URL: &str = "https://easylist.to/easylist/easylist.txt";

/// uBlock Origin's own filters: where the `##+js(...)` rules live.
pub const UBO_FILTERS_URL: &str = "https://ublockorigin.github.io/uAssets/filters/filters.txt";

/// uBlock Origin's `src/js/`, tracking `master` like the filters do.
///
/// Module paths are relative to *here*, not to `resources/`, because the graph does not
/// stay inside it — `resources/href-sanitizer.js` imports `../urlskip.js`, and a scriptlet
/// whose dependency failed to load is one the engine will not inject.
pub const UBO_SOURCE_BASE: &str = "https://raw.githubusercontent.com/gorhill/uBlock/master/src/js/";

/// How many modules to follow before deciding the import graph is not what we think it is.
const MAX_MODULES: usize = 200;

/// Where the fetched lists are cached.
#[derive(Debug, Clone)]
pub struct CachePaths {
    /// EasyList.
    pub easylist: PathBuf,
    /// uBO's filters.
    pub ubo_filters: PathBuf,
    /// uBO's scriptlets.
    pub ubo_scriptlets: PathBuf,
}

impl Default for CachePaths {
    fn default() -> Self {
        let dir = cache_dir();
        Self {
            easylist: dir.join("easylist.txt"),
            ubo_filters: dir.join("ubo-filters.txt"),
            ubo_scriptlets: dir.join("ubo-scriptlets.json"),
        }
    }
}

/// A cache directory that survives a reboot, so a kiosk that comes up without network
/// still has last week's lists rather than none.
#[must_use]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("castaway")
}

/// How often the lists are re-fetched while the receiver runs.
///
/// Daily, because these lists change on the order of days and a kiosk stays up for weeks —
/// without this it would run whatever rules it booted with until someone restarted it.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Re-fetch the subscriptions every [`REFRESH_INTERVAL`], swapping the result in.
///
/// A plain thread rather than a tokio task: the work is a blocking fetch and a parse of a
/// couple of megabytes, which has no business on an async runtime, and this crate has no
/// runtime of its own to borrow.
///
/// Respects [`OFFLINE_ENV`] — a receiver pinned to its cache stays pinned.
pub fn spawn_daily_refresh(paths: CachePaths, blocker: crate::cef_browser::SharedBlocker) {
    if std::env::var_os(OFFLINE_ENV).is_some() {
        info!(target: "castaway::adblock", "offline: no periodic refresh");
        return;
    }
    std::thread::Builder::new()
        .name("adblock-refresh".into())
        .spawn(move || loop {
            std::thread::sleep(REFRESH_INTERVAL);
            // The counters live on the blocker being replaced, so say where they got to
            // before they go — otherwise a long-running kiosk silently resets them daily.
            if let Ok(current) = blocker.read() {
                info!(
                    target: "castaway::adblock",
                    seen = current.seen_count(),
                    blocked = current.blocked_count(),
                    "refreshing filter lists"
                );
            }
            let refreshed = load_or_fetch_all(&paths);
            match blocker.write() {
                Ok(mut slot) => {
                    *slot = std::sync::Arc::new(refreshed);
                    info!(target: "castaway::adblock", "filter lists refreshed");
                }
                Err(e) => warn!(target: "castaway::adblock", error = %e, "could not swap in refreshed lists"),
            }
        })
        .map_or_else(
            |e| warn!(target: "castaway::adblock", error = %e, "could not start the refresh thread"),
            |_| (),
        );
}

/// The newest modification time across the cached lists, or `None` if none exist.
///
/// How a render process notices a refresh: it holds its own engine built from these files,
/// and comparing this against what it built from is cheaper than rebuilding to find out.
#[must_use]
pub fn cache_stamp(paths: &CachePaths) -> Option<SystemTime> {
    [&paths.easylist, &paths.ubo_filters, &paths.ubo_scriptlets]
        .into_iter()
        .filter_map(|p| std::fs::metadata(p).ok()?.modified().ok())
        .max()
}

/// Set this to skip the startup fetch and use whatever is already cached.
///
/// For a kiosk that should come up identically every boot, and for working on the lists
/// themselves — a hand-edited cache survives a restart instead of being overwritten by
/// the first fetch.
pub const OFFLINE_ENV: &str = "CASTAWAY_FILTERLISTS_OFFLINE";

/// Fetch (or recover from cache) every subscription and build a blocker from all of them.
#[must_use]
pub fn load_or_fetch_all(paths: &CachePaths) -> AdBlocker {
    if std::env::var_os(OFFLINE_ENV).is_some() {
        info!(target: "castaway::adblock", "offline: using the cached lists as they are");
        return load_cached_only(paths).unwrap_or_else(AdBlocker::with_defaults);
    }
    load_or_fetch_all_from(paths, EASYLIST_URL, UBO_FILTERS_URL, UBO_SOURCE_BASE)
}

/// [`load_or_fetch_all`] with the sources as parameters, so a test can point them at
/// nothing and exercise the cache-only path.
#[must_use]
pub fn load_or_fetch_all_from(
    paths: &CachePaths,
    easylist_url: &str,
    ubo_filters_url: &str,
    ubo_scriptlets_base: &str,
) -> AdBlocker {
    let easylist = text_for("EasyList", easylist_url, &paths.easylist);
    let ubo = text_for("uBO filters", ubo_filters_url, &paths.ubo_filters);

    let mut combined = String::new();
    for part in [easylist.as_deref(), ubo.as_deref()].into_iter().flatten() {
        combined.push_str(part);
        combined.push('\n');
    }

    let mut blocker = if combined.trim().is_empty() {
        warn!(target: "castaway::adblock", "no filter list available; using the compact built-in list");
        AdBlocker::with_defaults()
    } else {
        AdBlocker::from_list_text(&combined)
    };

    // Scriptlet rules without their bodies are the quiet failure this guards: the rules
    // are in the engine, they match, and nothing happens.
    match scriptlets(paths, ubo_scriptlets_base) {
        Some(resources) if !resources.is_empty() => blocker.use_resources(resources),
        _ => warn!(
            target: "castaway::adblock",
            "no scriptlet bundle: `##+js(...)` rules will match and inject nothing"
        ),
    }
    blocker
}

/// Build a blocker from whatever is already cached, fetching nothing.
///
/// For the render process: it runs per page load and must not block on the network, and
/// the browser process has already refreshed these files this boot. `None` when there is
/// no cached list at all, so the caller can skip injection rather than build an engine
/// that would match nothing.
#[must_use]
pub fn load_cached_only(paths: &CachePaths) -> Option<AdBlocker> {
    let mut combined = String::new();
    for path in [&paths.easylist, &paths.ubo_filters] {
        if let Ok(text) = std::fs::read_to_string(path) {
            combined.push_str(&text);
            combined.push('\n');
        }
    }
    if combined.trim().is_empty() {
        return None;
    }
    let mut blocker = AdBlocker::from_list_text(&combined);
    if let Some(resources) = read_cached_resources(&paths.ubo_scriptlets) {
        if !resources.is_empty() {
            blocker.use_resources(resources);
        }
    }
    Some(blocker)
}

/// Fetch uBO's scriptlet modules, evaluate them into resources, and cache the result.
///
/// The *converted* resources are cached rather than the raw modules: one file instead of
/// thirty-odd, and a render process then loads them without re-running a JS engine.
fn scriptlets(paths: &CachePaths, base_url: &str) -> Option<Vec<Resource>> {
    match fetch_modules(base_url) {
        Ok(modules) if !modules.is_empty() => match crate::ubo_scriptlets::convert(&modules) {
            Ok(resources) if !resources.is_empty() => {
                info!(
                    target: "castaway::adblock",
                    modules = modules.len(),
                    resources = resources.len(),
                    "evaluated uBO scriptlet modules"
                );
                write_cached_resources(&paths.ubo_scriptlets, &resources);
                return Some(resources);
            }
            Ok(_) => warn!(
                target: "castaway::adblock",
                "uBO's registry came back empty — upstream has moved"
            ),
            Err(e) => {
                warn!(target: "castaway::adblock", error = %e, "could not evaluate uBO's scriptlets")
            }
        },
        Ok(_) => warn!(target: "castaway::adblock", "no uBO scriptlet modules fetched"),
        Err(e) => warn!(target: "castaway::adblock", error = %e, "uBO scriptlet fetch failed"),
    }
    read_cached_resources(&paths.ubo_scriptlets)
}

/// Follow uBO's import graph from the entry module, fetching each one.
///
/// Transitive and relative-path aware: the entry lists the scriptlets but not the shared
/// helpers they depend on, and those live both beside them (`./safe-self.js`) and above
/// them (`../urlskip.js`).
fn fetch_modules(base_url: &str) -> Result<Vec<(String, String)>, String> {
    let mut pending = vec![crate::ubo_scriptlets::ENTRY_MODULE.to_string()];
    let mut seen = std::collections::HashSet::new();
    let mut modules = Vec::new();

    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) || modules.len() >= MAX_MODULES {
            continue;
        }
        let source = fetch(&format!("{base_url}{name}"))?;
        for import in imported_modules(&source) {
            let resolved = resolve_relative(&name, &import);
            if !seen.contains(&resolved) {
                pending.push(resolved);
            }
        }
        modules.push((name, source));
    }
    Ok(modules)
}

/// Resolve `./x.js` or `../x.js` against the importing module's path.
///
/// Plain string work rather than `Path`: these are URL paths with forward slashes on every
/// platform, and running them through a Windows `Path` would produce backslashes that the
/// module resolver does not recognise.
fn resolve_relative(importer: &str, specifier: &str) -> String {
    let mut segments: Vec<&str> = importer.split('/').collect();
    segments.pop(); // the importing file itself
    for part in specifier.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// The relative modules a source imports.
///
/// Scans the whole file rather than lines starting with `import`, because uBO writes its
/// imports across several lines — the path sits on the `} from './safe-self.js';` line,
/// which begins with neither. Missing those costs every shared dependency, and a scriptlet
/// whose dependency is absent is one the engine will not inject.
fn imported_modules(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for quote in ['\'', '"'] {
        let mut from = 0;
        while let Some(rel) = source[from..].find(quote) {
            let start = from + rel + 1;
            let Some(end) = source[start..].find(quote).map(|i| start + i) else {
                break;
            };
            let literal = &source[start..end];
            if literal.ends_with(".js") && (literal.starts_with("./") || literal.starts_with("../"))
            {
                out.push(literal.to_string());
            }
            from = end + 1;
        }
    }
    out
}

fn write_cached_resources(path: &Path, resources: &[Resource]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string(resources) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                warn!(target: "castaway::adblock", error = %e, "could not cache scriptlets");
            }
        }
        Err(e) => warn!(target: "castaway::adblock", error = %e, "could not encode scriptlets"),
    }
}

fn read_cached_resources(path: &Path) -> Option<Vec<Resource>> {
    let json = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Vec<Resource>>(&json) {
        Ok(resources) => {
            info!(target: "castaway::adblock", count = resources.len(), "using cached scriptlets");
            Some(resources)
        }
        Err(e) => {
            warn!(target: "castaway::adblock", error = %e, "cached scriptlets did not parse");
            None
        }
    }
}

/// Fetch `url`, caching to `path`; fall back to whatever was cached before.
fn text_for(label: &str, url: &str, path: &Path) -> Option<String> {
    match fetch(url) {
        Ok(text) if text.len() > 1024 => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(path, &text) {
                warn!(target: "castaway::adblock", %label, error = %e, "could not cache list");
            }
            info!(target: "castaway::adblock", %label, bytes = text.len(), "fetched");
            return Some(text);
        }
        // A 404 page is ~14 bytes and parses as an empty list, so short answers are
        // treated as failures rather than as "this list is empty today".
        Ok(short) => {
            warn!(target: "castaway::adblock", %label, bytes = short.len(), "fetch returned too little data");
        }
        Err(e) => warn!(target: "castaway::adblock", %label, error = %e, "fetch failed"),
    }
    match std::fs::read_to_string(path) {
        Ok(text) => {
            info!(target: "castaway::adblock", %label, path = %path.display(), "using cached copy");
            Some(text)
        }
        Err(_) => None,
    }
}

fn fetch(url: &str) -> Result<String, String> {
    ureq::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn paths_in(dir: &Path) -> CachePaths {
        CachePaths {
            easylist: dir.join("easylist.txt"),
            ubo_filters: dir.join("ubo.txt"),
            ubo_scriptlets: dir.join("scriptlets.js"),
        }
    }

    #[test]
    fn both_subscriptions_end_up_in_one_engine() {
        let dir = std::env::temp_dir().join("castaway-filterlists-test-both");
        std::fs::create_dir_all(&dir).unwrap();
        let paths = paths_in(&dir);
        // Pre-seed the caches and point the fetches at nothing, so this is offline.
        std::fs::write(
            &paths.easylist,
            format!("||tracker.example^\n{}", "!\n".repeat(600)),
        )
        .unwrap();
        std::fs::write(
            &paths.ubo_filters,
            format!("||ubo-only.example^\n{}", "!\n".repeat(600)),
        )
        .unwrap();
        let _ = std::fs::remove_file(&paths.ubo_scriptlets);

        let blocker = with_unreachable_urls(&paths);
        let page = "https://site.test/";
        assert!(
            blocker.should_block("https://tracker.example/a.js", page, "script"),
            "EasyList's rules have to survive the merge"
        );
        assert!(
            blocker.should_block("https://ubo-only.example/b.js", page, "script"),
            "so do uBO's — a subscription that silently loses one is worse than none"
        );
    }

    #[test]
    fn a_refresh_reaches_a_blocker_that_is_already_in_use() {
        // The failure this guards: the live client holds its own handle, so swapping a
        // plain `Arc` would update nothing that is actually blocking. Everything reads
        // through the cell, so a refresh has to be visible to a holder taken beforehand.
        let cell: crate::cef_browser::SharedBlocker = std::sync::Arc::new(std::sync::RwLock::new(
            std::sync::Arc::new(AdBlocker::from_list_text("||before.example^\n")),
        ));
        let holder = std::sync::Arc::clone(&cell);
        let page = "https://site.test/";
        assert!(holder
            .read()
            .unwrap()
            .should_block("https://before.example/a.js", page, "script"));

        *cell.write().unwrap() =
            std::sync::Arc::new(AdBlocker::from_list_text("||after.example^\n"));

        let current = holder.read().unwrap();
        assert!(
            current.should_block("https://after.example/a.js", page, "script"),
            "the refreshed rules must be live for a handle taken before the swap"
        );
        assert!(
            !current.should_block("https://before.example/a.js", page, "script"),
            "and the superseded ones must be gone"
        );
    }

    #[test]
    fn the_cache_stamp_moves_when_a_list_is_rewritten() {
        // How a render process notices a refresh without rebuilding to find out.
        let dir = std::env::temp_dir().join("castaway-filterlists-test-stamp");
        std::fs::create_dir_all(&dir).unwrap();
        let paths = paths_in(&dir);
        for p in [&paths.easylist, &paths.ubo_filters, &paths.ubo_scriptlets] {
            let _ = std::fs::remove_file(p);
        }
        assert_eq!(cache_stamp(&paths), None, "no lists, no stamp");

        std::fs::write(&paths.easylist, "||a.example^\n").unwrap();
        let first = cache_stamp(&paths).expect("a list exists now");

        // Filesystem timestamps are coarse; make the rewrite unambiguously later.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&paths.easylist, "||b.example^\n").unwrap();
        assert!(
            cache_stamp(&paths).expect("still exists") > first,
            "a rewritten list has to look different, or a renderer never re-reads"
        );
    }

    #[test]
    fn a_missing_scriptlet_bundle_leaves_the_blocker_usable() {
        // The failure mode to avoid is a receiver that refuses to start because GitHub is
        // unreachable. Network rules must still work; only injection is lost.
        let dir = std::env::temp_dir().join("castaway-filterlists-test-noscriptlets");
        std::fs::create_dir_all(&dir).unwrap();
        let paths = paths_in(&dir);
        std::fs::write(
            &paths.easylist,
            format!("||tracker.example^\n{}", "!\n".repeat(600)),
        )
        .unwrap();
        let _ = std::fs::remove_file(&paths.ubo_filters);
        let _ = std::fs::remove_file(&paths.ubo_scriptlets);

        let blocker = with_unreachable_urls(&paths);
        assert_eq!(blocker.scriptlet_count(), 0);
        assert!(blocker.should_block(
            "https://tracker.example/a.js",
            "https://site.test/",
            "script"
        ));
    }

    /// The production path, with every source unreachable, so only the caches are used.
    fn with_unreachable_urls(paths: &CachePaths) -> AdBlocker {
        load_or_fetch_all_from(
            paths,
            "http://127.0.0.1:1/a",
            "http://127.0.0.1:1/b",
            "http://127.0.0.1:1/c",
        )
    }
}
