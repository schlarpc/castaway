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
use tracing::{debug, info};

use crate::error::MiracastError;
use crate::media::MediaReceiver;
use crate::params::SinkCapabilities;
use crate::session::{
    NegotiatedConfig, OutgoingRequest, OutgoingResponse, SinkOutput, WfdRequest, WfdResponse,
    WfdSession,
};

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
    let mut inbound = Vec::new();
    let mut datagram = vec![0u8; DATAGRAM_BUF];
    let mut last_idr = tokio::time::Instant::now()
        .checked_sub(IDR_MIN_INTERVAL)
        .unwrap_or_else(tokio::time::Instant::now);
    let mut read_buf = vec![0u8; 8192];

    info!(%peer, "WFD session opened");
    loop {
        tokio::select! {
            // Reading bytes is all that happens in the branch; the parse is below.
            read = control.read(&mut read_buf) => {
                let n = read.map_err(|e| MiracastError::Connection(e.to_string()))?;
                if n == 0 {
                    debug!(%peer, "source closed the control connection");
                    break;
                }
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
                    if apply(&outputs, &mut control, &mut planes, &sink, &peer).await? {
                        return Ok(());
                    }
                }
            }
            received = rtp.recv(&mut datagram) => {
                let n = received.map_err(|e| MiracastError::Connection(e.to_string()))?;
                let frames = media.push_datagram(Bytes::copy_from_slice(&datagram[..n]));
                if let Some(planes) = &mut planes {
                    for frame in frames {
                        planes.deliver(frame);
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
                            debug!("asking the source for an IDR");
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
    let _ = sink.emit(SessionEvent::End).await;
    Ok(())
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
    sink: &SessionSink,
    peer: &SocketAddr,
) -> Result<bool, MiracastError> {
    for output in outputs {
        match output {
            SinkOutput::Respond(resp) => write_response(control, resp).await?,
            SinkOutput::Send(req) => write_request(control, req).await?,
            SinkOutput::MediaStarted(config) => {
                *planes = Some(start_media(config, sink, peer).await?);
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
}

impl MiracastAdapter {
    /// An adapter driving `backend`, advertising `device_name` in the P2P beacon.
    #[must_use]
    pub fn new(backend: Arc<dyn castaway_core::MiracastBackend>, device_name: String) -> Self {
        Self {
            backend,
            device_name,
        }
    }
}

#[async_trait::async_trait]
impl castaway_core::SourceAdapter for MiracastAdapter {
    fn kind(&self) -> castaway_core::ProtocolKind {
        crate::kind()
    }

    fn advertisements(&self) -> Vec<castaway_core::Advertisement> {
        // Not mDNS and not SSDP: this one is an 802.11 beacon, which the shared responders
        // cannot carry (architecture §1e).
        vec![castaway_core::Advertisement::WifiDirect {
            device_name: self.device_name.clone(),
        }]
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), castaway_core::CoreError> {
        Arc::clone(&self.backend).run(sink).await
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

    /// A scripted WFD *source*: it accepts the sink's connection and walks M1→M7, then
    /// sends one RTP datagram of transport stream. Everything a real source does over a
    /// socket, with none of the radio.
    async fn scripted_source(
        listener: tokio::net::TcpListener,
        rtp_port: u16,
    ) -> Result<(), std::io::Error> {
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
                return Ok(());
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
                return Ok(());
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            seen.contains(&format!("unicast {rtp_port} 0 mode=play")),
            "the advertised port must be the one we bound: {seen}"
        );

        // M4 and M5 in one segment — which real sources do, and which is the framing
        // hazard a recv()-per-message loop fails on.
        let m4 = "wfd_video_formats: 00 00 03 10 00000100 00000000 00000000 00 0000 0000 00 \
                  none none\r\nwfd_audio_codecs: AAC 00000001 00\r\n\
                  wfd_presentation_URL: rtsp://127.0.0.1/wfd1.0/streamid=0 none\r\n";
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
                return Ok(());
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(
            seen.contains(&format!("client_port={rtp_port}")),
            "SETUP must name the bound port: {seen}"
        );
        send(
            &mut stream,
            "RTSP/1.0 200 OK\r\nCSeq: 2\r\nSession: 4242;timeout=30\r\n\
             Transport: RTP/AVP/UDP;unicast;client_port=1028;server_port=19000\r\n\r\n"
                .to_owned(),
        )
        .await?;

        seen.clear();
        while !seen.contains("PLAY") {
            let n = stream.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        send(
            &mut stream,
            "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: 4242\r\n\r\n".to_owned(),
        )
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_scripted_source_drives_the_sink_from_m1_to_a_running_mirror() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = listener.local_addr().unwrap();
        let rtp = bind_rtp(0).await.unwrap();
        let rtp_port = rtp.local_addr().unwrap().port();

        let source = tokio::spawn(scripted_source(listener, rtp_port));

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
