//! # proto-dlna
//!
//! A DLNA MediaRenderer: AVTransport + RenderingControl + ConnectionManager over SOAP,
//! mounted on the shared SSDP/HTTP substrate. Point VLC or an Android media app at
//! "castaway", hit cast, and a `SessionEvent::Play` lands on the session manager.
//!
//! Layering (ground rule 3): [`soap`] and [`state`] are pure and unit-tested against
//! XML fixtures; [`service`] is the axum shell that turns HTTP into calls on them.
#![forbid(unsafe_code)]

pub mod descriptions;
pub mod error;
pub mod service;
pub mod soap;
pub mod state;

use std::sync::Arc;

use axum::Router;
use castaway_core::{Advertisement, ProtocolKind, SessionSink};
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
                renderer: Mutex::new(Renderer::default()),
                sink,
                friendly_name: friendly_name.into(),
                uuid: uuid.into(),
            }),
        }
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
}
