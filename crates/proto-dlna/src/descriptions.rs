//! UPnP description documents: the root device description and the three service
//! SCPDs. These are served over the shared HTTP host; `LOCATION` in SSDP points here.
//!
//! The SCPDs list the actions our [`crate::state::Renderer`] actually implements.
//! They're intentionally the pragmatic subset real control points (VLC, BubbleUPnP,
//! Android media apps) exercise, not the full AVTransport:1 surface.

/// Paths (relative to the HTTP host root) this service is mounted at.
pub mod paths {
    /// Root device description.
    pub const DESCRIPTION: &str = "/dlna/description.xml";
    /// AVTransport SCPD.
    pub const AVT_SCPD: &str = "/dlna/AVTransport.xml";
    /// RenderingControl SCPD.
    pub const RC_SCPD: &str = "/dlna/RenderingControl.xml";
    /// ConnectionManager SCPD.
    pub const CM_SCPD: &str = "/dlna/ConnectionManager.xml";
    /// AVTransport control endpoint.
    pub const AVT_CONTROL: &str = "/dlna/control/AVTransport";
    /// RenderingControl control endpoint.
    pub const RC_CONTROL: &str = "/dlna/control/RenderingControl";
    /// ConnectionManager control endpoint.
    pub const CM_CONTROL: &str = "/dlna/control/ConnectionManager";
    /// AVTransport GENA event endpoint.
    pub const AVT_EVENT: &str = "/dlna/event/AVTransport";
    /// RenderingControl GENA event endpoint.
    pub const RC_EVENT: &str = "/dlna/event/RenderingControl";
    /// ConnectionManager GENA event endpoint.
    pub const CM_EVENT: &str = "/dlna/event/ConnectionManager";
}

/// Service type URNs.
pub mod service_types {
    /// AVTransport service type.
    pub const AVTRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
    /// RenderingControl service type.
    pub const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
    /// ConnectionManager service type.
    pub const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
    /// The MediaRenderer device type.
    pub const MEDIA_RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
}

/// Render the root device description XML for a renderer named `friendly_name` with
/// device `uuid` (bare, without the `uuid:` prefix).
///
/// The skeleton and the escaping come from [`substrate_ssdp::DeviceDescription`] — the
/// builder escapes the free-text fields, which is what keeps a panel named `Bar & Grill`
/// from producing XML no control point will parse (the history is on the builder and in
/// #222). Only the DLNA-specific tail is written here.
#[must_use]
pub fn device_description(friendly_name: &str, uuid: &str) -> String {
    let services = format!(
        r#"    <dlna:X_DLNADOC xmlns:dlna="urn:schemas-dlna-org:device-1-0">DMR-1.50</dlna:X_DLNADOC>
    <serviceList>
      <service>
        <serviceType>{avt_type}</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <SCPDURL>{avt_scpd}</SCPDURL>
        <controlURL>{avt_control}</controlURL>
        <eventSubURL>{avt_event}</eventSubURL>
      </service>
      <service>
        <serviceType>{rc_type}</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <SCPDURL>{rc_scpd}</SCPDURL>
        <controlURL>{rc_control}</controlURL>
        <eventSubURL>{rc_event}</eventSubURL>
      </service>
      <service>
        <serviceType>{cm_type}</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>{cm_scpd}</SCPDURL>
        <controlURL>{cm_control}</controlURL>
        <eventSubURL>{cm_event}</eventSubURL>
      </service>
    </serviceList>"#,
        avt_type = service_types::AVTRANSPORT,
        rc_type = service_types::RENDERING_CONTROL,
        cm_type = service_types::CONNECTION_MANAGER,
        avt_scpd = paths::AVT_SCPD,
        rc_scpd = paths::RC_SCPD,
        cm_scpd = paths::CM_SCPD,
        avt_control = paths::AVT_CONTROL,
        rc_control = paths::RC_CONTROL,
        cm_control = paths::CM_CONTROL,
        avt_event = paths::AVT_EVENT,
        rc_event = paths::RC_EVENT,
        cm_event = paths::CM_EVENT,
    );
    substrate_ssdp::DeviceDescription {
        device_type: service_types::MEDIA_RENDERER.to_owned(),
        friendly_name: friendly_name.to_owned(),
        udn: format!("uuid:{uuid}"),
        manufacturer_url: Some("https://github.com/schlarpc/castaway".to_owned()),
        model_description: Some("Universal cast receiver".to_owned()),
        model_number: Some("0.1".to_owned()),
        extra_device_xml: services,
    }
    .render()
}

/// The AVTransport SCPD (action list for the subset we implement).
pub const AVTRANSPORT_SCPD: &str = include_str!("scpd/avtransport.xml");
/// The RenderingControl SCPD.
pub const RENDERING_CONTROL_SCPD: &str = include_str!("scpd/renderingcontrol.xml");
/// The ConnectionManager SCPD.
pub const CONNECTION_MANAGER_SCPD: &str = include_str!("scpd/connectionmanager.xml");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn device_description_embeds_name_and_udn() {
        let xml = device_description("Hackerspace TV", "abc-123");
        assert!(xml.contains("<friendlyName>Hackerspace TV</friendlyName>"));
        assert!(xml.contains("<UDN>uuid:abc-123</UDN>"));
        assert!(xml.contains(service_types::AVTRANSPORT));
    }

    #[test]
    fn scpds_are_nonempty_xml() {
        for scpd in [
            AVTRANSPORT_SCPD,
            RENDERING_CONTROL_SCPD,
            CONNECTION_MANAGER_SCPD,
        ] {
            assert!(scpd.contains("<scpd"));
            assert!(scpd.contains("<actionList>"));
        }
    }
}
