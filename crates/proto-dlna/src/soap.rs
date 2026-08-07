//! Pure SOAP layer: parse a UPnP control request body into an action name + flat
//! argument map, and render action responses / faults. UPnP SOAP bodies are shallow
//! (an action element whose direct children are string arguments), so we don't need a
//! general SOAP stack — just the one shape.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::DlnaError;

/// A parsed SOAP control request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoapAction {
    /// The action name (local name of the element inside `s:Body`, prefix stripped).
    pub name: String,
    /// The action's direct child arguments as `(name, text)` pairs, in document order.
    pub args: Vec<(String, String)>,
}

impl SoapAction {
    /// Look up an argument by name.
    #[must_use]
    pub fn arg(&self, name: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Look up a required argument.
    ///
    /// # Errors
    /// [`DlnaError::MissingArgument`] if absent.
    pub fn require(&self, name: &'static str) -> Result<&str, DlnaError> {
        self.arg(name).ok_or(DlnaError::MissingArgument(name))
    }

    /// Refuse an action addressed to a virtual instance this renderer does not have.
    ///
    /// Every AVTransport and RenderingControl action carries an `InstanceID`, this
    /// renderer has exactly one — 0 — and the argument was read by nothing at all. A
    /// control point addressing instance 1 therefore drove instance 0 in silence: it
    /// believed it had created a second transport, and what actually happened was that the
    /// first one started playing.
    ///
    /// A missing or unparseable `InstanceID` is treated as 0 rather than refused. Every
    /// real control point sends it and sends `0`, and the ones that leave it out of a
    /// zero-argument getter are not asking about a second instance.
    ///
    /// # Errors
    /// [`DlnaError::InvalidInstanceId`] for anything other than instance 0.
    pub fn require_instance_zero(&self) -> Result<(), DlnaError> {
        match self.arg("InstanceID") {
            None => Ok(()),
            Some(id) => match id.trim() {
                "" | "0" => Ok(()),
                other => Err(DlnaError::InvalidInstanceId(other.to_string())),
            },
        }
    }

    /// Parse a SOAP control request body.
    ///
    /// # Errors
    /// [`DlnaError::MalformedSoap`] on unparseable XML or a missing action element.
    pub fn parse(body: &str) -> Result<Self, DlnaError> {
        let mut reader = Reader::from_str(body);
        // A bare `&` is malformed XML, and quick-xml 0.38 began failing the *read* on one
        // rather than only the unescape. Failing the read discards every remaining
        // argument, so one sloppy title would take the URI down with it; tolerate it and
        // let the character stand for itself, which is what this parser did before.
        reader.config_mut().allow_dangling_amp = true;

        // Walk to the first element inside <...:Body>. Track whether we've entered Body.
        let mut in_body = false;
        let mut action: Option<String> = None;
        let mut args: Vec<(String, String)> = Vec::new();
        let mut current_arg: Option<String> = None;
        // An argument's text arrives in fragments (see `xmlref`), so it is accumulated
        // here and recorded when the element closes rather than at the first `Text`.
        let mut arg_text = String::new();
        let mut depth_in_action = 0i32;

        loop {
            match reader
                .read_event()
                .map_err(|_| DlnaError::MalformedSoap("xml read error"))?
            {
                Event::Start(e) => {
                    let local = local_name(e.name().as_ref());
                    if !in_body {
                        if local.eq_ignore_ascii_case("Body") {
                            in_body = true;
                        }
                        continue;
                    }
                    if action.is_none() {
                        action = Some(local);
                        depth_in_action = 0;
                    } else {
                        depth_in_action += 1;
                        if depth_in_action == 1 {
                            current_arg = Some(local);
                            arg_text.clear();
                        }
                    }
                }
                // Only an argument's *own* character data counts, which is what
                // `depth_in_action == 1` says: text belonging to a nested element is not
                // this argument's value, and was never recorded as one.
                Event::Text(t) if current_arg.is_some() && depth_in_action == 1 => {
                    arg_text.push_str(
                        &t.xml10_content()
                            .map_err(|_| DlnaError::MalformedSoap("bad text encoding"))?,
                    );
                }
                // An entity reference is its own event now. An undeclared one is malformed
                // XML, and this parser rejected the body for it before.
                Event::GeneralRef(r) if current_arg.is_some() && depth_in_action == 1 => {
                    arg_text.push_str(
                        &crate::xmlref::resolve(&r)
                            .ok_or(DlnaError::MalformedSoap("bad text escape"))?,
                    );
                }
                // CDATA is character data too, and control points really do wrap the DIDL
                // blob in it — it is the natural way to embed an XML document in an XML
                // document without escaping every angle bracket. This used to fall into
                // the catch-all below, so the argument was recorded as empty and the card
                // came up blank with nothing logged. Not unescaped, because the whole
                // point of CDATA is that its content is already literal.
                Event::CData(c) if current_arg.is_some() && depth_in_action == 1 => {
                    arg_text.push_str(&String::from_utf8_lossy(c.as_ref()));
                }
                Event::End(_) => {
                    if in_body && action.is_some() {
                        if depth_in_action == 0 {
                            break; // closing the action element
                        }
                        if depth_in_action == 1 {
                            // The argument's text is complete here; an arg that carried
                            // none records the empty string, as it always did.
                            if let Some(name) = current_arg.take() {
                                args.push((name, arg_text.trim().to_string()));
                                arg_text.clear();
                            }
                        }
                        depth_in_action -= 1;
                    }
                }
                Event::Empty(e) => {
                    if !in_body {
                        continue;
                    }
                    let local = local_name(e.name().as_ref());
                    if action.is_some() {
                        // A self-closing argument like <Speed/> — record an empty value.
                        args.push((local, String::new()));
                    } else {
                        // The *action itself*, self-closed: `<u:GetProtocolInfo/>` is the
                        // same document as `<u:GetProtocolInfo></u:GetProtocolInfo>`, and
                        // this arm used to be gated on an action already being open — so
                        // the zero-argument form of a required action fell through to the
                        // malformed-SOAP error and came back as HTTP 500 on legal XML.
                        // There are no arguments to collect, so the scan is done.
                        action = Some(local);
                        break;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }

        let name = action.ok_or(DlnaError::MalformedSoap("no action element in Body"))?;
        Ok(SoapAction { name, args })
    }
}

/// Render a successful SOAP action response envelope.
///
/// `service_type` is the full URN (e.g. `urn:schemas-upnp-org:service:AVTransport:1`),
/// `action` the action name; `out_args` become `<name>value</name>` children of the
/// `<u:{action}Response>` element.
#[must_use]
pub fn action_response(service_type: &str, action: &str, out_args: &[(String, String)]) -> String {
    let mut body = String::new();
    for (k, v) in out_args {
        body.push_str(&format!("<{k}>{}</{k}>", xml_escape(v)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>\
         <u:{action}Response xmlns:u=\"{service_type}\">{body}</u:{action}Response>\
         </s:Body></s:Envelope>"
    )
}

/// Render a SOAP fault for a failed action.
#[must_use]
pub fn fault(code: u16, description: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><s:Fault>\
         <faultcode>s:Client</faultcode>\
         <faultstring>UPnPError</faultstring>\
         <detail>\
         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode>\
         <errorDescription>{}</errorDescription>\
         </UPnPError></detail>\
         </s:Fault></s:Body></s:Envelope>",
        xml_escape(description)
    )
}

/// Strip a namespace prefix, returning the local element name as an owned `String`.
fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

// The shared escape, re-exported for this crate's other XML writers (GENA property
// sets, and historically the device description). It lived here as a private copy until
// #222 moved it to the substrate — DIAL carried an identical one, and the two crates
// fixed the same raw-friendly-name bug independently, three days apart.
pub(crate) use substrate_ssdp::xml_escape;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const SET_URI: &str = r#"<?xml version="1.0"?>
    <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
      <s:Body>
        <u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
          <InstanceID>0</InstanceID>
          <CurrentURI>http://10.0.0.9/video.mp4</CurrentURI>
          <CurrentURIMetaData></CurrentURIMetaData>
        </u:SetAVTransportURI>
      </s:Body>
    </s:Envelope>"#;

    /// `<u:GetProtocolInfo/>` and `<u:GetProtocolInfo></u:GetProtocolInfo>` are the same
    /// document. The self-closing form used to reach the malformed-SOAP path and come back
    /// as HTTP 500 on legal XML, for a *required* action.
    #[test]
    fn a_self_closing_action_element_is_the_same_as_an_empty_one() {
        let envelope = concat!(
            r#"<?xml version="1.0"?><s:Envelope "#,
            r#"xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
            r#"<u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/>"#,
            "</s:Body></s:Envelope>",
        );
        let a = SoapAction::parse(envelope).unwrap();
        assert_eq!(a.name, "GetProtocolInfo");
        assert!(a.args.is_empty());
    }

    /// A control point embedding the DIDL blob in CDATA is doing the natural thing for
    /// putting XML inside XML. This used to be dropped, giving a blank card in silence.
    #[test]
    fn cdata_argument_text_is_captured() {
        let envelope = concat!(
            r#"<?xml version="1.0"?><s:Envelope "#,
            r#"xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
            r#"<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#,
            "<InstanceID>0</InstanceID><CurrentURI>http://h/a.mp3</CurrentURI>",
            "<CurrentURIMetaData><![CDATA[<DIDL-Lite><item>",
            "<dc:title>Cdata Title</dc:title></item></DIDL-Lite>]]></CurrentURIMetaData>",
            "</u:SetAVTransportURI></s:Body></s:Envelope>",
        );
        let a = SoapAction::parse(envelope).unwrap();
        let blob = a.arg("CurrentURIMetaData").unwrap();
        assert!(blob.starts_with("<DIDL-Lite>"), "{blob}");
        assert_eq!(
            crate::didl::parse(blob).title.as_deref(),
            Some("Cdata Title")
        );
    }

    /// quick-xml 0.38 stopped inlining entities into the text event and began emitting
    /// each as its own `GeneralRef`, so an argument's text now arrives in fragments. A
    /// reader that still takes the first fragment as the whole value truncates every
    /// string at its first entity — and a URI is the argument most likely to carry one,
    /// because every query string after the first parameter does.
    #[test]
    fn an_argument_is_not_truncated_at_its_first_entity() {
        let envelope = concat!(
            r#"<?xml version="1.0"?><s:Envelope "#,
            r#"xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
            r#"<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#,
            "<InstanceID>0</InstanceID>",
            "<CurrentURI>http://h/v.mp4?a=1&amp;b=2&amp;c=3</CurrentURI>",
            "<CurrentURIMetaData>Tom &amp; Jerry &lt;x&gt; &#65;</CurrentURIMetaData>",
            "</u:SetAVTransportURI></s:Body></s:Envelope>",
        );
        let a = SoapAction::parse(envelope).unwrap();
        assert_eq!(a.arg("CurrentURI").unwrap(), "http://h/v.mp4?a=1&b=2&c=3");
        // Interior spacing survives too: the fragments are trimmed as one value, not
        // individually, or this would read "Tom&Jerry<x>A".
        assert_eq!(a.arg("CurrentURIMetaData").unwrap(), "Tom & Jerry <x> A");
    }

    /// A bare `&` is malformed, but quick-xml 0.38 escalated it from "this one unescape
    /// failed" to "this read failed" — which ends the scan and would take every argument
    /// after it down with it, including the URI the request exists to deliver.
    #[test]
    fn a_bare_ampersand_costs_nothing_but_itself() {
        let envelope = concat!(
            r#"<?xml version="1.0"?><s:Envelope "#,
            r#"xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
            r#"<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#,
            "<InstanceID>0</InstanceID><CurrentURIMetaData>AT&T</CurrentURIMetaData>",
            "<CurrentURI>http://h/v.mp4</CurrentURI>",
            "</u:SetAVTransportURI></s:Body></s:Envelope>",
        );
        let a = SoapAction::parse(envelope).unwrap();
        assert_eq!(a.arg("CurrentURI").unwrap(), "http://h/v.mp4");
    }

    #[test]
    fn parses_action_and_args() {
        let a = SoapAction::parse(SET_URI).unwrap();
        assert_eq!(a.name, "SetAVTransportURI");
        assert_eq!(a.arg("InstanceID"), Some("0"));
        assert_eq!(a.arg("CurrentURI"), Some("http://10.0.0.9/video.mp4"));
        assert_eq!(a.arg("CurrentURIMetaData"), Some(""));
    }

    #[test]
    fn require_missing_arg_errors() {
        let a = SoapAction::parse(SET_URI).unwrap();
        assert!(a.require("Nope").is_err());
    }

    #[test]
    fn parses_play_with_speed() {
        let body = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
          <s:Body><u:Play xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
          <InstanceID>0</InstanceID><Speed>1</Speed>
          </u:Play></s:Body></s:Envelope>"#;
        let a = SoapAction::parse(body).unwrap();
        assert_eq!(a.name, "Play");
        assert_eq!(a.arg("Speed"), Some("1"));
    }

    #[test]
    fn response_wraps_out_args() {
        let out = action_response(
            "urn:schemas-upnp-org:service:AVTransport:1",
            "GetTransportInfo",
            &[("CurrentTransportState".into(), "PLAYING".into())],
        );
        assert!(out.contains("<u:GetTransportInfoResponse"));
        assert!(out.contains("<CurrentTransportState>PLAYING</CurrentTransportState>"));
    }

    #[test]
    fn fault_carries_code() {
        let out = fault(401, "Invalid Action");
        assert!(out.contains("<errorCode>401</errorCode>"));
    }

    #[test]
    fn escaping_prevents_injection() {
        let out = action_response("svc", "X", &[("A".into(), "a<b>&\"c".into())]);
        assert!(out.contains("a&lt;b&gt;&amp;&quot;c"));
    }
}
