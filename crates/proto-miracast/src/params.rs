//! The `wfd-kv` parameter language: `text/parameters` bodies, and a type per parameter.
//!
//! The grammar is trivial — `name: value` lines terminated by CRLF, plus a name-only form
//! the M3 *request* uses to ask "what can you do". What is not trivial is the value space,
//! and this module is the boundary where wire text becomes types (ground rule 1). Nothing
//! downstream sees a `HashMap<String, String>`: a session holds a [`ClientRtpPorts`] that
//! could not have been built with a non-zero RTCP port, not two integers and a comment.
//!
//! Two conventions from the notes are load-bearing and easy to lose:
//!
//! - **Answer only what was asked.** MiracleCast's `check_and_response_option()` emits a
//!   parameter only if the M3 request named it, and neither Android nor Windows minds a
//!   missing parameter — both reject a malformed one. So [`SinkCapabilities::respond_to`]
//!   takes the request's name list and answers the intersection.
//! - **`none` is a value, not an absence.** It means "I know this parameter and support
//!   nothing", which is different from omitting it. `AC3 00000000 00` says the same thing
//!   about a codec, and a parser that treats it as an error rejects a real Windows sink.

use std::fmt;

use crate::error::ParamError;
use crate::video::VideoFormats;

/// Every parameter name this sink knows, plus an escape hatch for the ones vendors invent.
///
/// An enum rather than strings because dispatch on a misspelled name is a silent no-answer
/// — the source asks for a capability, gets nothing back, and proceeds as though we said
/// no. Names are matched case-insensitively (AOSP lowercases both sides) and emitted in
/// the canonical spelling, which for exactly one parameter has an uppercase segment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParamName {
    /// `wfd_video_formats`.
    VideoFormats,
    /// `wfd_audio_codecs`.
    AudioCodecs,
    /// `wfd_3d_video_formats`.
    ThreeDVideoFormats,
    /// `wfd_content_protection`.
    ContentProtection,
    /// `wfd_display_edid`.
    DisplayEdid,
    /// `wfd_coupled_sink`.
    CoupledSink,
    /// `wfd_trigger_method`.
    TriggerMethod,
    /// `wfd_presentation_URL` — the one canonical name with uppercase in it.
    PresentationUrl,
    /// `wfd_client_rtp_ports`.
    ClientRtpPorts,
    /// `wfd_route`.
    Route,
    /// `wfd_I2C` — the other one with uppercase in it.
    I2c,
    /// `wfd_av_format_change_timing`.
    AvFormatChangeTiming,
    /// `wfd_preferred_display_mode`.
    PreferredDisplayMode,
    /// `wfd_uibc_capability`.
    UibcCapability,
    /// `wfd_uibc_setting`.
    UibcSetting,
    /// `wfd_standby_resume_capability`.
    StandbyResumeCapability,
    /// `wfd_standby` — a bare name with no value.
    Standby,
    /// `wfd_connector_type`.
    ConnectorType,
    /// `wfd_idr_request_capability`.
    IdrRequestCapability,
    /// `wfd_idr_request` — a bare name with no value (M13).
    IdrRequest,
    /// A parameter we do not implement. Kept whole: vendors invent these constantly and
    /// the right answer to one is to leave it out of the response, not to fail.
    Unknown(String),
}

impl ParamName {
    /// The canonical wire spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::VideoFormats => "wfd_video_formats",
            Self::AudioCodecs => "wfd_audio_codecs",
            Self::ThreeDVideoFormats => "wfd_3d_video_formats",
            Self::ContentProtection => "wfd_content_protection",
            Self::DisplayEdid => "wfd_display_edid",
            Self::CoupledSink => "wfd_coupled_sink",
            Self::TriggerMethod => "wfd_trigger_method",
            Self::PresentationUrl => "wfd_presentation_URL",
            Self::ClientRtpPorts => "wfd_client_rtp_ports",
            Self::Route => "wfd_route",
            Self::I2c => "wfd_I2C",
            Self::AvFormatChangeTiming => "wfd_av_format_change_timing",
            Self::PreferredDisplayMode => "wfd_preferred_display_mode",
            Self::UibcCapability => "wfd_uibc_capability",
            Self::UibcSetting => "wfd_uibc_setting",
            Self::StandbyResumeCapability => "wfd_standby_resume_capability",
            Self::Standby => "wfd_standby",
            Self::ConnectorType => "wfd_connector_type",
            Self::IdrRequestCapability => "wfd_idr_request_capability",
            Self::IdrRequest => "wfd_idr_request",
            Self::Unknown(name) => name,
        }
    }

    /// Recognise a name, case-insensitively.
    #[must_use]
    pub fn parse(name: &str) -> Self {
        const KNOWN: &[ParamName] = &[
            ParamName::VideoFormats,
            ParamName::AudioCodecs,
            ParamName::ThreeDVideoFormats,
            ParamName::ContentProtection,
            ParamName::DisplayEdid,
            ParamName::CoupledSink,
            ParamName::TriggerMethod,
            ParamName::PresentationUrl,
            ParamName::ClientRtpPorts,
            ParamName::Route,
            ParamName::I2c,
            ParamName::AvFormatChangeTiming,
            ParamName::PreferredDisplayMode,
            ParamName::UibcCapability,
            ParamName::UibcSetting,
            ParamName::StandbyResumeCapability,
            ParamName::Standby,
            ParamName::ConnectorType,
            ParamName::IdrRequestCapability,
            ParamName::IdrRequest,
        ];
        let trimmed = name.trim();
        KNOWN
            .iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(trimmed))
            .cloned()
            .unwrap_or_else(|| Self::Unknown(trimmed.to_owned()))
    }
}

impl fmt::Display for ParamName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One audio format a peer claims, with its mode bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioCodecEntry {
    /// Which codec.
    pub format: AudioFormat,
    /// The mode bitmap, whose meaning is per-codec.
    pub modes: u32,
    /// Declared latency, in units of 5 ms.
    pub latency_units: u8,
}

impl AudioCodecEntry {
    /// Declared latency in milliseconds.
    #[must_use]
    pub const fn latency_ms(self) -> u16 {
        (self.latency_units as u16) * 5
    }

    /// Whether this entry claims any mode at all.
    ///
    /// Worth asking separately from "is the codec listed", because they differ: Windows
    /// lists `AC3 00000000 00`, meaning it knows AC-3 and supports no mode of it.
    #[must_use]
    pub const fn supports_anything(self) -> bool {
        self.modes != 0
    }
}

impl fmt::Display for AudioCodecEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:08X} {:02X}",
            self.format, self.modes, self.latency_units
        )
    }
}

/// The three audio formats WFD defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioFormat {
    /// Linear PCM. Mandatory for any sink with speakers — see [`AudioCodecs::sink_default`].
    Lpcm,
    /// AAC-LC.
    Aac,
    /// Dolby Digital.
    Ac3,
}

impl AudioFormat {
    /// Mode bit 1 of LPCM: 48 kHz, 16-bit, 2 channels. The one mandatory mode.
    pub const LPCM_48K_STEREO: u32 = 0x0000_0002;
    /// Mode bit 0 of AAC: 48 kHz, 2 channels, AAC-LC.
    pub const AAC_48K_STEREO: u32 = 0x0000_0001;

    /// Parse the format token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token.trim().to_ascii_uppercase().as_str() {
            "LPCM" => Some(Self::Lpcm),
            "AAC" => Some(Self::Aac),
            "AC3" => Some(Self::Ac3),
            _ => None,
        }
    }

    /// The sample rate and channel count a mode bit names.
    ///
    /// Every defined mode of every codec is 48 kHz except LPCM bit 0, which is the only
    /// reason this returns a rate at all rather than a channel count.
    #[must_use]
    pub const fn mode(self, bit: u8) -> Option<(u32, u16)> {
        match (self, bit) {
            (Self::Lpcm, 0) => Some((44_100, 2)),
            (Self::Lpcm, 1) => Some((48_000, 2)),
            (Self::Aac, 0) | (Self::Ac3, 0) => Some((48_000, 2)),
            (Self::Aac, 1) | (Self::Ac3, 1) => Some((48_000, 4)),
            (Self::Aac, 2) | (Self::Ac3, 2) => Some((48_000, 6)),
            (Self::Aac, 3) => Some((48_000, 8)),
            _ => None,
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lpcm => "LPCM",
            Self::Aac => "AAC",
            Self::Ac3 => "AC3",
        })
    }
}

/// The whole `wfd_audio_codecs` value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioCodecs(pub Vec<AudioCodecEntry>);

impl AudioCodecs {
    /// What a sink with speakers should advertise.
    ///
    /// Both, and this is not defensive interop advice. LPCM bit 1 is a conformance
    /// requirement — Miracast v2.3 §5.1.7.1 makes 48 kHz stereo LPCM mandatory for every
    /// WFD device that renders audio, which is why sources are entitled to assume it
    /// works. AAC is what Android reaches for first. A sink that advertises only AAC gets
    /// silence from a source configured for LPCM, with nothing in any log to say why.
    #[must_use]
    pub fn sink_default() -> Self {
        Self(vec![
            AudioCodecEntry {
                format: AudioFormat::Lpcm,
                modes: AudioFormat::LPCM_48K_STEREO,
                latency_units: 0,
            },
            AudioCodecEntry {
                format: AudioFormat::Aac,
                modes: AudioFormat::AAC_48K_STEREO,
                latency_units: 0,
            },
        ])
    }

    /// The entry for `format`, if the value lists it.
    #[must_use]
    pub fn entry(&self, format: AudioFormat) -> Option<AudioCodecEntry> {
        self.0.iter().copied().find(|e| e.format == format)
    }

    /// Parse a `wfd_audio_codecs` value.
    ///
    /// # Errors
    /// [`ParamError`] if an entry is not `<format> <8 hex> <2 hex>`.
    pub fn parse(value: &str) -> Result<Self, ParamError> {
        const KEY: &str = "wfd_audio_codecs";
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("none") {
            return Ok(Self(Vec::new()));
        }
        let mut out = Vec::new();
        for (i, entry) in trimmed.split(',').enumerate() {
            let fields: Vec<&str> = entry.split_whitespace().collect();
            if fields.len() != 3 {
                return Err(ParamError::FieldCount {
                    key: KEY,
                    expected: 3,
                    found: fields.len(),
                });
            }
            let format = AudioFormat::parse(fields[0]).ok_or(ParamError::OutOfRange {
                key: KEY,
                detail: "audio format is not LPCM, AAC or AC3",
            })?;
            out.push(AudioCodecEntry {
                format,
                modes: u32::from_str_radix(fields[1], 16).map_err(|_| ParamError::NotHex {
                    key: KEY,
                    field: i * 3 + 1,
                })?,
                latency_units: u8::from_str_radix(fields[2], 16).map_err(|_| {
                    ParamError::NotHex {
                        key: KEY,
                        field: i * 3 + 2,
                    }
                })?,
            });
        }
        Ok(Self(out))
    }
}

impl fmt::Display for AudioCodecs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (i, entry) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{entry}")?;
        }
        Ok(())
    }
}

/// The transport profile of `wfd_client_rtp_ports`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtpProfile {
    /// `RTP/AVP/UDP;unicast` — the only one anything uses.
    UdpUnicast,
    /// `RTP/AVP/TCP;unicast`.
    TcpUnicast,
    /// `RTP/AVP/TCP;interleaved` — RTP inside the RTSP connection. Carries no ports.
    TcpInterleaved,
}

impl RtpProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::UdpUnicast => "RTP/AVP/UDP;unicast",
            Self::TcpUnicast => "RTP/AVP/TCP;unicast",
            Self::TcpInterleaved => "RTP/AVP/TCP;interleaved",
        }
    }
}

/// The `wfd_client_rtp_ports` value: where the sink will receive RTP.
///
/// The RTCP port is deliberately not a field. It is nominally `port1`, and the profile as
/// every implementation enforces it requires that number to be **zero** — AOSP rejects the
/// whole message with "Sink chose its wfd_client_rtp_ports poorly" otherwise, dropping the
/// session. Leaving it out of the type is what makes that unrepresentable rather than a
/// comment somebody removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientRtpPorts {
    /// The transport profile.
    pub profile: RtpProfile,
    /// The UDP port the sink will receive RTP on. Must be non-zero.
    port0: u16,
}

impl ClientRtpPorts {
    /// The sink's RTP port.
    ///
    /// # Errors
    /// [`ParamError::OutOfRange`] if `port0` is zero, which AOSP also rejects.
    pub fn new(profile: RtpProfile, port0: u16) -> Result<Self, ParamError> {
        if port0 == 0 {
            return Err(ParamError::OutOfRange {
                key: "wfd_client_rtp_ports",
                detail: "port0 must be non-zero",
            });
        }
        Ok(Self { profile, port0 })
    }

    /// The RTP port.
    #[must_use]
    pub const fn port(self) -> u16 {
        self.port0
    }

    /// Parse the value.
    ///
    /// # Errors
    /// [`ParamError`] if the profile token is unknown or the ports are malformed.
    pub fn parse(value: &str) -> Result<Self, ParamError> {
        const KEY: &str = "wfd_client_rtp_ports";
        let fields: Vec<&str> = value.split_whitespace().collect();
        let profile = match fields.first().copied() {
            Some(p) if p.eq_ignore_ascii_case(RtpProfile::UdpUnicast.as_str()) => {
                RtpProfile::UdpUnicast
            }
            Some(p) if p.eq_ignore_ascii_case(RtpProfile::TcpUnicast.as_str()) => {
                RtpProfile::TcpUnicast
            }
            Some(p) if p.eq_ignore_ascii_case(RtpProfile::TcpInterleaved.as_str()) => {
                return Ok(Self {
                    profile: RtpProfile::TcpInterleaved,
                    // Interleaved carries no ports; the RTSP connection is the transport.
                    // Non-zero so the invariant on `port0` still holds, and unused.
                    port0: u16::MAX,
                });
            }
            _ => {
                return Err(ParamError::OutOfRange {
                    key: KEY,
                    detail: "unknown RTP transport profile",
                })
            }
        };
        if fields.len() < 3 {
            return Err(ParamError::FieldCount {
                key: KEY,
                expected: 4,
                found: fields.len(),
            });
        }
        let port0 = fields[1]
            .parse::<u16>()
            .map_err(|_| ParamError::OutOfRange {
                key: KEY,
                detail: "port0 is not a port number",
            })?;
        let port1 = fields[2]
            .parse::<u16>()
            .map_err(|_| ParamError::OutOfRange {
                key: KEY,
                detail: "port1 is not a port number",
            })?;
        if port1 != 0 {
            return Err(ParamError::OutOfRange {
                key: KEY,
                detail: "port1 must be 0 — the WFD profile has no RTCP port here",
            });
        }
        Self::new(profile, port0)
    }
}

impl fmt::Display for ClientRtpPorts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.profile {
            RtpProfile::TcpInterleaved => write!(f, "{} mode=play", self.profile.as_str()),
            _ => write!(f, "{} {} 0 mode=play", self.profile.as_str(), self.port0),
        }
    }
}

/// The `wfd_trigger_method` value: which RTSP request the sink must now issue.
///
/// An exhaustive enum because the whole point of M5 is that the sink acts on it — a
/// trigger it does not recognise is a session that answers `200 OK` and then does nothing,
/// which looks to the user like a device that connected and never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMethod {
    /// Issue M6 `SETUP`.
    Setup,
    /// Issue M7 `PLAY`.
    Play,
    /// Issue M9 `PAUSE`.
    Pause,
    /// Issue M8 `TEARDOWN`.
    Teardown,
}

impl TriggerMethod {
    /// Parse the token.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "SETUP" => Some(Self::Setup),
            "PLAY" => Some(Self::Play),
            "PAUSE" => Some(Self::Pause),
            "TEARDOWN" => Some(Self::Teardown),
            _ => None,
        }
    }
}

impl fmt::Display for TriggerMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Setup => "SETUP",
            Self::Play => "PLAY",
            Self::Pause => "PAUSE",
            Self::Teardown => "TEARDOWN",
        })
    }
}

/// The `wfd_content_protection` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentProtection {
    /// No HDCP. What both open-source sinks, and Windows' own sink, answer — and what we
    /// answer (notes §6).
    None,
    /// HDCP 2.0 on a port the sink listens on. Prohibited by v2.3 for new devices;
    /// parsed, never emitted.
    Hdcp2_0(u16),
    /// HDCP 2.1 on a port the sink listens on.
    Hdcp2_1(u16),
}

impl ContentProtection {
    /// Parse the value.
    ///
    /// # Errors
    /// [`ParamError::OutOfRange`] for anything AOSP would call `ERROR_MALFORMED`.
    pub fn parse(value: &str) -> Result<Self, ParamError> {
        const KEY: &str = "wfd_content_protection";
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        // AOSP parses the port by skipping a hard-coded eight bytes — `"HDCP2.x "` — so
        // the space is part of the grammar, not whitespace to be tolerant about.
        let (version, rest) = trimmed.split_once(' ').ok_or(ParamError::OutOfRange {
            key: KEY,
            detail: "expected `HDCP2.x port=<n>`",
        })?;
        let port = rest
            .trim()
            .strip_prefix("port=")
            .and_then(|p| p.trim().parse::<u16>().ok())
            .filter(|p| *p != 0)
            .ok_or(ParamError::OutOfRange {
                key: KEY,
                detail: "port= is missing or not a port number",
            })?;
        match version {
            "HDCP2.0" => Ok(Self::Hdcp2_0(port)),
            "HDCP2.1" => Ok(Self::Hdcp2_1(port)),
            _ => Err(ParamError::OutOfRange {
                key: KEY,
                detail: "content protection version is not HDCP2.0 or HDCP2.1",
            }),
        }
    }
}

impl fmt::Display for ContentProtection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("none"),
            Self::Hdcp2_0(port) => write!(f, "HDCP2.0 port={port}"),
            Self::Hdcp2_1(port) => write!(f, "HDCP2.1 port={port}"),
        }
    }
}

/// The `wfd_presentation_URL` value: the URL the sink must use for M6–M9.
///
/// Held as the string the source sent rather than a parsed [`url::Url`], deliberately. The
/// sink's only job with it is to echo it verbatim as the request-URI — the notes are
/// explicit that reconstructing it is wrong, because the source puts its own IP in `url0`
/// while every other request-URI in the session is the literal `rtsp://localhost/wfd1.0`.
/// Round-tripping through a URL parser is an opportunity to normalise away a difference
/// the source is checking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationUrls {
    /// The primary sink's stream URL, or `None` for the `none` token.
    pub url0: Option<String>,
    /// The secondary sink's, for a coupled pair.
    pub url1: Option<String>,
}

impl PresentationUrls {
    /// Parse the value.
    ///
    /// # Errors
    /// [`ParamError::FieldCount`] if the value is not two space-separated fields.
    pub fn parse(value: &str) -> Result<Self, ParamError> {
        let fields: Vec<&str> = value.split_whitespace().collect();
        if fields.len() != 2 {
            return Err(ParamError::FieldCount {
                key: "wfd_presentation_URL",
                expected: 2,
                found: fields.len(),
            });
        }
        let of = |s: &str| (!s.eq_ignore_ascii_case("none")).then(|| s.to_owned());
        Ok(Self {
            url0: of(fields[0]),
            url1: of(fields[1]),
        })
    }
}

impl fmt::Display for PresentationUrls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}",
            self.url0.as_deref().unwrap_or("none"),
            self.url1.as_deref().unwrap_or("none")
        )
    }
}

/// The `wfd_connector_type` value (Miracast v2.3 Table 91).
///
/// Only the values a display sink can honestly claim are named; the rest round-trip
/// through [`ConnectorType::Other`]. `255` is deliberately reachable but documented as a
/// mistake: the spec warns that some sources cannot recognise a sink that reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectorType {
    /// HDMI. The right answer for an HDMI-attached panel.
    Hdmi,
    /// DisplayPort.
    DisplayPort,
    /// DVI.
    Dvi,
    /// VGA.
    Vga,
    /// "Miracast" as a connector in its own right.
    Miracast,
    /// Any other value from the table.
    Other(u8),
}

impl ConnectorType {
    /// The wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Vga => 0,
            Self::Dvi => 4,
            Self::Hdmi => 5,
            Self::Miracast => 7,
            Self::DisplayPort => 10,
            Self::Other(v) => v,
        }
    }

    /// Read the two hex digits.
    #[must_use]
    pub const fn from_wire(raw: u8) -> Self {
        match raw {
            0 => Self::Vga,
            4 => Self::Dvi,
            5 => Self::Hdmi,
            7 => Self::Miracast,
            10 => Self::DisplayPort,
            other => Self::Other(other),
        }
    }
}

impl fmt::Display for ConnectorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02X}", self.wire())
    }
}

/// A parsed `name: value` (or bare `name`) line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamLine {
    /// The parameter.
    pub name: ParamName,
    /// Its value, or `None` for the bare-name form used by M3 requests, `wfd_standby`
    /// and `wfd_idr_request`.
    pub value: Option<String>,
}

/// A `text/parameters` body, parsed into lines but not yet into values.
///
/// The two-stage split is deliberate: the framing is common to every message, while which
/// parameters are meaningful depends on which M-message carried the body. Parsing values
/// eagerly would mean a malformed parameter nobody in this state reads could fail a
/// message that is otherwise fine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParamBody(pub Vec<ParamLine>);

impl ParamBody {
    /// Parse a body.
    ///
    /// # Errors
    /// [`ParamError::NotUtf8`] if the bytes are not text.
    pub fn parse(body: &[u8]) -> Result<Self, ParamError> {
        let text = std::str::from_utf8(body).map_err(|_| ParamError::NotUtf8)?;
        let mut lines = Vec::new();
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            // A colon splits name from value; without one this is the M3-request form.
            // Splitting on the *first* colon matters — `wfd_presentation_URL`'s value is a
            // URL and has one of its own.
            match line.split_once(':') {
                Some((name, value)) => lines.push(ParamLine {
                    name: ParamName::parse(name),
                    value: Some(value.trim().to_owned()),
                }),
                None => lines.push(ParamLine {
                    name: ParamName::parse(line),
                    value: None,
                }),
            }
        }
        Ok(Self(lines))
    }

    /// The names this body asks about, in order — the M3 request's whole content.
    #[must_use]
    pub fn requested_names(&self) -> Vec<ParamName> {
        self.0.iter().map(|l| l.name.clone()).collect()
    }

    /// The value of `name`, if the body carries it with one.
    #[must_use]
    pub fn value(&self, name: &ParamName) -> Option<&str> {
        self.0
            .iter()
            .find(|l| l.name == *name)
            .and_then(|l| l.value.as_deref())
    }

    /// Whether the body carries `name` at all, with or without a value.
    #[must_use]
    pub fn contains(&self, name: &ParamName) -> bool {
        self.0.iter().any(|l| l.name == *name)
    }
}

/// Render a body from `name: value` pairs.
///
/// CRLF-terminated including the last line, because AOSP's `Parameters::parse` scans for
/// `\r\n` explicitly and a body ending without one loses its final parameter — and
/// AOSP's M13 handler substring-matches `"wfd_idr_request\r\n"`, so for that message the
/// terminator is the entire content.
#[must_use]
pub fn render_body(lines: &[(ParamName, Option<String>)]) -> Vec<u8> {
    let mut out = String::new();
    for (name, value) in lines {
        match value {
            Some(v) => out.push_str(&format!("{name}: {v}\r\n")),
            None => out.push_str(&format!("{name}\r\n")),
        }
    }
    out.into_bytes()
}

/// Everything this sink can answer an M3 request with.
///
/// A struct with one field per answerable parameter rather than a map, so adding a
/// parameter to the protocol is a compile error at every construction site instead of a
/// lookup that quietly returns nothing (notes §9.3).
#[derive(Debug, Clone)]
pub struct SinkCapabilities {
    /// What video the sink can decode.
    pub video_formats: VideoFormats,
    /// What audio it can render.
    pub audio_codecs: AudioCodecs,
    /// Where it will receive RTP.
    pub client_rtp_ports: ClientRtpPorts,
    /// Content protection. Always [`ContentProtection::None`] here — see notes §6.
    pub content_protection: ContentProtection,
    /// The physical connector the panel is on.
    pub connector_type: ConnectorType,
    /// Whether the sink will send M13 `wfd_idr_request`.
    ///
    /// Answering `false` is not free: a source that knows the sink will never ask for an
    /// IDR inserts them more often to compensate, spending bitrate to buy back the
    /// recovery we declined.
    pub idr_request: bool,
    /// The UIBC capability line, or `None` to answer `none`.
    pub uibc: Option<String>,
}

impl SinkCapabilities {
    /// Answer an M3 request: emit exactly the parameters it named, in the order it named
    /// them, and nothing else.
    ///
    /// Omitting a parameter we cannot answer is deliberate and is what MiracleCast does.
    /// Neither AOSP nor Windows errors on a missing parameter; both error on a malformed
    /// value, so inventing a plausible-looking answer is strictly worse than silence.
    #[must_use]
    pub fn respond_to(&self, requested: &[ParamName]) -> Vec<u8> {
        let mut lines: Vec<(ParamName, Option<String>)> = Vec::new();
        for name in requested {
            let value = match name {
                ParamName::VideoFormats => Some(self.video_formats.to_string()),
                ParamName::AudioCodecs => Some(self.audio_codecs.to_string()),
                ParamName::ClientRtpPorts => Some(self.client_rtp_ports.to_string()),
                ParamName::ContentProtection => Some(self.content_protection.to_string()),
                ParamName::ConnectorType => Some(self.connector_type.to_string()),
                ParamName::IdrRequestCapability => Some(u8::from(self.idr_request).to_string()),
                ParamName::UibcCapability => {
                    Some(self.uibc.clone().unwrap_or_else(|| "none".to_owned()))
                }
                // `none` is the documented "supported: nothing" value for each of these,
                // so answering it is an answer rather than a dodge.
                ParamName::ThreeDVideoFormats
                | ParamName::CoupledSink
                | ParamName::DisplayEdid
                | ParamName::I2c
                | ParamName::StandbyResumeCapability
                | ParamName::PreferredDisplayMode => Some("none".to_owned()),
                // Everything else is either source-to-sink only or a vendor parameter we
                // do not implement. Leave it out.
                _ => None,
            };
            if let Some(value) = value {
                lines.push((name.clone(), Some(value)));
            }
        }
        render_body(&lines)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The M3 request Windows really sends, from the notes' §2.5 transcript.
    const M3_REQUEST: &[u8] = b"wfd_content_protection\r\nwfd_video_formats\r\nwfd_audio_codecs\r\n\
        wfd_client_rtp_ports\r\nwfd_uibc_capability\r\nwfd_display_edid\r\nwfd_connector_type\r\n\
        wfd_idr_request_capability\r\nmicrosoft_latency_management_capability\r\nmicrosoft_cursor\r\n";

    fn caps() -> SinkCapabilities {
        SinkCapabilities {
            video_formats: VideoFormats::parse(
                "00 00 03 10 0001FFFF 1FFFFFFF 00000FFF 00 0000 0000 00 none none",
            )
            .unwrap(),
            audio_codecs: AudioCodecs::sink_default(),
            client_rtp_ports: ClientRtpPorts::new(RtpProfile::UdpUnicast, 1028).unwrap(),
            content_protection: ContentProtection::None,
            connector_type: ConnectorType::Hdmi,
            idr_request: true,
            uibc: None,
        }
    }

    #[test]
    fn an_m3_request_is_names_without_values() {
        let body = ParamBody::parse(M3_REQUEST).unwrap();
        assert_eq!(body.0.len(), 10);
        assert!(body.0.iter().all(|l| l.value.is_none()));
        assert_eq!(body.0[1].name, ParamName::VideoFormats);
        // A vendor parameter survives as itself rather than being dropped.
        assert_eq!(
            body.0[9].name,
            ParamName::Unknown("microsoft_cursor".to_owned())
        );
    }

    #[test]
    fn the_response_answers_only_what_was_asked_and_in_order() {
        let requested = ParamBody::parse(M3_REQUEST).unwrap().requested_names();
        let response = caps().respond_to(&requested);
        let text = String::from_utf8(response).unwrap();
        // Two vendor parameters were asked for and cannot be answered; they are omitted
        // rather than answered `none`, which is what MiracleCast does and what neither
        // Android nor Windows minds.
        assert!(!text.contains("microsoft_cursor"));
        assert!(!text.contains("microsoft_latency"));
        let names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split(':').next())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            names,
            vec![
                "wfd_content_protection",
                "wfd_video_formats",
                "wfd_audio_codecs",
                "wfd_client_rtp_ports",
                "wfd_uibc_capability",
                "wfd_display_edid",
                "wfd_connector_type",
                "wfd_idr_request_capability",
            ]
        );
    }

    #[test]
    fn every_line_of_a_rendered_body_ends_with_crlf() {
        // AOSP's parser scans for \r\n explicitly, so a body without a final one loses
        // its last parameter — and M13 is nothing *but* a name and a terminator.
        let requested = ParamBody::parse(M3_REQUEST).unwrap().requested_names();
        let body = caps().respond_to(&requested);
        assert!(body.ends_with(b"\r\n"));
        assert_eq!(
            body.windows(2).filter(|w| *w == b"\r\n").count(),
            8,
            "one terminator per emitted parameter"
        );
        let idr = render_body(&[(ParamName::IdrRequest, None)]);
        assert_eq!(idr, b"wfd_idr_request\r\n");
    }

    #[test]
    fn a_value_containing_a_colon_survives_the_split() {
        // wfd_presentation_URL's value is a URL, and splitting on the last colon or on
        // every colon truncates it to "rtsp".
        let body = ParamBody::parse(
            b"wfd_presentation_URL: rtsp://192.168.173.1/wfd1.0/streamid=0 none\r\n",
        )
        .unwrap();
        let urls =
            PresentationUrls::parse(body.value(&ParamName::PresentationUrl).unwrap()).unwrap();
        assert_eq!(
            urls.url0.as_deref(),
            Some("rtsp://192.168.173.1/wfd1.0/streamid=0")
        );
        assert_eq!(urls.url1, None);
    }

    #[test]
    fn names_are_matched_case_insensitively_but_emitted_canonically() {
        assert_eq!(
            ParamName::parse("WFD_VIDEO_FORMATS"),
            ParamName::VideoFormats
        );
        // The two names with uppercase in their canonical spelling.
        assert_eq!(
            ParamName::parse("wfd_presentation_url"),
            ParamName::PresentationUrl
        );
        assert_eq!(ParamName::PresentationUrl.as_str(), "wfd_presentation_URL");
        assert_eq!(ParamName::parse("wfd_i2c"), ParamName::I2c);
        assert_eq!(ParamName::I2c.as_str(), "wfd_I2C");
    }

    #[test]
    fn a_non_zero_rtcp_port_is_rejected_the_way_android_rejects_it() {
        // AOSP drops the whole session with "Sink chose its wfd_client_rtp_ports poorly".
        let err = ClientRtpPorts::parse("RTP/AVP/UDP;unicast 1028 1029 mode=play").unwrap_err();
        assert!(matches!(err, ParamError::OutOfRange { .. }));
        assert!(ClientRtpPorts::parse("RTP/AVP/UDP;unicast 1028 0 mode=play").is_ok());
    }

    #[test]
    fn client_rtp_ports_round_trips_the_working_value() {
        let value = "RTP/AVP/UDP;unicast 1028 0 mode=play";
        let parsed = ClientRtpPorts::parse(value).unwrap();
        assert_eq!(parsed.port(), 1028);
        assert_eq!(parsed.to_string(), value);
    }

    #[test]
    fn a_zero_rtp_port_cannot_be_constructed() {
        assert!(ClientRtpPorts::new(RtpProfile::UdpUnicast, 0).is_err());
    }

    #[test]
    fn audio_codecs_round_trip_including_the_zero_mode_entry() {
        // Windows lists AC3 with an all-zero mask: it knows the codec and supports no
        // mode of it. That is not the same as omitting it, and not an error.
        let value = "LPCM 00000003 00, AAC 00000001 00, AC3 00000000 00";
        let parsed = AudioCodecs::parse(value).unwrap();
        assert_eq!(parsed.0.len(), 3);
        let ac3 = parsed.entry(AudioFormat::Ac3).unwrap();
        assert!(!ac3.supports_anything());
        assert_eq!(parsed.to_string(), value);
    }

    #[test]
    fn the_sink_default_carries_the_mandatory_lpcm_mode() {
        // §5.1.7.1 makes 48 kHz stereo LPCM mandatory for a sink that renders audio, and
        // a sink advertising only AAC gets silence from an LPCM-configured source.
        let caps = AudioCodecs::sink_default();
        let lpcm = caps.entry(AudioFormat::Lpcm).expect("LPCM is mandatory");
        assert_eq!(
            lpcm.modes & AudioFormat::LPCM_48K_STEREO,
            AudioFormat::LPCM_48K_STEREO
        );
        assert_eq!(AudioFormat::Lpcm.mode(1), Some((48_000, 2)));
        assert!(caps.entry(AudioFormat::Aac).is_some());
        assert_eq!(caps.to_string(), "LPCM 00000002 00, AAC 00000001 00");
    }

    #[test]
    fn audio_latency_is_five_millisecond_units() {
        let parsed = AudioCodecs::parse("AAC 00000001 04").unwrap();
        assert_eq!(parsed.0[0].latency_ms(), 20);
    }

    #[test]
    fn content_protection_needs_the_space_before_port() {
        // AOSP parses the port by skipping exactly `"HDCP2.x "`, so the space is grammar.
        assert_eq!(
            ContentProtection::parse("HDCP2.1 port=1189").unwrap(),
            ContentProtection::Hdcp2_1(1189)
        );
        assert!(ContentProtection::parse("HDCP2.1port=1189").is_err());
        assert_eq!(
            ContentProtection::parse("none").unwrap(),
            ContentProtection::None
        );
        assert_eq!(ContentProtection::None.to_string(), "none");
        // 4444 is folklore; there is no default port and the sink chooses it.
        assert_eq!(
            ContentProtection::Hdcp2_1(53002).to_string(),
            "HDCP2.1 port=53002"
        );
    }

    #[test]
    fn every_trigger_maps_to_a_request() {
        for (token, expected) in [
            ("SETUP", TriggerMethod::Setup),
            ("PLAY", TriggerMethod::Play),
            ("PAUSE", TriggerMethod::Pause),
            ("TEARDOWN", TriggerMethod::Teardown),
        ] {
            assert_eq!(TriggerMethod::parse(token), Some(expected));
            assert_eq!(expected.to_string(), token);
        }
        assert_eq!(TriggerMethod::parse("RECORD"), None);
    }

    #[test]
    fn connector_type_hdmi_is_05() {
        assert_eq!(ConnectorType::Hdmi.to_string(), "05");
        assert_eq!(ConnectorType::from_wire(5), ConnectorType::Hdmi);
        assert_eq!(ConnectorType::from_wire(200), ConnectorType::Other(200));
    }

    #[test]
    fn a_body_that_is_not_utf8_is_an_error_not_a_panic() {
        assert_eq!(
            ParamBody::parse(&[0xff, 0xfe, 0xfd]).unwrap_err(),
            ParamError::NotUtf8
        );
    }

    #[test]
    fn bare_lines_and_valued_lines_coexist() {
        // M13's body is a bare name; M5's is a name and a value. Both are legal bodies.
        let body = ParamBody::parse(b"wfd_idr_request\r\nwfd_trigger_method: SETUP\r\n").unwrap();
        assert!(body.contains(&ParamName::IdrRequest));
        assert_eq!(body.value(&ParamName::IdrRequest), None);
        assert_eq!(body.value(&ParamName::TriggerMethod), Some("SETUP"));
    }
}
