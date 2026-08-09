//! Request-level ad/tracker blocking for the browser, using Brave's `adblock`
//! engine. This is the answer to "can we run uBlock Origin?" — an offscreen kiosk
//! browser can't host uBO, but blocking at the request layer (the browser asks about
//! every resource load)
//! is cleaner for a kiosk and just as effective for the general web. Every block is
//! **logged** on the `castaway::adblock` target so it's visible.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use adblock::request::Request;
use adblock::resources::Resource;
use adblock::Engine;
use tracing::info;

/// A compact default block list; a full EasyList can be loaded via [`AdBlocker::from_list_text`].
const DEFAULT_RULES: &str = include_str!("adblock_default.txt");

/// The engine behind a lock, so the daily refresh can swap it under a running browser.
///
/// Two layers on purpose: the `RwLock` is held only long enough to clone the `Arc`, so a
/// refresh never stalls a page load waiting for a decision to finish.
///
/// Every *decision* goes through [`SharedBlocker::current`], taken at the moment of the
/// decision — a held `Arc<AdBlocker>` is a snapshot no refresh can reach. That is not
/// hypothetical: when this was a bare type alias, the browser kept its boot-time snapshot
/// and the daily refresh swapped an engine nothing read, so refreshed lists only took
/// effect at process restart (#239). The methods are the whole interface precisely so a
/// caller cannot hold the inner `Arc` by accident.
#[derive(Clone)]
pub struct SharedBlocker {
    cell: Arc<RwLock<Arc<AdBlocker>>>,
}

impl SharedBlocker {
    /// Wrap the engine the receiver boots with.
    #[must_use]
    pub fn new(initial: AdBlocker) -> Self {
        Self {
            cell: Arc::new(RwLock::new(Arc::new(initial))),
        }
    }

    /// The engine as of *now*. Take it per decision, never per session.
    ///
    /// A poisoned lock is recovered rather than propagated: the guarded value is a single
    /// pointer swapped whole, so it cannot be half-written, and a page load must not
    /// inherit a panic from the refresh thread.
    #[must_use]
    pub fn current(&self) -> Arc<AdBlocker> {
        Arc::clone(&self.cell.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Install a freshly built engine; every subsequent [`Self::current`] answers with it.
    pub fn install(&self, next: AdBlocker) {
        *self.cell.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
    }
}

/// Wraps an adblock [`Engine`] and counts/logs blocks.
pub struct AdBlocker {
    engine: Engine,
    blocked: AtomicU64,
    seen: AtomicU64,
    /// host → (seen, blocked) — diagnostic tally so we can see what a page actually loads.
    hosts: Mutex<BTreeMap<String, (u32, u32)>>,
    /// How many scriptlet bodies the engine can substitute into `##+js(...)` rules.
    scriptlets: usize,
}

impl AdBlocker {
    /// Build from Adblock-Plus-syntax filter text (e.g. an EasyList file).
    #[must_use]
    pub fn from_list_text(text: &str) -> Self {
        let engine = Engine::new_with_list_text(text);
        let rules = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('!') && !l.trim().is_empty())
            .count();
        info!(target: "castaway::adblock", rules, "ad blocker loaded");
        Self {
            engine,
            blocked: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            hosts: Mutex::new(BTreeMap::new()),
            scriptlets: 0,
        }
    }

    /// Give the engine the scriptlet bodies that `##+js(...)` rules name.
    ///
    /// Without these, cosmetic filters still parse but [`Self::injected_script`] returns
    /// nothing for them: a rule says *which* scriptlet to run with *which* arguments, and
    /// the implementation lives in a separate resource bundle.
    pub fn use_resources(&mut self, resources: Vec<Resource>) {
        let count = resources.len();
        self.engine.use_resources(resources);
        self.scriptlets = count;
        info!(target: "castaway::adblock", scriptlets = count, "scriptlet resources loaded");
    }

    /// How many scriptlet resources are loaded.
    #[must_use]
    pub fn scriptlet_count(&self) -> usize {
        self.scriptlets
    }

    /// The JavaScript to run at document start for `url`, if any rule calls for it.
    ///
    /// This is the half of uBlock Origin that request blocking cannot do: rules like
    /// `##+js(set-constant, foo, true)` do not name a request to cancel, they name code to
    /// run inside the page before its own scripts. Returns `None` when no rule matches, so
    /// the caller can skip the injection entirely rather than evaluating an empty string.
    #[must_use]
    pub fn injected_script(&self, url: &str) -> Option<String> {
        let script = self.engine.url_cosmetic_resources(url).injected_script;
        (!script.trim().is_empty()).then_some(script)
    }

    /// A sorted diagnostic of the hosts seen: `(host, seen, blocked)`, most-seen first.
    #[must_use]
    pub fn host_tally(&self) -> Vec<(String, u32, u32)> {
        let map = self.hosts.lock().map(|g| g.clone()).unwrap_or_default();
        let mut v: Vec<_> = map.into_iter().map(|(h, (s, b))| (h, s, b)).collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        v
    }

    /// Build with the compact built-in list.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::from_list_text(DEFAULT_RULES)
    }

    /// Total requests blocked so far.
    #[must_use]
    pub fn blocked_count(&self) -> u64 {
        self.blocked.load(Ordering::Relaxed)
    }

    /// Total requests inspected so far (blocked + allowed).
    #[must_use]
    pub fn seen_count(&self) -> u64 {
        self.seen.load(Ordering::Relaxed)
    }

    /// Decide whether to block a request, logging (visibly) when it does. `resource_type`
    /// is an Adblock request-type string (`script`, `image`, `xmlhttprequest`, …).
    #[must_use]
    pub fn should_block(&self, url: &str, source_url: &str, resource_type: &str) -> bool {
        self.seen.fetch_add(1, Ordering::Relaxed);
        let Ok(request) = Request::new(url, source_url, resource_type, "GET") else {
            return false;
        };
        let result = self.engine.check_network_request(&request);
        let blocked = result.filter.is_some() && result.exception.is_none();
        if blocked {
            let total = self.blocked.fetch_add(1, Ordering::Relaxed) + 1;
            info!(target: "castaway::adblock", %url, kind = resource_type, total, "BLOCKED");
        }
        if let Some(host) = url
            .split_once("://")
            .and_then(|(_, rest)| rest.split(['/', ':', '?']).next())
        {
            if let Ok(mut map) = self.hosts.lock() {
                // Bound the diagnostic map: update existing hosts always, cap new ones.
                if map.contains_key(host) || map.len() < 2048 {
                    let e = map.entry(host.to_string()).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += u32::from(blocked);
                }
            }
        }
        blocked
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use adblock::resources::{MimeType, ResourceType};
    use base64::Engine as _;

    /// A scriptlet resource in the shape the engine wants: base64 body, and a name ending
    /// in `.js` — a `##+js(probe, …)` rule resolves to the resource `probe.js`, so a
    /// bundle whose names lack the extension silently matches nothing.
    fn scriptlet(name: &str, body: &str) -> Resource {
        Resource {
            name: name.to_string(),
            aliases: vec![],
            kind: ResourceType::Mime(MimeType::ApplicationJavascript),
            content: base64::prelude::BASE64_STANDARD.encode(body),
            dependencies: vec![],
            permission: Default::default(),
        }
    }

    #[test]
    fn a_scriptlet_rule_becomes_javascript_for_the_matching_site_only() {
        // The whole point of the injection path: this rule names code to run *inside* the
        // page, which no amount of request blocking can express.
        let mut ab = AdBlocker::from_list_text("example.com##+js(probe, hello)\n");
        assert_eq!(
            ab.injected_script("https://example.com/"),
            None,
            "a rule without its scriptlet body has nothing to inject"
        );

        // Function-style, as modern bundles are: the engine finds the function name and
        // invokes it with the rule's arguments.
        ab.use_resources(vec![scriptlet(
            "probe.js",
            "function probe(a) { console.log('probe:' + a); }",
        )]);
        let script = ab
            .injected_script("https://example.com/")
            .expect("the rule matches this site");
        assert!(
            script.contains("probe"),
            "the scriptlet body has to reach the page: {script}"
        );
        assert!(
            script.contains("hello"),
            "the rule's argument has to be substituted in: {script}"
        );
        assert_eq!(ab.scriptlet_count(), 1);

        // Scriptlets are per-site; injecting one everywhere would be a different product.
        assert_eq!(ab.injected_script("https://other.test/"), None);
    }

    #[test]
    fn a_rule_naming_an_unknown_scriptlet_injects_nothing() {
        // Lists reference scriptlets our bundle may not carry. That must be silence, not
        // a broken script tag inside someone else's page.
        let mut ab = AdBlocker::from_list_text("example.com##+js(not-in-our-bundle, x)\n");
        ab.use_resources(vec![scriptlet("probe.js", "function probe() {}")]);
        assert_eq!(ab.injected_script("https://example.com/"), None);
    }

    #[test]
    fn an_install_reaches_a_handle_cloned_before_it() {
        // The failure this guards is #239: a client that clones the *cell* stays current
        // across a refresh, where a client that held the inner `Arc` would not — and the
        // methods are the only interface, so holding the inner `Arc` takes deliberate
        // effort rather than being the natural spelling.
        let shared = SharedBlocker::new(AdBlocker::from_list_text("||before.example^\n"));
        let holder = shared.clone();
        let page = "https://site.test/";
        assert!(holder
            .current()
            .should_block("https://before.example/a.js", page, "script"));

        shared.install(AdBlocker::from_list_text("||after.example^\n"));

        let current = holder.current();
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
    fn blocks_a_known_ad_domain_and_passes_content() {
        let ab = AdBlocker::with_defaults();
        let page = "https://news.example.com/";
        assert!(
            ab.should_block("https://www.googletagmanager.com/gtm.js", page, "script"),
            "known tracker should be blocked"
        );
        assert!(
            !ab.should_block("https://news.example.com/article.js", page, "script"),
            "first-party content should pass"
        );
        assert_eq!(ab.blocked_count(), 1);
    }
}
