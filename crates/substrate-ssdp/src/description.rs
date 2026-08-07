//! The UPnP device-description document both SSDP consumers serve.
//!
//! DLNA and DIAL each answer their SSDP `LOCATION` with the same UPnP
//! `<root>`/`<specVersion>`/`<device>` skeleton; only the device type, the trimmings and
//! the service list differ. This is serialization the protocols inherit from UPnP, not
//! protocol semantics, so it lives here with the rest of the discovery substrate.
//!
//! [`xml_escape`] is here for the same reason, and its history is the argument: both
//! crates carried a private copy, both interpolated the operator-configured friendly name
//! raw, and both fixed it independently, three days apart (df700c8, 8410ea9). A panel
//! named `Bar & Grill` produced XML that is not well-formed, so every control point's
//! parser rejected the description — the device answered M-SEARCH, served its LOCATION
//! with a 200, and appeared in no picker anywhere, logging nothing (#222).

/// Escape the five XML-special characters for element text.
#[must_use]
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// A UPnP root-device description, rendered by [`DeviceDescription::render`].
///
/// The free-text fields are escaped at render time; a caller never pre-escapes.
/// Element order inside `<device>` follows the UPnP 1.0 device template, which some
/// control-point parsers hold us to.
#[derive(Debug, Clone)]
pub struct DeviceDescription {
    /// The device type URN (e.g. `urn:schemas-upnp-org:device:MediaRenderer:1`).
    pub device_type: String,
    /// The operator-configured receiver name a sender shows in its picker.
    pub friendly_name: String,
    /// The device UDN, *including* the `uuid:` prefix. `<UDN>` is not optional and its
    /// absence is not forgiving: Chromium's DIAL parser treats an empty unique-id as a
    /// parse failure and drops the device outright, and Android senders use it to tie
    /// the SSDP `USN` back to the description they fetched.
    pub udn: String,
    /// `<manufacturerURL>`, when the protocol's description carries one.
    pub manufacturer_url: Option<String>,
    /// `<modelDescription>`, when carried.
    pub model_description: Option<String>,
    /// `<modelNumber>`, when carried.
    pub model_number: Option<String>,
    /// Raw, already-well-formed XML appended inside `<device>` after `<UDN>` — the
    /// protocol-specific tail (DLNA's `X_DLNADOC` and `<serviceList>`). The caller
    /// escapes any free text within, because from here it is markup.
    pub extra_device_xml: String,
}

impl DeviceDescription {
    /// Render the description document.
    #[must_use]
    pub fn render(&self) -> String {
        let mut device = String::new();
        let mut line = |text: String| {
            device.push_str("    ");
            device.push_str(&text);
            device.push('\n');
        };
        line(format!(
            "<deviceType>{}</deviceType>",
            xml_escape(&self.device_type)
        ));
        line(format!(
            "<friendlyName>{}</friendlyName>",
            xml_escape(&self.friendly_name)
        ));
        line("<manufacturer>castaway</manufacturer>".to_owned());
        if let Some(url) = &self.manufacturer_url {
            line(format!(
                "<manufacturerURL>{}</manufacturerURL>",
                xml_escape(url)
            ));
        }
        if let Some(desc) = &self.model_description {
            line(format!(
                "<modelDescription>{}</modelDescription>",
                xml_escape(desc)
            ));
        }
        line("<modelName>castaway</modelName>".to_owned());
        if let Some(number) = &self.model_number {
            line(format!("<modelNumber>{}</modelNumber>", xml_escape(number)));
        }
        line(format!("<UDN>{}</UDN>", xml_escape(&self.udn)));
        if !self.extra_device_xml.is_empty() {
            device.push_str(&self.extra_device_xml);
            if !self.extra_device_xml.ends_with('\n') {
                device.push('\n');
            }
        }
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <root xmlns=\"urn:schemas-upnp-org:device-1-0\">\n\
             \x20\x20<specVersion><major>1</major><minor>0</minor></specVersion>\n\
             \x20\x20<device>\n\
             {device}\
             \x20\x20</device>\n\
             </root>"
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn escapes_all_five_specials() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(xml_escape("plain"), "plain");
    }

    fn minimal() -> DeviceDescription {
        DeviceDescription {
            device_type: "urn:schemas-upnp-org:device:tvdevice:1".into(),
            friendly_name: "Hackerspace TV".into(),
            udn: "uuid:abc-123".into(),
            manufacturer_url: None,
            model_description: None,
            model_number: None,
            extra_device_xml: String::new(),
        }
    }

    #[test]
    fn renders_the_minimal_skeleton() {
        let xml = minimal().render();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("<friendlyName>Hackerspace TV</friendlyName>"));
        assert!(xml.contains("<UDN>uuid:abc-123</UDN>"));
        assert!(xml.contains("<manufacturer>castaway</manufacturer>"));
        // Nothing optional leaks in as an empty element.
        assert!(!xml.contains("manufacturerURL"));
        assert!(!xml.contains("modelDescription"));
        assert!(!xml.contains("modelNumber"));
        assert!(xml.ends_with("</root>"));
    }

    #[test]
    fn optional_elements_appear_in_upnp_template_order() {
        let mut desc = minimal();
        desc.manufacturer_url = Some("https://example.com".into());
        desc.model_description = Some("Universal cast receiver".into());
        desc.model_number = Some("0.1".into());
        desc.extra_device_xml = "    <serviceList></serviceList>".into();
        let xml = desc.render();
        let order = [
            "<deviceType>",
            "<friendlyName>",
            "<manufacturer>",
            "<manufacturerURL>",
            "<modelDescription>",
            "<modelName>",
            "<modelNumber>",
            "<UDN>",
            "<serviceList>",
        ];
        let positions: Vec<_> = order.iter().map(|tag| xml.find(tag).unwrap()).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "order: {xml}");
    }

    #[test]
    fn a_hostile_friendly_name_stays_well_formed() {
        // The regression both crates shipped separately: `Bar & Grill` must never reach
        // the document as a bare ampersand.
        let mut desc = minimal();
        desc.friendly_name = "Bar & Grill".into();
        let xml = desc.render();
        assert!(xml.contains("<friendlyName>Bar &amp; Grill</friendlyName>"));
        assert!(!xml.contains("Bar & Grill"));
    }
}
