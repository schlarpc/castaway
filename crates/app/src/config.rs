//! Runtime configuration. Loaded from `castaway.toml` if present, else defaults.
//! (OPEN-QUESTIONS Q4: confirm TOML is the config source you want.)

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::Path;

use serde::Deserialize;

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The name senders see (mDNS instance / UPnP friendlyName).
    pub friendly_name: String,
    /// Stable device UUID (bare, no `uuid:` prefix). Regenerate to re-provision.
    pub uuid: String,
    /// HTTP host port for the shared UPnP/DIAL/Spotify server.
    pub http_port: u16,
    /// LAN IPv4 to advertise. `None` auto-detects the default-route interface.
    pub interface: Option<Ipv4Addr>,
    /// Which protocols to enable.
    pub enable: Enable,
}

/// Per-protocol enable flags.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Enable {
    /// DLNA MediaRenderer (SSDP + HTTP). Live.
    pub dlna: bool,
    /// Spotify Connect onboarding (mDNS + HTTP). Live (pairing only).
    pub spotify: bool,
    /// DIAL → YouTube Lounge (SSDP + HTTP). Live launch; Lounge client is follow-up.
    pub dial: bool,
    /// Advertise Google Cast over mDNS. Off by default until the TLS actor lands.
    pub cast: bool,
    /// Advertise AirPlay/RAOP over mDNS. Off by default until the RTSP actor lands.
    pub airplay: bool,
}

impl Default for Enable {
    fn default() -> Self {
        Self {
            dlna: true,
            spotify: true,
            dial: true,
            cast: false,
            airplay: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            friendly_name: "Hackerspace TV".to_string(),
            uuid: "0f8c2e10-castaway-0001-000000000001".to_string(),
            http_port: 8080,
            interface: None,
            enable: Enable::default(),
        }
    }
}

impl Config {
    /// Load from `path` if it exists, else return defaults.
    ///
    /// # Errors
    /// Returns an error if the file exists but can't be parsed.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let text = std::fs::read_to_string(path)?;
            Ok(toml::from_str(&text)?)
        } else {
            Ok(Self::default())
        }
    }

    /// The advertised interface IPv4 — configured, or auto-detected, or loopback.
    #[must_use]
    pub fn resolved_interface(&self) -> Ipv4Addr {
        self.interface
            .or_else(detect_ipv4)
            .unwrap_or(Ipv4Addr::LOCALHOST)
    }

    /// The base URL of the HTTP host (used for SSDP LOCATION / DIAL Application-URL).
    #[must_use]
    pub fn http_base_url(&self) -> String {
        format!("http://{}:{}", self.resolved_interface(), self.http_port)
    }
}

/// Best-effort default-route IPv4 detection: open a UDP socket "toward" a public
/// address (no packets are sent by connect) and read the chosen local address.
fn detect_ipv4() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.http_port, 8080);
        assert!(c.enable.dlna && c.enable.spotify && c.enable.dial);
        assert!(!c.enable.cast && !c.enable.airplay);
    }

    #[test]
    fn parses_partial_toml() {
        let toml = r#"
            friendly_name = "Lab TV"
            http_port = 9090
            [enable]
            cast = true
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.friendly_name, "Lab TV");
        assert_eq!(c.http_port, 9090);
        assert!(c.enable.cast);
        // Unspecified flags fall back to their defaults.
        assert!(c.enable.dlna);
    }

    #[test]
    fn http_base_url_uses_port() {
        let c = Config {
            interface: Some(Ipv4Addr::new(10, 0, 0, 5)),
            ..Config::default()
        };
        assert_eq!(c.http_base_url(), "http://10.0.0.5:8080");
    }
}
