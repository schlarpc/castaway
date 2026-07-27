//! The M1–M16 RTSP exchange, as a pure state machine.
//!
//! ## The thing that makes WFD's RTSP different
//!
//! Both endpoints are a full RTSP client *and* a full RTSP server on one socket, and the
//! roles split by message rather than by endpoint. The source drives `OPTIONS`,
//! `GET_PARAMETER` and `SET_PARAMETER` (M1, M3, M4, M5, M10–M12, M14–M16); the **sink**
//! drives `SETUP`, `PLAY`, `PAUSE`, `TEARDOWN` and its own `SET_PARAMETER` (M6–M9, M13).
//! So there are two independent `CSeq` counters, and a response arriving is not
//! necessarily a response to the last thing we sent — requests and responses interleave
//! freely. This is the single biggest structural difference from AirPlay's RTSP dialect
//! and the place naive implementations break.
//!
//! It is also why the sink is the TCP *client*, connecting out to the source's port 7236,
//! despite advertising a "Session Management Control Port" in its own beacon that reads
//! exactly like a listen port. `docs/miracast-protocol-notes.md` §2.1 confirms that four
//! ways; getting it backwards produces a sink that waits forever with nothing in any log.
//!
//! ## Shape
//!
//! Sans-I/O per ground rule 3: [`WfdSession::on_request`] and [`WfdSession::on_response`]
//! take a parsed message and return [`SinkOutput`]s for the actor to write. Nothing here
//! knows about sockets, and the whole M1→M7 handshake is therefore driven from a
//! checked-in transcript in the tests below.
//!
//! The state is an enum whose variants carry exactly the data valid in that state — the
//! session id exists only from [`SessionState::Ready`] onward, and [`NegotiatedConfig`]
//! only from M4 — rather than the `Session<S: State>` typestate the notes sketch. The
//! reason is the driver: an actor has to own the state across `await` points and mutate it
//! in place, and a `self`-consuming typestate would have to be boxed and re-boxed through
//! every one. The invariant that actually matters is kept either way: `NegotiatedConfig`
//! has private fields and is constructible only by the M4 handler, so "we started decoding
//! at a resolution nobody negotiated" is still unrepresentable.

use std::collections::HashMap;

use tracing::{debug, info, warn};

use crate::error::MiracastError;
use crate::params::{
    render_body, AudioCodecEntry, ClientRtpPorts, ParamBody, ParamName, PresentationUrls,
    SinkCapabilities, TriggerMethod,
};
use crate::video::{NegotiatedVideo, Profile, ResolutionTable, VideoFormats, VideoMode};

/// The feature tag every WFD peer requires. A sink that answers `551 Option Not
/// Supported` to it is abandoned by real senders.
pub const WFD_FEATURE: &str = "org.wfa.wfd1.0";

/// The default RTSP control port, IANA-registered as `display/7236/tcp`.
pub const DEFAULT_CONTROL_PORT: u16 = 7236;

/// What the sink advertises in the M1 response.
///
/// The two methods are not an understatement: a sink never *receives* `SETUP`, `PLAY`,
/// `PAUSE` or `TEARDOWN` — it sends them. This is the value both working open-source
/// sinks send and Windows accepts.
const SINK_PUBLIC: &str = "org.wfa.wfd1.0, SET_PARAMETER, GET_PARAMETER";

/// The request-URI the source uses for its own messages, and which the sink echoes on
/// M13. It is a literal, not a reachable address — the *presentation* URL from M4 is the
/// one that names a host.
pub const CONTROL_URI: &str = "rtsp://localhost/wfd1.0";

/// An incoming RTSP request, as the actor parsed it.
#[derive(Debug, Clone, Copy)]
pub struct WfdRequest<'a> {
    /// The method, upper-case.
    pub method: &'a str,
    /// The request-URI as it arrived.
    pub uri: &'a str,
    /// Headers in arrival order.
    pub headers: &'a [(String, String)],
    /// The body.
    pub body: &'a [u8],
}

/// An incoming RTSP response to something the sink sent.
#[derive(Debug, Clone, Copy)]
pub struct WfdResponse<'a> {
    /// The status code.
    pub status: u16,
    /// Headers in arrival order.
    pub headers: &'a [(String, String)],
    /// The body.
    pub body: &'a [u8],
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

/// A response the sink must write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingResponse {
    /// The status to send.
    pub status: u16,
    /// The `CSeq` being answered.
    pub cseq: u32,
    /// Extra headers beyond `CSeq`.
    pub headers: Vec<(&'static str, String)>,
    /// The body, if any.
    pub body: Vec<u8>,
}

/// A request the sink must write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingRequest {
    /// The method.
    pub method: &'static str,
    /// The request-URI.
    pub uri: String,
    /// The sink's own `CSeq` — a counter independent of the source's.
    pub cseq: u32,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
    /// The body, if any.
    pub body: Vec<u8>,
}

/// What the session wants done next.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SinkOutput {
    /// Write a response.
    Respond(OutgoingResponse),
    /// Write a request.
    Send(OutgoingRequest),
    /// The stream is live: bind the media plane and start decoding.
    MediaStarted(Box<NegotiatedConfig>),
    /// The stream stopped but the session survives (M9 `PAUSE`).
    MediaStopped,
    /// The session is over.
    Ended,
}

/// Which sink-originated request a response belongs to.
///
/// Tracked by `CSeq` rather than by "the last thing we sent", because responses interleave
/// with the source's own requests — a sink that assumed the next message it read was its
/// answer would mistake an M16 keep-alive for a `SETUP` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Options,
    Setup,
    Play,
    Pause,
    Teardown,
    IdrRequest,
}

/// What the M4 exchange settled.
///
/// Private fields on purpose. The only way to obtain one is [`WfdSession::on_request`]'s
/// M4 arm, which builds it from the intersection of what the sink advertised and what the
/// source chose — so a media plane cannot be started for a format nobody negotiated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedConfig {
    video: NegotiatedVideo,
    audio: Option<AudioCodecEntry>,
    presentation_url: String,
    rtp_port: u16,
    uibc_port: Option<u16>,
}

impl NegotiatedConfig {
    /// The video format the source will encode to.
    #[must_use]
    pub const fn video(&self) -> NegotiatedVideo {
        self.video
    }

    /// The picture's dimensions and rate.
    #[must_use]
    pub fn mode(&self) -> VideoMode {
        self.video.mode()
    }

    /// The H.264 profile the source will use.
    #[must_use]
    pub const fn profile(&self) -> Profile {
        self.video.profile
    }

    /// The audio format, if the source chose one.
    #[must_use]
    pub const fn audio(&self) -> Option<AudioCodecEntry> {
        self.audio
    }

    /// The sample rate and channel count the audio decoder needs.
    ///
    /// Derived from the single mode bit the source set, because nothing in the transport
    /// stream states it: LPCM carries no in-band rate at all, and reading it back out of
    /// an ADTS header would only work for AAC.
    #[must_use]
    pub fn audio_format(&self) -> Option<(u32, u16)> {
        let entry = self.audio?;
        let bit = u8::try_from(entry.modes.trailing_zeros()).ok()?;
        entry.format.mode(bit)
    }

    /// The URL to use as the request-URI for M6–M9.
    ///
    /// Verbatim from M4. Reconstructing it is wrong: the source puts its own address here
    /// while every other request-URI in the session is the literal `rtsp://localhost`, and
    /// some sources cross-check.
    #[must_use]
    pub fn presentation_url(&self) -> &str {
        &self.presentation_url
    }

    /// The UDP port the sink will receive RTP on.
    #[must_use]
    pub const fn rtp_port(&self) -> u16 {
        self.rtp_port
    }

    /// The source's UIBC port, if it opened one.
    #[must_use]
    pub const fn uibc_port(&self) -> Option<u16> {
        self.uibc_port
    }
}

/// Where the session is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionState {
    /// Connected; the source has not yet said anything.
    AwaitingM1,
    /// M1 answered and M2 sent; waiting for the capability query.
    Handshaking,
    /// M3 answered; waiting for the source to state its choice.
    AwaitingConfiguration,
    /// M4 landed. Everything needed to receive media is known; nothing is bound yet.
    Configured(Box<NegotiatedConfig>),
    /// M6 sent, waiting for the source's `Session:`.
    SettingUp(Box<NegotiatedConfig>),
    /// The source gave us a session id. M7 is out.
    Ready(Box<NegotiatedConfig>),
    /// Media is flowing.
    Playing(Box<NegotiatedConfig>),
    /// Paused by M9; the session and its id survive.
    Paused(Box<NegotiatedConfig>),
    /// Torn down.
    Closed,
}

impl SessionState {
    /// A short name for logs and for [`MiracastError::UnexpectedMessage`].
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::AwaitingM1 => "awaiting-m1",
            Self::Handshaking => "handshaking",
            Self::AwaitingConfiguration => "awaiting-configuration",
            Self::Configured(_) => "configured",
            Self::SettingUp(_) => "setting-up",
            Self::Ready(_) => "ready",
            Self::Playing(_) => "playing",
            Self::Paused(_) => "paused",
            Self::Closed => "closed",
        }
    }

    /// The negotiated configuration, once there is one.
    #[must_use]
    pub fn config(&self) -> Option<&NegotiatedConfig> {
        match self {
            Self::Configured(c)
            | Self::SettingUp(c)
            | Self::Ready(c)
            | Self::Playing(c)
            | Self::Paused(c) => Some(c),
            Self::AwaitingM1 | Self::Handshaking | Self::AwaitingConfiguration | Self::Closed => {
                None
            }
        }
    }
}

/// The sink's half of a WFD RTSP session.
#[derive(Debug)]
pub struct WfdSession {
    caps: SinkCapabilities,
    state: SessionState,
    /// Our own request counter. Independent of the source's, and both sides start at 1 —
    /// lazycast sends `CSeq: 1` in both directions at once and Windows accepts it.
    next_cseq: u32,
    pending: HashMap<u32, Pending>,
    /// The session id from the M6 response, with any `;timeout=` stripped.
    session_id: Option<String>,
    /// What the source's `Session:` asked for, in seconds.
    session_timeout: Option<u32>,
    /// The source's `Server:` header. `MSMiracastSource/...` is the reliable way to know
    /// we are talking to Windows, and it arrives on the M2 response — early enough to
    /// switch behaviour on.
    source_server: Option<String>,
    /// The `Public:` the source advertised.
    source_methods: Option<String>,
}

impl WfdSession {
    /// A session that has just connected to a source.
    #[must_use]
    pub fn new(caps: SinkCapabilities) -> Self {
        Self {
            caps,
            state: SessionState::AwaitingM1,
            next_cseq: 1,
            pending: HashMap::new(),
            session_id: None,
            session_timeout: None,
            source_server: None,
            source_methods: None,
        }
    }

    /// Where the session is.
    #[must_use]
    pub const fn state(&self) -> &SessionState {
        &self.state
    }

    /// The negotiated configuration, once M4 has landed.
    #[must_use]
    pub fn config(&self) -> Option<&NegotiatedConfig> {
        self.state.config()
    }

    /// The source's `Server:` header, if it sent one.
    #[must_use]
    pub fn source_server(&self) -> Option<&str> {
        self.source_server.as_deref()
    }

    /// Whether the peer identified itself as Windows.
    ///
    /// [MS-WFDPE] specifies the `Server:` value exactly, so this is a fact rather than a
    /// guess — and it is the switch for every Windows-specific behaviour we grow later.
    #[must_use]
    pub fn source_is_windows(&self) -> bool {
        self.source_server
            .as_deref()
            .is_some_and(|s| s.starts_with("MSMiracastSource"))
    }

    /// How long the source wants between keep-alives, if it said.
    #[must_use]
    pub const fn session_timeout_secs(&self) -> Option<u32> {
        self.session_timeout
    }

    fn take_cseq(&mut self) -> u32 {
        let cseq = self.next_cseq;
        self.next_cseq = self.next_cseq.saturating_add(1);
        cseq
    }

    fn request(&mut self, method: &'static str, uri: String, pending: Pending) -> OutgoingRequest {
        let cseq = self.take_cseq();
        self.pending.insert(cseq, pending);
        let mut headers = Vec::new();
        // Every sink-originated request inside a session must echo the id; before M6
        // there is none to echo.
        if let Some(id) = &self.session_id {
            headers.push(("Session", id.clone()));
        }
        OutgoingRequest {
            method,
            uri,
            cseq,
            headers,
            body: Vec::new(),
        }
    }

    /// Handle a request from the source. Returns what to write.
    ///
    /// # Errors
    /// [`MiracastError`] if the message cannot be handled in the current state, or if a
    /// body it must understand is malformed.
    pub fn on_request(&mut self, req: &WfdRequest<'_>) -> Result<Vec<SinkOutput>, MiracastError> {
        let cseq = header(req.headers, "CSeq")
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| MiracastError::UnexpectedMessage {
                method: format!("{} without CSeq", req.method),
                state: self.state.name(),
            })?;
        match req.method.to_ascii_uppercase().as_str() {
            "OPTIONS" => Ok(self.on_options(cseq)),
            "GET_PARAMETER" => self.on_get_parameter(cseq, req.body),
            "SET_PARAMETER" => self.on_set_parameter(cseq, req.body),
            "TEARDOWN" => {
                // Either side may send M8. When the source does, the session is over the
                // moment we have answered.
                self.state = SessionState::Closed;
                Ok(vec![SinkOutput::Respond(self.ok(cseq)), SinkOutput::Ended])
            }
            other => Err(MiracastError::UnexpectedMessage {
                method: other.to_owned(),
                state: self.state.name(),
            }),
        }
    }

    fn ok(&self, cseq: u32) -> OutgoingResponse {
        OutgoingResponse {
            status: 200,
            cseq,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// M1: answer with our method set, then immediately probe the source with M2.
    fn on_options(&mut self, cseq: u32) -> Vec<SinkOutput> {
        let mut out = vec![SinkOutput::Respond(OutgoingResponse {
            status: 200,
            cseq,
            headers: vec![("Public", SINK_PUBLIC.to_owned())],
            body: Vec::new(),
        })];
        // Only the first OPTIONS starts the handshake. A source that probes again
        // mid-session gets the same answer and no second M2.
        if matches!(self.state, SessionState::AwaitingM1) {
            let mut m2 = self.request("OPTIONS", "*".to_owned(), Pending::Options);
            m2.headers.push(("Require", WFD_FEATURE.to_owned()));
            out.push(SinkOutput::Send(m2));
            self.state = SessionState::Handshaking;
        }
        out
    }

    /// M3 (a body of names) or M16 (an empty body, the keep-alive).
    fn on_get_parameter(
        &mut self,
        cseq: u32,
        body: &[u8],
    ) -> Result<Vec<SinkOutput>, MiracastError> {
        if body.iter().all(u8::is_ascii_whitespace) {
            // M16. Answering is the whole obligation; the session does not move.
            debug!("keep-alive");
            return Ok(vec![SinkOutput::Respond(self.ok(cseq))]);
        }
        let requested = ParamBody::parse(body)?.requested_names();
        let answer = self.caps.respond_to(&requested);
        info!(requested = requested.len(), "answered the capability query");
        if matches!(self.state, SessionState::Handshaking) {
            self.state = SessionState::AwaitingConfiguration;
        }
        Ok(vec![SinkOutput::Respond(OutgoingResponse {
            status: 200,
            cseq,
            headers: vec![("Content-Type", "text/parameters".to_owned())],
            body: answer,
        })])
    }

    /// M4, M5, and the mid-session M10–M12/M14/M15.
    fn on_set_parameter(
        &mut self,
        cseq: u32,
        body: &[u8],
    ) -> Result<Vec<SinkOutput>, MiracastError> {
        let parsed = ParamBody::parse(body)?;

        // M5 is the only one that makes the sink act, so it is checked first — a body
        // carrying a trigger carries nothing else.
        if let Some(value) = parsed.value(&ParamName::TriggerMethod) {
            let trigger = TriggerMethod::parse(value).ok_or(MiracastError::UnexpectedMessage {
                method: format!("wfd_trigger_method: {value}"),
                state: self.state.name(),
            })?;
            return self.on_trigger(cseq, trigger);
        }

        // M14/M15 can arrive at any point; capture the port and acknowledge.
        let uibc_port = parsed
            .value(&ParamName::UibcCapability)
            .and_then(parse_uibc_port);

        // M4 is recognised by carrying the presentation URL — the one parameter only it
        // sends, and the one without which no later message can be addressed.
        if parsed.contains(&ParamName::PresentationUrl) {
            let config = self.negotiate(&parsed, uibc_port)?;
            info!(
                mode = %config.mode(),
                profile = %config.profile(),
                rtp_port = config.rtp_port(),
                "negotiated"
            );
            self.state = SessionState::Configured(Box::new(config));
            return Ok(vec![SinkOutput::Respond(self.ok(cseq))]);
        }

        if let Some(port) = uibc_port {
            debug!(port, "source opened a UIBC channel");
        }
        // wfd_route, wfd_connector_type, wfd_standby, wfd_uibc_setting: a single-output
        // sink acknowledges and carries on. Answering anything else here would end a
        // session over a parameter that changes nothing for us.
        Ok(vec![SinkOutput::Respond(self.ok(cseq))])
    }

    /// Build the negotiated configuration from the source's M4.
    fn negotiate(
        &self,
        m4: &ParamBody,
        uibc_port: Option<u16>,
    ) -> Result<NegotiatedConfig, MiracastError> {
        let urls =
            PresentationUrls::parse(m4.value(&ParamName::PresentationUrl).unwrap_or_default())?;
        let presentation_url = urls.url0.ok_or(MiracastError::UnexpectedMessage {
            method: "M4 with wfd_presentation_URL: none".to_owned(),
            state: self.state.name(),
        })?;

        let chosen = VideoFormats::parse(m4.value(&ParamName::VideoFormats).unwrap_or("none"))?;
        let codec = chosen
            .codecs
            .first()
            .ok_or(MiracastError::NoCommonVideoFormat)?;
        // M4 sets exactly one bit across the three masks. Real sources honour that; a
        // source that sets several is answered by taking the one it would have scored
        // highest, rather than by failing a session over an ambiguity.
        let index = ResolutionTable::all()
            .into_iter()
            .flat_map(|t| codec.mask(t).modes())
            .max_by_key(|(_, mode)| mode.score())
            .map(|(index, _)| index)
            .ok_or(MiracastError::NoCommonVideoFormat)?;
        let profile = codec
            .profiles
            .lowest()
            .ok_or(MiracastError::NoCommonVideoFormat)?;
        let level_idc = codec
            .levels
            .lowest()
            .ok_or(MiracastError::NoCommonVideoFormat)?;
        let video = NegotiatedVideo {
            index,
            profile,
            level_idc,
        };
        // The intersection check that makes this constructor meaningful. A conforming
        // source really can land here — see `pick_best_format` — so this is a live path,
        // not a paranoia check.
        if !video.sink_can_decode(&self.caps.video_formats) {
            return Err(MiracastError::UnadvertisedFormat(format!(
                "{} {} level {}",
                index.mode(),
                profile,
                f32::from(level_idc) / 10.0
            )));
        }

        let audio = match m4.value(&ParamName::AudioCodecs) {
            Some(value) => {
                let codecs = crate::params::AudioCodecs::parse(value)?;
                // A source may list a codec with an all-zero mask; that is not a choice.
                codecs.0.into_iter().find(|e| e.supports_anything())
            }
            None => None,
        };
        if audio.is_none() {
            // Not fatal: a source may mirror silently, and a session with a picture and
            // no sound beats no session.
            warn!("the source chose no audio format");
        }

        // The source echoes our own port back. Trusting its echo rather than our
        // advertisement is deliberate: some sources cross-check the two, and if they have
        // diverged the source's value is the one the RTP will actually go to.
        let rtp_port = match m4.value(&ParamName::ClientRtpPorts) {
            Some(value) => ClientRtpPorts::parse(value)?.port(),
            None => self.caps.client_rtp_ports.port(),
        };

        Ok(NegotiatedConfig {
            video,
            audio,
            presentation_url,
            rtp_port,
            uibc_port,
        })
    }

    /// M5: acknowledge, then issue the request the trigger names.
    ///
    /// The `200 OK` is *not* the action. A sink that answers and stops looks to the user
    /// like a device that connected and then did nothing.
    fn on_trigger(
        &mut self,
        cseq: u32,
        trigger: TriggerMethod,
    ) -> Result<Vec<SinkOutput>, MiracastError> {
        let mut out = vec![SinkOutput::Respond(self.ok(cseq))];
        let url = self
            .state
            .config()
            .map(|c| c.presentation_url().to_owned())
            .ok_or(MiracastError::UnexpectedMessage {
                method: format!("wfd_trigger_method: {trigger} before M4"),
                state: self.state.name(),
            })?;
        match trigger {
            TriggerMethod::Setup => {
                let mut m6 = self.request("SETUP", url, Pending::Setup);
                let port = self.state.config().map_or(
                    self.caps.client_rtp_ports.port(),
                    NegotiatedConfig::rtp_port,
                );
                // One port, not a range. AOSP emits the `-`-range form only when the sink
                // gave two, and the WFD profile has no RTCP port to give.
                m6.headers.push((
                    "Transport",
                    format!("RTP/AVP/UDP;unicast;client_port={port}"),
                ));
                if let SessionState::Configured(config) =
                    std::mem::replace(&mut self.state, SessionState::Closed)
                {
                    self.state = SessionState::SettingUp(config);
                }
                out.push(SinkOutput::Send(m6));
            }
            TriggerMethod::Play => {
                out.push(SinkOutput::Send(self.request("PLAY", url, Pending::Play)));
            }
            TriggerMethod::Pause => {
                out.push(SinkOutput::Send(self.request("PAUSE", url, Pending::Pause)));
            }
            TriggerMethod::Teardown => {
                out.push(SinkOutput::Send(self.request(
                    "TEARDOWN",
                    url,
                    Pending::Teardown,
                )));
            }
        }
        Ok(out)
    }

    /// Handle a response to something the sink sent.
    ///
    /// # Errors
    /// [`MiracastError`] if the response has no usable `CSeq`, or if the source refused a
    /// request the session cannot continue without.
    pub fn on_response(
        &mut self,
        resp: &WfdResponse<'_>,
    ) -> Result<Vec<SinkOutput>, MiracastError> {
        let cseq = header(resp.headers, "CSeq").and_then(|v| v.parse::<u32>().ok());
        let Some(pending) = cseq.and_then(|c| self.pending.remove(&c)) else {
            // Not ours, or a duplicate. Dropping it is right: the alternative is guessing
            // which of our outstanding requests it answered.
            debug!(status = resp.status, "ignoring an uncorrelated response");
            return Ok(Vec::new());
        };
        if resp.status != 200 {
            return Err(MiracastError::UnexpectedMessage {
                method: format!("{pending:?} refused with {}", resp.status),
                state: self.state.name(),
            });
        }
        match pending {
            Pending::Options => {
                self.source_server = header(resp.headers, "Server").map(ToOwned::to_owned);
                self.source_methods = header(resp.headers, "Public").map(ToOwned::to_owned);
                if let Some(server) = &self.source_server {
                    info!(
                        server,
                        windows = self.source_is_windows(),
                        "source identified"
                    );
                }
                Ok(Vec::new())
            }
            Pending::Setup => self.on_setup_response(resp),
            Pending::Play => {
                let config = match std::mem::replace(&mut self.state, SessionState::Closed) {
                    SessionState::Ready(c) | SessionState::Paused(c) | SessionState::Playing(c) => {
                        c
                    }
                    other => {
                        self.state = other;
                        return Ok(Vec::new());
                    }
                };
                self.state = SessionState::Playing(config.clone());
                Ok(vec![SinkOutput::MediaStarted(config)])
            }
            Pending::Pause => {
                if let SessionState::Playing(c) =
                    std::mem::replace(&mut self.state, SessionState::Closed)
                {
                    self.state = SessionState::Paused(c);
                }
                Ok(vec![SinkOutput::MediaStopped])
            }
            Pending::Teardown => {
                self.state = SessionState::Closed;
                Ok(vec![SinkOutput::Ended])
            }
            // M13 needs no answer beyond "it was accepted"; the IDR arrives in the stream.
            Pending::IdrRequest => Ok(Vec::new()),
        }
    }

    fn on_setup_response(
        &mut self,
        resp: &WfdResponse<'_>,
    ) -> Result<Vec<SinkOutput>, MiracastError> {
        // `Session: 1804289383;timeout=30` — everything from the first `;` is parameters,
        // and echoing them back with the id is what MiracleCast is careful not to do.
        if let Some(raw) = header(resp.headers, "Session") {
            let (id, params) = raw.split_once(';').unwrap_or((raw, ""));
            self.session_id = Some(id.trim().to_owned());
            self.session_timeout = params
                .split(';')
                .find_map(|p| p.trim().strip_prefix("timeout="))
                .and_then(|t| t.trim().parse::<u32>().ok());
        } else {
            // AOSP carries a comment that older dongles omit this and falls back to the
            // one session it knows about. Being equally tolerant costs nothing: we have
            // exactly one session too.
            warn!("the source's SETUP response carried no Session header");
        }
        let config = match std::mem::replace(&mut self.state, SessionState::Closed) {
            SessionState::SettingUp(c) => c,
            other => {
                self.state = other;
                return Ok(Vec::new());
            }
        };
        let url = config.presentation_url().to_owned();
        self.state = SessionState::Ready(config);
        // PLAY follows SETUP without waiting for a second trigger — that is what both
        // working sinks do, and what the transcript in the notes shows.
        Ok(vec![SinkOutput::Send(self.request(
            "PLAY",
            url,
            Pending::Play,
        ))])
    }

    /// Ask the source for an IDR picture (M13).
    ///
    /// The sink's only recovery from a lost reference frame. Returns `None` unless media
    /// is actually flowing, because there is nothing to recover otherwise — and it should
    /// be rate-limited by the caller: Windows honours these, and spamming them collapses
    /// the bitrate.
    #[must_use]
    pub fn request_idr(&mut self) -> Option<SinkOutput> {
        if !matches!(self.state, SessionState::Playing(_)) {
            return None;
        }
        let mut m13 = self.request("SET_PARAMETER", CONTROL_URI.to_owned(), Pending::IdrRequest);
        m13.headers
            .push(("Content-Type", "text/parameters".to_owned()));
        // A bare name and a CRLF, and the CRLF is not decoration: AOSP detects M13 by
        // substring-matching `"wfd_idr_request\r\n"`.
        m13.body = render_body(&[(ParamName::IdrRequest, None)]);
        Some(SinkOutput::Send(m13))
    }

    /// End the session from our side (M8).
    #[must_use]
    pub fn teardown(&mut self) -> Option<SinkOutput> {
        let url = self.state.config()?.presentation_url().to_owned();
        Some(SinkOutput::Send(self.request(
            "TEARDOWN",
            url,
            Pending::Teardown,
        )))
    }
}

/// Pull `port=<n>` out of a `wfd_uibc_capability` value.
///
/// `port=none` is what the *sink* sends — it is saying what it can send, not where — so
/// only a numeric port from the source means a channel is open.
fn parse_uibc_port(value: &str) -> Option<u16> {
    value
        .split(';')
        .find_map(|f| f.trim().strip_prefix("port="))
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|p| *p != 0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::params::{AudioCodecs, ConnectorType, ContentProtection, RtpProfile};

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

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn request<'a>(
        method: &'a str,
        uri: &'a str,
        hdrs: &'a [(String, String)],
        body: &'a [u8],
    ) -> WfdRequest<'a> {
        WfdRequest {
            method,
            uri,
            headers: hdrs,
            body,
        }
    }

    fn only_response(out: &[SinkOutput]) -> &OutgoingResponse {
        out.iter()
            .find_map(|o| match o {
                SinkOutput::Respond(r) => Some(r),
                _ => None,
            })
            .expect("a response")
    }

    fn sent_request(out: &[SinkOutput]) -> &OutgoingRequest {
        out.iter()
            .find_map(|o| match o {
                SinkOutput::Send(r) => Some(r),
                _ => None,
            })
            .expect("a request")
    }

    /// The M4 body Windows really sends, from the notes' §2.5 transcript, adjusted to a
    /// mode this sink advertises.
    const M4_BODY: &[u8] =
        b"wfd_video_formats: 00 00 03 10 00000100 00000000 00000000 00 0000 0000 00 none none\r\n\
        wfd_audio_codecs: AAC 00000001 00\r\n\
        wfd_presentation_URL: rtsp://192.168.173.1/wfd1.0/streamid=0 none\r\n\
        wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play\r\n";

    /// Walk the whole M1→M7 handshake, asserting each step, and return the session.
    fn handshake() -> WfdSession {
        let mut s = WfdSession::new(caps());

        // M1. The response carries our method set; M2 goes out on its own counter.
        let h = headers(&[("CSeq", "1"), ("Require", WFD_FEATURE)]);
        let out = s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        assert_eq!(only_response(&out).cseq, 1);
        assert_eq!(
            only_response(&out).headers,
            vec![("Public", SINK_PUBLIC.to_owned())]
        );
        let m2 = sent_request(&out);
        assert_eq!(m2.method, "OPTIONS");
        assert_eq!(m2.uri, "*");
        assert_eq!(m2.cseq, 1, "our counter is independent of the source's");
        assert!(m2.headers.contains(&("Require", WFD_FEATURE.to_owned())));

        // The M2 response identifies the source.
        let h = headers(&[
            ("CSeq", "1"),
            (
                "Server",
                "MSMiracastSource/10.00.10011.0000 guid/be113d06-9e40-43e4-98e6-540a325e9ced",
            ),
            (
                "Public",
                "org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER",
            ),
        ]);
        let out = s
            .on_response(&WfdResponse {
                status: 200,
                headers: &h,
                body: b"",
            })
            .unwrap();
        assert!(out.is_empty());
        assert!(s.source_is_windows());

        // M3.
        let h = headers(&[("CSeq", "2")]);
        let out = s
            .on_request(&request(
                "GET_PARAMETER",
                CONTROL_URI,
                &h,
                b"wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n",
            ))
            .unwrap();
        let body = String::from_utf8(only_response(&out).body.clone()).unwrap();
        assert!(body.contains("wfd_video_formats: 00 00 03 10"));
        assert!(body.contains("wfd_client_rtp_ports: RTP/AVP/UDP;unicast 1028 0 mode=play"));

        // M4.
        let h = headers(&[("CSeq", "3")]);
        let out = s
            .on_request(&request("SET_PARAMETER", CONTROL_URI, &h, M4_BODY))
            .unwrap();
        assert_eq!(only_response(&out).status, 200);
        assert!(only_response(&out).body.is_empty());
        let config = s.config().expect("M4 settled a configuration");
        assert_eq!(config.mode(), VideoMode::new(1920, 1080, 60, false));
        assert_eq!(
            config.presentation_url(),
            "rtsp://192.168.173.1/wfd1.0/streamid=0"
        );
        assert_eq!(config.rtp_port(), 1028);
        assert_eq!(config.audio_format(), Some((48_000, 2)));

        // M5 → M6.
        let h = headers(&[("CSeq", "4")]);
        let out = s
            .on_request(&request(
                "SET_PARAMETER",
                CONTROL_URI,
                &h,
                b"wfd_trigger_method: SETUP\r\n",
            ))
            .unwrap();
        let m6 = sent_request(&out);
        assert_eq!(m6.method, "SETUP");
        assert_eq!(m6.uri, "rtsp://192.168.173.1/wfd1.0/streamid=0");
        assert_eq!(
            m6.headers,
            vec![(
                "Transport",
                "RTP/AVP/UDP;unicast;client_port=1028".to_owned()
            )]
        );

        // The M6 response, and the M7 that follows without a second trigger.
        let h = headers(&[
            ("CSeq", "2"),
            ("Session", "1804289383;timeout=30"),
            (
                "Transport",
                "RTP/AVP/UDP;unicast;client_port=1028;server_port=19000",
            ),
        ]);
        let out = s
            .on_response(&WfdResponse {
                status: 200,
                headers: &h,
                body: b"",
            })
            .unwrap();
        let m7 = sent_request(&out);
        assert_eq!(m7.method, "PLAY");
        assert_eq!(
            m7.headers,
            vec![("Session", "1804289383".to_owned())],
            "the id is echoed without its parameters"
        );
        assert_eq!(s.session_timeout_secs(), Some(30));

        // The M7 response starts the media plane.
        let h = headers(&[("CSeq", "3"), ("Session", "1804289383")]);
        let out = s
            .on_response(&WfdResponse {
                status: 200,
                headers: &h,
                body: b"",
            })
            .unwrap();
        assert!(matches!(out.as_slice(), [SinkOutput::MediaStarted(_)]));
        assert!(matches!(s.state(), SessionState::Playing(_)));
        s
    }

    #[test]
    fn the_whole_handshake_walks_m1_to_m7() {
        handshake();
    }

    #[test]
    fn the_two_cseq_counters_are_independent() {
        // The source is on CSeq 4 by the time we send our second request on CSeq 2.
        // Sharing one counter is the classic break, and it produces responses that
        // correlate to the wrong request rather than an error.
        let s = handshake();
        assert_eq!(s.next_cseq, 4, "M2, M6, M7 — ours alone");
    }

    #[test]
    fn a_keep_alive_is_answered_and_changes_nothing() {
        let mut s = handshake();
        let before = s.state().name();
        let h = headers(&[("CSeq", "5"), ("Session", "1804289383")]);
        let out = s
            .on_request(&request("GET_PARAMETER", CONTROL_URI, &h, b""))
            .unwrap();
        assert_eq!(only_response(&out).status, 200);
        assert!(only_response(&out).body.is_empty());
        assert_eq!(s.state().name(), before);
    }

    #[test]
    fn an_interleaved_keep_alive_is_not_mistaken_for_a_setup_response() {
        // Requests and responses interleave freely, so a sink that assumed the next
        // message after SETUP was its answer would take the M16 for one.
        let mut s = WfdSession::new(caps());
        let h = headers(&[("CSeq", "1")]);
        s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        let h = headers(&[("CSeq", "2")]);
        s.on_request(&request(
            "GET_PARAMETER",
            CONTROL_URI,
            &h,
            b"wfd_video_formats\r\n",
        ))
        .unwrap();
        let h = headers(&[("CSeq", "3")]);
        s.on_request(&request("SET_PARAMETER", CONTROL_URI, &h, M4_BODY))
            .unwrap();
        let h = headers(&[("CSeq", "4")]);
        s.on_request(&request(
            "SET_PARAMETER",
            CONTROL_URI,
            &h,
            b"wfd_trigger_method: SETUP\r\n",
        ))
        .unwrap();
        assert!(matches!(s.state(), SessionState::SettingUp(_)));

        // An M16 arrives while SETUP is outstanding.
        let h = headers(&[("CSeq", "5")]);
        let out = s
            .on_request(&request("GET_PARAMETER", CONTROL_URI, &h, b""))
            .unwrap();
        assert_eq!(only_response(&out).cseq, 5);
        assert!(
            matches!(s.state(), SessionState::SettingUp(_)),
            "the keep-alive must not advance the session"
        );
    }

    #[test]
    fn a_response_with_an_unknown_cseq_is_dropped_not_guessed() {
        let mut s = handshake();
        let h = headers(&[("CSeq", "999")]);
        let out = s
            .on_response(&WfdResponse {
                status: 200,
                headers: &h,
                body: b"",
            })
            .unwrap();
        assert!(out.is_empty());
        assert!(matches!(s.state(), SessionState::Playing(_)));
    }

    #[test]
    fn the_session_id_is_echoed_without_its_parameters() {
        // `Session: 1804289383;timeout=30` echoed whole is what MiracleCast is careful
        // not to do, and some sources reject it.
        let mut s = handshake();
        let idr = s.request_idr().expect("media is flowing");
        let SinkOutput::Send(req) = idr else {
            panic!("expected a request")
        };
        assert!(req.headers.contains(&("Session", "1804289383".to_owned())));
    }

    #[test]
    fn the_idr_request_is_a_bare_name_and_a_crlf() {
        // AOSP substring-matches "wfd_idr_request\r\n", so a colon or a missing
        // terminator means the source never sees the request at all.
        let mut s = handshake();
        let SinkOutput::Send(req) = s.request_idr().unwrap() else {
            panic!("expected a request")
        };
        assert_eq!(req.method, "SET_PARAMETER");
        assert_eq!(req.uri, CONTROL_URI);
        assert_eq!(req.body, b"wfd_idr_request\r\n");
    }

    #[test]
    fn an_idr_request_before_playback_is_refused() {
        let mut s = WfdSession::new(caps());
        assert!(s.request_idr().is_none());
    }

    #[test]
    fn a_setup_response_without_a_session_header_still_proceeds() {
        // AOSP carries a comment that older dongles omit it. Failing here would refuse a
        // session that works.
        let mut s = WfdSession::new(caps());
        let h = headers(&[("CSeq", "1")]);
        s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        let h = headers(&[("CSeq", "2")]);
        s.on_request(&request("SET_PARAMETER", CONTROL_URI, &h, M4_BODY))
            .unwrap();
        let h = headers(&[("CSeq", "3")]);
        s.on_request(&request(
            "SET_PARAMETER",
            CONTROL_URI,
            &h,
            b"wfd_trigger_method: SETUP\r\n",
        ))
        .unwrap();
        let h = headers(&[("CSeq", "2")]);
        let out = s
            .on_response(&WfdResponse {
                status: 200,
                headers: &h,
                body: b"",
            })
            .unwrap();
        assert_eq!(sent_request(&out).method, "PLAY");
        assert!(matches!(s.state(), SessionState::Ready(_)));
    }

    #[test]
    fn a_format_we_never_advertised_is_refused_with_what_it_was() {
        // A conforming source can land here: AOSP takes the lower of the two sides'
        // *floors* for profile and level rather than an intersection.
        let mut s = WfdSession::new(caps());
        let h = headers(&[("CSeq", "1")]);
        s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        // VESA bit 28 = 1920x1200p30, which this sink's mask does claim — so pick
        // something it does not: level 5.0, outside the R1 range we advertise.
        let body = b"wfd_video_formats: 00 00 03 20 00000100 00000000 00000000 00 0000 0000 00 none none\r\n\
            wfd_presentation_URL: rtsp://192.168.1.1/wfd1.0/streamid=0 none\r\n";
        let h = headers(&[("CSeq", "2")]);
        let err = s
            .on_request(&request("SET_PARAMETER", CONTROL_URI, &h, body))
            .unwrap_err();
        match err {
            MiracastError::UnadvertisedFormat(what) => {
                assert!(what.contains("1920x1080p60"), "{what}");
                assert!(what.contains("level 5"), "{what}");
            }
            other => panic!("expected UnadvertisedFormat, got {other}"),
        }
    }

    #[test]
    fn a_trigger_before_m4_cannot_address_a_request() {
        // There is no presentation URL yet, and reconstructing one is exactly what the
        // notes say not to do.
        let mut s = WfdSession::new(caps());
        let h = headers(&[("CSeq", "1")]);
        s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        let h = headers(&[("CSeq", "2")]);
        let err = s
            .on_request(&request(
                "SET_PARAMETER",
                CONTROL_URI,
                &h,
                b"wfd_trigger_method: SETUP\r\n",
            ))
            .unwrap_err();
        assert!(matches!(err, MiracastError::UnexpectedMessage { .. }));
    }

    #[test]
    fn pause_stops_the_media_but_keeps_the_session() {
        let mut s = handshake();
        let h = headers(&[("CSeq", "9")]);
        let out = s
            .on_request(&request(
                "SET_PARAMETER",
                CONTROL_URI,
                &h,
                b"wfd_trigger_method: PAUSE\r\n",
            ))
            .unwrap();
        assert_eq!(sent_request(&out).method, "PAUSE");
        let h = headers(&[("CSeq", "4")]);
        let out = s
            .on_response(&WfdResponse {
                status: 200,
                headers: &h,
                body: b"",
            })
            .unwrap();
        assert_eq!(out, vec![SinkOutput::MediaStopped]);
        assert!(matches!(s.state(), SessionState::Paused(_)));
        assert!(s.config().is_some(), "the negotiation survives a pause");
    }

    #[test]
    fn a_teardown_from_the_source_ends_the_session_after_the_answer() {
        let mut s = handshake();
        let h = headers(&[("CSeq", "9"), ("Session", "1804289383")]);
        let out = s
            .on_request(&request(
                "TEARDOWN",
                "rtsp://192.168.173.1/wfd1.0/streamid=0",
                &h,
                b"",
            ))
            .unwrap();
        assert_eq!(only_response(&out).status, 200);
        assert!(out.contains(&SinkOutput::Ended));
        assert_eq!(s.state(), &SessionState::Closed);
    }

    #[test]
    fn the_uibc_port_comes_from_the_source_not_from_us() {
        // The sink sends `port=none` — it is saying what it can send, not where. Only a
        // numeric port from the source opens a channel.
        assert_eq!(
            parse_uibc_port("input_category_list=GENERIC;port=none"),
            None
        );
        assert_eq!(
            parse_uibc_port("input_category_list=GENERIC;generic_cap_list=Mouse;port=7239"),
            Some(7239)
        );
        let mut s = WfdSession::new(caps());
        let h = headers(&[("CSeq", "1")]);
        s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        let mut body = M4_BODY.to_vec();
        body.extend_from_slice(
            b"wfd_uibc_capability: input_category_list=GENERIC;generic_cap_list=Mouse, \
              SingleTouch;hidc_cap_list=none;port=7239\r\n",
        );
        let h = headers(&[("CSeq", "2")]);
        s.on_request(&request("SET_PARAMETER", CONTROL_URI, &h, &body))
            .unwrap();
        assert_eq!(s.config().unwrap().uibc_port(), Some(7239));
    }

    #[test]
    fn mid_session_parameters_are_acknowledged_rather_than_refused() {
        // wfd_route, wfd_connector_type and wfd_standby change nothing for a
        // single-output sink; ending a session over one would be a bug that looks like a
        // dropped connection.
        let mut s = handshake();
        for body in [
            &b"wfd_route: secondary\r\n"[..],
            &b"wfd_connector_type: 05\r\n"[..],
            &b"wfd_standby\r\n"[..],
            &b"wfd_uibc_setting: disable\r\n"[..],
        ] {
            let h = headers(&[("CSeq", "20")]);
            let out = s
                .on_request(&request("SET_PARAMETER", CONTROL_URI, &h, body))
                .unwrap();
            assert_eq!(only_response(&out).status, 200);
            assert!(matches!(s.state(), SessionState::Playing(_)));
        }
    }

    #[test]
    fn a_request_without_a_cseq_is_an_error_rather_than_a_guess() {
        let mut s = WfdSession::new(caps());
        let h = headers(&[]);
        assert!(s.on_request(&request("OPTIONS", "*", &h, b"")).is_err());
    }

    #[test]
    fn a_refused_setup_is_not_silently_swallowed() {
        let mut s = WfdSession::new(caps());
        let h = headers(&[("CSeq", "1")]);
        s.on_request(&request("OPTIONS", "*", &h, b"")).unwrap();
        let h = headers(&[("CSeq", "1")]);
        let err = s
            .on_response(&WfdResponse {
                status: 500,
                headers: &h,
                body: b"",
            })
            .unwrap_err();
        assert!(matches!(err, MiracastError::UnexpectedMessage { .. }));
    }
}
