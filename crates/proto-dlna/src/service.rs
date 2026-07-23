//! The axum wiring: HTTP → parse SOAP → drive the [`Renderer`] → emit `SessionEvent` +
//! return a SOAP response. This is the thin I/O shell; all decisions live in
//! [`crate::state`]. The router is meant to be merged onto the shared HTTP host.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use castaway_core::{OsdSink, SessionSink};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::descriptions::{self, paths, service_types};
use crate::error::DlnaError;
use crate::soap::{self, SoapAction};
use crate::state::Renderer;

/// Which UPnP service a control request targets.
#[derive(Debug, Clone, Copy)]
enum ServiceKind {
    AvTransport,
    RenderingControl,
    ConnectionManager,
}

impl ServiceKind {
    const fn service_type(self) -> &'static str {
        match self {
            ServiceKind::AvTransport => service_types::AVTRANSPORT,
            ServiceKind::RenderingControl => service_types::RENDERING_CONTROL,
            ServiceKind::ConnectionManager => service_types::CONNECTION_MANAGER,
        }
    }
}

/// Shared handler state.
pub(crate) struct DlnaState {
    pub(crate) renderer: Mutex<Renderer>,
    pub(crate) sink: SessionSink,
    pub(crate) friendly_name: String,
    /// Bare UUID (no `uuid:` prefix).
    pub(crate) uuid: String,
    /// Optional overlay sink for transport feedback ("Volume 60%", "Muted").
    pub(crate) osd: OnceLock<OsdSink>,
}

/// Build the DLNA router over shared state.
pub(crate) fn router(state: Arc<DlnaState>) -> Router {
    Router::new()
        .route(paths::DESCRIPTION, get(description))
        .route(paths::AVT_SCPD, get(|| async { xml_ok(descriptions::AVTRANSPORT_SCPD.to_string()) }))
        .route(paths::RC_SCPD, get(|| async { xml_ok(descriptions::RENDERING_CONTROL_SCPD.to_string()) }))
        .route(paths::CM_SCPD, get(|| async { xml_ok(descriptions::CONNECTION_MANAGER_SCPD.to_string()) }))
        .route(paths::AVT_CONTROL, post(control_avt))
        .route(paths::RC_CONTROL, post(control_rc))
        .route(paths::CM_CONTROL, post(control_cm))
        // GENA eventing: accept SUBSCRIBE/UNSUBSCRIBE (non-standard HTTP methods) via
        // `any` and acknowledge. Full eventing (LastChange NOTIFY) is not implemented;
        // control points fall back to polling GetTransportInfo, which we answer.
        .route(paths::AVT_EVENT, any(subscribe_ack))
        .route(paths::RC_EVENT, any(subscribe_ack))
        .route("/dlna/event/ConnectionManager", any(subscribe_ack))
        .with_state(state)
}

async fn description(State(st): State<Arc<DlnaState>>) -> Response {
    xml_ok(descriptions::device_description(
        &st.friendly_name,
        &st.uuid,
    ))
}

async fn control_avt(
    State(st): State<Arc<DlnaState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_control(&st, ServiceKind::AvTransport, &headers, &body).await
}

async fn control_rc(
    State(st): State<Arc<DlnaState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_control(&st, ServiceKind::RenderingControl, &headers, &body).await
}

async fn control_cm(
    State(st): State<Arc<DlnaState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    handle_control(&st, ServiceKind::ConnectionManager, &headers, &body).await
}

async fn handle_control(
    st: &DlnaState,
    kind: ServiceKind,
    _headers: &HeaderMap,
    body: &str,
) -> Response {
    let action = match SoapAction::parse(body) {
        Ok(a) => a,
        Err(e) => return fault_response(&e),
    };
    debug!(service = ?kind, action = %action.name, "DLNA control");

    let outcome = {
        let mut r = st.renderer.lock().await;
        match kind {
            ServiceKind::AvTransport => r.av_transport(&action),
            ServiceKind::RenderingControl => r.rendering_control(&action),
            ServiceKind::ConnectionManager => r.connection_manager(&action),
        }
    };

    match outcome {
        Ok(out) => {
            if let Some(event) = out.event {
                if let Err(e) = st.sink.emit(event).await {
                    warn!(error = %e, "failed to emit DLNA session event");
                }
            }
            post_control_osd(st, &action);
            let xml = soap::action_response(kind.service_type(), &action.name, &out.out_args);
            xml_ok(xml)
        }
        Err(e) => fault_response(&e),
    }
}

/// Surface transport feedback on the overlay for actions the "Now casting" banner
/// doesn't cover (volume/mute changes).
fn post_control_osd(st: &DlnaState, action: &SoapAction) {
    let Some(osd) = st.osd.get() else { return };
    let text = match action.name.as_str() {
        "SetVolume" => action.arg("DesiredVolume").map(|v| format!("Volume {v}%")),
        "SetMute" => Some(
            if matches!(action.arg("DesiredMute"), Some("1" | "true" | "True")) {
                "Muted".to_string()
            } else {
                "Unmuted".to_string()
            },
        ),
        _ => None,
    };
    if let Some(text) = text {
        osd.banner(text, Duration::from_secs(2));
    }
}

async fn subscribe_ack(headers: HeaderMap) -> Response {
    // Reply with a plausible SID + timeout so control points consider the subscription
    // established, even though we don't push events. Echo NT/callback presence loosely.
    let mut resp = StatusCode::OK.into_response();
    let h = resp.headers_mut();
    // A stable-ish SID derived from the callback header, or a fixed placeholder.
    let sid = headers
        .get("SID")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| "uuid:castaway-sub-0".to_string(), ToString::to_string);
    if let Ok(v) = sid.parse() {
        h.insert("SID", v);
    }
    if let Ok(v) = "Second-1800".parse() {
        h.insert("TIMEOUT", v);
    }
    resp
}

fn xml_ok(body: String) -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        body,
    )
        .into_response()
}

fn fault_response(err: &DlnaError) -> Response {
    let xml = soap::fault(err.upnp_code(), &err.to_string());
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [("Content-Type", "text/xml; charset=\"utf-8\"")],
        xml,
    )
        .into_response()
}
