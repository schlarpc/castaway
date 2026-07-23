//! Pure SSDP message layer: parse `M-SEARCH`/`NOTIFY` requests and build the
//! `HTTP/1.1 200 OK` search responses and `NOTIFY` advertisements. HTTP-over-UDP, so
//! this is line-oriented header parsing with no body.

use crate::device::Target;
use crate::error::SsdpError;

/// The `ST` (search target) of an `M-SEARCH`, parsed into the cases we dispatch on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchTarget {
    /// `ssdp:all` — respond with every advertised target.
    All,
    /// `upnp:rootdevice` — respond with the root device only.
    RootDevice,
    /// `uuid:...` — a specific device instance.
    Uuid(String),
    /// `urn:...:device:...:N` or `urn:...:service:...:N` — a device or service type.
    Urn(String),
}

impl SearchTarget {
    /// Parse an `ST`/`NT` header value.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        match s {
            "ssdp:all" => SearchTarget::All,
            "upnp:rootdevice" => SearchTarget::RootDevice,
            _ if s.starts_with("uuid:") => SearchTarget::Uuid(s.to_string()),
            _ => SearchTarget::Urn(s.to_string()),
        }
    }

    /// Does this search target select the given advertised [`Target`]?
    #[must_use]
    pub fn selects(&self, target: &Target) -> bool {
        match self {
            SearchTarget::All => true,
            SearchTarget::RootDevice => target.nt == "upnp:rootdevice",
            SearchTarget::Uuid(u) => &target.nt == u,
            SearchTarget::Urn(u) => &target.nt == u,
        }
    }
}

/// A parsed inbound SSDP request (only the request line + the headers we consult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsdpRequest {
    /// `M-SEARCH * HTTP/1.1` with a `MAN: "ssdp:discover"`.
    Search {
        /// The parsed `ST` header.
        st: SearchTarget,
        /// The `MX` (max wait seconds) hint, if present and valid.
        mx: Option<u8>,
    },
    /// A `NOTIFY` from another device on the network (we mostly ignore these, but
    /// parsing them keeps the responder from mis-handling the datagram).
    Notify {
        /// The `NTS` sub-type (`ssdp:alive`, `ssdp:byebye`, `ssdp:update`).
        nts: String,
    },
}

impl SsdpRequest {
    /// Parse a UDP datagram as an SSDP request.
    ///
    /// # Errors
    /// [`SsdpError::Malformed`] if it isn't a recognizable `M-SEARCH`/`NOTIFY`.
    pub fn parse(bytes: &[u8]) -> Result<Self, SsdpError> {
        let text = std::str::from_utf8(bytes).map_err(|_| SsdpError::Malformed("not utf-8"))?;
        let mut lines = text.split("\r\n");
        let request_line = lines.next().ok_or(SsdpError::Malformed("empty datagram"))?;
        let method = request_line
            .split_whitespace()
            .next()
            .ok_or(SsdpError::Malformed("no method"))?;

        let mut headers = HeaderMap::default();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim(), v.trim());
            }
        }

        match method.to_ascii_uppercase().as_str() {
            "M-SEARCH" => {
                // A conforming M-SEARCH carries MAN: "ssdp:discover"; be lenient but
                // require an ST to know what to answer.
                let st = headers
                    .get("ST")
                    .ok_or(SsdpError::Malformed("M-SEARCH without ST"))?;
                let mx = headers.get("MX").and_then(|v| v.parse::<u8>().ok());
                Ok(SsdpRequest::Search {
                    st: SearchTarget::parse(st),
                    mx,
                })
            }
            "NOTIFY" => {
                let nts = headers.get("NTS").unwrap_or("").to_string();
                Ok(SsdpRequest::Notify { nts })
            }
            _ => Err(SsdpError::Malformed("unsupported SSDP method")),
        }
    }
}

/// Builder for the outbound `HTTP/1.1 200 OK` unicast search response and the
/// multicast `NOTIFY` advertisements.
#[derive(Debug, Clone)]
pub struct SsdpResponse {
    /// `LOCATION` — the description URL, e.g. `http://192.168.1.5:8080/dlna/desc.xml`.
    pub location: String,
    /// `SERVER` header, e.g. `castaway/0.1 UPnP/1.0`.
    pub server: String,
    /// `CACHE-CONTROL` max-age seconds.
    pub max_age: u32,
}

impl SsdpResponse {
    /// Build the unicast `200 OK` reply to an `M-SEARCH` for one matched target.
    #[must_use]
    pub fn search_ok(&self, target: &Target) -> String {
        format!(
            "HTTP/1.1 200 OK\r\n\
             CACHE-CONTROL: max-age={max_age}\r\n\
             EXT:\r\n\
             LOCATION: {location}\r\n\
             SERVER: {server}\r\n\
             ST: {nt}\r\n\
             USN: {usn}\r\n\
             \r\n",
            max_age = self.max_age,
            location = self.location,
            server = self.server,
            nt = target.nt,
            usn = target.usn,
        )
    }

    /// Build a multicast `NOTIFY ssdp:alive` for one target.
    #[must_use]
    pub fn notify_alive(&self, target: &Target) -> String {
        self.notify(target, "ssdp:alive")
    }

    /// Build a multicast `NOTIFY ssdp:byebye` for one target (graceful shutdown).
    #[must_use]
    pub fn notify_byebye(&self, target: &Target) -> String {
        // byebye carries no LOCATION/CACHE-CONTROL per spec, but including them is
        // harmless and simpler; strict controllers only read NT/NTS/USN here.
        format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             NT: {nt}\r\n\
             NTS: ssdp:byebye\r\n\
             USN: {usn}\r\n\
             \r\n",
            nt = target.nt,
            usn = target.usn,
        )
    }

    fn notify(&self, target: &Target, nts: &str) -> String {
        format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             CACHE-CONTROL: max-age={max_age}\r\n\
             LOCATION: {location}\r\n\
             NT: {nt}\r\n\
             NTS: {nts}\r\n\
             SERVER: {server}\r\n\
             USN: {usn}\r\n\
             \r\n",
            max_age = self.max_age,
            location = self.location,
            nt = target.nt,
            server = self.server,
            usn = target.usn,
        )
    }
}

/// A tiny case-insensitive header map (SSDP header names are case-insensitive).
#[derive(Default)]
struct HeaderMap {
    entries: Vec<(String, String)>,
}

impl HeaderMap {
    fn insert(&mut self, key: &str, value: &str) {
        self.entries
            .push((key.to_ascii_uppercase(), value.to_string()));
    }

    fn get(&self, key: &str) -> Option<&str> {
        let key = key.to_ascii_uppercase();
        self.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const MSEARCH: &[u8] = b"M-SEARCH * HTTP/1.1\r\n\
        HOST: 239.255.255.250:1900\r\n\
        MAN: \"ssdp:discover\"\r\n\
        MX: 2\r\n\
        ST: urn:dial-multiscreen-org:service:dial:1\r\n\r\n";

    #[test]
    fn parses_msearch() {
        let req = SsdpRequest::parse(MSEARCH).unwrap();
        assert_eq!(
            req,
            SsdpRequest::Search {
                st: SearchTarget::Urn("urn:dial-multiscreen-org:service:dial:1".into()),
                mx: Some(2),
            }
        );
    }

    #[test]
    fn parses_ssdp_all_and_rootdevice() {
        assert_eq!(SearchTarget::parse("ssdp:all"), SearchTarget::All);
        assert_eq!(
            SearchTarget::parse("upnp:rootdevice"),
            SearchTarget::RootDevice
        );
        assert_eq!(
            SearchTarget::parse("uuid:abc"),
            SearchTarget::Uuid("uuid:abc".into())
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let dg = b"M-SEARCH * HTTP/1.1\r\nst: ssdp:all\r\nMx: 1\r\n\r\n";
        let req = SsdpRequest::parse(dg).unwrap();
        assert_eq!(
            req,
            SsdpRequest::Search {
                st: SearchTarget::All,
                mx: Some(1)
            }
        );
    }

    #[test]
    fn msearch_without_st_is_malformed() {
        let dg = b"M-SEARCH * HTTP/1.1\r\nMAN: \"ssdp:discover\"\r\n\r\n";
        assert!(SsdpRequest::parse(dg).is_err());
    }

    #[test]
    fn parses_notify() {
        let dg = b"NOTIFY * HTTP/1.1\r\nNTS: ssdp:alive\r\nNT: upnp:rootdevice\r\n\r\n";
        assert_eq!(
            SsdpRequest::parse(dg).unwrap(),
            SsdpRequest::Notify {
                nts: "ssdp:alive".into()
            }
        );
    }

    #[test]
    fn search_ok_has_required_headers() {
        let resp = SsdpResponse {
            location: "http://10.0.0.1:8080/desc.xml".into(),
            server: "castaway/0.1 UPnP/1.0".into(),
            max_age: 1800,
        };
        let target = Target {
            nt: "upnp:rootdevice".into(),
            usn: "uuid:x::upnp:rootdevice".into(),
        };
        let out = resp.search_ok(&target);
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(out.contains("LOCATION: http://10.0.0.1:8080/desc.xml\r\n"));
        assert!(out.contains("ST: upnp:rootdevice\r\n"));
        assert!(out.contains("USN: uuid:x::upnp:rootdevice\r\n"));
        assert!(out.ends_with("\r\n\r\n"));
    }
}
