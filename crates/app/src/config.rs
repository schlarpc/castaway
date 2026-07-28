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
    /// AirPlay settings.
    pub airplay: AirPlay,
    /// Spotify Connect settings.
    pub spotify: Spotify,
    /// Skipping sponsor segments in YouTube playback.
    pub sponsorblock: SponsorBlock,
    /// Google Cast settings.
    pub cast: Cast,
    /// Miracast / Wi-Fi Display settings.
    pub miracast: Miracast,
    /// GameStream / Sunshine client settings.
    pub gamestream: GameStream,
    /// Where the browser subprocess lives (D36). Defaults work for a Nix-built artifact
    /// and for running from the repo, so most deployments never set these.
    #[serde(default)]
    pub browser: Browser,
}

/// GameStream / Sunshine client settings.
///
/// The inverted protocol (D37): the panel is the Moonlight client, so there is nothing
/// to advertise and nothing arrives unbidden. A host must be paired with first, which
/// means someone typing a PIN into *the host's* UI while we hold a request open.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GameStream {
    /// Where the client certificate and the per-host pairings live. This directory is
    /// the credential: a host trusts exactly one certificate, so losing it means
    /// re-pairing with every host. Must be writable and persistent, and readable only
    /// by the service user.
    pub state_dir: PathBuf,
    /// Pair with this host at startup, using `pair_pin`. Both must be set; the PIN is
    /// consumed once and pairing is persisted, so this is meant to be removed from the
    /// config afterwards rather than left in place.
    pub pair_host: Option<String>,
    /// The PIN to type into the host's own UI during that pairing.
    pub pair_pin: Option<String>,
    /// Start streaming from this host as soon as the receiver is up. Off unless set:
    /// a panel that launches a game at boot is a panel nobody asked.
    pub autostart_host: Option<String>,
    /// Which app to launch. `None` takes whatever the host lists first, which on
    /// Sunshine is the desktop.
    pub autostart_app: Option<String>,
    /// Requested stream width in pixels.
    pub width: u32,
    /// Requested stream height in pixels.
    pub height: u32,
    /// Requested frame rate.
    pub fps: u32,
    /// Video bitrate in kbps, inclusive of FEC overhead.
    pub bitrate_kbps: u32,
    /// Let the host change the game's own resolution to match the request.
    pub optimize_settings: bool,
    /// Also play the audio on the host's speakers.
    pub play_audio_on_host: bool,
    /// Offer HEVC when the host supports it. Off by default for the same reason
    /// AirPlay's HEVC offer is: the decode path is proven on H.264, and a codec we
    /// negotiate but decode badly looks like a broken host.
    pub allow_hevc: bool,
}

impl Default for GameStream {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/castaway/gamestream"),
            pair_host: None,
            pair_pin: None,
            autostart_host: None,
            autostart_app: None,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            optimize_settings: false,
            play_audio_on_host: false,
            allow_hevc: false,
        }
    }
}

/// Miracast settings.
///
/// Miracast is the one protocol here that needs a radio rather than a socket: it forms a
/// Wi-Fi Direct group instead of riding the LAN, which means a driver that supports
/// `P2P-GO` alongside the station interface, a wpa_supplicant we can talk to, and a DHCP
/// server on the group interface. `docs/miracast-protocol-notes.md` §7.6 has the checks.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Miracast {
    /// The wpa_supplicant-managed interface to form the group on — the *parent*, not the
    /// `p2p-*` group interface, which does not exist until the group starts.
    pub interface: String,
    /// Where wpa_supplicant's control sockets live.
    pub control_dir: String,
    /// Operating frequency in MHz. `None` lets wpa_supplicant choose, which in practice
    /// means a 2.4 GHz social channel — the busiest part of the spectrum, and the one
    /// place mirroring has no slack.
    pub freq_mhz: Option<u16>,
    /// The UDP port to receive RTP on. Advertised in M3 and echoed in `SETUP`.
    pub rtp_port: u16,
    /// Maximum throughput to advertise, in Mbps.
    pub max_throughput_mbps: u16,
}

impl Default for Miracast {
    fn default() -> Self {
        Self {
            interface: "wlan0".to_owned(),
            control_dir: "/run/wpa_supplicant".to_owned(),
            freq_mhz: None,
            // What lazycast uses and what every capture shows; nothing requires it.
            rtp_port: 1028,
            max_throughput_mbps: 200,
        }
    }
}

/// AirPlay settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlay {
    /// Offer HEVC mirroring as well as H.264 (feature bit 42).
    ///
    /// Off by default, and that default is the safe one: what a sender encodes is
    /// decided entirely by what we advertise, and a sender that picks HEVC against a
    /// build with no HEVC path sends an empty codec-config packet and stops — which
    /// looks exactly like a mirror that never started rather than like a codec problem.
    ///
    /// It is a knob rather than a constant so both paths can be exercised against one
    /// device in one sitting; there is no other way to see the second one at all.
    pub offer_hevc: bool,
    /// The mirroring height to advertise, in pixels.
    ///
    /// The sender treats *height* as the controlling dimension and fits the width to
    /// however the device is being held, so this is a budget rather than a geometry.
    /// 1080 keeps senders on H.264; 2160 is what makes a Mac reach for HEVC, and is
    /// only worth asking for alongside `offer_hevc`.
    pub mirror_height: u32,
}

impl Default for AirPlay {
    fn default() -> Self {
        Self {
            offer_hevc: false,
            mirror_height: 1080,
        }
    }
}

/// Google Cast settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Cast {
    /// The device credential CASTv2 authenticates with.
    pub credential: CastCredential,
}

/// Where the Cast device credential comes from.
///
/// Leaving this unset generates one at startup, which is fine for everything on the LAN
/// that skips device auth and useless for everything that does not: an official sender
/// only accepts a chain rooted in Google's Cast device CA, whose key is fused into
/// licensed silicon. Getting such a credential means extracting it from hardware you own;
/// what this section does is make that a matter of provisioning rather than of code.
///
/// The paths are read at startup and never copied anywhere. Point them at files the
/// service user can read and nothing else can — a device credential identifies one
/// specific piece of hardware, and it is the one secret in this project that would be
/// genuinely bad to publish. It does not belong in the Nix store, which is world-readable.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CastCredential {
    /// PKCS#8 PEM holding the device's RSA private key.
    pub key_file: Option<PathBuf>,
    /// The device (leaf) certificate, DER. Signed by the key in `key_file`.
    pub certificate_file: Option<PathBuf>,
    /// Intermediate certificates, DER, ordered leaf-ward first — the path from the
    /// device certificate up to (but not including) the root a sender already trusts.
    #[serde(default)]
    pub intermediate_files: Vec<PathBuf>,
}

/// A device credential read off disk.
pub struct LoadedCredential {
    /// PKCS#8 PEM for the device key.
    pub key_pem: String,
    /// The device (leaf) certificate, DER.
    pub certificate_der: Vec<u8>,
    /// Intermediates, DER, leaf-ward first.
    pub intermediates_der: Vec<Vec<u8>>,
}

impl CastCredential {
    /// The key and certificate, or `None` when no credential is configured.
    ///
    /// A half-configured credential is an error rather than a fallback: someone who set
    /// one of these two meant to authenticate, and quietly booting with a self-signed dev
    /// key would look identical to success right up until a sender refused the panel.
    ///
    /// # Errors
    /// If only one of the two files is set, or any of them cannot be read.
    pub fn load(&self) -> anyhow::Result<Option<LoadedCredential>> {
        let (key_file, certificate_file) = match (&self.key_file, &self.certificate_file) {
            (Some(k), Some(c)) => (k, c),
            (None, None) => return Ok(None),
            (Some(_), None) => anyhow::bail!(
                "cast.credential.key_file is set but certificate_file is not; a key with no \
                 certificate cannot answer a device-auth challenge"
            ),
            (None, Some(_)) => anyhow::bail!(
                "cast.credential.certificate_file is set but key_file is not; there is nothing \
                 to sign with"
            ),
        };
        let key_pem = std::fs::read_to_string(key_file)
            .with_context(|| format!("reading the Cast device key from {}", key_file.display()))?;
        let certificate_der = std::fs::read(certificate_file).with_context(|| {
            format!(
                "reading the Cast device certificate from {}",
                certificate_file.display()
            )
        })?;
        let intermediates_der = self
            .intermediate_files
            .iter()
            .map(|p| {
                std::fs::read(p).with_context(|| {
                    format!(
                        "reading a Cast intermediate certificate from {}",
                        p.display()
                    )
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Some(LoadedCredential {
            key_pem,
            certificate_der,
            intermediates_der,
        }))
    }
}

/// SponsorBlock settings.
///
/// Defaults are the conservative set. `sponsor`, `selfpromo` and `music_offtopic` are the
/// categories nobody asks to sit through; `intro`/`outro`/`filler` are left off because
/// people notice those being cut and disagree about whether they should be.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SponsorBlock {
    /// Skip segments during YouTube playback. Needs the `cef` build — there is no
    /// playback to skip without one.
    pub enabled: bool,
    /// Which categories to skip.
    pub categories: Vec<sponsorblock::Category>,
    /// Ignore segments shorter than this. A sub-second seek is a visible stutter that
    /// saves nobody anything.
    pub minimum_seconds: f64,
    /// Say so on the overlay when a segment is skipped. On by default: a screen that
    /// silently jumps looks broken, and the toast is also where the database gets the
    /// credit its licence asks for.
    pub toast: bool,
    /// Press the screen's own "Skip Ad" button as soon as it lights up.
    ///
    /// This is YouTube's ads, not SponsorBlock's segments — a different mechanism that
    /// happens to ride the same Lounge session, which is why it lives here. It only
    /// reaches *skippable* ads after their countdown; unskippable ones play, and nothing
    /// is muted (a mute that failed to lift would leave a silent display, which is worse
    /// than the ad).
    pub skip_ads: bool,
}

impl Default for SponsorBlock {
    fn default() -> Self {
        Self {
            enabled: true,
            categories: vec![
                sponsorblock::Category::Sponsor,
                sponsorblock::Category::SelfPromo,
                sponsorblock::Category::MusicOfftopic,
            ],
            minimum_seconds: 1.0,
            toast: true,
            skip_ads: true,
        }
    }
}

impl Config {
    /// The Electron binary to run the browser host with.
    ///
    /// `$CASTAWAY_ELECTRON` first so a developer can point at a build under test without
    /// editing config, then the configured path, then `electron` on `PATH`. The packaged
    /// artifacts set the environment variable, which is why there is no clever search
    /// here: on a panel the answer is known at build time.
    #[must_use]
    pub fn browser_program(&self) -> std::path::PathBuf {
        std::env::var_os("CASTAWAY_ELECTRON")
            .map(std::path::PathBuf::from)
            .or_else(|| self.browser.electron_path.clone().map(Into::into))
            .unwrap_or_else(|| std::path::PathBuf::from("electron"))
    }

    /// The directory holding the browser host app (`browser-host/`).
    #[must_use]
    pub fn browser_app_dir(&self) -> std::path::PathBuf {
        std::env::var_os("CASTAWAY_BROWSER_APP")
            .map(std::path::PathBuf::from)
            .or_else(|| self.browser.app_dir.clone().map(Into::into))
            .unwrap_or_else(|| std::path::PathBuf::from("browser-host"))
    }

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

/// Spotify Connect settings.
///
/// There is deliberately no account here. The receiver holds no credentials: whoever
/// walks up hands it theirs over zeroconf by picking castaway in their Spotify app, and
/// they are dropped when the next person pairs (DECISION-LOG D30). A `username`/`password`
/// pair in this file would be both a downgrade in behaviour and a secret on disk.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Spotify {
    /// Volume the device comes up at, `0.0..=1.0`.
    ///
    /// Applies once per session, when someone pairs. Half scale because a panel that
    /// comes up at full volume in a shared space is the kind of thing that only happens
    /// once before somebody unplugs it.
    pub initial_volume: f32,

    /// Stream quality: 96, 160 or 320 kbps.
    ///
    /// librespot's own default is 160, which is what a Premium account entitled to 320
    /// was quietly getting — audible on a PA, and discoverable from no log line. 320 here
    /// because the account decides what it can actually have; asking for more than the
    /// entitlement costs nothing.
    pub bitrate: u16,

    /// Apply Spotify's loudness normalisation.
    ///
    /// On, because the alternative is track-to-track volume jumps in a shared room, which
    /// every real Connect speaker smooths out and people reach for the volume knob over.
    /// librespot defaults this off.
    pub normalisation: bool,

    /// Directories to search for tracks a user synced from their own files.
    ///
    /// Empty by default, and the default is a position rather than an omission: this
    /// receiver holds nobody's music library, so a playlist's local files genuinely
    /// cannot play here. What was wrong before is that it did not *say* so — the card
    /// rendered the local track in full, the player then found nothing and skipped, and
    /// from the room that reads as the panel dropping songs at random (G50).
    ///
    /// Point this at a share the panel can read and those tracks play like any other.
    /// Paths are handed to librespot as-is.
    pub local_file_directories: Vec<PathBuf>,
}

impl Default for Spotify {
    fn default() -> Self {
        Self {
            initial_volume: 0.5,
            bitrate: 320,
            normalisation: true,
            local_file_directories: Vec::new(),
        }
    }
}

/// Per-protocol enable flags.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Enable {
    /// DLNA MediaRenderer (SSDP + HTTP). Live.
    pub dlna: bool,
    /// Spotify Connect (mDNS + HTTP onboarding, then playback). Live.
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
    /// GameStream / Sunshine client. Off by default because it is the one protocol
    /// where the panel dials *out*: it needs a host to have been paired with, which is
    /// a deliberate act, and it is useless until then.
    pub gamestream: bool,
    /// Miracast sink. Off by default, and for a heavier reason than the others: it takes
    /// the Wi-Fi radio into group-owner mode. On a box whose upstream is that same radio
    /// the two roles time-share, and mirroring — the one workload with no slack — is what
    /// pays for it (architecture §7.5). Turn it on with Ethernet upstream.
    pub miracast: bool,
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
            gamestream: false,
            miracast: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            browser: Browser::default(),
            friendly_name: "dma.space/screen".to_string(),
            uuid: "0f8c2e10-castaway-0001-000000000001".to_string(),
            http_port: 8080,
            interface: None,
            enable: Enable::default(),
            attract_widget_url: Some("https://digitalclock.live/".to_string()),
            bluetooth: Bluetooth::default(),
            airplay: AirPlay::default(),
            spotify: Spotify::default(),
            sponsorblock: SponsorBlock::default(),
            cast: Cast::default(),
            miracast: Miracast::default(),
            gamestream: GameStream::default(),
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

    /// How many configured SponsorBlock categories this build did not recognise.
    ///
    /// Categories parse leniently so that a category the API *adds* cannot break the
    /// parse of a response that also contains ones we know. Config is the other case
    /// entirely: an unrecognised name there is a typo, and left alone it would quietly
    /// mean "skip nothing of this kind" for as long as nobody looked.
    #[must_use]
    pub fn unknown_sponsorblock_categories(&self) -> usize {
        self.sponsorblock
            .categories
            .iter()
            .filter(|c| **c == sponsorblock::Category::Unknown)
            .count()
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
    fn sponsorblock_categories_come_from_the_toml() {
        let toml = r#"
            [sponsorblock]
            categories = ["sponsor", "intro", "outro"]
            minimum_seconds = 2.5
            toast = false
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            c.sponsorblock.categories,
            vec![
                sponsorblock::Category::Sponsor,
                sponsorblock::Category::Intro,
                sponsorblock::Category::Outro
            ]
        );
        assert!((c.sponsorblock.minimum_seconds - 2.5).abs() < f64::EPSILON);
        assert!(!c.sponsorblock.toast);
        // Unmentioned keys keep their defaults.
        assert!(c.sponsorblock.enabled);
    }

    #[test]
    fn a_misspelled_category_is_reported_rather_than_silently_ignored() {
        // Categories deserialize with a catch-all so a category the API adds cannot fail
        // the parse of a response. The same leniency applied to *config* would turn a
        // typo into "skips nothing", silently, forever — so the loader names them.
        let toml = r#"
            [sponsorblock]
            categories = ["sponsor", "sponser"]
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.unknown_sponsorblock_categories(), 1);
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
            "dma.space/screen#youtube"
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
        assert!(name.ends_with("#youtube"));
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

/// Browser subprocess locations (D36). Both are normally supplied by the packaging, so a
/// hand-written `castaway.toml` never needs this section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Browser {
    /// Path to the Electron binary. `$CASTAWAY_ELECTRON` overrides it.
    pub electron_path: Option<String>,
    /// Path to the host app directory. `$CASTAWAY_BROWSER_APP` overrides it.
    pub app_dir: Option<String>,
}
