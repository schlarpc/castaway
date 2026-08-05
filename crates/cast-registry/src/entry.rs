//! Parsing one registry response (ground rule 3: no sockets in here).
//!
//! The response is JSON behind Google's anti-hijacking prefix, and the interesting part
//! is not any single field but what the *combination* says about whether a receiver page
//! exists at all.

use serde::Deserialize;

use crate::RegistryError;

/// Google's anti-JSON-hijacking prefix. Every registry response carries it; a body that
/// does not is not a registry response (a 404 arrives as HTML).
const XSSI_PREFIX: &[u8] = b")]}'";

/// What a sender's `appId` turns out to name.
///
/// An enum rather than an `Option<String>` because the two ways a lookup can fail to
/// yield a page are *different facts with different handling*, and collapsing them is how
/// a mirroring app id ends up navigated to about:blank:
///
/// - a **native** app is a real, hostable Cast application that is not a web page at all.
///   The three Cast Streaming ids resolve this way (`native_app: true, external: true`,
///   and — decisively — **no `url` field**), because on a real Chromecast they are
///   binaries, not receivers. `proto-cast` already terminates those over RTP, so the
///   right answer is "not the browser's", not "unavailable".
/// - an **absent** app is one the registry does not know. That is a decline.
///
/// Deriving this from the response rather than from a hardcoded id list is the point:
/// Google adds streaming app ids without asking, and a list would silently start routing
/// the new one to a browser that cannot serve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppSurface {
    /// A web receiver: host `url` in the browser and speak the platform protocol to it.
    Web {
        /// Where the receiver page lives.
        url: String,
        /// The name to show while it loads, and to report in `RECEIVER_STATUS`.
        display_name: String,
    },
    /// A native application. Hostable, but not by a browser — mirroring is ours to
    /// terminate.
    Native {
        /// The name the registry gives it.
        display_name: String,
    },
    /// The registry does not have this app id.
    Absent,
}

impl AppSurface {
    /// The receiver page, if this is one.
    #[must_use]
    pub fn page_url(&self) -> Option<&str> {
        match self {
            Self::Web { url, .. } => Some(url),
            Self::Native { .. } | Self::Absent => None,
        }
    }

    /// The registry's name for the app, if it has one.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        match self {
            Self::Web { display_name, .. } | Self::Native { display_name } => Some(display_name),
            Self::Absent => None,
        }
    }
}

/// The subset of the registry response this receiver reads.
///
/// Deliberately partial. The response also carries promo artwork in a dozen locales, a
/// `whitelisting` blob, and feature bitfields whose meanings are Google's alone; parsing
/// them would be inventing obligations from fields we do not act on.
#[derive(Debug, Clone, Deserialize)]
struct RawEntry {
    app_id: Option<String>,
    display_name: Option<String>,
    /// Present exactly when there is a page to load.
    url: Option<String>,
    /// Set on apps that run as a binary on the device rather than as a page.
    #[serde(default)]
    native_app: bool,
}

/// Parse a registry response body into what it says about the app.
///
/// # Errors
/// [`RegistryError::NotRegistryJson`] if the body is not a prefixed JSON object — which
/// is what a 404 for an unknown app id looks like, since the registry answers those with
/// an HTML error page rather than with JSON.
pub fn parse(body: &[u8]) -> Result<AppSurface, RegistryError> {
    let json = body.strip_prefix(XSSI_PREFIX).ok_or_else(|| {
        RegistryError::NotRegistryJson(String::from_utf8_lossy(&body[..body.len().min(120)]).into())
    })?;
    let raw: RawEntry = serde_json::from_slice(json)
        .map_err(|e| RegistryError::Malformed(format!("{e} in {}", preview(json))))?;

    // An entry with neither an id nor a name is not an app description, whatever it
    // parsed as — `serde` will happily accept `{}` into a struct of options.
    if raw.app_id.is_none() && raw.display_name.is_none() {
        return Ok(AppSurface::Absent);
    }
    let display_name = raw.display_name.unwrap_or_else(|| {
        raw.app_id
            .clone()
            .unwrap_or_else(|| "Cast application".to_owned())
    });

    // `url` decides and `native_app` corroborates. The url is the operative field —
    // it is what there is to load — but the two are supposed to agree, and an entry
    // where they do not is the registry telling us something we do not model yet.
    // Logged rather than errored: a disagreement is a reason to look, not a reason to
    // refuse a launch that would otherwise work.
    match raw.url {
        Some(url) if !url.is_empty() => {
            if raw.native_app {
                tracing::warn!(
                    app_id = raw.app_id.as_deref().unwrap_or("?"),
                    %url,
                    "cast registry: an app marked native also carries a page; treating it as a page"
                );
            }
            Ok(AppSurface::Web { url, display_name })
        }
        _ => {
            if !raw.native_app {
                tracing::debug!(
                    app_id = raw.app_id.as_deref().unwrap_or("?"),
                    "cast registry: no page and not marked native; nothing to host in a browser"
                );
            }
            Ok(AppSurface::Native { display_name })
        }
    }
}

/// A bounded slice of a body, for an error a person has to read.
fn preview(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Every fixture is a real response, captured 2026-08-05. The point of the table is
    /// breadth: a first-party web receiver, two third-party ones, the reference sample,
    /// all three mirroring ids, and an id that does not exist.
    fn fixture(app_id: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/registry")
                .join(format!("{app_id}.json")),
        )
        .unwrap()
    }

    #[test]
    fn the_default_media_receiver_is_a_page() {
        let surface = parse(&fixture("CC1AD845")).unwrap();
        assert_eq!(surface.display_name(), Some("Default Media Receiver"));
        assert!(surface
            .page_url()
            .unwrap()
            .starts_with("https://www.gstatic.com/cast/sdk/default_receiver/1.0/app.html"));
    }

    #[test]
    fn youtube_resolves_to_the_leanback_page_the_browser_already_renders() {
        let surface = parse(&fixture("233637DE")).unwrap();
        assert_eq!(
            surface,
            AppSurface::Web {
                url: "https://www.youtube.com/tv?castv=2.0".into(),
                display_name: "YouTube".into(),
            }
        );
    }

    /// The app id that sent us here: Plex's picker was empty because we declined this.
    #[test]
    fn plex_resolves_to_a_page() {
        let surface = parse(&fixture("9AC194DC")).unwrap();
        assert_eq!(surface.page_url(), Some("https://app.plex.tv/cast"));
        assert_eq!(surface.display_name(), Some("Plex"));
    }

    #[test]
    fn netflix_resolves_to_its_bootloader() {
        // Resolving is not the same as running: Netflix gates on device certification
        // beyond the protocol and is expected to refuse. What must not happen is this
        // step failing, because then the refusal has no chance to be the *observed*
        // one and every Netflix cast looks like our bug.
        let surface = parse(&fixture("CA5E8412")).unwrap();
        assert!(surface.page_url().unwrap().contains("netflix.com"));
    }

    /// The heart of the type: all three streaming ids are native, and the registry says
    /// so by omitting `url`. Routing any of them to a browser would break the mirroring
    /// that already works.
    #[test]
    fn every_mirroring_app_id_is_native_and_has_no_page() {
        for app_id in ["0F5096E8", "85CDB22F", "674A0243"] {
            let surface = parse(&fixture(app_id)).unwrap();
            assert!(
                matches!(surface, AppSurface::Native { .. }),
                "{app_id} resolved to {surface:?}, which would send a mirroring session to the browser"
            );
            assert_eq!(surface.page_url(), None, "{app_id}");
        }
    }

    #[test]
    fn third_party_sample_receivers_resolve() {
        // Google's own CastVideos sample and a community one. Both are ordinary web
        // receivers, and both are what a person testing this will reach for first.
        let castvideos = parse(&fixture("4F8B3483")).unwrap();
        assert_eq!(
            castvideos.page_url(),
            Some("https://storage.googleapis.com/cast-reference-receiver/player.html")
        );
        let sharpcaster = parse(&fixture("B3419EF5")).unwrap();
        assert!(sharpcaster.page_url().unwrap().contains("default_receiver"));
    }

    /// An unknown app id: the registry answers 404 with an HTML body, not with JSON
    /// saying "no". So the absence has to be recognised from the shape of the reply.
    #[test]
    fn an_unknown_app_id_is_not_registry_json_at_all() {
        let err = parse(&fixture("DEADBEEF")).unwrap_err();
        assert!(
            matches!(err, RegistryError::NotRegistryJson(_)),
            "{err:?}: a 404 must not be mistaken for a malformed entry"
        );
    }

    #[test]
    fn a_prefixed_empty_object_is_absent_rather_than_a_nameless_app() {
        assert_eq!(parse(b")]}'\n{}").unwrap(), AppSurface::Absent);
    }

    #[test]
    fn a_body_without_the_prefix_is_refused_even_when_it_is_valid_json() {
        // Bare JSON means something answered that is not the registry — a captive
        // portal, a proxy error page with a JSON content type. Accepting it would let
        // an attacker on the path choose the page the panel loads.
        let err = parse(br#"{"app_id":"AAAAAAAA","url":"https://evil.example/"}"#).unwrap_err();
        assert!(matches!(err, RegistryError::NotRegistryJson(_)), "{err:?}");
    }

    #[test]
    fn an_empty_url_is_not_a_page() {
        let surface = parse(br#")]}'{"app_id":"AAAAAAAA","display_name":"X","url":""}"#).unwrap();
        assert_eq!(
            surface,
            AppSurface::Native {
                display_name: "X".into()
            }
        );
    }
}
