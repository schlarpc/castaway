//! # proto-dlna
//!
//! A DLNA MediaRenderer: AVTransport + RenderingControl + ConnectionManager over SOAP,
//! mounted on the shared SSDP/HTTP substrate. Point VLC or an Android media app at
//! "castaway", hit cast, and a `SessionEvent::Play` lands on the session manager.
//!
//! Layering (ground rule 3): [`soap`] and [`state`] are pure and unit-tested against
//! XML fixtures; [`service`] is the axum shell that turns HTTP into calls on them.
#![forbid(unsafe_code)]

pub mod control;
pub mod descriptions;
pub mod didl;
pub mod error;
pub mod gena;
pub mod notify;
mod probe;
pub mod service;
pub mod soap;
pub mod state;
mod xmlref;

use std::sync::{Arc, OnceLock};

use axum::Router;
use castaway_core::{Advertisement, OsdSink, PlaybackReport, ProtocolKind, SessionSink};
use substrate_ssdp::SsdpDevice;
use tokio::sync::Mutex;

pub use error::DlnaError;
pub use state::{Renderer, TransportState};

use crate::descriptions::{paths, service_types};
use crate::service::DlnaState;

/// A DLNA MediaRenderer instance. Provides an axum [`Router`] to merge onto the shared
/// HTTP host and an [`SsdpDevice`] to register with the shared SSDP responder.
pub struct DlnaService {
    state: Arc<DlnaState>,
}

impl DlnaService {
    /// Create a renderer named `friendly_name` with a stable `uuid` (bare, no `uuid:`
    /// prefix), emitting events to `sink`.
    #[must_use]
    pub fn new(
        friendly_name: impl Into<String>,
        uuid: impl Into<String>,
        sink: SessionSink,
    ) -> Self {
        Self {
            state: Arc::new(DlnaState {
                renderer: Arc::new(Mutex::new(Renderer::default())),
                sink,
                friendly_name: friendly_name.into(),
                uuid: uuid.into(),
                osd: OnceLock::new(),
                playback: OnceLock::new(),
                subscribers: std::sync::Mutex::new(crate::gena::Subscribers::new()),
                published: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// The shared state behind this service, for tests that drive it directly rather than
    /// through HTTP.
    #[cfg(test)]
    pub(crate) fn state(&self) -> Arc<DlnaState> {
        Arc::clone(&self.state)
    }

    /// Give this renderer an [`OsdSink`] so volume/mute changes show on the overlay.
    #[must_use]
    pub fn with_osd(self, osd: OsdSink) -> Self {
        let _ = self.state.osd.set(osd);
        self
    }

    /// Let this renderer ask the pipeline where playback has reached.
    ///
    /// For DLNA the receiver *is* the player, so the position a control point draws its
    /// scrubber from can only come from here. Without it `GetPositionInfo` answers with
    /// the spec's `NOT_IMPLEMENTED` sentinel, which is honest and is what a build with no
    /// decoder in it should say — but it means no phone ever shows progress.
    #[must_use]
    pub fn with_playback(self, report: Arc<dyn PlaybackReport>) -> Self {
        let _ = self.state.playback.set(report);
        self
    }

    /// The axum router serving this renderer's description, SCPDs, and control
    /// endpoints. Merge it onto the shared HTTP host.
    pub fn router(&self) -> Router {
        service::router(self.state.clone())
    }

    /// The SSDP device to register with the shared responder.
    #[must_use]
    pub fn ssdp_device(&self) -> SsdpDevice {
        SsdpDevice {
            uuid: format!("uuid:{}", self.state.uuid),
            device_type: service_types::MEDIA_RENDERER.to_string(),
            services: vec![
                service_types::AVTRANSPORT.to_string(),
                service_types::RENDERING_CONTROL.to_string(),
                service_types::CONNECTION_MANAGER.to_string(),
            ],
        }
    }

    /// The HTTP path the device description is served at (for the SSDP `LOCATION`).
    #[must_use]
    pub fn description_path(&self) -> &'static str {
        paths::DESCRIPTION
    }

    /// The core-level advertisement hint for this device (root device search target).
    #[must_use]
    pub fn advertisement(&self) -> Advertisement {
        Advertisement::SsdpDevice {
            st: service_types::MEDIA_RENDERER.to_string(),
            description_path: paths::DESCRIPTION.to_string(),
        }
    }

    /// The protocol this service implements.
    #[must_use]
    pub fn kind(&self) -> ProtocolKind {
        ProtocolKind::Dlna
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // Tests bind ephemeral loopback sockets; the registry governs production binds.
    #![allow(clippy::disallowed_methods)]
    use std::time::Duration;

    use super::*;
    use castaway_core::SourceId;
    use tokio::sync::mpsc;

    fn service() -> (DlnaService, mpsc::Receiver<castaway_core::SourceMessage>) {
        let (tx, rx) = mpsc::channel(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "test"), tx);
        (DlnaService::new("Test TV", "abcd-1234", sink), rx)
    }

    #[test]
    fn ssdp_device_lists_three_services() {
        let (svc, _rx) = service();
        let dev = svc.ssdp_device();
        assert_eq!(dev.uuid, "uuid:abcd-1234");
        assert_eq!(dev.services.len(), 3);
        // 3 services + root + uuid + device-type = 6 advertised targets.
        assert_eq!(dev.targets().len(), 6);
    }

    #[tokio::test]
    async fn end_to_end_seturi_play_emits_event() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (svc, mut rx) = service();
        let app = svc.router();

        let set_uri = r#"<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
        <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
        <InstanceID>0</InstanceID><CurrentURI>http://10.0.0.9/v.mp4</CurrentURI>
        <CurrentURIMetaData></CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"#;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(paths::AVT_CONTROL)
                    .header(
                        "SOAPACTION",
                        "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"",
                    )
                    .body(Body::from(set_uri))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let play = r#"<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
        <u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
        <InstanceID>0</InstanceID><Speed>1</Speed></u:Play></s:Body></s:Envelope>"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(paths::AVT_CONTROL)
                    .body(Body::from(play))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = rx.recv().await.unwrap();
        assert!(matches!(
            msg.event,
            castaway_core::SessionEvent::Play { .. }
        ));
    }

    /// A one-shot HTTP server that answers every request with `status` and `content_type`,
    /// and its URL. Enough to be the far end of a `HEAD`.
    async fn stub_server(status: &'static str, content_type: Option<&'static str>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let ct = content_type
                        .map(|ct| format!("Content-Type: {ct}\r\n"))
                        .unwrap_or_default();
                    let _ = sock
                        .write_all(
                            format!("HTTP/1.1 {status}\r\n{ct}Content-Length: 0\r\n\r\n")
                                .as_bytes(),
                        )
                        .await;
                });
            }
        });
        format!("http://127.0.0.1:{port}/clip.mp4")
    }

    async fn set_uri(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, String) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let body = format!(
            r#"<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
            <InstanceID>0</InstanceID><CurrentURI>{uri}</CurrentURI>
            <CurrentURIMetaData></CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"#
        );
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(paths::AVT_CONTROL)
                    .header(
                        "SOAPACTION",
                        "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"",
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The whole point of #99: the fault arrives while the control point is still
    /// listening, instead of `ERROR_OCCURRED` arriving seconds later at a phone that has
    /// already been shown a healthy session.
    ///
    /// Three shapes, and 714 is the one that had never once been produced outside its own
    /// unit test — the code existed, was mapped, and was unreachable, because nothing ever
    /// looked at what the resource was.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_resource_the_server_disowns_is_refused_at_set_time() {
        let (svc, _rx) = service();
        let app = svc.router();

        // An HTML error page: the thing a control point most often actually hands us when
        // the media has moved. 714.
        let url = stub_server("200 OK", Some("text/html; charset=utf-8")).await;
        let (status, body) = set_uri(&app, &url).await;
        assert_eq!(status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body.contains("<errorCode>714</errorCode>"), "{body}");

        // Gone. 716, synchronously, where before it was 200 then silence.
        let url = stub_server("404 Not Found", Some("text/html")).await;
        let (_, body) = set_uri(&app, &url).await;
        assert!(body.contains("<errorCode>716</errorCode>"), "{body}");

        // And the case that must keep working, which is every other cast: real media is
        // taken, and the transport moves on to `STOPPED` ready for `Play`.
        let url = stub_server("200 OK", Some("video/mp4")).await;
        let (status, _) = set_uri(&app, &url).await;
        assert_eq!(status, axum::http::StatusCode::OK);
    }

    /// The probe must not answer a question that was not the one that was wrong. A
    /// `SetAVTransportURI` naming an instance this renderer has not got is 718 whatever the
    /// resource turns out to be — and the probe opening a socket to find out otherwise
    /// would be both the wrong answer and a needless request.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_instance_outranks_whatever_the_resource_is() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (svc, _rx) = service();
        let app = svc.router();
        let url = stub_server("200 OK", Some("text/html")).await;

        let body = format!(
            r#"<?xml version="1.0"?>
            <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
            <InstanceID>1</InstanceID><CurrentURI>{url}</CurrentURI>
            <CurrentURIMetaData></CurrentURIMetaData></u:SetAVTransportURI></s:Body></s:Envelope>"#
        );
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(paths::AVT_CONTROL)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("<errorCode>718</errorCode>"), "{body}");
    }

    /// Leniency is not a nicety here — each of these is a real server's real behaviour,
    /// and refusing on any of them turns a cast that would have played into one the phone
    /// says was rejected, with no way for a guest in the room to override it.
    #[tokio::test(flavor = "multi_thread")]
    async fn servers_that_answer_ambiguously_are_still_played() {
        let (svc, _rx) = service();
        let app = svc.router();

        for (status, content_type) in [
            // Will not do HEAD at all. Common, and says nothing about the resource.
            ("405 Method Not Allowed", None),
            ("501 Not Implemented", None),
            // A signed URL whose signature covers the method, and auth we are not
            // carrying. Both routinely coexist with a GET that works.
            ("403 Forbidden", Some("text/html")),
            ("401 Unauthorized", Some("text/html")),
            // A rate limit that will have passed by the time the decoder asks.
            ("429 Too Many Requests", Some("text/html")),
            // Having a bad minute. The decoder's own fetch retries; a probe does not.
            ("503 Service Unavailable", Some("text/html")),
            // Names no type, or the universal "here are some bytes" that plenty of real
            // servers send for an mp4.
            ("200 OK", None),
            ("200 OK", Some("application/octet-stream")),
        ] {
            let url = stub_server(status, content_type).await;
            let (code, body) = set_uri(&app, &url).await;
            assert_eq!(
                code,
                axum::http::StatusCode::OK,
                "{status} {content_type:?} was refused: {body}"
            );
        }
    }

    /// A control point that subscribes gets told things — which is the entire claim, and
    /// the one this service used to make falsely.
    ///
    /// Driven over a real socket rather than against the handler, because the half that was
    /// missing is the half that leaves the process: a `SUBSCRIBE` that returns a `SID` and
    /// is never followed by a `NOTIFY` looks identical, from inside, to one that works.
    ///
    /// The subscriber here is what a control point is — a listener on a port it names in
    /// its `CALLBACK` header — and it asserts on the two things that make an event usable:
    /// `SEQ 0` carrying the *complete* state, and a later `SEQ` carrying the change.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_subscriber_is_sent_the_initial_state_and_then_every_change() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tower::ServiceExt;

        // A control point's callback listener: collect each NOTIFY, answer 200.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (events_tx, mut events_rx) = mpsc::channel::<String>(8);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let events_tx = events_tx.clone();
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 4096];
                    // One read: the whole NOTIFY goes out in a single write.
                    if let Ok(n) = sock.read(&mut buf).await {
                        seen.extend_from_slice(&buf[..n]);
                    }
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    let _ = events_tx
                        .send(String::from_utf8_lossy(&seen).into_owned())
                        .await;
                });
            }
        });

        let (svc, _rx) = service();
        let app = svc.router();

        let subscribed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri(paths::AVT_EVENT)
                    .header("CALLBACK", format!("<http://127.0.0.1:{port}/cb>"))
                    .header("NT", "upnp:event")
                    .header("TIMEOUT", "Second-1800")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(subscribed.status(), StatusCode::OK);
        let sid = subscribed
            .headers()
            .get("SID")
            .and_then(|v| v.to_str().ok())
            .expect("a subscription with no SID is one the control point cannot renew")
            .to_string();
        assert!(sid.starts_with("uuid:"));
        // The lease is ours to set, and a control point renews against it.
        assert_eq!(subscribed.headers().get("TIMEOUT").unwrap(), "Second-1800");

        // The initial event: mandatory (UDA 1.1 §4.3), SEQ 0, and carrying the complete
        // state rather than a change — everything after it is a delta against this.
        let initial = tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
            .await
            .expect("no initial NOTIFY arrived")
            .unwrap();
        assert!(initial.starts_with("NOTIFY /cb HTTP/1.1\r\n"));
        assert!(initial.contains(&format!("SID: {sid}\r\n")));
        assert!(initial.contains("SEQ: 0\r\n"));
        assert!(initial.contains("NTS: upnp:propchange\r\n"));
        assert!(initial.contains("&lt;TransportState val=&quot;NO_MEDIA_PRESENT&quot;/&gt;"));

        // Now change something the subscriber cares about.
        let set_uri = r#"<?xml version="1.0"?>
        <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
        <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
        <InstanceID>0</InstanceID><CurrentURI>http://10.0.0.9/v.mp4</CurrentURI>
        </u:SetAVTransportURI></s:Body></s:Envelope>"#;
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(paths::AVT_CONTROL)
                    .body(Body::from(set_uri))
                    .unwrap(),
            )
            .await
            .unwrap();

        let changed = tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
            .await
            .expect("a state change reached no subscriber")
            .unwrap();
        assert!(changed.contains("SEQ: 1\r\n"), "sequence must advance");
        assert!(changed.contains("&lt;TransportState val=&quot;STOPPED&quot;/&gt;"));
        assert!(changed.contains("v.mp4"));

        // …and unsubscribing stops it. A control point that has gone away and a control
        // point that said so are different, and only the second should be instant.
        let gone = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("UNSUBSCRIBE")
                    .uri(paths::AVT_EVENT)
                    .header("SID", &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::OK);

        // A stale SID is 412, not 404: that is how a control point learns to start over.
        let stale = app
            .oneshot(
                Request::builder()
                    .method("UNSUBSCRIBE")
                    .uri(paths::AVT_EVENT)
                    .header("SID", &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    }

    /// Renewal is how a subscription outlives its lease, and a control point that cannot
    /// renew has to tear down and re-subscribe — which costs it the whole state again.
    #[tokio::test]
    async fn a_subscription_can_be_renewed_and_an_unknown_one_cannot() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let (svc, _rx) = service();
        let app = svc.router();

        // No callback listener here on purpose: the initial NOTIFY will fail to deliver,
        // and a subscription must survive that — one unreachable delivery is not the same
        // as a subscriber that has gone.
        let subscribed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri(paths::RC_EVENT)
                    .header("CALLBACK", "<http://127.0.0.1:1/cb>")
                    .header("NT", "upnp:event")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(subscribed.status(), StatusCode::OK);
        let sid = subscribed
            .headers()
            .get("SID")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let renewed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri(paths::RC_EVENT)
                    .header("SID", &sid)
                    .header("TIMEOUT", "Second-300")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(renewed.status(), StatusCode::OK);
        assert_eq!(renewed.headers().get("SID").unwrap(), sid.as_str());

        let unknown = app
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri(paths::RC_EVENT)
                    .header("SID", "uuid:not-ours")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::PRECONDITION_FAILED);
    }
}
