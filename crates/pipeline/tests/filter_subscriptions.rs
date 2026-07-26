//! The filter subscriptions, against the real upstreams. `#[ignore]`d: it needs the
//! network, so it is a `cargo test -- --ignored` check rather than part of `nix flake
//! check`, for the same reason `yt-selfplay` is a `nix run`.
//!
//! What it protects is the failure that has no symptom: uBlock Origin's rules are mostly
//! *cosmetic*, and a scriptlet bundle that stops parsing turns every one of them into a
//! no-op while the receiver still looks fine and still blocks network-level ads. That has
//! already happened once — uBO moved to ES modules and `adblock`'s assembler reads zero
//! resources from the current tree, which is why the bundle is pinned. This test is what
//! notices if the pin ever stops yielding scriptlets, or if the lists move again.
#![cfg(feature = "cef")]

use pipeline::filterlists::{load_or_fetch_all, CachePaths};

/// Where to cache during the test. Overridable so a run can be pointed at a hand-edited
/// cache (with `CASTAWAY_FILTERLISTS_OFFLINE=1`) to try a rule out.
fn cache_paths() -> CachePaths {
    match std::env::var_os("CASTAWAY_LIST_CACHE") {
        Some(dir) => {
            let dir = std::path::PathBuf::from(dir);
            CachePaths {
                easylist: dir.join("easylist.txt"),
                ubo_filters: dir.join("ubo-filters.txt"),
                ubo_scriptlets: dir.join("ubo-scriptlets.js"),
            }
        }
        None => {
            let dir = std::env::temp_dir().join("castaway-filter-subscription-test");
            let _ = std::fs::create_dir_all(&dir);
            CachePaths {
                easylist: dir.join("easylist.txt"),
                ubo_filters: dir.join("ubo-filters.txt"),
                ubo_scriptlets: dir.join("ubo-scriptlets.js"),
            }
        }
    }
}

#[test]
#[ignore = "fetches EasyList, uBO's filters, and uBO's scriptlet bundle"]
fn the_real_lists_still_produce_real_injections() {
    let blocker = load_or_fetch_all(&cache_paths());

    // The pinned bundle carries dozens. Zero is what a format change looks like, and it
    // is silent everywhere else.
    let scriptlets = blocker.scriptlet_count();
    println!("scriptlets assembled: {scriptlets}");
    assert!(
        scriptlets > 20,
        "only {scriptlets} scriptlets assembled — the bundle format has probably moved \
         again (see UBO_SCRIPTLETS_URL)"
    );

    // Network rules still have to work; they are the half that does not depend on the
    // bundle at all.
    assert!(
        blocker.should_block(
            "https://pagead2.googlesyndication.com/pagead/js/adsbygoogle.js",
            "https://news.example.com/",
            "script"
        ),
        "the merged lists must still block AdSense — EasyList's most canonical entry. \
         (Note it is *ads* that EasyList covers; trackers live in EasyPrivacy, which we \
         do not subscribe to.)"
    );

    // And something, somewhere, has to actually inject — a subscription that parses but
    // matches nothing would pass every other assertion here.
    let injected = blocker.injected_script("https://www.youtube.com/");
    println!(
        "youtube.com injection: {} bytes",
        injected.as_deref().map_or(0, str::len)
    );
    assert!(
        injected.is_some(),
        "uBO's list carries scriptlet rules for youtube.com; getting none means the \
         cosmetic half of the subscription is not reaching the engine"
    );
}
