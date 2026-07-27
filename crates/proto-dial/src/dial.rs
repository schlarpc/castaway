//! DIAL (DIscovery And Launch): the REST surface a YouTube cast button hits to launch
//! our "YouTube app". Mounted on the shared HTTP host; discovery is the shared SSDP
//! responder answering `ST: urn:dial-multiscreen-org:service:dial:1`.
//!
//! A `POST /apps/YouTube` launches the app: we flip state to running and notify the app
//! layer (via a channel) to start the Lounge bind-channel client, which then drives
//! playback. DIAL itself carries no media — it's pure launch/stop/state.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use castaway_core::OsdSink;
use substrate_ssdp::SsdpDevice;

use crate::error::DialError;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

/// The DIAL service type senders search for.
pub const DIAL_SERVICE_TYPE: &str = "urn:dial-multiscreen-org:service:dial:1";
/// The DIAL app we register.
pub const APP_NAME: &str = "YouTube";

/// Emitted when a sender launches the YouTube app via DIAL. The app layer uses it to
/// kick off Lounge screen registration.
#[derive(Debug, Clone)]
pub struct LaunchParams {
    /// The DIAL `pairingCode` from the launch body, if present (used to bind the Lounge
    /// screen to the sender's session).
    pub pairing_code: Option<String>,
    /// The raw launch body, for any fields we don't model yet.
    pub raw_body: String,
}

impl LaunchParams {
    /// The YouTube leanback (TV) URL for this launch. The DIAL launch body is
    /// `application/x-www-form-urlencoded` and YouTube's receiver contract is to pass
    /// those fields through as query params on `youtube.com/tv`, so the sender's
    /// `pairingCode` binds its Lounge session to this screen.
    #[must_use]
    pub fn leanback_url(&self) -> String {
        let body = self.raw_body.trim();
        if body.is_empty() {
            "https://www.youtube.com/tv".to_string()
        } else {
            format!("https://www.youtube.com/tv?{body}")
        }
    }
}

/// The Lounge screen id of the page we launched.
///
/// Senders need this to attach to an app that is *already* running: with it they can mint
/// a lounge token and drive the screen directly, with no launch and no pairing code at
/// all. Without it, a sender that finds the app running has no way in — the only other
/// route to a screen is a pairing code it supplied itself, on a launch it therefore has
/// to make. That is why a real TV publishes this in its app-info XML, and why we do.
///
/// Parsed, not trusted: it comes back from a network lookup and goes straight into XML
/// every sender on the LAN reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenId(String);

impl ScreenId {
    /// Parse a screen id. The Lounge's are 64 hex characters; the bound here is
    /// deliberately looser than that (any ASCII alphanumeric, `-`, or `_`) so a format
    /// change does not break us, and deliberately not "any string".
    ///
    /// # Errors
    /// [`DialError::NotAScreenId`] if it is empty, over 128 characters, or carries
    /// anything outside that set.
    pub fn parse(raw: &str) -> Result<Self, DialError> {
        if raw.is_empty() {
            return Err(DialError::NotAScreenId("empty"));
        }
        if raw.len() > 128 {
            return Err(DialError::NotAScreenId("too long"));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(DialError::NotAScreenId("unexpected characters"));
        }
        Ok(Self(raw.to_string()))
    }

    /// The id as it goes on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The current screen id, shared between the DIAL routes and whatever resolves it.
///
/// It is a slot rather than a constructor argument because the id does not exist yet when
/// the service is built, or even when a launch is answered: the page has to load and
/// register with the Lounge first. Empty is the honest state until then.
#[derive(Clone, Default)]
pub struct ScreenSlot {
    inner: Arc<Mutex<Option<ScreenId>>>,
}

impl ScreenSlot {
    /// Publish the screen id senders should use to attach.
    pub async fn set(&self, id: ScreenId) {
        *self.inner.lock().await = Some(id);
    }

    /// Forget it — the page is gone, or a new launch is about to replace it. A stale id
    /// is worse than none: a sender would attach to a screen that no longer exists.
    pub async fn clear(&self) {
        *self.inner.lock().await = None;
    }

    /// The current id, if the page has registered one.
    pub async fn get(&self) -> Option<ScreenId> {
        self.inner.lock().await.clone()
    }
}

/// A DIAL app-lifecycle event, sent to the app layer over the service's channel.
#[derive(Debug, Clone)]
pub enum DialEvent {
    /// A sender launched the app (`POST /apps/YouTube`).
    Launched(LaunchParams),
    /// A sender stopped the app (`DELETE`); the display surface should be dismissed.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppState {
    Stopped,
    Running,
}

struct DialInner {
    state: Mutex<AppState>,
    /// The receiver's friendly name — what the sender's cast picker shows.
    friendly_name: String,
    /// Absolute base URL of the DIAL REST service, e.g. `http://10.0.0.5:8080/dial`.
    base_url: String,
    events: mpsc::Sender<DialEvent>,
    /// Optional overlay sink so this adapter can post its own status ("Launching …").
    osd: OnceLock<OsdSink>,
    /// The launched page's Lounge screen id, once something has resolved it.
    screen: ScreenSlot,
    /// This device's UUID, without the `uuid:` prefix.
    ///
    /// Needed by the description as well as the advertisement: UPnP requires `<UDN>`, and
    /// senders use it to tie the SSDP `USN` to the description they just fetched.
    uuid: String,
}

/// The DIAL service. Exposes a [`Router`] to merge and an [`SsdpDevice`] to advertise.
///
/// Cheap to clone — everything is behind one `Arc` — so the routes, the advertisement and
/// anything that needs to change the app state (the browser giving up on a page, say) can
/// each hold one.
#[derive(Clone)]
pub struct DialService {
    inner: Arc<DialInner>,
}

impl DialService {
    /// Create the service. `friendly_name` is what the sender's cast picker lists;
    /// `base_url` is the absolute URL this router is mounted at (advertised as
    /// `Application-URL`); launch/stop events are sent on `events`.
    #[must_use]
    pub fn new(
        friendly_name: impl Into<String>,
        base_url: impl Into<String>,
        uuid: impl Into<String>,
        events: mpsc::Sender<DialEvent>,
    ) -> Self {
        Self {
            inner: Arc::new(DialInner {
                state: Mutex::new(AppState::Stopped),
                friendly_name: friendly_name.into(),
                base_url: base_url.into(),
                events,
                osd: OnceLock::new(),
                screen: ScreenSlot::default(),
                uuid: uuid.into(),
            }),
        }
    }

    /// Mark the app as stopped without a sender having asked.
    ///
    /// For the case where the *page* died rather than the cast ending: a crashed
    /// renderer that we could not recover leaves nothing on screen, and continuing to
    /// answer `<state>running</state>` with a published screen id invites senders to
    /// attach to something that is not there. That is the half of a browser crash a
    /// phone can see.
    ///
    /// Does not emit [`DialEvent::Stopped`]: the caller is the thing that gave up, so
    /// telling it to dismiss a surface it has already dismissed would be a loop.
    pub async fn abandoned(&self) {
        let mut state = self.inner.state.lock().await;
        if *state == AppState::Running {
            *state = AppState::Stopped;
            self.inner.screen.clear().await;
            warn!("DIAL: the launched page is gone; no longer advertising it as running");
        }
    }

    /// The slot holding the launched page's screen id. Whatever resolves the id writes
    /// here; the app-info route reads it.
    #[must_use]
    pub fn screen_slot(&self) -> ScreenSlot {
        self.inner.screen.clone()
    }

    /// Give this adapter an [`OsdSink`] so it can surface its own status on the overlay.
    #[must_use]
    pub fn with_osd(self, osd: OsdSink) -> Self {
        let _ = self.inner.osd.set(osd);
        self
    }

    /// The DIAL router. Mount its paths under the host root.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/dial/dd.xml", get(device_description))
            .route(
                "/dial/apps/YouTube",
                get(app_state).post(launch).delete(stop),
            )
            .route("/dial/apps/YouTube/run", axum::routing::delete(stop))
            .with_state(self.inner.clone())
            .layer(axum::middleware::map_response(add_cors))
    }

    /// The SSDP device to register with the shared responder.
    #[must_use]
    pub fn ssdp_device(&self) -> SsdpDevice {
        SsdpDevice {
            uuid: format!("uuid:{}", self.inner.uuid),
            device_type: "urn:schemas-upnp-org:device:tvdevice:1".to_string(),
            services: vec![DIAL_SERVICE_TYPE.to_string()],
        }
    }

    /// The path the SSDP `LOCATION` should point at (the DIAL device description).
    #[must_use]
    pub fn description_path(&self) -> &'static str {
        "/dial/dd.xml"
    }
}

/// DIAL 2.1 requires the REST service to answer CORS, and to *expose* `Location`.
///
/// A browser-based sender cannot otherwise read the header that tells it where the running
/// app is: `Location` is not among the headers a cross-origin response exposes by default.
/// Native phone apps do not care; anything driving DIAL over XHR does.
///
/// `*` rather than a list of origins: they are whatever page a guest happens to have open,
/// we have no list to check against, and there is nothing behind these routes worth
/// guarding with one — every one of them is already reachable by anybody on the LAN.
async fn add_cors(mut response: Response) -> Response {
    response.headers_mut().insert(
        "Access-Control-Allow-Origin",
        axum::http::HeaderValue::from_static("*"),
    );
    response.headers_mut().insert(
        "Access-Control-Expose-Headers",
        axum::http::HeaderValue::from_static("Location"),
    );
    response
}

async fn device_description(State(st): State<Arc<DialInner>>) -> Response {
    let name = xml_escape(&st.friendly_name);
    // `<UDN>` is not optional and its absence is not forgiving. UPnP mandates it, and
    // Chromium's DIAL device-description parser treats an empty unique-id as a parse
    // failure and drops the device outright — so we were invisible to a whole family of
    // senders while `curl` and `yt-selfplay`, neither of which reads it, both passed.
    // Android senders use it to tie the SSDP `USN` back to the description they fetched.
    let udn = xml_escape(&format!("uuid:{}", st.uuid));
    let xml = format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:tvdevice:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>castaway</manufacturer>
    <modelName>castaway</modelName>
    <UDN>{udn}</UDN>
  </device>
</root>"#
    );
    // DIAL: the description response MUST carry Application-URL pointing at the app base.
    let app_url = format!("{}/dial/apps/", st.base_url.trim_end_matches('/'));
    (
        StatusCode::OK,
        [
            ("Content-Type", "text/xml".to_string()),
            ("Application-URL", app_url),
        ],
        xml,
    )
        .into_response()
}

async fn app_state(State(st): State<Arc<DialInner>>) -> Response {
    let running = *st.state.lock().await == AppState::Running;
    let (state_str, link) = if running {
        ("running", "<link rel=\"run\" href=\"run\"/>")
    } else {
        ("stopped", "")
    };
    // The screen id is what a sender reads to attach to an app that is already running,
    // so it is only meaningful while it is. Published under `additionalData`, where the
    // DIAL spec puts app-specific state and where senders look for it.
    let additional = match st.screen.get().await.filter(|_| running) {
        Some(id) => format!(
            "\n  <additionalData>\n    <screenId>{}</screenId>\n  </additionalData>",
            xml_escape(id.as_str())
        ),
        None => String::new(),
    };
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="2.1">
  <name>{APP_NAME}</name>
  <options allowStop="true"/>
  <state>{state_str}</state>
  {link}{additional}
</service>"#
    );
    (StatusCode::OK, [("Content-Type", "text/xml")], xml).into_response()
}

async fn launch(State(st): State<Arc<DialInner>>, body: String) -> Response {
    // DIAL distinguishes starting an app from re-launching one that is already up: 201
    // Created for the first, 200 OK for the second. Senders read it — a 201 means "I made
    // this", and answering it for an app that was already running invites a client to
    // believe it owns a session someone else started.
    let relaunch = {
        let mut state = st.state.lock().await;
        let was_running = *state == AppState::Running;
        *state = AppState::Running;
        was_running
    };
    // A launch reloads the page, and the new page registers a new screen. Publishing the
    // old id until the new one resolves would point senders at a screen that is gone.
    st.screen.clear().await;
    let pairing_code = form_field(&body, "pairingCode");
    info!(pairing = ?pairing_code, "DIAL launched YouTube");
    if let Some(osd) = st.osd.get() {
        osd.banner("Launching YouTube\u{2026}", Duration::from_secs(4));
    }
    let _ = st
        .events
        .send(DialEvent::Launched(LaunchParams {
            pairing_code,
            raw_body: body,
        }))
        .await;
    let location = format!(
        "{}/dial/apps/YouTube/run",
        st.base_url.trim_end_matches('/')
    );
    let status = if relaunch {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    (status, [("Location", location)], "").into_response()
}

async fn stop(State(st): State<Arc<DialInner>>) -> Response {
    *st.state.lock().await = AppState::Stopped;
    st.screen.clear().await;
    info!("DIAL stopped YouTube");
    let _ = st.events.send(DialEvent::Stopped).await;
    StatusCode::OK.into_response()
}

/// Escape the five XML-special characters for element text (the friendly name is
/// operator-configured free text).
fn xml_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&apos;".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// Extract a field from an `application/x-www-form-urlencoded` body (no external dep for
/// this one lookup).
fn form_field(body: &str, key: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Decode an `application/x-www-form-urlencoded` value.
///
/// `+` is a space and `%XX` is a byte. Only the first half was handled, so a pairing code
/// carrying an escaped character resolved the wrong screen — latent, because today's
/// senders use plain alphanumerics, and silent when it does happen: the screen lookup
/// simply never matches and the phone can never queue anything.
///
/// Invalid escapes are left as written rather than dropped. A sender that means a literal
/// `%` has produced a value we should pass through unharmed, and mangling it further is
/// not an improvement on mangling it once.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn service() -> (DialService, mpsc::Receiver<DialEvent>) {
        let (tx, rx) = mpsc::channel(4);
        (
            DialService::new(
                "Test & Screen",
                "http://10.0.0.5:8080",
                "0f8c1e2a-0000-4000-8000-000000000001",
                tx,
            ),
            rx,
        )
    }

    #[tokio::test]
    async fn description_carries_application_url_and_friendly_name() {
        let (svc, _rx) = service();
        let resp = svc
            .router()
            .oneshot(
                Request::builder()
                    .uri("/dial/dd.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let app_url = resp.headers().get("Application-URL").unwrap();
        assert_eq!(app_url, "http://10.0.0.5:8080/dial/apps/");
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let body = String::from_utf8_lossy(&body).to_string();
        // The configured name is what the sender's cast picker shows — XML-escaped.
        assert!(
            body.contains("<friendlyName>Test &amp; Screen</friendlyName>"),
            "dd.xml should carry the escaped friendly name: {body}"
        );
    }

    #[test]
    fn a_form_value_is_percent_decoded_not_just_plus_decoded() {
        // Latent but silent: only `+` was handled, so a pairing code carrying an escaped
        // character resolved the wrong screen — the lookup simply never matched and the
        // phone could never queue anything. Today's senders use plain alphanumerics,
        // which is why nothing noticed.
        assert_eq!(
            form_field("pairingCode=ab%2Dcd&v=x", "pairingCode").as_deref(),
            Some("ab-cd")
        );
        assert_eq!(
            form_field("name=Chaz%27s+Phone", "name").as_deref(),
            Some("Chaz's Phone"),
            "both encodings appear in one value"
        );
        // A literal percent a sender meant to send survives rather than being eaten.
        assert_eq!(form_field("v=100%", "v").as_deref(), Some("100%"));
        assert_eq!(form_field("v=%zz", "v").as_deref(), Some("%zz"));
        // Multi-byte UTF-8 arrives as escapes and must be reassembled, not decoded byte
        // by byte into replacement characters.
        assert_eq!(form_field("v=%E6%95%B4", "v").as_deref(), Some("\u{6574}"));
    }

    #[tokio::test]
    async fn relaunching_a_running_app_answers_200_rather_than_201() {
        // 201 Created means "I made this". Answering it for an app that was already
        // running invites a sender to believe it owns a session someone else started.
        let (svc, _rx) = service();
        let launch = |body: &'static str| {
            let app = svc.router();
            async move {
                app.oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/dial/apps/YouTube")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };

        let first = launch("pairingCode=one").await;
        assert_eq!(
            first.status(),
            StatusCode::CREATED,
            "the app was not running"
        );
        // Either way the sender is told where the running app is.
        assert!(first.headers().get("Location").is_some());

        let again = launch("pairingCode=two").await;
        assert_eq!(
            again.status(),
            StatusCode::OK,
            "a relaunch is not a creation"
        );
        assert!(again.headers().get("Location").is_some());
    }

    #[tokio::test]
    async fn the_rest_service_answers_cors_and_exposes_location() {
        // DIAL 2.1 asks for both. Without `Access-Control-Expose-Headers`, a
        // browser-based sender cannot read `Location` at all — it is not exposed by
        // default on a cross-origin response — so it cannot find the app it just started.
        let (svc, _rx) = service();
        let resp = svc
            .router()
            .oneshot(
                Request::builder()
                    .uri("/dial/apps/YouTube")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.headers().get("Access-Control-Allow-Origin").unwrap(),
            "*"
        );
        assert_eq!(
            resp.headers().get("Access-Control-Expose-Headers").unwrap(),
            "Location"
        );
    }

    #[tokio::test]
    async fn launch_sets_running_and_emits_params() {
        let (svc, mut rx) = service();
        let app = svc.router();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dial/apps/YouTube")
                    .body(Body::from("pairingCode=abcd1234&theme=cl"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert!(resp.headers().get("Location").is_some());
        let DialEvent::Launched(params) = rx.recv().await.unwrap() else {
            panic!("expected a Launched event");
        };
        assert_eq!(params.pairing_code.as_deref(), Some("abcd1234"));
        assert_eq!(
            params.leanback_url(),
            "https://www.youtube.com/tv?pairingCode=abcd1234&theme=cl"
        );

        // Stop emits its own event so the app layer can dismiss the surface.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/dial/apps/YouTube/run")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(matches!(rx.recv().await.unwrap(), DialEvent::Stopped));

        // Re-launch so the state check below still sees "running".
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dial/apps/YouTube")
                    .body(Body::from("pairingCode=abcd1234&theme=cl"))
                    .unwrap(),
            )
            .await
            .unwrap();

        // State now reports running.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/dial/apps/YouTube")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("<state>running</state>"));
    }

    #[tokio::test]
    async fn launch_posts_osd_banner() {
        use castaway_core::{osd_channel, OsdCommand};
        let (tx, _rx) = mpsc::channel(4);
        let (osd, osd_rx) = osd_channel();
        let svc = DialService::new(
            "screen",
            "http://10.0.0.5:8080",
            "0f8c1e2a-0000-4000-8000-000000000001",
            tx,
        )
        .with_osd(osd);
        svc.router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dial/apps/YouTube")
                    .body(Body::from("pairingCode=xyz"))
                    .unwrap(),
            )
            .await
            .unwrap();
        match osd_rx.try_recv() {
            Some(OsdCommand::Show(m)) => assert!(m.text.contains("YouTube")),
            other => panic!("expected an OSD banner, got {other:?}"),
        }
    }

    #[test]
    fn leanback_url_with_empty_body_has_no_query() {
        let params = LaunchParams {
            pairing_code: None,
            raw_body: String::new(),
        };
        assert_eq!(params.leanback_url(), "https://www.youtube.com/tv");
    }

    async fn app_info(app: &Router) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/dial/apps/YouTube")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    async fn launch_it(app: &Router) {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dial/apps/YouTube")
                    .body(Body::from("pairingCode=abcd1234"))
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_running_app_publishes_the_screen_id_senders_attach_with() {
        let (svc, _rx) = service();
        let slot = svc.screen_slot();
        let app = svc.router();

        // Before a launch there is no page, so nothing to attach to.
        assert!(!app_info(&app).await.contains("additionalData"));

        launch_it(&app).await;
        // Launched, but the page has not registered yet: still nothing to publish, and
        // publishing a guess would send senders to a screen that does not exist.
        assert!(!app_info(&app).await.contains("additionalData"));

        slot.set(ScreenId::parse("f970ef4ce158e4a15ca9b7f228103591").unwrap())
            .await;
        let body = app_info(&app).await;
        assert!(
            body.contains("<screenId>f970ef4ce158e4a15ca9b7f228103591</screenId>"),
            "a running app must publish its screen id: {body}"
        );
    }

    #[tokio::test]
    async fn the_screen_id_does_not_outlive_the_page_that_registered_it() {
        let (svc, _rx) = service();
        let slot = svc.screen_slot();
        let app = svc.router();
        launch_it(&app).await;
        slot.set(ScreenId::parse("aaaa1111").unwrap()).await;
        assert!(app_info(&app).await.contains("aaaa1111"));

        // A relaunch reloads the page, and the new page is a new screen.
        launch_it(&app).await;
        assert!(
            !app_info(&app).await.contains("aaaa1111"),
            "a relaunch must not keep publishing the old screen"
        );

        slot.set(ScreenId::parse("bbbb2222").unwrap()).await;
        app.clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/dial/apps/YouTube/run")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = app_info(&app).await;
        assert!(body.contains("<state>stopped</state>"), "{body}");
        assert!(
            !body.contains("additionalData"),
            "a stopped app has no screen to attach to: {body}"
        );
    }

    #[test]
    fn screen_ids_are_parsed_not_trusted() {
        assert!(ScreenId::parse("").is_err());
        assert!(ScreenId::parse(&"a".repeat(129)).is_err());
        // It lands in XML that every sender on the LAN reads.
        assert!(ScreenId::parse("abc</screenId><evil>").is_err());
        assert_eq!(
            ScreenId::parse("Ab-9_z").unwrap().as_str(),
            "Ab-9_z",
            "the character set the Lounge actually uses must survive"
        );
    }

    #[test]
    fn ssdp_device_advertises_dial_service() {
        let (svc, _rx) = service();
        let dev = svc.ssdp_device();
        assert!(dev.services.contains(&DIAL_SERVICE_TYPE.to_string()));
        assert!(dev.targets().iter().any(|t| t.nt == DIAL_SERVICE_TYPE));
    }

    #[tokio::test]
    async fn the_description_carries_a_udn_matching_the_advertisement() {
        // Chromium's DIAL parser drops a device whose description has no unique id, and
        // Android senders use the UDN to tie the SSDP `USN` to the description they just
        // fetched — so these two disagreeing is as bad as the tag being missing. Neither
        // `curl` nor `yt-selfplay` reads it, which is why this went unnoticed.
        let (svc, _rx) = service();
        let resp = svc
            .router()
            .oneshot(
                Request::builder()
                    .uri("/dial/dd.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        let body = String::from_utf8_lossy(&body).to_string();
        let dev = svc.ssdp_device();
        assert!(
            body.contains("<UDN>uuid:0f8c1e2a-0000-4000-8000-000000000001</UDN>"),
            "no UDN in {body}"
        );
        assert!(
            body.contains(&format!("<UDN>{}</UDN>", dev.uuid)),
            "the UDN must be the uuid we advertise"
        );
    }
}
