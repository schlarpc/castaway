//! DIAL (DIscovery And Launch): the REST surface a YouTube cast button hits to launch
//! our "YouTube app". Mounted on the shared HTTP host; discovery is the shared SSDP
//! responder answering `ST: urn:dial-multiscreen-org:service:dial:1`.
//!
//! A `POST /apps/YouTube` launches the app: we flip state to running and notify the app
//! layer (via a channel) to start the Lounge bind-channel client, which then drives
//! playback. DIAL itself carries no media — it's pure launch/stop/state.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppState {
    Stopped,
    Running,
}

struct DialInner {
    state: Mutex<AppState>,
    /// Absolute base URL of the DIAL REST service, e.g. `http://10.0.0.5:8080/dial`.
    base_url: String,
    launch_tx: mpsc::Sender<LaunchParams>,
}

/// The DIAL service. Exposes a [`Router`] to merge and an [`SsdpDevice`] to advertise.
pub struct DialService {
    inner: Arc<DialInner>,
}

impl DialService {
    /// Create the service. `base_url` is the absolute URL this router is mounted at
    /// (advertised as `Application-URL`); launches are sent on `launch_tx`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, launch_tx: mpsc::Sender<LaunchParams>) -> Self {
        Self {
            inner: Arc::new(DialInner {
                state: Mutex::new(AppState::Stopped),
                base_url: base_url.into(),
                launch_tx,
            }),
        }
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
    let xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:tvdevice:1</deviceType>
    <friendlyName>castaway</friendlyName>
    <manufacturer>castaway</manufacturer>
    <modelName>castaway</modelName>
  </device>
</root>"#;
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
    let _ = st
        .launch_tx
        .send(LaunchParams {
            pairing_code,
            raw_body: body,
        })
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
    StatusCode::OK.into_response()
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

    fn service() -> (DialService, mpsc::Receiver<LaunchParams>) {
        let (tx, rx) = mpsc::channel(4);
        (DialService::new("http://10.0.0.5:8080", tx), rx)
    }

    #[tokio::test]
    async fn description_carries_application_url() {
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
        let params = rx.recv().await.unwrap();
        assert_eq!(params.pairing_code.as_deref(), Some("abcd1234"));

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

    #[test]
    fn ssdp_device_advertises_dial_service() {
        let (svc, _rx) = service();
        let dev = svc.ssdp_device("dial-uuid");
        assert!(dev.services.contains(&DIAL_SERVICE_TYPE.to_string()));
        assert!(dev.targets().iter().any(|t| t.nt == DIAL_SERVICE_TYPE));
    }
}
