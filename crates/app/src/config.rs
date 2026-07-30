//! Runtime configuration. Loaded from `castaway.toml` if present, else defaults.
//! (OPEN-QUESTIONS Q4: confirm TOML is the config source you want.)

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
pub use cast_replay::Identity;
use castaway_core::{MediaPorts, PortRange, PortRangeError, ProtocolKind};
use serde::Deserialize;

/// Top-level configuration.
///
/// Some sections are read only by builds that have the subsystem they configure —
/// `theme` and `browser` need `electron`, `audio` needs an output backend. They stay
/// in the struct unconditionally because this type is the *file schema*: a key that
/// vanishes with a feature flag turns an operator's working config into a parse error
/// on a build that merely cannot act on it. So the fields are deliberately unread in
/// some builds rather than absent from them.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[cfg_attr(not(feature = "electron"), allow(dead_code))]
pub struct Config {
    /// The name senders see (mDNS instance / UPnP friendlyName).
    pub friendly_name: String,
    /// Stable device UUID (bare, no `uuid:` prefix). Regenerate to re-provision.
    pub uuid: String,
    /// HTTP host port for the shared UPnP/DIAL/Spotify server.
    pub http_port: u16,
    /// The port range AirPlay and Cast bind their per-session media sockets from.
    #[serde(default)]
    pub media_ports: MediaPortsConfig,
    /// LAN IPv4 to advertise. `None` auto-detects the default-route interface.
    pub interface: Option<Ipv4Addr>,
    /// Which protocols to enable.
    pub enable: Enable,
    /// Console and on-disk logging.
    #[serde(default)]
    pub log: Log,
    /// Which palette the idle screen wears. `auto` follows the calendar; `plain` is the
    /// panel's own dark ramp; naming a season wears it all year.
    #[serde(default)]
    pub theme: pipeline::theme::ThemeChoice,
    /// A page to render live in the idle screen's widget card (a clock, a dashboard).
    /// `None` leaves the attract scene text-only, full width. Needs the `electron` build.
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
    /// Audio output settings. This section is also written *by* the receiver — the
    /// settings screen persists the picked output device here.
    #[serde(default)]
    pub audio: Audio,
}

/// Audio output settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Audio {
    /// Which output device to play through.
    pub output: AudioOutput,
}

/// Which output device each backend should use.
///
/// Keyed per backend rather than one shared value, because the ids share no vocabulary:
/// a PipeWire `node.name` means nothing to WASAPI and vice versa, and one config file
/// travels between the Linux box and the Windows panel. Each build reads only its own
/// key and leaves the others alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct AudioOutput {
    /// The PipeWire sink (`node.name`) — Linux builds with the native backend.
    pub pipewire: OutputChoice,
    /// The WASAPI device name — the Windows panel.
    pub windows: OutputChoice,
    /// The ALSA PCM name — Linux builds without the PipeWire backend.
    pub alsa: OutputChoice,
}

impl AudioOutput {
    /// The config key `backend` reads and the settings screen writes, or `None` where
    /// there is nothing to select (the null backend).
    #[must_use]
    pub const fn key_for(
        backend: pipeline::audio_select::OutputBackendKind,
    ) -> Option<&'static str> {
        use pipeline::audio_select::OutputBackendKind as B;
        match backend {
            B::PipeWire => Some("pipewire"),
            B::Windows => Some("windows"),
            B::Alsa => Some("alsa"),
            B::Null => None,
        }
    }

    /// This build's choice — the one behind [`Self::key_for`] of the active backend.
    ///
    /// Read where the selector is seeded (the render build) and in tests; the headless
    /// build parses the section without acting on it, like `theme`.
    #[cfg_attr(not(feature = "render"), allow(dead_code))]
    #[must_use]
    pub fn choice_for(&self, backend: pipeline::audio_select::OutputBackendKind) -> &OutputChoice {
        use pipeline::audio_select::OutputBackendKind as B;
        match backend {
            B::PipeWire => &self.pipewire,
            B::Windows => &self.windows,
            // Null reads as "default", which is also what it ignores.
            B::Alsa | B::Null => &self.alsa,
        }
    }
}

/// `"default"`, or a device id in the backend's own vocabulary.
///
/// The literal string `"default"` is the policy, not a device that happens to carry the
/// name — which is unambiguous for the two backends that persist selections: PipeWire
/// node names are `alsa_output.…`-shaped and WASAPI names are human labels. (ALSA does
/// have a PCM literally called `default`; for it the two readings coincide anyway.)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OutputChoice {
    /// Follow the system default.
    #[default]
    Default,
    /// A specific device, by backend-specific id.
    Device(String),
}

impl OutputChoice {
    /// What this choice means to the pipeline.
    #[must_use]
    pub fn selection(&self) -> pipeline::audio_select::OutputSelection {
        match self {
            Self::Default => pipeline::audio_select::OutputSelection::SystemDefault,
            Self::Device(id) => pipeline::audio_select::OutputSelection::Device(id.clone()),
        }
    }

    /// The string form the config file carries.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Default => "default",
            Self::Device(id) => id,
        }
    }
}

impl<'de> serde::Deserialize<'de> for OutputChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(if s == "default" {
            Self::Default
        } else {
            Self::Device(s)
        })
    }
}

/// Console and on-disk logging.
///
/// The two sinks have separate filters because they have separate audiences: `level` (or
/// `RUST_LOG`, which wins) is for whoever is watching the box now, `file_level` is what a
/// panel running unattended for a month writes to its own disk. Turning the console up to
/// `debug` deliberately does *not* turn the file up with it — see `logging.rs`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Log {
    /// Console filter, in `RUST_LOG` syntax, used when `RUST_LOG` is unset.
    pub level: String,
    /// Also write a rotated log file. On by default: the deploy box is a panel on a wall
    /// that nobody is attached to a terminal of, and "what happened last Tuesday" has no
    /// other answer there. Turn it off where something else already persists the
    /// console — journald does, though it is not what the Windows target has.
    pub to_file: bool,
    /// Where those files go. `None` uses the platform's log directory:
    /// `$XDG_STATE_HOME/castaway/logs`, or `%LOCALAPPDATA%\castaway\logs`.
    pub directory: Option<PathBuf>,
    /// File filter, in `RUST_LOG` syntax. Deliberately not inherited from `level`.
    pub file_level: String,
    /// How often to start a new file.
    pub rotation: Rotation,
    /// How many rotated files to keep, oldest deleted first. Ignored when `rotation` is
    /// `never`, since nothing rotates for the pruning to happen at.
    pub max_files: u16,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            to_file: true,
            directory: None,
            // Not `debug`, and not `level`: at debug the mirroring paths log per frame,
            // which on a panel left running is megabytes an hour of disk for a detail
            // nobody asked to keep.
            file_level: "info".to_owned(),
            rotation: Rotation::Daily,
            // Two weeks. Long enough to cover "it broke while we were away", short
            // enough that a `warn`-heavy fortnight is still tens of megabytes.
            max_files: 14,
        }
    }
}

/// How often the log file rolls over.
///
/// An enum rather than a string so an unrecognised value is a config parse error at
/// startup, not a silent fallback to some default rotation nobody chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rotation {
    /// A new file every minute. For chasing something short-lived, not for running.
    Minutely,
    /// A new file every hour.
    Hourly,
    /// A new file per day, which is the unit people ask questions in.
    Daily,
    /// One file, forever. `max_files` cannot bound it — only the disk does.
    Never,
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
            // A subdirectory of the platform's state directory rather than a hardcoded
            // `/var/lib/castaway/gamestream`: under the NixOS unit that is still where it
            // lands (XDG_STATE_HOME=%S), and on the Windows deploy target the old literal
            // was simply an unwritable path that lost the pairing on every restart.
            state_dir: castaway_paths::host().state().join("gamestream"),
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
    /// The group interface's subnet, as `address/prefix` — our own address first.
    ///
    /// Must agree with whatever addresses the group interface and serves DHCP on it
    /// (the NixOS module derives both from this value). The backend sweeps this range
    /// to resolve a freshly-associated peer: in WFD the sink dials the source, so the
    /// peer has no reason to send us the packet that would fill the neighbour table.
    pub group_cidr: String,
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
            group_cidr: "192.168.77.1/24".to_owned(),
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
    /// The device credential CASTv2 authenticates with. Takes precedence over
    /// [`Cast::replay`] when set — an operator who provisioned their own hardware
    /// credential meant to use it.
    pub credential: CastCredential,
    /// The CKS-provisioned credential, used when `credential` is unset.
    #[serde(alias = "cks")]
    pub replay: Replay,
}

/// The CKS credential: a real Google-issued device chain with precomputed
/// signatures, from a backend or from the table checked into `cast-replay`.
///
/// On by default, because without it device auth fails against every official
/// sender and Cast is decorative. Read `crates/cast-replay/fixtures/README.md`
/// before deciding to leave it on: the identity is shared with every install of
/// the app it came from, Google can revoke it, and the offline table stops on
/// 2027-12-06.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Replay {
    /// Whether to use a CKS credential at all. With this off and no
    /// `cast.credential`, the receiver falls back to a self-generated key that
    /// official senders reject.
    pub enabled: bool,
    /// Whether either live endpoint may be contacted.
    ///
    /// Turning this off pins the receiver to the checked-in tables — which works,
    /// offline, until 2027-12-06 and not one window past it. The live endpoints are
    /// the only thing that extends past that date, and the only thing that stops the
    /// checked-in databases from being the receiver's expiry date.
    pub network: bool,
    /// Whether AirServer's live endpoint may be contacted, on top of `network`.
    ///
    /// On by default. The fetched database covers ~30 rolling windows, so one request
    /// buys about a month and the receiver spends almost all its time answering from
    /// the cached file with no network at all. It is refreshed three days before the
    /// set runs out, so a panel that is offline for a fortnight still rolls over when
    /// its uplink returns.
    ///
    /// Turning it off leaves that identity on its bundled table and its hard
    /// 2027-03-21 end. The response is ~14 MB, which is the only reason an operator
    /// on a metered link might want it off.
    pub airserver_live: bool,
    /// Which identities to try, in order.
    ///
    /// Two are shipped, and they are *different devices on different branches of
    /// the Cast PKI* — `cks` is AirReceiver's (`Eureka Gen1 ICA`, through
    /// 2027-12-06), `airserver` is AirServer's (`Widevine Cast Subroot`, through
    /// 2027-03-21). The default order prefers `cks` because it lasts eight months
    /// longer.
    ///
    /// The reason to change it is **revocation**. Both identities are borrowed and
    /// shared with every install of the product they came from, and Google can
    /// revoke either one; the symptom is a clean TLS handshake followed by every
    /// sender refusing to talk, with nothing in our logs saying why. If that
    /// happens, reversing this list is the fix. Nothing in the receiver can detect
    /// it, which is why this is a knob and not an inference.
    ///
    /// Each identity is tried cache → live endpoint → checked-in table before the
    /// next is considered, so reordering changes which identity the panel presents
    /// rather than merely which table it falls back to.
    ///
    /// An empty list means no Cast credential at all: the receiver then presents a
    /// self-generated key that official senders reject.
    pub identity_order: Vec<Identity>,
}

impl Default for Replay {
    fn default() -> Self {
        Self {
            enabled: true,
            network: true,
            airserver_live: true,
            identity_order: vec![Identity::Cks, Identity::AirServer],
        }
    }
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
    /// Skip segments during YouTube playback. Needs the `electron` build — there is no
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
    #[cfg_attr(not(feature = "electron"), allow(dead_code))]
    pub fn browser_program(&self) -> std::path::PathBuf {
        std::env::var_os("CASTAWAY_ELECTRON")
            .map(std::path::PathBuf::from)
            .or_else(|| self.browser.electron_path.clone().map(Into::into))
            .unwrap_or_else(|| std::path::PathBuf::from("electron"))
    }

    /// The directory holding the browser host app (`browser-host/`).
    #[must_use]
    #[cfg_attr(not(feature = "electron"), allow(dead_code))]
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

/// The port range AirPlay and Cast bind their per-session media sockets from.
///
/// These are the sockets a `SETUP` or an ANSWER names — RAOP audio/control/timing and
/// the mirroring data channel for AirPlay, the mirroring RTP socket for Cast. They used
/// to bind ephemeral OS-assigned ports, which no firewall rule can cover: the control
/// plane looked perfect and the media arrived at a closed port. A declared range is
/// what lets the NixOS module (and a Windows firewall rule) open exactly what the
/// process may listen on — the range here is the one `docs/network-surface.md`
/// documents and `nix/network-surface.json` carries to the firewall.
///
/// The default width is 32 ports. One AirPlay session takes four (three UDP, one TCP)
/// and one Cast mirroring session takes one, so the default sustains several times the
/// concurrent-session count a single panel can display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MediaPortsConfig {
    /// First port of the inclusive range.
    pub first: u16,
    /// Last port of the inclusive range.
    pub last: u16,
}

impl Default for MediaPortsConfig {
    fn default() -> Self {
        Self {
            // An unassigned-in-practice block well clear of every fixed port this
            // receiver or its peers use.
            first: 41000,
            last: 41031,
        }
    }
}

impl MediaPortsConfig {
    /// The validated policy the adapters bind with.
    ///
    /// # Errors
    /// [`PortRangeError`] if the range starts at 0 or is backwards. Deliberately not
    /// lenient: an operator who wrote a broken range meant to control these ports, and
    /// quietly falling back to ephemeral would undo exactly what they asked for.
    pub fn policy(self) -> Result<MediaPorts, PortRangeError> {
        PortRange::new(self.first, self.last).map(MediaPorts::Range)
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
    /// Where link keys and other persistent state live. `None` uses the platform's state
    /// directory: `$XDG_STATE_HOME/castaway`, or `%LOCALAPPDATA%\castaway\state`.
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
///
/// Everything is on by default: the receiver's job is to catch whatever the LAN throws
/// at it, and each adapter that needs hardware the box lacks logs a warning and skips
/// itself rather than failing the process — so "all on" degrades to "all that this box
/// can do". These flags exist to *withhold* a capability deliberately (e.g. Miracast on
/// a box whose only upstream is the same Wi-Fi radio it would take into group-owner
/// mode, or Bluetooth when the one controller is needed elsewhere).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Enable {
    /// DLNA MediaRenderer (SSDP + HTTP).
    pub dlna: bool,
    /// Spotify Connect (mDNS + HTTP onboarding, then playback).
    pub spotify: bool,
    /// DIAL → YouTube Lounge (SSDP + HTTP). Only advertised when there is a browser to
    /// launch into (`electron` builds); otherwise gated off at startup regardless.
    pub dial: bool,
    /// Google Cast: mDNS advertisement, TLS actor on 8009, media LOAD and mirroring.
    pub cast: bool,
    /// AirPlay/RAOP: mDNS advertisement, RTSP control on 7000, audio and mirroring.
    pub airplay: bool,
    /// Bluetooth A2DP sink. Claims a Bluetooth controller *exclusively* (USB by
    /// default, `socket:N` on Linux) — turn this off if the box's one controller is
    /// needed for anything else. No controller present just logs and skips.
    pub bluetooth: bool,
    /// GameStream / Sunshine client. The one protocol where the panel dials *out*:
    /// until a host is paired (a PIN exchange someone must drive from the host side)
    /// the adapter only browses mDNS for hosts, which costs nothing — so it stays on
    /// and pairing remains the only gate.
    pub gamestream: bool,
    /// Miracast sink. The expensive one: it takes the Wi-Fi radio into group-owner
    /// mode. On a box whose upstream is that same radio the two roles time-share, and
    /// mirroring — the one workload with no slack — is what pays for it (architecture
    /// §7.5). Fine with Ethernet upstream; turn it off if the radio is the uplink.
    /// A radio that can't do P2P group-owner logs and skips.
    pub miracast: bool,
}

impl Default for Enable {
    fn default() -> Self {
        Self {
            dlna: true,
            spotify: true,
            dial: true,
            cast: true,
            airplay: true,
            bluetooth: true,
            gamestream: true,
            miracast: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: pipeline::theme::ThemeChoice::default(),
            browser: Browser::default(),
            friendly_name: "dma.space/screen".to_string(),
            uuid: "0f8c2e10-castaway-0001-000000000001".to_string(),
            http_port: 8080,
            media_ports: MediaPortsConfig::default(),
            interface: None,
            enable: Enable::default(),
            log: Log::default(),
            attract_widget_url: Some("https://digitalclock.live/".to_string()),
            bluetooth: Bluetooth::default(),
            airplay: AirPlay::default(),
            spotify: Spotify::default(),
            sponsorblock: SponsorBlock::default(),
            cast: Cast::default(),
            miracast: Miracast::default(),
            gamestream: GameStream::default(),
            audio: Audio::default(),
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
    /// Config says where if it wants to; otherwise the platform's state directory
    /// (`castaway-paths`). Deliberately *not* the config directory: link keys are state a
    /// running receiver writes, not something an operator edits.
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        if let Some(dir) = &self.bluetooth.state_dir {
            return PathBuf::from(dir);
        }
        castaway_paths::host().state().to_path_buf()
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
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the 8.8.8.8 connect-only entry in surface.rs's outbound table"
)]
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
        assert!(c.enable.cast && c.enable.airplay);
        assert!(c.enable.bluetooth && c.enable.gamestream && c.enable.miracast);
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

    /// On by default: without a CKS credential, device auth fails against every
    /// official sender and Cast is decorative.
    #[test]
    fn cks_is_enabled_unless_it_is_turned_off() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.cast.replay.enabled);
        assert!(c.cast.replay.network);

        let c: Config = toml::from_str("[cast.replay]\nnetwork = false\n").unwrap();
        assert!(
            c.cast.replay.enabled,
            "one key set must not clear the other"
        );
        assert!(
            !c.cast.replay.network,
            "with the backend off the receiver is pinned to the table's 2027 end"
        );

        let c: Config = toml::from_str("[cast.replay]\nenabled = false\n").unwrap();
        assert!(!c.cast.replay.enabled);
    }

    /// The two credential sources are independent in the file; precedence between
    /// them is `spawn_cast`'s, and a provisioned credential wins.
    #[test]
    fn a_provisioned_credential_parses_alongside_cks() {
        let toml = r#"
            [cast.credential]
            key_file = "/run/secrets/cast-key.pem"
            certificate_file = "/run/secrets/cast-cert.der"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.cast.credential.key_file.is_some());
        assert!(c.cast.replay.enabled);
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
    fn output_devices_are_keyed_per_backend_and_default_to_default() {
        use pipeline::audio_select::{OutputBackendKind, OutputSelection};

        let c = Config::default();
        assert_eq!(c.audio.output.pipewire, OutputChoice::Default);

        let toml = r#"
            [audio.output]
            pipewire = "alsa_output.usb-DAC.analog-stereo"
            windows = "default"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            c.audio
                .output
                .choice_for(OutputBackendKind::PipeWire)
                .selection(),
            OutputSelection::Device("alsa_output.usb-DAC.analog-stereo".into())
        );
        // The literal "default" is the policy, not a device named that.
        assert_eq!(
            c.audio
                .output
                .choice_for(OutputBackendKind::Windows)
                .selection(),
            OutputSelection::SystemDefault
        );
        // An unmentioned backend follows the system default.
        assert_eq!(
            c.audio
                .output
                .choice_for(OutputBackendKind::Alsa)
                .selection(),
            OutputSelection::SystemDefault
        );
    }

    #[test]
    fn every_selectable_backend_has_a_config_key() {
        use pipeline::audio_select::OutputBackendKind as B;
        for backend in [B::PipeWire, B::Windows, B::Alsa] {
            assert!(AudioOutput::key_for(backend).is_some());
        }
        assert_eq!(AudioOutput::key_for(B::Null), None);
    }

    #[test]
    fn media_ports_default_to_a_firewallable_range() {
        // The default range is load-bearing: nix/network-surface.json carries it to the
        // NixOS firewall, so a change here without regenerating that file must fail the
        // surface freshness test, not just this one.
        let c = Config::default();
        assert_eq!(c.media_ports.policy().unwrap().to_string(), "41000-41031");
    }

    #[test]
    fn a_broken_media_port_range_is_an_error_not_a_fallback() {
        // Ephemeral fallback would silently reopen the un-firewallable behaviour the
        // range exists to close; an operator who wrote a range meant to control it.
        let c: Config = toml::from_str("[media_ports]\nfirst = 0\nlast = 10\n").unwrap();
        assert!(c.media_ports.policy().is_err());
        let c: Config = toml::from_str("[media_ports]\nfirst = 50\nlast = 40\n").unwrap();
        assert!(c.media_ports.policy().is_err());
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
