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

    /// Parse a SOAP control request body.
    ///
    /// # Errors
    /// [`DlnaError::MalformedSoap`] on unparseable XML or a missing action element.
    pub fn parse(body: &str) -> Result<Self, DlnaError> {
        let mut reader = Reader::from_str(body);
        reader.config_mut().trim_text(true);

        // Walk to the first element inside <...:Body>. Track whether we've entered Body.
        let mut in_body = false;
        let mut action: Option<String> = None;
        let mut args: Vec<(String, String)> = Vec::new();
        let mut current_arg: Option<String> = None;
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
                        }
                    }
                }
                Event::Text(t) => {
                    if let Some(name) = &current_arg {
                        let text = t
                            .unescape()
                            .map_err(|_| DlnaError::MalformedSoap("bad text escape"))?
                            .into_owned();
                        args.push((name.clone(), text));
                        current_arg = None; // recorded; ignore until next arg element
                    }
                }
                Event::End(_) => {
                    if in_body && action.is_some() {
                        if depth_in_action == 0 {
                            break; // closing the action element
                        }
                        if depth_in_action == 1 {
                            // Empty-element arg (no text) — record empty string.
                            if let Some(name) = current_arg.take() {
                                args.push((name, String::new()));
                            }
                        }
                        depth_in_action -= 1;
                    }
                }
                Event::Empty(e) => {
                    // Self-closing arg like <Speed/> — record empty value.
                    if in_body && action.is_some() {
                        let local = local_name(e.name().as_ref());
                        args.push((local, String::new()));
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

/// Minimal XML text escaping for response bodies.
fn xml_escape(s: &str) -> String {
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
