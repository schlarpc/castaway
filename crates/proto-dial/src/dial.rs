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
use tokio::sync::{mpsc, Mutex};
use tracing::info;

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
}

/// The DIAL service. Exposes a [`Router`] to merge and an [`SsdpDevice`] to advertise.
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
        events: mpsc::Sender<DialEvent>,
    ) -> Self {
        Self {
            inner: Arc::new(DialInner {
                state: Mutex::new(AppState::Stopped),
                friendly_name: friendly_name.into(),
                base_url: base_url.into(),
                events,
                osd: OnceLock::new(),
            }),
        }
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
    }

    /// The SSDP device to register with the shared responder.
    #[must_use]
    pub fn ssdp_device(&self, uuid: impl Into<String>) -> SsdpDevice {
        SsdpDevice {
            uuid: format!("uuid:{}", uuid.into()),
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

async fn device_description(State(st): State<Arc<DialInner>>) -> Response {
    let name = xml_escape(&st.friendly_name);
    let xml = format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:tvdevice:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <manufacturer>castaway</manufacturer>
    <modelName>castaway</modelName>
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
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<service xmlns="urn:dial-multiscreen-org:schemas:dial" dialVer="2.1">
  <name>{APP_NAME}</name>
  <options allowStop="true"/>
  <state>{state_str}</state>
  {link}
</service>"#
    );
    (StatusCode::OK, [("Content-Type", "text/xml")], xml).into_response()
}

async fn launch(State(st): State<Arc<DialInner>>, body: String) -> Response {
    *st.state.lock().await = AppState::Running;
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
    (StatusCode::CREATED, [("Location", location)], "").into_response()
}

async fn stop(State(st): State<Arc<DialInner>>) -> Response {
    *st.state.lock().await = AppState::Stopped;
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
        (k == key).then(|| v.replace('+', " "))
    })
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
            DialService::new("Test & Screen", "http://10.0.0.5:8080", tx),
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
        let svc = DialService::new("screen", "http://10.0.0.5:8080", tx).with_osd(osd);
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

    #[test]
    fn ssdp_device_advertises_dial_service() {
        let (svc, _rx) = service();
        let dev = svc.ssdp_device("dial-uuid");
        assert!(dev.services.contains(&DIAL_SERVICE_TYPE.to_string()));
        assert!(dev.targets().iter().any(|t| t.nt == DIAL_SERVICE_TYPE));
    }
}
