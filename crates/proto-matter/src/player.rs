//! What a cluster invoke *means* for the panel — the sans-I/O half of the receiver.
//!
//! Matter Casting carries no media. `LaunchURL` is a sentence: "open this, call it that".
//! Everything in this module turns those sentences into [`CastCommand`]s and holds the
//! answer to the questions a Casting Client asks back (what is playing, how far in,
//! which apps do you have). No sockets, no `rs-matter`, no `tokio` — the cluster handlers
//! in [`crate::node`] are a thin shell over this, and the tests drive it directly.

use std::sync::Mutex;
use std::time::Duration;

/// Matter endpoint ids are `u16`. Aliased so signatures say which `u16` they mean.
pub type EndpointId = u16;

/// The endpoint hosting the Casting Video Player device type. Endpoint 0 is the root
/// node, so the player is the first application endpoint, which is also where the
/// reference `tv-app` puts it — a client that guesses rather than reading the descriptor
/// guesses this number.
pub const PLAYER_ENDPOINT: EndpointId = 1;

/// The first endpoint a content app can occupy. The reference `tv-app` starts its content
/// apps at 6 and clients have been observed to assume it, so we do too rather than pack
/// them tight behind the player.
pub const FIRST_CONTENT_APP_ENDPOINT: EndpointId = 6;

/// How many content apps one panel will host.
///
/// A bound rather than a `Vec` because every one of them is an endpoint in the node's
/// metadata, which a client reads in full on connect: an operator who lists forty apps
/// would produce a descriptor no phone will finish reading.
pub const MAX_CONTENT_APPS: usize = 8;

/// What launching into an app actually does on this panel.
///
/// The two arms are the two things the panel can do with a sentence, and they are not
/// interchangeable: one ends at the media pipeline and the other at the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTarget {
    /// Hand the URL straight to the player, which fetches and decodes it. What
    /// `LaunchURL` is for, and the only arm that can honour a bare `LaunchContent`
    /// search request not at all.
    MediaUrl,
    /// Open a page in the panel's browser. `search` is a template with `{query}` in it,
    /// used to answer `LaunchContent` — an app that can be searched says so by having
    /// one, and an app that cannot declines rather than opening its home page and
    /// pretending that was the ask.
    Browser {
        /// Template for a `LaunchContent` search, e.g. `https://example.com/s?q={query}`.
        search: Option<String>,
    },
}

impl LaunchTarget {
    /// Which half of the panel opens a URL launched into this app.
    #[must_use]
    pub const fn surface(&self) -> Surface {
        match self {
            Self::MediaUrl => Surface::Player,
            Self::Browser { .. } => Surface::Browser,
        }
    }
}

/// One content app this panel hosts: an endpoint a Casting Client can aim at.
///
/// A client picks an endpoint by matching its own app against `ApplicationBasic`, so
/// these fields are not decoration — they are the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentApp {
    /// Which endpoint this app occupies.
    pub endpoint: EndpointId,
    /// CSA vendor id the app belongs to.
    pub vendor_id: u16,
    /// Product id within that vendor.
    pub product_id: u16,
    /// Vendor name, as shown by a client listing what this panel can play.
    pub vendor_name: String,
    /// The app's name.
    pub name: String,
    /// The app's id in its catalog — a package name, or a vendor-specific string.
    pub application_id: String,
    /// Which catalog `application_id` is read against. 0 is the CSA-assigned "the
    /// vendor's own catalog"; a platform id (Google Play, the App Store) names theirs.
    pub catalog_vendor_id: u16,
    /// What a launch into this app does here.
    pub launch: LaunchTarget,
}

impl ContentApp {
    /// Whether a client's `targetAppList` entry names this app.
    ///
    /// Product id 0 is a wildcard on the *client's* side — it means "any product of this
    /// vendor" — so it matches. A wildcard in the other direction is not a thing: our own
    /// product id is a fact, not a query.
    #[must_use]
    pub fn matches(&self, target: &crate::udc::TargetApp) -> bool {
        self.vendor_id == target.vendor_id
            && (target.product_id == 0 || target.product_id == self.product_id)
    }
}

/// Where playback is, as `MediaPlayback` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    /// Media is loaded and running.
    Playing,
    /// Media is loaded and stopped at a position.
    Paused,
    /// Nothing is loaded, or the session ended.
    #[default]
    NotPlaying,
    /// Loaded, not yet running: fetching, decoding, waiting on the network.
    Buffering,
}

/// What the panel will tell a client about the current media.
///
/// Not a copy of the pipeline's state — a *projection* of it into the four things this
/// cluster can express. The session manager pushes updates in; the cluster handlers read
/// them out on demand, from whatever task the interaction model happens to be on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerSnapshot {
    /// Playing, paused, buffering, or nothing.
    pub state: PlaybackState,
    /// How far into the current item.
    pub position: Duration,
    /// Total length, when it is known. Live streams have none.
    pub duration: Option<Duration>,
    /// Which content-app endpoint launched what is playing.
    pub app: Option<EndpointId>,
}

/// Shared, interior-mutable [`PlayerSnapshot`].
///
/// A `std::sync::Mutex` and not a `tokio` one on purpose: the cluster handlers are
/// synchronous by trait signature, so a lock that could yield would not be usable there.
/// Nothing awaits while holding it, which is what makes that safe.
#[derive(Debug, Default)]
pub struct PlayerState {
    snapshot: Mutex<PlayerSnapshot>,
}

impl PlayerState {
    /// A state with nothing playing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the current snapshot.
    ///
    /// # Panics
    /// Never in practice: nothing held across an await, so the lock cannot be poisoned by
    /// a panicking holder. A poisoned lock is recovered from rather than propagated —
    /// stale playback metadata is a better answer to a phone than a dead cluster.
    #[must_use]
    pub fn get(&self) -> PlayerSnapshot {
        match self.snapshot.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Replace the snapshot.
    pub fn set(&self, snapshot: PlayerSnapshot) {
        match self.snapshot.lock() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    /// Apply a change to the snapshot in place.
    pub fn update(&self, f: impl FnOnce(&mut PlayerSnapshot)) {
        match self.snapshot.lock() {
            Ok(mut guard) => f(&mut guard),
            Err(poisoned) => f(&mut poisoned.into_inner()),
        }
    }
}

/// Transport verbs `MediaPlayback` can ask for.
///
/// Its own enum rather than [`castaway_core::ControlTxn`] because two of them —
/// the skips — are relative to a position only the panel knows, so they cannot be
/// translated until the moment they are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transport {
    /// Resume.
    Play,
    /// Pause in place.
    Pause,
    /// Stop and release the media.
    Stop,
    /// Back to the beginning of the current item.
    StartOver,
    /// Previous item in the queue.
    Previous,
    /// Next item in the queue.
    Next,
    /// Seek to an absolute position.
    Seek(Duration),
    /// Jump forward from wherever we are.
    SkipForward(Duration),
    /// Jump backward from wherever we are.
    SkipBackward(Duration),
}

/// What a Casting Client's invoke asks the panel to do.
///
/// The whole surface of this protocol, in four verbs. Pure data, so the mapping from
/// cluster to intent is testable without a Matter session, and the mapping from intent to
/// [`castaway_core::SessionEvent`] is testable without a cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CastCommand {
    /// Play this, on behalf of that endpoint's app.
    Launch {
        /// Which content app (or the player endpoint itself) asked.
        app: EndpointId,
        /// What to open. Already resolved through the app's [`LaunchTarget`], so this is
        /// a thing the panel can actually open, not the client's raw request.
        url: String,
        /// A title for the now-playing card, when the client sent one.
        title: Option<String>,
        /// Whether to start playing immediately. `LaunchURL` has no autoplay field —
        /// launching *is* the play — but `LaunchContent` does.
        autoplay: bool,
        /// Which of the panel's two ways of opening a thing this goes to.
        surface: Surface,
    },
    /// Drive whatever is playing.
    Transport(Transport),
    /// A client selected a different target in `TargetNavigator`.
    SelectTarget(EndpointId),
    /// The client is done: end the session.
    End,
}

/// Which half of the panel opens a launched URL.
///
/// Resolved here rather than in the adapter because it follows from the *app*, and the
/// app is a fact this module holds — an adapter deciding it from the URL would send a
/// browser app's page to the media decoder the first time one ended in `.mp4`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The media pipeline fetches and decodes it.
    Player,
    /// The panel's browser opens it as a page.
    Browser,
}

/// Why a launch could not be honoured, in the vocabulary `ContentLauncher` has for it.
///
/// The panel declines in the sender's own words rather than failing the invoke, so the
/// phone can say something true to the person holding it (D32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchRefusal {
    /// The URL scheme is not one the panel can open.
    UrlNotAvailable,
    /// The app cannot be searched — it declared no search template.
    NotAllowed,
    /// No endpoint here hosts the app the client aimed at.
    NoAppFound,
}

/// The panel's content-app catalogue, and the launch policy over it.
#[derive(Debug, Clone, Default)]
pub struct Catalogue {
    apps: Vec<ContentApp>,
}

impl Catalogue {
    /// Build a catalogue, keeping at most [`MAX_CONTENT_APPS`] apps and assigning each an
    /// endpoint from [`FIRST_CONTENT_APP_ENDPOINT`] upward.
    ///
    /// The endpoint on each [`ContentApp`] passed in is *overwritten*: an endpoint number
    /// is a position in this node's tree, not a property of the app, and letting config
    /// choose it would let two apps claim one endpoint.
    #[must_use]
    pub fn new(apps: impl IntoIterator<Item = ContentApp>) -> Self {
        let apps = apps
            .into_iter()
            .take(MAX_CONTENT_APPS)
            .enumerate()
            .map(|(i, mut app)| {
                // The cast cannot truncate: `i` is bounded by MAX_CONTENT_APPS.
                #[allow(clippy::cast_possible_truncation)]
                let offset = i as u16;
                app.endpoint = FIRST_CONTENT_APP_ENDPOINT + offset;
                app
            })
            .collect();
        Self { apps }
    }

    /// Every app, in endpoint order.
    #[must_use]
    pub fn apps(&self) -> &[ContentApp] {
        &self.apps
    }

    /// The app hosted at an endpoint.
    #[must_use]
    pub fn at(&self, endpoint: EndpointId) -> Option<&ContentApp> {
        self.apps.iter().find(|a| a.endpoint == endpoint)
    }

    /// Whether any hosted app is named in a client's `targetAppList`.
    ///
    /// An empty list means "anything you have", which is true whenever we have anything —
    /// so an empty catalogue and an empty request still disagree, and the client is told
    /// so before a passcode goes on the screen.
    #[must_use]
    pub fn hosts_any(&self, targets: &[crate::udc::TargetApp]) -> bool {
        if self.apps.is_empty() {
            return false;
        }
        if targets.is_empty() {
            return true;
        }
        targets
            .iter()
            .any(|t| self.apps.iter().any(|app| app.matches(t)))
    }

    /// Resolve a `LaunchURL` against the app at `endpoint`.
    ///
    /// # Errors
    /// [`LaunchRefusal`] when no such app exists, or the URL is not one the panel opens.
    pub fn launch_url(
        &self,
        endpoint: EndpointId,
        url: &str,
        title: Option<&str>,
    ) -> Result<CastCommand, LaunchRefusal> {
        // The player endpoint itself takes a URL directly: a client that never selected a
        // content app is asking the *panel* to play something, which is the one case where
        // there is no app to consult.
        let target = if endpoint == PLAYER_ENDPOINT {
            // A client that never selected a content app is asking the *panel* to play
            // something, which is the one case where there is no app to consult.
            &LaunchTarget::MediaUrl
        } else {
            &self.at(endpoint).ok_or(LaunchRefusal::NoAppFound)?.launch
        };

        if !is_playable_url(url) {
            return Err(LaunchRefusal::UrlNotAvailable);
        }

        Ok(CastCommand::Launch {
            app: endpoint,
            url: url.to_string(),
            title: title.map(ToString::to_string),
            autoplay: true,
            surface: target.surface(),
        })
    }

    /// Resolve a `LaunchContent` search against the app at `endpoint`.
    ///
    /// # Errors
    /// [`LaunchRefusal`] when no such app exists, the app cannot be searched, or the
    /// search carried nothing to search for.
    pub fn launch_search(
        &self,
        endpoint: EndpointId,
        query: &str,
        autoplay: bool,
    ) -> Result<CastCommand, LaunchRefusal> {
        let app = self.at(endpoint).ok_or(LaunchRefusal::NoAppFound)?;

        let LaunchTarget::Browser { search: Some(tmpl) } = &app.launch else {
            // A media-URL app has no notion of "find me something called X", and neither
            // does a browser app that declared no search. Saying so is the whole point:
            // the alternative is opening a home page and calling it a result.
            return Err(LaunchRefusal::NotAllowed);
        };

        if query.trim().is_empty() {
            return Err(LaunchRefusal::NotAllowed);
        }

        Ok(CastCommand::Launch {
            app: endpoint,
            url: tmpl.replace("{query}", &url_encode(query)),
            title: Some(query.to_string()),
            autoplay,
            // A search only exists on a browser app, so there is nothing to choose.
            surface: Surface::Browser,
        })
    }
}

/// Whether the panel can open this URL at all.
///
/// Deliberately narrow. A client may send anything here, and the panel's answer to a
/// `file://` or a `javascript:` is not "try it and see".
fn is_playable_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Percent-encode a search term for a query string.
///
/// Hand-rolled rather than pulled in: the input is one query parameter, the unreserved
/// set is four lines, and the alternative is a dependency in a crate that has no other
/// use for one.
fn url_encode(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::udc::TargetApp;

    fn browser_app(vendor: u16, product: u16, search: Option<&str>) -> ContentApp {
        ContentApp {
            endpoint: 0,
            vendor_id: vendor,
            product_id: product,
            vendor_name: "Example".into(),
            name: "Example".into(),
            application_id: "com.example.app".into(),
            catalog_vendor_id: 0,
            launch: LaunchTarget::Browser {
                search: search.map(ToString::to_string),
            },
        }
    }

    fn catalogue() -> Catalogue {
        Catalogue::new([
            browser_app(4996, 1, Some("https://example.com/s?q={query}")),
            browser_app(4362, 2, None),
        ])
    }

    /// An endpoint is a position in our tree, not something an app brings with it.
    #[test]
    fn endpoints_are_assigned_not_configured() {
        let cat = catalogue();
        assert_eq!(cat.apps()[0].endpoint, FIRST_CONTENT_APP_ENDPOINT);
        assert_eq!(cat.apps()[1].endpoint, FIRST_CONTENT_APP_ENDPOINT + 1);
    }

    #[test]
    fn the_catalogue_is_bounded() {
        let many = (0..40).map(|i| browser_app(i, 0, None));
        assert_eq!(Catalogue::new(many).apps().len(), MAX_CONTENT_APPS);
    }

    /// Product id 0 on the *client's* side is a wildcard over the vendor.
    #[test]
    fn a_target_may_wildcard_the_product() {
        let cat = catalogue();
        assert!(cat.hosts_any(&[TargetApp {
            vendor_id: 4996,
            product_id: 0
        }]));
        assert!(cat.hosts_any(&[TargetApp {
            vendor_id: 4996,
            product_id: 1
        }]));
        assert!(!cat.hosts_any(&[TargetApp {
            vendor_id: 4996,
            product_id: 9
        }]));
    }

    /// An empty target list means "whatever you have" — which is still a mismatch when we
    /// have nothing, and the client should hear that before a passcode appears.
    #[test]
    fn an_empty_catalogue_hosts_nothing_even_for_an_empty_request() {
        assert!(catalogue().hosts_any(&[]));
        assert!(!Catalogue::default().hosts_any(&[]));
    }

    #[test]
    fn the_player_endpoint_takes_a_url_with_no_app() {
        let cmd = catalogue()
            .launch_url(PLAYER_ENDPOINT, "https://example.com/a.mp4", Some("A"))
            .unwrap();
        assert_eq!(
            cmd,
            CastCommand::Launch {
                app: PLAYER_ENDPOINT,
                url: "https://example.com/a.mp4".into(),
                title: Some("A".into()),
                autoplay: true,
                surface: Surface::Player,
            }
        );
    }

    /// A client may send anything. The answer to a `file://` is not "try it".
    #[test]
    fn only_http_urls_are_playable() {
        let cat = catalogue();
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,<script>",
            "",
        ] {
            assert_eq!(
                cat.launch_url(PLAYER_ENDPOINT, url, None),
                Err(LaunchRefusal::UrlNotAvailable),
                "{url}"
            );
        }
        assert!(cat
            .launch_url(PLAYER_ENDPOINT, "HTTPS://EXAMPLE.COM/x", None)
            .is_ok());
    }

    #[test]
    fn a_launch_at_an_endpoint_we_do_not_host_is_refused() {
        assert_eq!(
            catalogue().launch_url(99, "https://example.com/", None),
            Err(LaunchRefusal::NoAppFound)
        );
    }

    #[test]
    fn a_search_goes_through_the_app_template() {
        let cmd = catalogue()
            .launch_search(FIRST_CONTENT_APP_ENDPOINT, "the thing & more", true)
            .unwrap();
        assert_eq!(
            cmd,
            CastCommand::Launch {
                app: FIRST_CONTENT_APP_ENDPOINT,
                url: "https://example.com/s?q=the+thing+%26+more".into(),
                title: Some("the thing & more".into()),
                autoplay: true,
                surface: Surface::Browser,
            }
        );
    }

    /// An app with no search declines rather than opening its home page and calling that
    /// a result.
    #[test]
    fn an_unsearchable_app_declines() {
        let cat = catalogue();
        assert_eq!(
            cat.launch_search(FIRST_CONTENT_APP_ENDPOINT + 1, "anything", true),
            Err(LaunchRefusal::NotAllowed)
        );
        assert_eq!(
            cat.launch_search(FIRST_CONTENT_APP_ENDPOINT, "   ", true),
            Err(LaunchRefusal::NotAllowed)
        );
    }

    #[test]
    fn the_snapshot_survives_a_poisoned_lock() {
        let state = std::sync::Arc::new(PlayerState::new());
        state.set(PlayerSnapshot {
            state: PlaybackState::Playing,
            position: Duration::from_secs(3),
            ..PlayerSnapshot::default()
        });

        let poisoner = std::sync::Arc::clone(&state);
        let _ = std::thread::spawn(move || {
            poisoner.update(|_| panic!("poison the lock"));
        })
        .join();

        // Stale playback metadata is a better answer to a phone than a dead cluster.
        assert_eq!(state.get().state, PlaybackState::Playing);
        state.update(|s| s.state = PlaybackState::Paused);
        assert_eq!(state.get().state, PlaybackState::Paused);
    }
}
