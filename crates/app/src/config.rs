//! Runtime configuration. Loaded from `castaway.toml` if present, else defaults.
//! (OPEN-QUESTIONS Q4: confirm TOML is the config source you want.)

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use castaway_core::ProtocolKind;
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
    /// A page to render live in the idle screen's widget card (a clock, a dashboard).
    /// `None` leaves the attract scene text-only, full width. Needs the `cef` build.
    pub attract_widget_url: Option<String>,
    /// Bluetooth A2DP sink settings.
    pub bluetooth: Bluetooth,
}

impl Config {
    /// The name one protocol advertises itself under: `<friendly_name>#<protocol>`.
    ///
    /// One box shows up in several pickers at once — AirPlay, Cast, DLNA, Bluetooth — and
    /// with a single name they are indistinguishable: you pick one, something happens, and
    /// you do not know which path you took. The suffix makes the picker say which surface
    /// it is offering, which matters most when one of them is broken.
    ///
    /// Takes a [`ProtocolKind`] rather than a string so a surface cannot be labelled with
    /// a typo, and so adding a protocol cannot silently skip this.
    ///
    /// A name too long for an mDNS label is truncated in the *base*, never the suffix:
    /// dropping the suffix would defeat the point, while a clipped name is still
    /// recognisable. Truncation respects char boundaries, so a multi-byte name cannot be
    /// cut into invalid UTF-8.
    #[must_use]
    pub fn advertised_name(&self, kind: ProtocolKind) -> String {
        let suffix = format!("#{}", kind.slug());
        let room = MAX_ADVERTISED_LEN.saturating_sub(suffix.len());
        let base = &self.friendly_name;
        if base.len() <= room {
            return format!("{base}{suffix}");
        }
        let mut end = room.min(base.len());
        while end > 0 && !base.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{suffix}", &base[..end])
    }
}

/// Bluetooth A2DP sink settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Bluetooth {
    /// How to reach the controller.
    ///
    /// - `"usb"` (default) claims a USB device directly and loads its firmware. The only
    ///   option on Windows, and the only one that exercises `ControllerInit`.
    /// - `"socket:N"` attaches to the kernel's `hciN` over `HCI_CHANNEL_USER`. Linux
    ///   only, and the controller must be down. This is how a *virtual* controller is
    ///   reached — `btvirt -l2` needs no firmware — which is what makes the
    ///   no-hardware integration test possible (architecture §11.7).
    pub transport: String,
    /// Which USB controller to claim, as `vendor:product` the way `lsusb` prints it
    /// (`8087:0029` for an AX200). `None` takes the first Bluetooth device found.
    /// Ignored when `transport` is a socket.
    pub controller: Option<String>,
    /// A directory of firmware laid out like `linux-firmware`. `None` uses whatever was
    /// embedded at build time (architecture §11.3b).
    pub firmware_dir: Option<String>,
    /// Advertise only these codecs, by name: `sbc`, `aac`, `aptx`, `aptx-hd`, `ldac`.
    /// `None` advertises everything the build supports.
    ///
    /// A sender takes the first endpoint it also supports, so this is the only way to
    /// exercise a specific codec on real hardware — narrow it to `["sbc"]` and every
    /// phone falls back to the mandatory codec.
    pub codecs: Option<Vec<String>>,
    /// Where link keys and other persistent state live. `None` uses
    /// `$XDG_STATE_HOME/castaway`, falling back to the working directory.
    pub state_dir: Option<String>,
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
    /// Bluetooth A2DP sink. Off by default: it claims a USB controller exclusively, so
    /// turning it on without a dedicated one takes the box's Bluetooth away.
    pub bluetooth: bool,
}

impl Default for Enable {
    fn default() -> Self {
        Self {
            dlna: true,
            spotify: true,
            dial: true,
            cast: false,
            airplay: false,
            bluetooth: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            friendly_name: "dma.space/screen".to_string(),
            uuid: "0f8c2e10-castaway-0001-000000000001".to_string(),
            http_port: 8080,
            interface: None,
            enable: Enable::default(),
            attract_widget_url: Some("https://digitalclock.live/".to_string()),
            bluetooth: Bluetooth::default(),
        }
    }
}

impl Default for Bluetooth {
    fn default() -> Self {
        Self {
            transport: "usb".to_owned(),
            controller: None,
            firmware_dir: None,
            codecs: None,
            state_dir: None,
        }
    }
}

/// Environment variable naming the config file, for run-as-a-service deployments where
/// the working directory isn't ours to choose (the NixOS module points this at the
/// generated `castaway.toml` in the Nix store).
pub const CONFIG_ENV: &str = "CASTAWAY_CONFIG";

/// mDNS instance labels cap at 63 octets, and UPnP friendly names are not much kinder.
const MAX_ADVERTISED_LEN: usize = 63;

/// The config file looked for in the working directory when [`CONFIG_ENV`] is unset.
pub const DEFAULT_CONFIG_FILE: &str = "castaway.toml";

impl Config {
    /// Load the config the environment selects.
    ///
    /// The two sources differ in how strict they are, on purpose: an operator who set
    /// `$CASTAWAY_CONFIG` meant *that* file, so a missing one is a hard error rather than
    /// a silent boot with defaults, while the conventional working-directory file stays
    /// optional (running `cargo run` in a bare checkout should just work).
    ///
    /// # Errors
    /// If `$CASTAWAY_CONFIG` names a file that can't be read, or any config file present
    /// fails to parse.
    /// Where persistent state lives.
    ///
    /// Config says where if it wants to; otherwise `$XDG_STATE_HOME/castaway`, falling
    /// back to the working directory. Deliberately *not* the config directory: link keys
    /// are state a running receiver writes, not something an operator edits.
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        if let Some(dir) = &self.bluetooth.state_dir {
            return PathBuf::from(dir);
        }
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })
            .map_or_else(|| PathBuf::from("."), |base| base.join("castaway"))
    }

    pub fn from_env() -> anyhow::Result<Self> {
        match std::env::var_os(CONFIG_ENV) {
            Some(path) => {
                let path = PathBuf::from(path);
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {CONFIG_ENV}={}", path.display()))?;
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            None => Self::load(DEFAULT_CONFIG_FILE),
        }
    }

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
    fn missing_conventional_file_falls_back_to_defaults() {
        let absent = std::env::temp_dir().join("castaway-does-not-exist.toml");
        let c = Config::load(absent).unwrap();
        assert_eq!(c.http_port, Config::default().http_port);
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

#[cfg(test)]
mod advertised_name_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn every_surface_says_which_one_it_is() {
        let c = Config::default();
        assert_eq!(
            c.advertised_name(ProtocolKind::Bluetooth),
            "dma.space/screen#bluetooth"
        );
        assert_eq!(
            c.advertised_name(ProtocolKind::AirPlay),
            "dma.space/screen#airplay"
        );
        assert_eq!(
            c.advertised_name(ProtocolKind::YouTubeLounge),
            "dma.space/screen#youtube-lounge"
        );
    }

    #[test]
    fn a_long_name_loses_its_tail_rather_than_its_suffix() {
        // An mDNS instance label caps at 63 octets. Truncating the suffix away would
        // leave two surfaces indistinguishable, which is the thing this exists to fix.
        let c = Config {
            friendly_name: "x".repeat(200),
            ..Config::default()
        };
        let name = c.advertised_name(ProtocolKind::YouTubeLounge);
        assert!(name.len() <= 63, "{} octets", name.len());
        assert!(name.ends_with("#youtube-lounge"));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // A name of multi-byte characters must not be cut mid-codepoint; the result has
        // to still be a string, and `String` would panic on a bad boundary.
        let c = Config {
            friendly_name: "\u{1f4fa}".repeat(40),
            ..Config::default()
        };
        let name = c.advertised_name(ProtocolKind::Cast);
        assert!(name.len() <= 63, "{} octets", name.len());
        assert!(name.ends_with("#cast"));
    }
}
