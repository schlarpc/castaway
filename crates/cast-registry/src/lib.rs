//! # cast-registry
//!
//! `appId` → receiver page. A Cast sender names an application by an eight-hex-digit id
//! and expects the device to know what that is; the mapping lives in a public,
//! unauthenticated Google endpoint that every Chromecast queries:
//!
//! ```text
//! GET https://clients3.google.com/cast/chromecast/device/app?a=233637DE
//! )]}'{"display_name":"YouTube","url":"https://www.youtube.com/tv?castv=2.0","uses_ipc":true,…}
//! ```
//!
//! ## Why this is a dependency on a cloud service, and why that is allowed
//!
//! Ground rule 9 says reimplement rather than depend. It does not apply, for the same
//! reason D30 carved out Spotify: **the peer here is not a device speaking a frozen
//! spec, it is a registry whose contents Google changes unilaterally.** There is nothing
//! to reimplement — the data is the product. What ground rule 9 *does* still buy is the
//! shape of the dependency: [`entry`] is a pure parser tested against captured responses
//! (`tests/fixtures/registry`), so the wire format is pinned in this tree even though the
//! contents are not.
//!
//! ## What the cache is for
//!
//! Not latency. An unattended panel that cannot reach Google must still launch the app
//! somebody launched yesterday, so a resolution is kept on disk and a **stale entry beats
//! a failed fetch** ([`Registry::resolve`]). The panel degrades to "the apps it has seen"
//! rather than to "no apps".

#![forbid(unsafe_code)]

pub mod cache;
pub mod entry;
pub mod resolver;

pub use entry::{parse, AppSurface};
pub use resolver::Registry;

/// The registry endpoint, as every Chromecast queries it.
pub const DEFAULT_ENDPOINT: &str = "https://clients3.google.com/cast/chromecast/device/app";

/// Why an app id could not be resolved.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The body was not a registry response. An unknown app id lands here, because the
    /// endpoint answers those with an HTML 404 rather than with JSON saying "no".
    #[error("not a Cast registry response: {0}")]
    NotRegistryJson(String),
    /// Prefixed correctly, but the JSON behind it did not parse.
    #[error("malformed registry entry: {0}")]
    Malformed(String),
    /// The lookup itself failed — no uplink, DNS, TLS, a 5xx.
    #[error("looking up {app_id}: {reason}")]
    Lookup {
        /// The app id being resolved.
        app_id: String,
        /// What went wrong. A string rather than a typed source: it crosses a
        /// `spawn_blocking` boundary, and `ureq::Error` is large and not `Sync`.
        reason: String,
    },
    /// The app id is not eight hexadecimal digits, so it is not an app id and no
    /// request was made.
    #[error("{0:?} is not a Cast application id (eight hex digits)")]
    NotAnAppId(String),
    /// The cache file could not be read or written. Never fatal to a resolution — a
    /// lookup that succeeded is still a lookup that succeeded.
    #[error("registry cache at {path}: {source}")]
    Cache {
        /// The file.
        path: std::path::PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
}

/// Whether `app_id` is syntactically an application id.
///
/// Checked before any request, because the id goes into a URL query and an unvalidated
/// one is a sender-controlled string reaching a third-party endpoint. Real ids are eight
/// hex digits; a sender that sends something else has not named an app.
#[must_use]
pub fn is_app_id(app_id: &str) -> bool {
    app_id.len() == 8 && app_id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_eight_hex_digits_and_nothing_else() {
        assert!(is_app_id("CC1AD845"));
        assert!(is_app_id("233637de"), "case is not significant");
        // The shapes that matter are the ones a hostile sender would try: something
        // that escapes the query, and something that walks the cache path.
        assert!(!is_app_id("CC1AD845&a=x"));
        assert!(!is_app_id("../../etc/passwd"));
        assert!(!is_app_id(""));
        assert!(!is_app_id("CC1AD84"));
        assert!(!is_app_id("CC1AD8455"));
        assert!(!is_app_id("ZZZZZZZZ"));
    }
}
