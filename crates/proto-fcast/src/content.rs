//! Where the receiver serves the media it was handed rather than pointed at (#249).
//!
//! FCast senders describe media three ways, and only one of them is a URL the decoder can
//! open:
//!
//! 1. a `source_url` — the ordinary case, and the one Grayjay uses;
//! 2. **inline content** — the bytes themselves in the `play` message, which is how the
//!    terminal sender's `cat dash.mpd | fcast play --mime-type application/dash+xml` works;
//! 3. **`fcomp://`** — "the file is on my phone", read back over the control connection
//!    ([`crate::companion`]).
//!
//! The last two both resolve to the first, on this receiver's own HTTP host (D7). Not for
//! symmetry: libavformat cannot open either, and the alternatives are worse. A `data:` URI
//! carries the bytes but gives a DASH manifest no base to resolve its relative segment
//! URLs against, and `fcomp://` would need an AVIO callback — `unsafe` FFI, on the decode
//! thread, for a transfer a loopback socket already does.
//!
//! Serving them from *our* host also gives the manifest a real base URL, which is the
//! whole difference between a pushed manifest that plays and one whose every segment 404s.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::companion::CompanionUrl;

/// The path prefix inline content is served under.
pub const CONTENT_PATH: &str = "/fcast/content";

/// The path prefix `fcomp://` resources are proxied under.
pub const COMPANION_PATH: &str = "/fcast/companion";

/// How much pushed content is kept at once.
///
/// A cap rather than a lifetime because nothing tells us when the decoder is finished
/// with a manifest — it may re-fetch one for the whole session — and because a sender can
/// push as often as it likes. Four items and 16 MiB is far more than the manifests and
/// playlists this path exists for, and far less than a sender could use to exhaust a
/// wall panel's memory.
const MAX_ENTRIES: usize = 4;
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// Where this receiver's own HTTP host answers, as a sender-independent base URL.
///
/// Held as a string rather than a parsed URL because the only thing done with it is
/// concatenation, and because the address it names is the *advertised* one: a loopback
/// base would work for our own decoder and break the moment anything else — a hosted page,
/// a second process — had to fetch the same resource.
#[derive(Debug, Clone)]
pub struct LocalHost {
    base: String,
}

impl LocalHost {
    /// A host serving at `base`, e.g. `http://10.0.0.5:8008`. A trailing slash is
    /// tolerated and dropped.
    #[must_use]
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into();
        Self {
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    /// Where published content with this id is served.
    #[must_use]
    pub fn content_url(&self, id: u64) -> String {
        format!("{}{CONTENT_PATH}/{id}", self.base)
    }

    /// Where a sender's `fcomp://` resource is proxied.
    #[must_use]
    pub fn companion_url(&self, url: CompanionUrl) -> String {
        format!(
            "{}{COMPANION_PATH}/{}/{}",
            self.base, url.provider, url.resource
        )
    }

    /// Which companion resource one of *our own* URLs proxies, if it is one.
    ///
    /// The inverse of [`Self::companion_url`], and the reason that inverse is needed: a
    /// load rewrites `fcomp://3.fcast/7` to this host before the player ever sees it, so
    /// "is the thing playing right now a file on somebody's phone?" is a question about
    /// our own path (#336).
    #[must_use]
    pub fn companion_of(&self, url: &str) -> Option<CompanionUrl> {
        let rest = url
            .strip_prefix(&self.base)?
            .strip_prefix(COMPANION_PATH)?
            .strip_prefix('/')?;
        let (provider, resource) = rest.split_once('/')?;
        Some(CompanionUrl {
            provider: provider.parse().ok()?,
            resource: resource.parse().ok()?,
        })
    }
}

/// One published blob.
#[derive(Debug, Clone)]
pub struct Content {
    /// The MIME type the sender declared. Handed straight back as `Content-Type`, because
    /// it is what tells libavformat's demuxer probe what it is looking at.
    pub mime: String,
    /// The bytes.
    pub bytes: bytes::Bytes,
}

/// Bytes senders pushed inline, held so our own decoder can fetch them back.
#[derive(Debug)]
pub struct ContentStore {
    entries: Mutex<VecDeque<(u64, Content)>>,
    next: AtomicU64,
}

impl Default for ContentStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentStore {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            next: AtomicU64::new(1),
        }
    }

    /// Publish `bytes` under `mime`, returning the id they are served at.
    ///
    /// Ids are never reused. A sender that pushes twice gets two URLs, so a decoder still
    /// reading the first one is not handed the second's bytes halfway through — which is
    /// exactly what a fixed path would do.
    pub fn publish(&self, mime: &str, bytes: bytes::Bytes) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let Ok(mut entries) = self.entries.lock() else {
            return id;
        };
        entries.push_back((
            id,
            Content {
                mime: mime.to_owned(),
                bytes,
            },
        ));
        // Oldest out first, on both limits. A store that refused new content instead would
        // fail the cast the *user* just started in favour of one they have forgotten —
        // and for the same reason the newest is never evicted, however large it is: a
        // single push over the byte budget is the only thing anybody is waiting for, and
        // dropping it on arrival would serve a 404 to our own decoder.
        while entries.len() > 1
            && (entries.len() > MAX_ENTRIES
                || entries.iter().map(|(_, c)| c.bytes.len()).sum::<usize>() > MAX_BYTES)
        {
            entries.pop_front();
        }
        id
    }

    /// Fetch published content back, if it is still held.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<Content> {
        let entries = self.entries.lock().ok()?;
        entries
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, content)| content.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn urls_are_built_off_one_base_with_or_without_its_slash() {
        for base in ["http://10.0.0.5:8008", "http://10.0.0.5:8008/"] {
            let host = LocalHost::new(base);
            assert_eq!(host.content_url(7), "http://10.0.0.5:8008/fcast/content/7");
            assert_eq!(
                host.companion_url(CompanionUrl {
                    provider: 2,
                    resource: 9
                }),
                "http://10.0.0.5:8008/fcast/companion/2/9"
            );
        }
    }

    /// A companion URL this host built is one it can read back, and nothing else is.
    ///
    /// The reading back is what tells a load-time check which resource the thing now
    /// playing came from (#336); mistaking somebody else's URL for one of ours would
    /// have us interrogating a sender about a resource it never offered.
    #[test]
    fn our_own_companion_urls_read_back_and_no_others_do() {
        let host = LocalHost::new("http://10.0.0.5:8008");
        let url = CompanionUrl {
            provider: 2,
            resource: 9,
        };
        assert_eq!(host.companion_of(&host.companion_url(url)), Some(url));
        for foreign in [
            "http://10.0.0.5:8008/fcast/content/7",
            "http://10.0.0.5:8008/fcast/companion/2",
            "http://10.0.0.5:8008/fcast/companion//9",
            "http://10.0.0.5:8008/fcast/companion/70000/9",
            "http://example.com/fcast/companion/2/9",
            "fcomp://2.fcast/9",
        ] {
            assert_eq!(host.companion_of(foreign), None, "{foreign}");
        }
    }

    /// The store hands the decoder back exactly what the sender pushed, under the MIME
    /// type it declared — which is what the demuxer probe reads.
    #[test]
    fn published_content_comes_back_whole() {
        let store = ContentStore::new();
        let id = store.publish("application/dash+xml", bytes::Bytes::from_static(b"<MPD/>"));
        let held = store.get(id).unwrap();
        assert_eq!(held.mime, "application/dash+xml");
        assert_eq!(&held.bytes[..], b"<MPD/>");
        assert!(store.get(id + 1000).is_none());
    }

    /// Ids are never reused, so a decoder still reading the first push is never handed
    /// the second's bytes partway through.
    #[test]
    fn a_second_push_gets_its_own_url() {
        let store = ContentStore::new();
        let first = store.publish("text/plain", bytes::Bytes::from_static(b"a"));
        let second = store.publish("text/plain", bytes::Bytes::from_static(b"b"));
        assert_ne!(first, second);
        assert_eq!(&store.get(first).unwrap().bytes[..], b"a");
        assert_eq!(&store.get(second).unwrap().bytes[..], b"b");
    }

    /// A sender that pushes without limit evicts its own oldest, rather than growing a
    /// wall panel's memory until something else fails.
    #[test]
    fn the_store_is_bounded_and_evicts_the_oldest() {
        let store = ContentStore::new();
        let ids: Vec<u64> = (0..MAX_ENTRIES + 2)
            .map(|i| store.publish("text/plain", bytes::Bytes::from(vec![b'x'; i + 1])))
            .collect();
        assert!(store.get(ids[0]).is_none(), "the first push was evicted");
        assert!(
            store.get(ids[ids.len() - 1]).is_some(),
            "the newest is always held"
        );
        // A single push far over the byte budget is held rather than dropped on arrival:
        // it is the only thing anybody is waiting for.
        let big = store.publish("video/mp4", bytes::Bytes::from(vec![0u8; MAX_BYTES + 1]));
        assert!(store.get(big).is_some());
    }
}
