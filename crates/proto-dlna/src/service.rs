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
    /// Shared with [`crate::control::DlnaRemote`], so a press on the panel and a poll
    /// from the control point are looking at the same transport state.
    pub(crate) renderer: Arc<Mutex<Renderer>>,
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
            for event in out.events {
                // A fresh `Play` is the moment this source takes the screen, and so the
                // moment the panel may drive it. Published right behind the play itself
                // because the session manager drops a control surface from a source that
                // does not hold the screen — the same ordering the metadata needs.
                let started = matches!(event, castaway_core::SessionEvent::Play { .. });
                if let Err(e) = st.sink.emit(event).await {
                    warn!(error = %e, "failed to emit DLNA session event");
                    continue;
                }
                if started {
                    let remote =
                        crate::control::DlnaRemote::new(Arc::clone(&st.renderer), st.sink.clone());
                    if let Err(e) = st
                        .sink
                        .emit(castaway_core::SessionEvent::ControlSurface(Arc::new(
                            remote,
                        )))
                        .await
                    {
                        warn!(error = %e, "failed to publish the DLNA control surface");
                    }
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

async fn subscribe_ack(_headers: HeaderMap) -> Response {
    // 501, not 200 — and the difference is not pedantry, it is the whole behaviour.
    //
    // This used to answer 200 with an invented `SID` "so control points consider the
    // subscription established, even though we don't push events". That is exactly
    // backwards. A control point that believes it is subscribed *stops polling*:
    // `async_upnp_client` — which Home Assistant's dlna_dmr runs on — guards its entire
    // polling fallback on `is_subscribed`, and documents the alternative itself
    // ("Device rejected subscription request. State variables will need to be polled").
    // So accepting the subscription and then going silent froze transport state, volume
    // and mute at whatever they were when the control point connected, forever, while the
    // device went on looking perfectly healthy.
    //
    // Refusing puts every such control point back on its polling path, which works today.
    // Implementing GENA properly — a subscriber table, per-subscription UUID SIDs, the
    // mandatory initial NOTIFY, SEQ from 0, renewals, and `LastChange` for AVTransport and
    // RenderingControl — is GAPS G68 and is the real answer.
    (
        StatusCode::NOT_IMPLEMENTED,
        "castaway does not implement GENA eventing; poll instead",
    )
        .into_response()
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
