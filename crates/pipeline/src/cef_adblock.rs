//! Request-level ad/tracker blocking for the CEF browser, using Brave's `adblock`
//! engine. This is the answer to "can we run uBlock Origin?" — CEF's extension APIs
//! can't host uBO, but blocking at the request layer (CEF hands us every resource load)
//! is cleaner for a kiosk and just as effective for the general web. Every block is
//! **logged** on the `castaway::adblock` target so it's visible.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use adblock::request::Request;
use adblock::Engine;
use tracing::info;

/// A compact default block list; a full EasyList can be loaded via [`AdBlocker::from_list_text`].
const DEFAULT_RULES: &str = include_str!("adblock_default.txt");

/// Wraps an adblock [`Engine`] and counts/logs blocks.
pub struct AdBlocker {
    engine: Engine,
    blocked: AtomicU64,
    seen: AtomicU64,
    /// host → (seen, blocked) — diagnostic tally so we can see what a page actually loads.
    hosts: Mutex<BTreeMap<String, (u32, u32)>>,
}

impl AdBlocker {
    /// Build from Adblock-Plus-syntax filter text (e.g. an EasyList file).
    #[must_use]
    pub fn from_list_text(text: &str) -> Self {
        let engine = Engine::new_with_list_text(text);
        let rules = text.lines().filter(|l| !l.trim_start().starts_with('!') && !l.trim().is_empty()).count();
        info!(target: "castaway::adblock", rules, "ad blocker loaded");
        Self {
            engine,
            blocked: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            hosts: Mutex::new(BTreeMap::new()),
        }
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
    use super::*;

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
