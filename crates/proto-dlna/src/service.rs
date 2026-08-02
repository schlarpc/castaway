//! The axum wiring: HTTP → parse SOAP → drive the [`Renderer`] → emit `SessionEvent` +
//! return a SOAP response. This is the thin I/O shell; all decisions live in
//! [`crate::state`]. The router is meant to be merged onto the shared HTTP host.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use castaway_core::{OsdSink, PlaybackReport, SessionSink};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::descriptions::{self, paths, service_types};
use crate::error::DlnaError;
use crate::gena::{self, EventedService, SubscribeRequest, Subscribers};
use crate::soap::{self, SoapAction};
use crate::state::Renderer;

/// The `(name, value)` pairs one service's subscribers are sent.
///
/// Named because it appears in three places and reads as noise in all of them: what a
/// service currently reports, what it last reported, and the difference that decides
/// whether anything is sent.
type Properties = Vec<(&'static str, String)>;

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
    /// Where playback has got to, asked of the pipeline on each control request.
    ///
    /// Absent in a build with no decoder in it, which is the honest configuration for the
    /// null pipeline: it never fetches anything, so it has no position to report and
    /// `GetPositionInfo` correctly answers with the spec's sentinel.
    pub(crate) playback: OnceLock<Arc<dyn PlaybackReport>>,
    /// Who is subscribed to what, and how far through their sequence they are.
    ///
    /// A `std` mutex rather than a `tokio` one because every critical section is a handful
    /// of vector operations with no `await` inside — the delivery that *does* await happens
    /// after the lock has been given back, which is also what keeps a slow subscriber from
    /// blocking a control request.
    pub(crate) subscribers: std::sync::Mutex<Subscribers>,
    /// The last thing published per service, so a state change that changed nothing is not
    /// sent.
    ///
    /// Publishing on a *diff* rather than from each mutation is what makes this correct
    /// without threading an event through every setter: there is one place that knows the
    /// state, and every path that touches it ends up back here.
    pub(crate) published: std::sync::Mutex<Vec<(EventedService, Properties)>>,
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
        // GENA eventing. `SUBSCRIBE` and `UNSUBSCRIBE` are not HTTP methods axum has
        // routing for, so each endpoint takes `any` and dispatches on the method itself.
        .route(paths::AVT_EVENT, any(event_avt))
        .route(paths::RC_EVENT, any(event_rc))
        .route(paths::CM_EVENT, any(event_cm))
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
    st: &Arc<DlnaState>,
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
        // Where the pipeline says we are, handed to the state machine as an *input* so
        // that module stays a pure function of what it is given (ground rule 3). Asked
        // per request rather than cached: `GetPositionInfo` is polled about once a second
        // for the whole item and a cached answer would be a scrubber a second behind.
        r.observe_progress(st.playback.get().and_then(|p| p.progress()));
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
                    let remote = crate::control::DlnaRemote::new(Arc::clone(st));
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
            // An action that changed something is the only source of AVTransport and
            // RenderingControl events, so this is where they are noticed. Both services,
            // not just the one addressed: `Stop` moves the transport and a preset could
            // move the volume, and a diff that finds nothing costs a comparison.
            publish_if_changed(st, EventedService::AvTransport).await;
            publish_if_changed(st, EventedService::RenderingControl).await;
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

async fn event_avt(
    State(st): State<Arc<DlnaState>>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    handle_event(&st, EventedService::AvTransport, &method, &headers).await
}

async fn event_rc(
    State(st): State<Arc<DlnaState>>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    handle_event(&st, EventedService::RenderingControl, &method, &headers).await
}

async fn event_cm(
    State(st): State<Arc<DlnaState>>,
    method: axum::http::Method,
    headers: HeaderMap,
) -> Response {
    handle_event(&st, EventedService::ConnectionManager, &method, &headers).await
}

/// `SUBSCRIBE` / `UNSUBSCRIBE` on one service's event endpoint.
///
/// This used to answer 200 with an invented `SID` and never send anything, which is worse
/// than refusing and not by a little: a control point that believes it is subscribed
/// *stops polling*. `async_upnp_client` — Home Assistant's — guards its whole polling
/// fallback on `is_subscribed`, so accepting and going silent froze transport state, volume
/// and mute at connect values forever on a device that went on looking healthy. It then
/// answered 501, which put that control point back on a path that works. This is the path
/// that was supposed to exist.
async fn handle_event(
    st: &Arc<DlnaState>,
    service: EventedService,
    method: &axum::http::Method,
    headers: &HeaderMap,
) -> Response {
    // `HeaderMap::get` is already case-insensitive, which matters here for the same reason
    // it does for `transferMode.dlna.org`: control points disagree about the casing of
    // every header they send, and the spec's own examples are inconsistent.
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    match method.as_str() {
        "SUBSCRIBE" => {
            subscribe(
                st,
                service,
                header("callback"),
                header("nt"),
                header("sid"),
                header("timeout"),
            )
            .await
        }
        "UNSUBSCRIBE" => {
            let Some(sid) = header("sid") else {
                return refuse(gena::SubscribeError::BadRequest("UNSUBSCRIBE needs a SID"));
            };
            let known = st
                .subscribers
                .lock()
                .map(|mut subs| subs.remove(sid.trim()))
                .unwrap_or(false);
            if known {
                info!(%sid, ?service, "dlna: unsubscribed");
                StatusCode::OK.into_response()
            } else {
                // 412, not 404: the request was perfectly well formed and referred to a
                // subscription that is not ours — which is how a control point holding a
                // stale SID learns to start over rather than to stop trying.
                refuse(gena::SubscribeError::PreconditionFailed(
                    "no such subscription",
                ))
            }
        }
        // The event endpoint is not a page. Saying so plainly beats a 404, which reads as
        // "this device has no eventing" — the thing we spent this module not being.
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            [("ALLOW", "SUBSCRIBE, UNSUBSCRIBE")],
            "this is a GENA event endpoint",
        )
            .into_response(),
    }
}

async fn subscribe(
    st: &Arc<DlnaState>,
    service: EventedService,
    callback: Option<&str>,
    nt: Option<&str>,
    sid: Option<&str>,
    timeout: Option<&str>,
) -> Response {
    let request = match gena::parse_subscribe(callback, nt, sid, timeout) {
        Ok(r) => r,
        Err(e) => return refuse(e),
    };
    // The duration is ours to choose — a subscriber's TIMEOUT is a request (§4.1.2) — and
    // one figure keeps renewals predictable.
    let granted = Duration::from_secs(gena::SUBSCRIPTION_SECS);
    let now = Instant::now();

    match request {
        SubscribeRequest::Renew { sid, .. } => {
            let ok = st
                .subscribers
                .lock()
                .map(|mut subs| {
                    subs.expire(now);
                    subs.renew(&sid, granted, now).is_some()
                })
                .unwrap_or(false);
            if !ok {
                return refuse(gena::SubscribeError::PreconditionFailed(
                    "no such subscription",
                ));
            }
            debug!(%sid, ?service, "dlna: subscription renewed");
            accepted(&sid, granted)
        }
        SubscribeRequest::New { callbacks, .. } => {
            let sid = format!("uuid:{}", uuid::Uuid::new_v4());
            let sub = {
                let Ok(mut subs) = st.subscribers.lock() else {
                    return refuse(gena::SubscribeError::PreconditionFailed(
                        "the subscriber table is unavailable",
                    ));
                };
                subs.expire(now);
                subs.add(sid.clone(), service, callbacks, granted, now)
            };
            info!(%sid, ?service, callbacks = sub.callbacks.len(), "dlna: subscribed");

            // The initial event, and it is not optional: UDA 1.1 §4.3 requires it "even if
            // the control point unsubscribes before the message is delivered", because it
            // is what carries the *complete* state — everything after it is a change
            // against a baseline the subscriber would otherwise not have.
            //
            // Spawned rather than awaited: the response has to reach the subscriber before
            // the NOTIFY does, or a control point that has not finished reading its SID
            // gets an event for a subscription it does not yet know it has.
            let st = Arc::clone(st);
            tokio::spawn(async move {
                publish_to(&st, service, &sub.sid).await;
            });
            accepted(&sid, granted)
        }
    }
}

/// The 200 a subscriber reads its `SID` and lease out of.
fn accepted(sid: &str, granted: Duration) -> Response {
    (
        StatusCode::OK,
        [
            ("SID", sid.to_string()),
            ("TIMEOUT", gena::timeout_header(granted)),
            ("CONTENT-LENGTH", "0".to_string()),
        ],
        (),
    )
        .into_response()
}

fn refuse(e: gena::SubscribeError) -> Response {
    debug!(
        status = e.status(),
        reason = e.reason(),
        "dlna: subscription refused"
    );
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_REQUEST);
    (status, e.reason()).into_response()
}

/// Send the current state of `service` to one subscriber, by `SID`.
async fn publish_to(st: &Arc<DlnaState>, service: EventedService, sid: &str) {
    let properties = {
        let renderer = st.renderer.lock().await;
        service.properties(&renderer)
    };
    let body = gena::propertyset(&properties);
    let Some((seq, callbacks)) = ({
        let Ok(mut subs) = st.subscribers.lock() else {
            return;
        };
        // Not `prepare(service).find(sid)`: that takes a sequence number from every
        // subscriber to the service and throws all but one away, leaving everybody else
        // permanently one ahead of what they have been sent.
        subs.prepare_one(sid, Instant::now())
    }) else {
        return;
    };
    send(st, service, sid, seq, &body, &callbacks).await;
}

/// Publish `service`'s state to every subscriber, if it has changed since last time.
///
/// The diff is what makes this cheap enough to call from every request handler: a control
/// point polling `GetTransportInfo` twice a second must not produce two events, and it is
/// far easier to be sure of that here than to remember an event at each of the dozen
/// places that mutate the renderer.
pub(crate) async fn publish_if_changed(st: &Arc<DlnaState>, service: EventedService) {
    let properties = {
        let renderer = st.renderer.lock().await;
        service.properties(&renderer)
    };
    {
        let Ok(mut published) = st.published.lock() else {
            return;
        };
        match published.iter_mut().find(|(s, _)| *s == service) {
            Some((_, last)) if *last == properties => return,
            Some((_, last)) => *last = properties.clone(),
            None => published.push((service, properties.clone())),
        }
    }

    let now = Instant::now();
    let batch = {
        let Ok(mut subs) = st.subscribers.lock() else {
            return;
        };
        subs.expire(now);
        subs.prepare(service, now)
    };
    if batch.is_empty() {
        return;
    }
    let body = gena::propertyset(&properties);
    for (sid, seq, callbacks) in batch {
        send(st, service, &sid, seq, &body, &callbacks).await;
    }
}

/// Deliver one event, trying each callback in turn and keeping the books.
async fn send(
    st: &Arc<DlnaState>,
    service: EventedService,
    sid: &str,
    seq: u32,
    body: &str,
    callbacks: &[String],
) {
    // §4.1.2: try the subscriber's callbacks in the order it gave them and stop at the
    // first that answers. A subscriber behind more than one interface uses the list to
    // tell us which of its addresses we can actually reach.
    let mut last_error = None;
    for url in callbacks {
        match crate::notify::deliver(url, service, sid, seq, body).await {
            Ok(()) => {
                if let Ok(mut subs) = st.subscribers.lock() {
                    subs.delivered(sid);
                }
                return;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let dropped = st
        .subscribers
        .lock()
        .map(|mut subs| subs.delivery_failed(sid))
        .unwrap_or(false);
    if dropped {
        warn!(%sid, ?service, error = ?last_error,
            "dlna: dropping a subscription that has stopped answering");
    } else {
        debug!(%sid, ?service, error = ?last_error, "dlna: event delivery failed");
    }
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
