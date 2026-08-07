//! The socket shell: one TCP connection, one UDP socket, and the pure core between them.
//!
//! Everything protocol-shaped lives in [`crate::session`], [`crate::params`] and
//! [`crate::ts`]; this module owns only the I/O and the ordering (ground rule 3). Nothing
//! is parsed inside the `select!` — a read yields bytes, and the bytes are handed to the
//! core afterwards, so no protocol decision is ever made in a branch that can be cancelled.
//!
//! Two things about the shape are worth stating, because both invert the usual RTSP actor:
//!
//! - **The sink dials out.** There is no listener here. We connect to the source's port
//!   7236 once the P2P group is up (`docs/miracast-protocol-notes.md` §2.1).
//! - **The RTP socket is bound before the RTSP session starts**, not when `PLAY` succeeds.
//!   The port we advertise in M3 has to be one we are already listening on, because a
//!   source may begin sending the moment it answers M7 — and because binding late means
//!   discovering the port is taken *after* promising it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use castaway_core::{
    AudioFormat, EncodedFrame, FrameSource, MirrorAudio, SessionEvent, SessionSink,
    SourceDescription,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::MiracastError;
use crate::media::MediaReceiver;
use crate::params::SinkCapabilities;
use crate::session::{
    NegotiatedConfig, OutgoingRequest, OutgoingResponse, SinkOutput, WfdRequest, WfdResponse,
    WfdSession,
};
use crate::uibc;

/// The largest RTSP message we will buffer. A `wfd_display_edid` with two blocks is
/// ~600 bytes; anything approaching this is a peer that has lost framing, and continuing
/// to buffer would turn that into unbounded memory.
const MAX_RTSP_MESSAGE: usize = 64 * 1024;

/// Datagram buffer. A WFD source sends 7 TS packets to a datagram (1316 bytes) plus the
/// RTP header, so this is comfortably above any real packet and below any MTU games.
const DATAGRAM_BUF: usize = 2048;

/// How many frames may sit between the depacketiser and the pipeline.
///
/// Three, matching the mirroring paths already in this workspace. Ground rule 4: for a
/// live mirror a deeper queue buys nothing but latency, and a full one drops rather than
/// blocking the socket.
const FRAME_QUEUE: usize = 3;

/// The shortest gap between two IDR requests.
///
/// Not politeness. AOSP's encoder default is an IDR every *fifteen seconds*, so M13 is the
/// only thing standing between a lost reference frame and fifteen seconds of corruption —
/// but each IDR collapses the bitrate, and a real capture shows a sink firing eight
/// back-to-back and turning a lossy link into an unusable one.
const IDR_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// The session timeout to use before the source has named one, and if it names none.
///
/// RFC 2326 §12.37's default, which is also what the WFD spec and intel/wds use. AOSP
/// sends 30; a real Samsung sink sends 60. Never hard-coded past this point — the source's
/// own `Session:` value is what drives the watchdog once M6 has been answered.
const DEFAULT_SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// How much longer than the negotiated timeout a silent source gets.
///
/// The notes are emphatic about this and it is the whole reason the constant exists rather
/// than the raw timeout: *"A sink should honour whatever the source put in `Session:` and
/// should not itself time the session out aggressively — several sources send M16 late"*,
/// and *"a sink that never receives M16 should not tear down on that basis alone; Windows
/// in some configurations relies on RTP flow rather than M16"* (§2.3).
///
/// So this is not a keep-alive deadline. It is a **liveness** deadline: three whole
/// timeout periods with nothing at all arriving — no keep-alive, no RTSP, and no RTP —
/// which is a peer that is gone rather than one that is late. At AOSP's 30 s that is 90 s
/// before the panel's single-source slot is handed back (#195).
const IDLE_GRACE: u32 = 3;

/// How long the UIBC back-channel gets to answer before we give up on it.
///
/// The source has already told us the port, so a listener that is not there is a source
/// that changed its mind — and a dial that hangs would hold the RTSP loop, which is the
/// one thing keeping the mirror alive.
const UIBC_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Run one WFD session to completion.
///
/// `control` must already be connected to the source's RTSP port, and `rtp` already bound
/// to the port `caps` advertises.
///
/// # Errors
/// [`MiracastError`] if the connection fails or the source sends something the session
/// state machine cannot act on.
pub async fn run_session(
    mut control: TcpStream,
    rtp: UdpSocket,
    caps: SinkCapabilities,
    sink: SessionSink,
) -> Result<(), MiracastError> {
    let peer = control
        .peer_addr()
        .map_err(|e| MiracastError::Connection(e.to_string()))?;
    // Nagle would coalesce our M2 with whatever came next, and the M2 is what unblocks an
    // Android source — `sendM3()` is only ever called from inside its OPTIONS handler, so
    // a delayed M2 is a source that waits its full 30 s and gives up.
    let _ = control.set_nodelay(true);

    let mut session = WfdSession::new(caps);
    let mut media = MediaReceiver::new();
    let mut planes: Option<Planes> = None;
    // The mode the session negotiated, kept because a UIBC port can arrive in a later
    // M14 — after the geometry it has to map touches into is already settled.
    let mut negotiated_mode: Option<crate::video::VideoMode> = None;
    let mut inbound = Vec::new();
    let mut datagram = vec![0u8; DATAGRAM_BUF];
    let mut last_idr = tokio::time::Instant::now()
        .checked_sub(IDR_MIN_INTERVAL)
        .unwrap_or_else(tokio::time::Instant::now);
    let mut idr_requests: u64 = 0;
    let mut read_buf = vec![0u8; 8192];
    // Anything at all from the source, on either socket. Not a keep-alive clock — see
    // `IDLE_GRACE` for why the two are different things.
    let mut last_heard = tokio::time::Instant::now();

    info!(%peer, "WFD session opened");
    loop {
        // Re-read every pass: the timeout is unknown until the source's M6 response
        // carries `Session: <id>;timeout=<n>`, and it is the source's number rather than
        // ours. Before that, and if it names none, RFC 2326's default.
        let idle_deadline = last_heard
            + session
                .session_timeout_secs()
                .map_or(DEFAULT_SESSION_TIMEOUT, |secs| {
                    Duration::from_secs(u64::from(secs))
                })
                * IDLE_GRACE;

        tokio::select! {
            // A source that dies without sending FIN — a phone that walks out of range, a
            // laptop that sleeps — otherwise holds this session, the panel's single-source
            // slot and the RTP socket until someone restarts the process (#195).
            () = tokio::time::sleep_until(idle_deadline) => {
                warn!(
                    %peer,
                    timeout_secs = session.session_timeout_secs().unwrap_or(
                        DEFAULT_SESSION_TIMEOUT.as_secs().try_into().unwrap_or(u32::MAX)
                    ),
                    "the source has said nothing for {IDLE_GRACE} session timeouts; \
                     giving the slot back"
                );
                break;
            }
            // Reading bytes is all that happens in the branch; the parse is below.
            read = control.read(&mut read_buf) => {
                let n = read.map_err(|e| MiracastError::Connection(e.to_string()))?;
                if n == 0 {
                    debug!(%peer, "source closed the control connection");
                    break;
                }
                last_heard = tokio::time::Instant::now();
                inbound.extend_from_slice(&read_buf[..n]);
                if inbound.len() > MAX_RTSP_MESSAGE {
                    return Err(MiracastError::Connection(
                        "control stream exceeded the message limit without framing".to_owned(),
                    ));
                }
                // A source routinely puts M4 and M5 in one segment and splits messages
                // across segments, so this drains as many whole messages as arrived.
                while let Some((message, consumed)) = substrate_rtsp::parse(&inbound)? {
                    inbound.drain(..consumed);
                    let outputs = dispatch(&mut session, &message)?;
                    if apply(
                        &outputs,
                        &mut control,
                        &mut planes,
                        &mut negotiated_mode,
                        &sink,
                        &peer,
                    )
                    .await?
                    {
                        // The clean ending: the source triggered a TEARDOWN and we
                        // answered it. It returns from inside the loop, so the summary
                        // has to be said here as well as at the bottom — and this is the
                        // path a well-behaved session actually takes, so a report only at
                        // the bottom would be one that almost never printed.
                        report_media(&peer, &media, idr_requests);
                        return Ok(());
                    }
                }
            }
            received = rtp.recv(&mut datagram) => {
                let n = received.map_err(|e| MiracastError::Connection(e.to_string()))?;
                last_heard = tokio::time::Instant::now();
                let gaps_before = media.demux().video_gap_count();
                let frames = media.push_datagram(Bytes::copy_from_slice(&datagram[..n]));
                let lost_video = media.demux().video_gap_count() != gaps_before;
                if let Some(planes) = &mut planes {
                    for frame in frames {
                        planes.deliver(frame);
                    }
                    // Bytes of the coded video went missing, so every frame from here
                    // references data nobody has: the access unit the gap broke was
                    // dropped, and the ones after it decode into a picture that is wrong
                    // and stays wrong. Only an IDR repairs that, and against an AOSP
                    // source's fifteen-second interval, only one we ask for (D35, #192).
                    if lost_video {
                        planes.needs_keyframe = true;
                    }
                    // A frame arriving with no keyframe yet means we joined mid-GOP, and
                    // AOSP's fifteen-second IDR interval makes waiting a very long stare
                    // at a black screen.
                    if planes.needs_keyframe && last_idr.elapsed() >= IDR_MIN_INTERVAL {
                        // Written directly rather than through `apply`: an IDR request is
                        // always a request, and routing it through the media-transition
                        // handler would need the planes to be moved out from under the
                        // loop that is using them.
                        if let Some(SinkOutput::Send(request)) = session.request_idr() {
                            last_idr = tokio::time::Instant::now();
                            idr_requests = idr_requests.saturating_add(1);
                            debug!(idr_requests, "asking the source for an IDR");
                            write_request(&mut control, &request).await?;
                        }
                    }
                }
            }
        }
    }
    if let Some(planes) = &mut planes {
        for frame in media.flush() {
            planes.deliver(frame);
        }
    }
    report_media(&peer, &media, idr_requests);
    let _ = sink.emit(SessionEvent::End).await;
    Ok(())
}

/// What the link actually did, once, at the end.
///
/// Every one of these numbers existed and none of them was ever said out loud, so a mirror
/// that looked bad in the field left nothing behind to tell a lossy radio from a slow
/// decoder from a source sending to the wrong port — three problems with three different
/// owners (#192).
fn report_media(peer: &SocketAddr, media: &MediaReceiver, idr_requests: u64) {
    info!(
        %peer,
        lost = media.lost_datagrams(),
        late = media.late_datagrams(),
        foreign = media.foreign_datagrams(),
        resyncs = media.demux().resync_count(),
        video_gaps = media.demux().video_gap_count(),
        idr_requests,
        "miracast: media plane closed"
    );
}

/// Turn a parsed RTSP message into session outputs.
fn dispatch(
    session: &mut WfdSession,
    message: &substrate_rtsp::RtspMessage,
) -> Result<Vec<SinkOutput>, MiracastError> {
    use substrate_rtsp::rtsp_types::Message;
    match message {
        Message::Request(req) => {
            let headers = collect_headers(req.headers());
            let method = substrate_rtsp::method_name(req.method()).to_owned();
            session.on_request(&WfdRequest {
                method: &method,
                uri: &substrate_rtsp::request_path(req),
                headers: &headers,
                body: req.body(),
            })
        }
        Message::Response(resp) => {
            let headers = collect_headers(resp.headers());
            session.on_response(&WfdResponse {
                status: u16::from(resp.status()),
                headers: &headers,
                body: resp.body(),
            })
        }
        // WFD never interleaves binary data on the control connection; the media plane is
        // its own UDP socket.
        Message::Data(_) => Ok(Vec::new()),
    }
}

fn collect_headers<'a>(
    headers: impl Iterator<
        Item = (
            &'a substrate_rtsp::rtsp_types::HeaderName,
            &'a substrate_rtsp::rtsp_types::HeaderValue,
        ),
    >,
) -> Vec<(String, String)> {
    headers
        .map(|(name, value)| (name.as_str().to_owned(), value.as_str().to_owned()))
        .collect()
}

/// Write what the session asked for, and act on the media transitions.
///
/// Returns `true` when the session is over.
async fn apply(
    outputs: &[SinkOutput],
    control: &mut TcpStream,
    planes: &mut Option<Planes>,
    mode: &mut Option<crate::video::VideoMode>,
    sink: &SessionSink,
    peer: &SocketAddr,
) -> Result<bool, MiracastError> {
    for output in outputs {
        match output {
            SinkOutput::Respond(resp) => write_response(control, resp).await?,
            SinkOutput::Send(req) => write_request(control, req).await?,
            SinkOutput::MediaStarted(config) => {
                *mode = Some(config.mode());
                *planes = Some(start_media(config, sink, peer).await?);
            }
            SinkOutput::UibcPort(port) => {
                // Best-effort: a back-channel that will not open costs the session its
                // touch, not the session. The source is streaming either way, and tearing
                // a working mirror down over an input channel would be the wrong trade.
                match mode {
                    Some(mode) => open_uibc(*port, *mode, sink, peer).await,
                    None => debug!(port, "miracast: UIBC port before a mode; ignoring it"),
                }
            }
            SinkOutput::UibcRevoked => {
                // The source is still streaming; only the input channel closed. Telling
                // the session manager is what stops the glass driving something that has
                // said it is not listening — the writer task ends on its own when the
                // surface is dropped and its channel closes.
                info!(%peer, "miracast: the source turned UIBC off; taking the touch surface down");
                if sink.emit(SessionEvent::TouchSurfaceRevoked).await.is_err() {
                    debug!("miracast: nothing was holding the UIBC touch surface");
                }
            }
            SinkOutput::MediaStopped => {
                // Dropping the senders closes the pipeline's receivers, which is how a
                // pause reaches the far end. The session and its negotiation survive.
                *planes = None;
            }
            SinkOutput::Ended => {
                *planes = None;
                let _ = sink.emit(SessionEvent::End).await;
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The two frame channels of a running mirror.
struct Planes {
    video: mpsc::Sender<EncodedFrame>,
    audio: Option<mpsc::Sender<EncodedFrame>>,
    /// Set until a keyframe has been seen; drives the IDR request.
    needs_keyframe: bool,
}

impl Planes {
    fn deliver(&mut self, frame: EncodedFrame) {
        if frame.video_codec.is_some() {
            if frame.keyframe {
                self.needs_keyframe = false;
            }
            // `try_send` and not `send`: a full queue means the decoder is behind, and for
            // a live mirror the right answer is to drop the frame rather than to stall the
            // socket and accumulate latency (ground rule 4).
            if self.video.try_send(frame).is_err() {
                debug!("dropped a video frame; the pipeline is behind");
            }
        } else if let Some(audio) = &self.audio {
            if audio.try_send(frame).is_err() {
                debug!("dropped an audio frame");
            }
        }
    }
}

async fn start_media(
    config: &NegotiatedConfig,
    sink: &SessionSink,
    peer: &SocketAddr,
) -> Result<Planes, MiracastError> {
    let (video_tx, video_rx) = mpsc::channel(FRAME_QUEUE);
    let mode = config.mode();
    // The audio format comes from the negotiation, not from the stream: LPCM states its
    // rate nowhere in-band, so a decoder opened from the frames alone would play it at
    // whatever rate it guessed.
    let audio = config.audio_format().and_then(|(rate, channels)| {
        let format = AudioFormat::from_hz(rate, channels)?;
        let (tx, rx) = mpsc::channel(FRAME_QUEUE * 4);
        Some((
            tx,
            MirrorAudio {
                source: FrameSource::Encoded(rx),
                format,
                // AAC arrives ADTS-framed and LPCM needs no decoder configuration, so
                // unlike AirPlay's AAC-ELD there is nothing out-of-band to carry.
                config: None,
            },
        ))
    });
    let (audio_tx, mirror_audio) = match audio {
        Some((tx, mirror)) => (Some(tx), Some(mirror)),
        None => (None, None),
    };
    info!(
        %peer,
        mode = %mode,
        profile = %config.profile(),
        audio = mirror_audio.is_some(),
        "mirroring started"
    );
    sink.emit(SessionEvent::SourceInfo(
        SourceDescription::new()
            .with_address(peer.ip().to_string())
            .with_link(format!("Miracast · {mode} · {}", config.profile())),
    ))
    .await
    .map_err(|e| MiracastError::Connection(e.to_string()))?;
    sink.emit(SessionEvent::Mirror {
        video: FrameSource::Encoded(video_rx),
        audio: mirror_audio,
    })
    .await
    .map_err(|e| MiracastError::Connection(e.to_string()))?;
    Ok(Planes {
        video: video_tx,
        audio: audio_tx,
        needs_keyframe: true,
    })
}

/// Dial the source's UIBC listener and publish a touch surface for it.
///
/// The half of UIBC the sink owes: both the IE bit and the M3 `wfd_uibc_capability`
/// answer are promises, and a sink that makes them and never connects is one whose panel
/// touch silently does nothing while both peers believe the feature is negotiated (#125).
///
/// The source is the TCP server here, exactly as it is for RTSP. `TCP_NODELAY` because
/// every frame is a few bytes of live input and Nagle would hold each one waiting for a
/// companion that is a finger-movement away (§5.2).
async fn open_uibc(
    port: u16,
    mode: crate::video::VideoMode,
    sink: &SessionSink,
    peer: &SocketAddr,
) {
    let target = SocketAddr::new(peer.ip(), port);
    let stream = match tokio::time::timeout(UIBC_CONNECT_TIMEOUT, TcpStream::connect(target)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!(%target, error = %e, "miracast: could not open the UIBC back-channel; panel touch will not reach this source");
            return;
        }
        Err(_) => {
            warn!(%target, "miracast: the UIBC back-channel did not answer; panel touch will not reach this source");
            return;
        }
    };
    if let Err(e) = stream.set_nodelay(true) {
        debug!(%target, error = %e, "miracast: no TCP_NODELAY on the UIBC channel");
    }

    let (frames, mut rx) = mpsc::channel::<Vec<u8>>(uibc::UIBC_QUEUE);
    let surface = Arc::new(uibc::UibcSurface::new(mode, frames));
    tokio::spawn(async move {
        let mut stream = stream;
        while let Some(frame) = rx.recv().await {
            if let Err(e) = stream.write_all(&frame).await {
                debug!(error = %e, "miracast: the UIBC channel went away");
                return;
            }
        }
    });

    info!(%target, mode = %mode, "miracast: UIBC back-channel open; the panel can drive this source");
    if sink
        .emit(SessionEvent::TouchSurface(surface))
        .await
        .is_err()
    {
        debug!("miracast: nothing took the UIBC touch surface");
    }
}

async fn write_response(
    control: &mut TcpStream,
    resp: &OutgoingResponse,
) -> Result<(), MiracastError> {
    use substrate_rtsp::rtsp_types::{Response, StatusCode, Version};
    let mut builder = Response::builder(Version::V1_0, StatusCode::from(resp.status)).header(
        substrate_rtsp::rtsp_types::headers::CSEQ,
        resp.cseq.to_string(),
    );
    for (name, value) in &resp.headers {
        builder = builder.header(header_name(name)?, value.clone());
    }
    write_message(control, &builder.build(resp.body.clone()).into()).await
}

async fn write_request(
    control: &mut TcpStream,
    req: &OutgoingRequest,
) -> Result<(), MiracastError> {
    use substrate_rtsp::rtsp_types::{Method, Request, Version};
    // `OPTIONS *` has no URI at all, and rtsp_types models that as an absent request-URI.
    let method = match req.method {
        "OPTIONS" => Method::Options,
        "SETUP" => Method::Setup,
        "PLAY" => Method::Play,
        "PAUSE" => Method::Pause,
        "TEARDOWN" => Method::Teardown,
        "SET_PARAMETER" => Method::SetParameter,
        "GET_PARAMETER" => Method::GetParameter,
        other => Method::Extension(other.to_owned()),
    };
    let mut builder = Request::builder(method, Version::V1_0).header(
        substrate_rtsp::rtsp_types::headers::CSEQ,
        req.cseq.to_string(),
    );
    if req.uri != "*" {
        let uri = substrate_rtsp::rtsp_types::Url::parse(&req.uri)
            .map_err(|_| MiracastError::Connection(format!("unusable request-URI {}", req.uri)))?;
        builder = builder.request_uri(uri);
    }
    for (name, value) in &req.headers {
        builder = builder.header(header_name(name)?, value.clone());
    }
    write_message(control, &builder.build(req.body.clone()).into()).await
}

/// Every header name this crate emits is a `&'static str` literal from
/// [`crate::session`], so the ASCII check can only fail on a literal we wrote — which is
/// why this is an error rather than an `expect` (ground rule 7).
fn header_name(
    name: &'static str,
) -> Result<substrate_rtsp::rtsp_types::HeaderName, MiracastError> {
    substrate_rtsp::rtsp_types::HeaderName::from_static_str(name)
        .map_err(|_| MiracastError::Connection(format!("unusable header name {name}")))
}

async fn write_message(
    control: &mut TcpStream,
    message: &substrate_rtsp::RtspMessage,
) -> Result<(), MiracastError> {
    let bytes = substrate_rtsp::write(message)?;
    control
        .write_all(&bytes)
        .await
        .map_err(|e| MiracastError::Connection(e.to_string()))
}

/// Bind the RTP socket the sink will advertise.
///
/// Bound to `0.0.0.0` rather than to the group interface's address on purpose: the P2P
/// interface's address is assigned by DHCP *after* the group comes up, and a socket bound
/// to a specific address before then binds to the wrong one.
///
/// # Errors
/// [`MiracastError::Connection`] if the port is taken.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the miracast/udp rtp_port entry in crates/app/src/surface.rs"
)]
pub async fn bind_rtp(port: u16) -> Result<UdpSocket, MiracastError> {
    UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
        .map_err(|e| MiracastError::Connection(format!("binding RTP port {port}: {e}")))
}

/// Connect to a source's RTSP port.
///
/// # Errors
/// [`MiracastError::Connection`] if the source is not listening.
pub async fn connect_control(peer: SocketAddr) -> Result<TcpStream, MiracastError> {
    TcpStream::connect(peer)
        .await
        .map_err(|e| MiracastError::Connection(format!("connecting to {peer}: {e}")))
}

/// A [`castaway_core::SourceAdapter`] that runs whatever backend brought a link up.
///
/// The adapter exists so `app` wires Miracast the same way it wires every other protocol,
/// while the part that differs per OS stays behind [`castaway_core::MiracastBackend`]
/// (ground rule 5).
pub struct MiracastAdapter {
    backend: Arc<dyn castaway_core::MiracastBackend>,
    device_name: String,
    /// Miracast over Infrastructure, if it is on (#166).
    ///
    /// `None` leaves the adapter exactly as it was: Wi-Fi Direct only, no mDNS
    /// registration, nothing listening on 7250. `Some` adds a second way *in* rather than
    /// a second protocol — the RTSP session either path reaches is the same one.
    mice: Option<MiceService>,
}

/// What Miracast over Infrastructure needs beyond the P2P beacon.
#[derive(Debug, Clone)]
pub struct MiceService {
    /// The GUID [MS-MICE] §3.1.3 requires in the service instance's TXT record.
    ///
    /// The receiver's own UUID, so a source that has seen this panel before recognises it
    /// across restarts — which is what a container id is for.
    pub container_id: String,
    /// What the sink advertises it can do. Bounded by what it can actually serve: the
    /// secured flows are refused rather than half-answered, so they are not advertised
    /// (see [`crate::mice`]).
    pub capability: crate::mice::Capability,
    /// The capabilities the RTSP session negotiates with, once a source has said where to
    /// dial it.
    pub caps: SinkCapabilities,
}

impl MiracastAdapter {
    /// An adapter driving `backend`, advertising `device_name` in the P2P beacon.
    #[must_use]
    pub fn new(backend: Arc<dyn castaway_core::MiracastBackend>, device_name: String) -> Self {
        Self {
            backend,
            device_name,
            mice: None,
        }
    }

    /// Also serve Miracast over Infrastructure.
    #[must_use]
    pub fn with_mice(mut self, mice: MiceService) -> Self {
        self.mice = Some(mice);
        self
    }
}

#[async_trait::async_trait]
impl castaway_core::SourceAdapter for MiracastAdapter {
    fn kind(&self) -> castaway_core::ProtocolKind {
        crate::kind()
    }

    fn advertisements(&self) -> Vec<castaway_core::Advertisement> {
        // The beacon is 802.11 — L2, OS-specific, and not something the shared responders
        // can carry (architecture §1e).
        let mut out = vec![castaway_core::Advertisement::WifiDirect {
            device_name: self.device_name.clone(),
        }];
        // MICE's is ordinary mDNS, and *does* go through the shared responder. That
        // convergence is not a coincidence: [MS-MICE] §3.1.3 and WFA R2 (Miracast v2.3
        // §4.4.1) independently landed on `_display._tcp` for the sink, so one responder
        // serves both.
        if let Some(mice) = &self.mice {
            out.push(castaway_core::Advertisement::MdnsService {
                ty: crate::mice::SERVICE_TYPE.to_string(),
                instance: self.device_name.clone(),
                port: crate::mice::CONTROL_PORT,
                txt: vec![(
                    crate::mice::CONTAINER_ID_KEY.to_string(),
                    mice.container_id.clone(),
                )],
            });
        }
        out
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), castaway_core::CoreError> {
        let Some(mice) = self.mice.clone() else {
            return Arc::clone(&self.backend).run(sink).await;
        };
        // Two ways in, one session at a time. Both are awaited together and the first to
        // finish ends the adapter, because either finishing means the thing that owns the
        // radio or the port has gone — carrying on with half a Miracast is a receiver that
        // is advertised and unreachable.
        let backend = Arc::clone(&self.backend);
        let backend_sink = sink.clone();
        let listener = crate::mice_actor::bind()
            .await
            .map_err(|e| castaway_core::CoreError::Adapter(e.to_string()))?;
        info!(
            port = crate::mice::CONTROL_PORT,
            "miracast: also serving over infrastructure"
        );
        tokio::select! {
            res = backend.run(backend_sink) => res,
            res = crate::mice_actor::serve(
                listener,
                self.device_name.clone(),
                mice.caps.clone(),
                sink,
            ) => res.map_err(|e| castaway_core::CoreError::Adapter(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // Tests bind ephemeral loopback sockets; the registry governs production binds.
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use crate::params::{
        AudioCodecs, ClientRtpPorts, ConnectorType, ContentProtection, RtpProfile,
    };
    use crate::video::VideoFormats;
    use castaway_core::{ProtocolKind, SourceId, SourceMessage};

    fn caps(port: u16) -> SinkCapabilities {
        SinkCapabilities {
            video_formats: VideoFormats::parse(
                "00 00 03 10 0001FFFF 1FFFFFFF 00000FFF 00 0000 0000 00 none none",
            )
            .unwrap(),
            audio_codecs: AudioCodecs::sink_default(),
            client_rtp_ports: ClientRtpPorts::new(RtpProfile::UdpUnicast, port).unwrap(),
            content_protection: ContentProtection::None,
            connector_type: ConnectorType::Hdmi,
            idr_request: true,
            uibc: None,
        }
    }

    /// One RTP datagram of MPEG2-TS: the MP2T payload type, a sequence number, and bytes.
    ///
    /// The sequence number is the parameter it needs to be. A constant one — which is what
    /// this used to build — makes every datagram after the first *late* to the sink's
    /// reorder buffer, so nothing reaches the demuxer and the media tests below are
    /// asserting on the datagram rather than on its contents (#192).
    fn rtp_datagram(seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut datagram = Vec::with_capacity(12 + payload.len());
        datagram.extend_from_slice(&[0x80, 33]);
        datagram.extend_from_slice(&seq.to_be_bytes());
        datagram.extend_from_slice(&[0, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF]);
        datagram.extend_from_slice(payload);
        datagram
    }

    /// A scripted WFD *source*: it accepts the sink's connection and walks M1→M7.
    ///
    /// **Control plane only.** This used to claim it "sends one RTP datagram of transport
    /// stream"; it does not and never did, so nothing in this file has ever put a TS
    /// packet through `MediaReceiver`. The media plane's coverage is `media.rs` and
    /// `ts.rs` against fixtures, and `miracast-vm` over a real emulated radio — where the
    /// source deliberately sends an IDR first, so the sink never needs M13 either (#192).
    ///
    /// Returns its own end of the control connection rather than dropping it, so a caller
    /// can choose between the two endings that matter: drop it and the sink sees FIN, or
    /// hold it and the sink sees a peer that is connected and saying nothing.
    ///
    /// `session_timeout` is what goes in the `Session:` header of the SETUP response —
    /// the source's number, which is the one the sink's watchdog has to honour rather than
    /// a value of its own (notes §2.3).
    async fn scripted_source(
        listener: tokio::net::TcpListener,
        rtp_port: u16,
        session_timeout: u32,
    ) -> Result<TcpStream, std::io::Error> {
        scripted_source_inner(listener, rtp_port, session_timeout, None).await
    }

    /// The body, with an optional `wfd_uibc_capability` port in M4.
    async fn scripted_source_inner(
        listener: tokio::net::TcpListener,
        rtp_port: u16,
        session_timeout: u32,
        uibc_port: Option<u16>,
    ) -> Result<TcpStream, std::io::Error> {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let mut seen = String::new();

        async fn send(stream: &mut TcpStream, text: String) -> Result<(), std::io::Error> {
            stream.write_all(text.as_bytes()).await
        }

        // M1.
        send(
            &mut stream,
            "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nRequire: org.wfa.wfd1.0\r\n\r\n".to_owned(),
        )
        .await?;
        // Read until we have both the M1 response and the sink's own M2.
        // Matched without the reason phrase: `rtsp_types` renders it "Ok", real sources
        // render it "OK", and nothing on either side parses it.
        while !seen.contains("RTSP/1.0 200") || !seen.contains("OPTIONS") {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(stream);
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            seen.contains("Public: org.wfa.wfd1.0"),
            "the sink must answer M1 with its method set: {seen}"
        );
        send(
            &mut stream,
            "RTSP/1.0 200 OK\r\nCSeq: 1\r\nPublic: org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, \
             PAUSE, GET_PARAMETER, SET_PARAMETER\r\nServer: MSMiracastSource/10.0\r\n\r\n"
                .to_owned(),
        )
        .await?;

        // M3.
        let names = "wfd_video_formats\r\nwfd_audio_codecs\r\nwfd_client_rtp_ports\r\n";
        send(
            &mut stream,
            format!(
                "GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 2\r\n\
                 Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{names}",
                names.len()
            ),
        )
        .await?;
        seen.clear();
        while !seen.contains("wfd_client_rtp_ports:") {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(stream);
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            seen.contains(&format!("unicast {rtp_port} 0 mode=play")),
            "the advertised port must be the one we bound: {seen}"
        );

        // M4 and M5 in one segment — which real sources do, and which is the framing
        // hazard a recv()-per-message loop fails on.
        let m4 = format!(
            "wfd_video_formats: 00 00 03 10 00000100 00000000 00000000 00 0000 0000 00 \
             none none\r\nwfd_audio_codecs: AAC 00000001 00\r\n\
             wfd_presentation_URL: rtsp://127.0.0.1/wfd1.0/streamid=0 none\r\n{}",
            uibc_port.map_or_else(String::new, |port| format!(
                "wfd_uibc_capability: input_category_list=GENERIC;\
                 generic_cap_list=SingleTouch;hidc_cap_list=none;port={port}\r\n"
            ))
        );
        let m4 = m4.as_str();
        let m5 = "wfd_trigger_method: SETUP\r\n";
        let coalesced = format!(
            "SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 3\r\n\
             Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{m4}\
             SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 4\r\n\
             Content-Type: text/parameters\r\nContent-Length: {}\r\n\r\n{m5}",
            m4.len(),
            m5.len()
        );
        send(&mut stream, coalesced).await?;

        // The sink answers both and sends SETUP.
        seen.clear();
        while !seen.contains("SETUP") {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(stream);
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            seen.contains(&format!("client_port={rtp_port}")),
            "SETUP must name the bound port: {seen}"
        );
        send(
            &mut stream,
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: 2\r\nSession: 4242;timeout={session_timeout}\r\n\
                 Transport: RTP/AVP/UDP;unicast;client_port=1028;server_port=19000\r\n\r\n"
            ),
        )
        .await?;

        seen.clear();
        while !seen.contains("PLAY") {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(stream);
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        send(
            &mut stream,
            "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 4242\r\n\r\n".to_owned(),
        )
        .await?;
        Ok(stream)
    }

    #[tokio::test]
    async fn a_scripted_source_drives_the_sink_from_m1_to_a_running_mirror() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = listener.local_addr().unwrap();
        let rtp = bind_rtp(0).await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();

        let source = tokio::spawn(scripted_source(listener, rtp_port, 30));

        let (tx, mut rx) = mpsc::channel::<SourceMessage>(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Miracast, "test"), tx);
        let control = connect_control(source_addr).await.unwrap();
        let session = tokio::spawn(run_session(control, rtp, caps(rtp_port), sink));

        // The mirror must be announced to the session manager, with audio, because the
        // source chose AAC.
        let mut saw_mirror = false;
        while let Some(message) = rx.recv().await {
            if let SessionEvent::Mirror { audio, .. } = message.event {
                assert!(audio.is_some(), "the source chose AAC in M4");
                saw_mirror = true;
                break;
            }
        }
        assert!(saw_mirror, "the session never reached a running mirror");

        source.await.unwrap().unwrap();
        // Dropping the source's socket ends the session cleanly rather than by error.
        session.abort();
    }

    /// A source that stops talking without saying goodbye gives the slot back.
    ///
    /// The `Session:` header's `;timeout=` was parsed and then discarded, and nothing in
    /// `run_session` timed out a silent peer — so a phone that walked out of range or a
    /// laptop that slept held the session, the panel's single-source slot and the RTP
    /// socket indefinitely, and recovery was a restart (#195).
    ///
    /// The distinction this is careful about: it is a *liveness* deadline, not a
    /// keep-alive one. The notes are emphatic that a sink must not time out on a missing
    /// M16 alone — several sources send them late, and Windows in some configurations
    /// relies on RTP flow instead — so the clock resets on anything at all from the
    /// source, on either socket, and only fires after `IDLE_GRACE` whole timeout periods.
    ///
    /// Driven by a source that says `timeout=1`, which keeps the test to three seconds and
    /// asserts the thing worth asserting: the watchdog runs on the **source's** number.
    /// A hard-coded 30 or 60 would sit here for a minute and a half and pass.
    #[tokio::test]
    async fn a_source_that_goes_silent_without_a_teardown_gives_the_slot_back() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = listener.local_addr().unwrap();
        let rtp = bind_rtp(0).await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();

        let source = tokio::spawn(scripted_source(listener, rtp_port, 1));

        let (tx, mut rx) = mpsc::channel::<SourceMessage>(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Miracast, "test"), tx);
        let control = connect_control(source_addr).await.unwrap();
        let session = tokio::spawn(run_session(control, rtp, caps(rtp_port), sink));

        while let Some(message) = rx.recv().await {
            if matches!(message.event, SessionEvent::Mirror { .. }) {
                break;
            }
        }

        // Held, not dropped: the socket stays open and nothing more is written to it. A
        // dropped socket is a FIN, which the loop already handled — the case that had no
        // handling is the peer that is still connected and gone.
        let _still_connected = source.await.unwrap().unwrap();

        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(30), session)
            .await
            .expect("the session is still holding the panel's slot")
            .unwrap();
        assert!(
            outcome.is_ok(),
            "an idle peer is a session that ended, not one that failed: {outcome:?}"
        );

        // …and it waited. A watchdog that fired immediately would satisfy the assertion
        // above and would end a session every time a source paused between messages.
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(1),
            "gave up after {waited:?}, which is inside a single session timeout"
        );
    }

    /// The source advertises a UIBC port, we dial it, and panel touch comes out the far
    /// side as source pixels.
    ///
    /// #125 was "UIBC is negotiated end to end, the sink never connects the back-channel,
    /// panel touch silently does nothing", and the fix landed with **no regression test**.
    /// The encoder below it has 22 excellent tests — the spec's own worked touch-down
    /// bytes, lazycast's mouse descriptor, multi-touch `5n+1` arithmetic, odd-length HIDC
    /// padding, a coordinate type that cannot carry panel pixels — and that is precisely
    /// why the bug survived: the encoder was never the problem, and everything that was
    /// tested kept passing while touch did nothing (#193).
    ///
    /// So this asserts the half that was missing and nothing the encoder already covers:
    /// the dial happens, a `TouchSurface` is published, and what arrives on the socket is
    /// in the **source's** pixel space — which is where the letterbox mapping can be
    /// silently wrong, because a panel-space coordinate is a perfectly well-formed frame
    /// that puts the finger somewhere else.
    #[tokio::test]
    async fn a_source_that_offers_uibc_gets_the_panels_touches() {
        // The source's back-channel listener, bound before M4 so the port it advertises is
        // one it is already listening on.
        let uibc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uibc_port = uibc.local_addr().unwrap().port();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = listener.local_addr().unwrap();
        let rtp = bind_rtp(0).await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();

        let source = tokio::spawn(scripted_source_with_uibc(listener, rtp_port, uibc_port));

        let (tx, mut rx) = mpsc::channel::<SourceMessage>(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Miracast, "test"), tx);
        let control = connect_control(source_addr).await.unwrap();
        let session = tokio::spawn(run_session(control, rtp, caps(rtp_port), sink));

        // The sink must dial *us*. A session that negotiates UIBC and never connects is
        // exactly #125, and it looks identical from every other vantage point.
        let accepted = tokio::time::timeout(Duration::from_secs(5), uibc.accept())
            .await
            .expect("the sink never dialled the UIBC port the source advertised")
            .unwrap();
        let (mut back_channel, _) = accepted;

        // …and publish a surface for the panel to drive.
        let mut surface = None;
        while let Some(message) = rx.recv().await {
            if let SessionEvent::TouchSurface(s) = message.event {
                surface = Some(s);
                break;
            }
        }
        let surface = surface.expect("the session never published a touch surface");

        // A press at the middle of the glass. The negotiated mode is 1280x720 (the M4
        // below picks it), and the source's space is that — not the panel's.
        surface.touch(castaway_core::SurfaceTouch {
            contact: 1,
            phase: castaway_core::TouchPhase::Down,
            x: 0.5,
            y: 0.5,
        });

        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(5), back_channel.read(&mut buf))
            .await
            .expect("no UIBC frame arrived within five seconds")
            .unwrap();
        assert!(n > 0, "the back-channel closed instead of carrying a frame");

        let (frame, _) = uibc::UibcFrame::parse(&buf[..n]).expect("a whole UIBC frame");
        let messages = frame
            .generic_messages()
            .expect("touch rides the generic category");
        let [uibc::GenericInput::TouchDown(pointers)] = messages.as_slice() else {
            panic!("expected one touch-down, got {messages:?}");
        };
        let [pointer] = pointers.as_slice() else {
            panic!("one finger went down, so one pointer: {pointers:?}");
        };

        // The assertion that matters, and the only one the encoder's own tests cannot
        // make: the coordinate is in the *source's* space. A panel-space 0.5 would come
        // through as some other number entirely, and both are well-formed frames.
        //
        // The M4 above sets CEA bit 8, which is 1920x1080p30 — so the middle of the glass
        // is (960, 540) and *not* 0.5 of anything. Written as a literal rather than read
        // back off the surface, because an expectation taken from the thing under test is
        // not an expectation. (It is also what this test got wrong first time, which is
        // the argument for the literal: a wrong constant fails loudly, a derived one
        // agrees with whatever happened.)
        assert_eq!(
            (u32::from(pointer.at.x()), u32::from(pointer.at.y())),
            (1920 / 2, 1080 / 2),
            "the middle of the glass must land in the middle of the source's picture"
        );

        session.abort();
        let _ = source.await;
    }

    /// [`scripted_source`], plus a `wfd_uibc_capability` naming `uibc_port` in M4.
    async fn scripted_source_with_uibc(
        listener: tokio::net::TcpListener,
        rtp_port: u16,
        uibc_port: u16,
    ) -> Result<TcpStream, std::io::Error> {
        scripted_source_inner(listener, rtp_port, 30, Some(uibc_port)).await
    }

    /// Joining mid-GOP asks the source for an IDR, and does not ask again for a second.
    ///
    /// `wfd_idr_request` is WFD's only loss-recovery primitive and the entire
    /// justification for hand-rolling the MPEG2-TS demuxer instead of using ffmpeg's
    /// `rtp_mpegts` (D35): AOSP's encoder puts IDRs *fifteen seconds* apart, so without
    /// M13 a single lost reference is a fifteen-second frozen screen.
    ///
    /// Two tests covered the message *shape*. **The trigger had never fired at any
    /// tier** — `needs_keyframe` and the `IDR_MIN_INTERVAL` rate limiter were executed by
    /// nothing, and `miracast-vm`'s source deliberately sends an IDR first so the sink
    /// never needs one (#192). It turned out this file could not have tested it either:
    /// the scripted source sent no RTP at all despite a comment saying it did (`c9a5b89`).
    ///
    /// Both halves are asserted, and the limiter is the one that matters in the field: a
    /// real capture shows a sink firing eight M13s back to back and turning a lossy link
    /// into an unusable one, because each IDR collapses the bitrate.
    #[tokio::test]
    async fn joining_mid_gop_asks_for_an_idr_and_then_stops_asking() {
        use crate::ts::tests as ts_fixtures;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = listener.local_addr().unwrap();
        let rtp = bind_rtp(0).await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();

        let source = tokio::spawn(scripted_source(listener, rtp_port, 30));

        let (tx, mut rx) = mpsc::channel::<SourceMessage>(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Miracast, "test"), tx);
        let control = connect_control(source_addr).await.unwrap();
        let session = tokio::spawn(run_session(control, rtp, caps(rtp_port), sink));

        while let Some(message) = rx.recv().await {
            if matches!(message.event, SessionEvent::Mirror { .. }) {
                break;
            }
        }
        let mut control = source.await.unwrap().unwrap();

        // A stream that starts on a *P-frame*: the source was already encoding when we
        // joined, which is the ordinary case for a sink walking up to a running cast.
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = std::net::SocketAddr::from(([127, 0, 0, 1], rtp_port));
        let mut cc = 0u8;
        // A real sequence number per datagram. It used to be the constant 1, and the
        // sink's reorder buffer calls anything at or behind where it has already emitted
        // *late* — so from the second datagram on, every one of them was dropped before
        // it reached the demuxer, and this test was asserting that the first datagram
        // triggers an M13 rather than that a P-frame does. Found while writing the loss
        // test below (#192).
        let mut next_seq = 0u16;
        let mut send_ts = |bytes: &[u8]| {
            let datagram = rtp_datagram(next_seq, bytes);
            next_seq = next_seq.wrapping_add(1);
            datagram
        };
        let psi = {
            let mut v = ts_fixtures::ts_packet(
                crate::ts::PAT_PID,
                true,
                0,
                &ts_fixtures::pat(ts_fixtures::PMT_PID),
            );
            v.extend_from_slice(&ts_fixtures::ts_packet(
                ts_fixtures::PMT_PID,
                true,
                0,
                &ts_fixtures::pmt(&[(ts_fixtures::VIDEO_PID, 0x1B)]),
            ));
            v
        };
        sender.send_to(&send_ts(&psi), target).await.unwrap();

        // Several P-frames. An unbounded PES ends only when the next one starts, so it
        // takes two to get one frame out of the demuxer.
        for pts in [90_000u64, 93_000, 96_000, 99_000] {
            let pes = ts_fixtures::pes(0xE0, Some(pts), &ts_fixtures::non_idr_access_unit(), false);
            let packets = ts_fixtures::packetize(ts_fixtures::VIDEO_PID, cc, &pes);
            cc = cc.wrapping_add(u8::try_from(packets.len() / 188).unwrap_or(1)) & 0x0F;
            sender.send_to(&send_ts(&packets), target).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // The sink must ask. Read the source's socket for an M13 — a bare
        // `wfd_idr_request` in a SET_PARAMETER, which is what AOSP substring-matches on.
        let mut seen = String::new();
        let mut buf = vec![0u8; 8192];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline && !seen.contains("wfd_idr_request") {
            let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(300), control.read(&mut buf)).await
            else {
                continue;
            };
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            seen.contains("wfd_idr_request"),
            "the sink joined mid-GOP and never asked for a keyframe; without M13 that is \
             a fifteen-second stare at a frozen screen on AOSP:\n{seen}"
        );

        // …and stops asking. Keep feeding P-frames for well under `IDR_MIN_INTERVAL` and
        // count: a sink that asks per frame turns a lossy link into an unusable one,
        // because every IDR collapses the bitrate.
        let before = seen.matches("wfd_idr_request").count();
        for pts in [102_000u64, 105_000, 108_000, 111_000, 114_000] {
            let pes = ts_fixtures::pes(0xE0, Some(pts), &ts_fixtures::non_idr_access_unit(), false);
            let packets = ts_fixtures::packetize(ts_fixtures::VIDEO_PID, cc, &pes);
            cc = cc.wrapping_add(u8::try_from(packets.len() / 188).unwrap_or(1)) & 0x0F;
            sender.send_to(&send_ts(&packets), target).await.unwrap();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(200), control.read(&mut buf)).await
        {
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        let after = seen.matches("wfd_idr_request").count();
        assert_eq!(
            after,
            before,
            "asked {} more times inside one IDR_MIN_INTERVAL; the rate limiter is what \
             stands between a lossy link and an unusable one",
            after - before
        );

        session.abort();
    }

    /// Read whatever the source has been sent, for up to `window`.
    async fn drain_control(control: &mut TcpStream, into: &mut String, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        let mut buf = vec![0u8; 8192];
        while tokio::time::Instant::now() < deadline {
            let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(100), control.read(&mut buf)).await
            else {
                continue;
            };
            if n == 0 {
                break;
            }
            into.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }

    /// A gap in the middle of a *running* stream asks for an IDR again.
    ///
    /// The other half of D35's justification, and the half the sink could not do at all.
    /// `needs_keyframe` was set once when a mirror started and cleared by the first
    /// keyframe, and nothing ever set it again — so a source that answered the join-time
    /// M13 and then dropped a reference left the panel showing a picture that was wrong
    /// and stayed wrong until the encoder's own next IDR. On AOSP that is fifteen seconds,
    /// which is the exact failure `wfd_idr_request` exists to prevent (#192).
    ///
    /// The loss is real rather than simulated: a datagram is withheld, the reorder buffer
    /// holds the hole open for its depth and then gives up on it, and the demuxer finds
    /// the continuity counter has jumped. Every link in that chain has to work for the
    /// request to go out, which is the point — a counter the session layer cannot see is
    /// what the sink was missing, not a decision it was getting wrong.
    #[tokio::test]
    async fn a_gap_in_a_running_stream_asks_for_an_idr_again() {
        use crate::ts::tests as ts_fixtures;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = listener.local_addr().unwrap();
        let rtp = bind_rtp(0).await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();

        let source = tokio::spawn(scripted_source(listener, rtp_port, 30));

        let (tx, mut rx) = mpsc::channel::<SourceMessage>(8);
        let sink = SessionSink::new(SourceId::new(ProtocolKind::Miracast, "test"), tx);
        let control = connect_control(source_addr).await.unwrap();
        let session = tokio::spawn(run_session(control, rtp, caps(rtp_port), sink));

        while let Some(message) = rx.recv().await {
            if matches!(message.event, SessionEvent::Mirror { .. }) {
                break;
            }
        }
        let mut control = source.await.unwrap().unwrap();

        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = std::net::SocketAddr::from(([127, 0, 0, 1], rtp_port));
        let mut seq = 0u16;
        let mut cc = 0u8;
        let mut pts = 90_000u64;

        let mut psi = ts_fixtures::ts_packet(
            crate::ts::PAT_PID,
            true,
            0,
            &ts_fixtures::pat(ts_fixtures::PMT_PID),
        );
        psi.extend_from_slice(&ts_fixtures::ts_packet(
            ts_fixtures::PMT_PID,
            true,
            0,
            &ts_fixtures::pmt(&[(ts_fixtures::VIDEO_PID, 0x1B)]),
        ));
        sender
            .send_to(&rtp_datagram(seq, &psi), target)
            .await
            .unwrap();
        seq = seq.wrapping_add(1);

        // One access unit per datagram, which is what a source sending small frames
        // produces and what makes a withheld datagram exactly one lost access unit.
        let send_au = |seq: u16, cc: u8, pts: u64, keyframe: bool| {
            let au = if keyframe {
                ts_fixtures::idr_access_unit()
            } else {
                ts_fixtures::non_idr_access_unit()
            };
            let pes = ts_fixtures::pes(0xE0, Some(pts), &au, false);
            rtp_datagram(
                seq,
                &ts_fixtures::packetize(ts_fixtures::VIDEO_PID, cc, &pes),
            )
        };

        // A keyframe and two P-frames: an unbounded PES completes only when the next one
        // starts, so this is what it takes to get the keyframe *delivered* and the
        // join-time request satisfied.
        for keyframe in [true, false, false] {
            let datagram = send_au(seq, cc, pts, keyframe);
            sender.send_to(&datagram, target).await.unwrap();
            seq = seq.wrapping_add(1);
            cc = cc.wrapping_add(1) & 0x0F;
            pts += 3_000;
        }

        let mut seen = String::new();
        drain_control(&mut control, &mut seen, Duration::from_millis(400)).await;
        let before = seen.matches("wfd_idr_request").count();
        assert_eq!(
            before, 1,
            "the join-time request is the one this test starts from:\n{seen}"
        );

        // Past the limiter, so what follows is attributable to the loss and not to a
        // request that was merely overdue.
        tokio::time::sleep(IDR_MIN_INTERVAL + Duration::from_millis(200)).await;
        drain_control(&mut control, &mut seen, Duration::from_millis(200)).await;
        assert_eq!(
            seen.matches("wfd_idr_request").count(),
            before,
            "silence costs nothing: the sink has a keyframe and no reason to ask:\n{seen}"
        );

        // Now lose one, and keep sending. The reorder buffer waits out its depth before
        // it will admit the datagram is gone, so the ones after it are what turn a hole
        // into a loss.
        seq = seq.wrapping_add(1);
        cc = cc.wrapping_add(1) & 0x0F;
        pts += 3_000;
        for _ in 0..12 {
            let datagram = send_au(seq, cc, pts, false);
            sender.send_to(&datagram, target).await.unwrap();
            seq = seq.wrapping_add(1);
            cc = cc.wrapping_add(1) & 0x0F;
            pts += 3_000;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        drain_control(&mut control, &mut seen, Duration::from_secs(2)).await;
        let after = seen.matches("wfd_idr_request").count();
        assert!(
            after > before,
            "a reference frame was lost and the sink never asked for a new one; on an \
             AOSP source that is fifteen seconds of a frozen, wrong picture:\n{seen}"
        );

        session.abort();
    }

    #[tokio::test]
    async fn binding_a_taken_port_fails_before_anything_is_advertised() {
        // The port in M3 is a promise; discovering it is taken afterwards means a source
        // sending RTP into a closed socket.
        let first = bind_rtp(0).await.unwrap();
        let port = first.local_addr().unwrap().port();
        assert!(bind_rtp(port).await.is_err());
    }

    #[tokio::test]
    async fn the_adapter_advertises_a_wifi_direct_beacon_and_nothing_ip() {
        struct Idle;
        #[async_trait::async_trait]
        impl castaway_core::MiracastBackend for Idle {
            async fn run(
                self: Arc<Self>,
                _sink: SessionSink,
            ) -> Result<(), castaway_core::CoreError> {
                Ok(())
            }
        }
        let adapter = MiracastAdapter::new(Arc::new(Idle), "castaway".to_owned());
        let ads = castaway_core::SourceAdapter::advertisements(&adapter);
        assert!(matches!(
            ads.as_slice(),
            [castaway_core::Advertisement::WifiDirect { .. }]
        ));
    }
}
