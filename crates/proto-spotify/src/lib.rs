//! # proto-spotify
//!
//! Spotify Connect **onboarding**: advertise `_spotify-connect._tcp`, answer `getInfo`
//! with our DH public key, and decrypt the `addUser` credentials blob (librespot's
//! pairing crypto, in [`crypto`]). This makes "castaway" appear in the Spotify device
//! picker and complete pairing.
//!
//! Post-pairing playback — the access point, the dealer WebSocket, connect-state, and
//! the audio pull — is librespot's, driven from [`session`] (DECISION-LOG D31). The
//! zeroconf half stays ours because it has to share this receiver's single HTTP host and
//! single mDNS responder, which librespot's own discovery would duplicate.
//!
//! [`crypto`] and [`discovery`] are pure and unit-tested; [`lib`](self) is the axum shell.
#![forbid(unsafe_code)]

pub mod control;
pub mod crypto;
pub mod discovery;
pub mod error;
pub mod session;
pub mod sink;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use castaway_core::{OsdSink, ProtocolKind, SessionSink};
use substrate_mdns::MdnsService;
use tokio::sync::Mutex;
use tracing::{info, warn};

pub use crypto::DhKeys;
pub use discovery::DeviceInfo;
pub use error::SpotifyError;
pub use session::{ConnectHandle, ConnectSettings, PairedUser};

/// The mDNS service type Spotify apps browse for.
pub const SPOTIFY_SERVICE_TYPE: &str = "_spotify-connect._tcp";
/// The HTTP path we host the zeroconf endpoint at (advertised as `CPath`).
pub const CPATH: &str = "/spotify";

struct SpotifyStateInner {
    keys: DhKeys,
    info: DeviceInfo,
    active_user: Mutex<String>,
    /// Where a successful pairing goes. `None` in tests and in builds that only want the
    /// device to appear in the picker without logging anyone in.
    connect: OnceLock<ConnectHandle>,
    /// Optional overlay sink so pairing is visible on screen.
    osd: OnceLock<OsdSink>,
}

/// A Spotify Connect onboarding endpoint. Like the other HTTP protocols it exposes a
/// [`Router`] to merge on the shared host and an [`MdnsService`] to advertise.
pub struct SpotifyService {
    state: Arc<SpotifyStateInner>,
}

impl SpotifyService {
    /// Create the service with a friendly `remote_name` and a stable `device_id`.
    ///
    /// On its own this only makes the receiver *appear* in the Spotify picker and pair.
    /// Playback needs [`SpotifyService::with_playback`]; without it a pairing is accepted,
    /// logged, and goes nowhere.
    #[must_use]
    pub fn new(remote_name: impl Into<String>, device_id: impl Into<String>) -> Self {
        Self {
            state: Arc::new(SpotifyStateInner {
                keys: DhKeys::generate(),
                info: DeviceInfo {
                    remote_name: remote_name.into(),
                    device_id: device_id.into(),
                },
                active_user: Mutex::new(String::new()),
                connect: OnceLock::new(),
                osd: OnceLock::new(),
            }),
        }
    }

    /// Start the Connect session runner and route pairings into it.
    ///
    /// The `device_id` handed to the runner is this service's own, and it has to be: it
    /// is what `getInfo` advertised, and the blob is encrypted against it.
    #[must_use]
    pub fn with_playback(self, sink: SessionSink, initial_volume: f32) -> Self {
        let handle = session::spawn(
            ConnectSettings {
                device_name: self.state.info.remote_name.clone(),
                device_id: self.state.info.device_id.clone(),
                initial_volume,
            },
            sink,
            self.state.osd.get().cloned(),
        );
        let _ = self.state.connect.set(handle);
        self
    }

    /// Give this adapter an [`OsdSink`] so pairing shows a banner on the overlay.
    ///
    /// Call before [`SpotifyService::with_playback`] — the runner takes its own clone, so
    /// an overlay attached afterwards will not reach session-level messages.
    #[must_use]
    pub fn with_osd(self, osd: OsdSink) -> Self {
        let _ = self.state.osd.set(osd);
        self
    }

    /// The axum router for the zeroconf endpoint.
    pub fn router(&self) -> Router {
        Router::new()
            .route(CPATH, get(handle_get).post(handle_post))
            .with_state(self.state.clone())
    }

    /// The mDNS advertisement for this endpoint, hosted on `http_port` at `host`.
    #[must_use]
    pub fn mdns_service(&self, http_port: u16, host: impl Into<String>) -> MdnsService {
        MdnsService::new(
            SPOTIFY_SERVICE_TYPE,
            &self.state.info.remote_name,
            host,
            http_port,
        )
        .with_txt("CPath", CPATH)
        .with_txt("VERSION", "1.0")
        .with_txt("Stack", "SP")
    }

    /// The protocol this service implements.
    #[must_use]
    pub fn kind(&self) -> ProtocolKind {
        ProtocolKind::Spotify
    }
}

async fn handle_get(
    State(st): State<Arc<SpotifyStateInner>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    match params.get("action").map(String::as_str) {
        Some("getInfo") | None => get_info_response(&st).await,
        Some(other) => bad_request(&format!("unknown GET action {other}")),
    }
}

async fn handle_post(State(st): State<Arc<SpotifyStateInner>>, body: String) -> Response {
    let params: HashMap<String, String> = serde_urlencoded_from(&body);
    match params.get("action").map(String::as_str) {
        Some("addUser") => match serde_urlencoded::from_str::<discovery::AddUser>(&body) {
            Ok(req) => match discovery::add_user(&req, &st.keys) {
                Ok(creds) => {
                    *st.active_user.lock().await = creds.user_name.clone();
                    info!(user = %creds.user_name, blob_len = creds.blob.len(),
                        "Spotify addUser paired");
                    match st.connect.get() {
                        Some(handle) => {
                            // Hand off and answer immediately. The phone is waiting on
                            // this response and the login behind it takes an AP handshake
                            // — blocking here makes the picker look like it hung, and the
                            // Spotify app gives up long before login5 finishes.
                            let user = PairedUser {
                                user_name: creds.user_name.clone(),
                                blob: creds.blob,
                            };
                            if let Err(e) = handle.paired(user).await {
                                warn!(error = %e, "Spotify pairing accepted but the runner is gone");
                            }
                        }
                        None => {
                            // Pairing works, playback was never wired. Say so rather than
                            // leaving a device that joins and stays silent.
                            warn!("Spotify paired with no playback backend configured");
                            if let Some(osd) = st.osd.get() {
                                osd.banner(
                                    format!("Spotify: {} paired (no playback)", creds.user_name),
                                    Duration::from_secs(4),
                                );
                            }
                        }
                    }
                    json_ok(discovery::add_user_ok())
                }
                Err(e) => {
                    warn!(error = %e, "Spotify addUser decrypt failed");
                    bad_request(&e.to_string())
                }
            },
            Err(e) => bad_request(&format!("bad addUser form: {e}")),
        },
        Some("getInfo") => get_info_response(&st).await,
        _ => bad_request("missing/unknown POST action"),
    }
}

async fn get_info_response(st: &SpotifyStateInner) -> Response {
    let active = st.active_user.lock().await.clone();
    json_ok(discovery::get_info(&st.info, &st.keys, &active))
}

fn serde_urlencoded_from(body: &str) -> HashMap<String, String> {
    serde_urlencoded::from_str(body).unwrap_or_default()
}

fn json_ok(body: String) -> Response {
    (StatusCode::OK, [("Content-Type", "application/json")], body).into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn mdns_service_has_cpath_txt() {
        let svc = SpotifyService::new("castaway", "deadbeef");
        let m = svc.mdns_service(8080, "castaway");
        assert!(m.txt.iter().any(|(k, v)| k == "CPath" && v == CPATH));
        assert_eq!(m.service_type, SPOTIFY_SERVICE_TYPE);
    }

    #[tokio::test]
    async fn getinfo_over_http_returns_public_key() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let svc = SpotifyService::new("castaway", "deadbeef");
        let app = svc.router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/spotify?action=getInfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("publicKey"));
    }
}
