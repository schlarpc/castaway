//! The AirPlay RTSP socket actor (OPEN-QUESTIONS Q15): the thin async shell around the
//! pure [`AirPlaySession`]. It owns the listeners and one task per sender connection,
//! and per request does exactly three things — parse, hand to the session, write back
//! what the session decided. No protocol decisions live here (ground rule 3).
//!
//! One listener, one dispatch. `_airplay._tcp` and `_raop._tcp` are both advertised on
//! 7000 and answered by the same socket, because that is what every reference
//! implementation does — shairport-sync and UxPlay each register both services on a
//! single port, and airplay2-receiver publishes no RAOP service at all. This used to
//! bind a second listener on 7011, which is not a control port at all: 7011 is the
//! AirPlay 1 **UDP timing** port.
//!
//! So which media plane a session belongs to is not something the socket can say. It is
//! decided by what the sender negotiates — an `ANNOUNCE` with an SDP body is the audio
//! flow, a `SETUP` carrying a stream of type 110 is mirroring — and it is the session
//! state machine's business to know, not the actor's.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use castaway_core::{
    Advertisement, AudioFormat, CoreError, EncodedFrame, FrameSource, MediaPorts, ProtocolKind,
    SessionEvent, SessionSink, SourceAdapter,
};
use substrate_rtsp::rtsp_types::headers::{HeaderName, CONTENT_TYPE, CSEQ, SERVER};
use substrate_rtsp::rtsp_types::{Message, Response, StatusCode, Version};
use substrate_rtsp::{ByteTransform, Identity, RtspMessage};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::advert::{AirPlayIdentity, AIRPLAY_PORT, SOURCE_VERSION};
use crate::audio::{AudioOutput, AudioStream};
use crate::clock::{NtpTime, ResendTracker, StreamOrigin, TimingClient};
use crate::diagnostics::SessionDiagnostics;
use crate::error::AirPlayError;
use crate::mirror::{MirrorKeys, MirrorOutput, MirrorStream};
use crate::session::{AirPlayRequest, AirPlaySession};
use crate::transport::{ReceiverPorts, SenderPorts};

/// Cap on a single buffered RTSP message. `/fp-setup` and plist bodies are a few KiB;
/// `Content-Length` is attacker-controlled, so it gets a bound rather than a buffer that
/// grows to whatever a sender claims.
const MAX_MESSAGE: usize = 1 << 20;

/// What senders see in the `Server` header. AirPlay clients sniff this for the feature
/// generation, so it tracks the `srcvers` in the mDNS advertisement — shairport-sync
/// parses exactly this number out of a sender's `User-Agent` to decide how to compute
/// latency, and senders do the reverse to us. Built from [`SOURCE_VERSION`] so the two
/// cannot drift; they were `377.40.00` here and `220.68` there a moment ago.
const SERVER_HEADER_PREFIX: &str = "AirTunes/";

/// Bytes as hex, for the `trace`-level wire capture.
///
/// A whole message per line and no truncation: a capture that elides the middle of a
/// body is not a capture you can replay as a fixture, which is the only reason to take
/// one (ground rule 9).
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A listening AirPlay receiver: one TCP listener, one [`AirPlaySession`] per
/// connection.
pub struct AirPlayReceiver {
    identity: AirPlayIdentity,
    addr: SocketAddr,
    media_ports: MediaPorts,
}

impl AirPlayReceiver {
    /// Build a receiver for `identity` on the default AirPlay port, binding each
    /// session's media sockets (RAOP audio/control/timing UDP, the mirroring data
    /// channel TCP) according to `media_ports`.
    ///
    /// A required argument rather than a defaulted builder: the ephemeral fallback is
    /// invisible to a firewall, so choosing it has to be written down at the call site.
    #[must_use]
    pub fn new(identity: AirPlayIdentity, media_ports: MediaPorts) -> Self {
        Self {
            identity,
            addr: SocketAddr::from(([0, 0, 0, 0], AIRPLAY_PORT)),
            media_ports,
        }
    }

    /// Override the listen address (tests bind an ephemeral port).
    #[must_use]
    pub fn with_addr(mut self, addr: SocketAddr) -> Self {
        self.addr = addr;
        self
    }

    /// Serve one accepted connection to completion.
    async fn serve(&self, mut stream: TcpStream, peer: SocketAddr, sink: SessionSink) {
        // Nagle would sit on the small control messages this protocol is made of.
        if let Err(e) = stream.set_nodelay(true) {
            debug!(%peer, error = %e, "could not disable Nagle");
        }
        info!(%peer, "AirPlay sender connected");

        let mut audio_sockets: Option<AudioSockets> = None;
        let mut session = AirPlaySession::new(self.identity.clone());
        // Bind before serving so a SETUP answers with ports that are already listening.
        let local_ip = stream
            .local_addr()
            .map_or(IpAddr::from([0, 0, 0, 0]), |a| a.ip());
        session.set_local_addr(local_ip);
        // The mirroring data channel is a second TCP listener, bound now for the same
        // reason the UDP sockets are: a SETUP has to answer with a port that already
        // exists, not one we intend to create.
        let mut mirror_listener = match bind_tcp_media(local_ip, self.media_ports).await {
            Ok(listener) => match listener.local_addr() {
                Ok(addr) => {
                    session.set_mirror_data_port(addr.port());
                    Some(listener)
                }
                Err(e) => {
                    warn!(%peer, error = %e, "mirror listener has no address");
                    None
                }
            },
            Err(e) => {
                warn!(%peer, error = %e, "could not bind the mirroring data port");
                None
            }
        };
        match AudioSockets::bind(local_ip, self.media_ports).await {
            Ok((sockets, ports)) => {
                session.set_local_ports(ports);
                audio_sockets = Some(sockets);
            }
            // Without them a SETUP can only be refused, but /info and the handshake
            // still work — so the connection is served rather than dropped.
            Err(e) => warn!(%peer, error = %e, "could not bind the RAOP audio sockets"),
        }
        // Identity today: the encrypted control channel starts only after pair-verify,
        // which is not implemented (Q1). The seam is here so landing pairing is a swap
        // of this transform, not a rewrite of the loop.
        let mut transform: Box<dyn ByteTransform> = Box::new(Identity);

        match pump(
            &mut stream,
            &mut session,
            &mut *transform,
            &sink,
            peer,
            &mut audio_sockets,
            &mut mirror_listener,
        )
        .await
        {
            // Which of these it was is the difference between "the sender said it was
            // done" and "the sender walked off mid-conversation", and the two want
            // opposite things looked at next. They used to log the same line.
            Ok(PumpEnd::PeerClosed) => {
                info!(%peer, "AirPlay sender disconnected: the peer closed the connection")
            }
            Ok(PumpEnd::EndedByRequest) => {
                info!(%peer, "AirPlay sender disconnected: the session was ended by request")
            }
            Err(e) => warn!(%peer, error = %e, "AirPlay connection ended with an error"),
        }
        // A dropped connection is a finished session, however it ended: tell the manager
        // so the pipeline doesn't hold the screen for a sender that walked away.
        let _ = sink.emit(SessionEvent::End).await;
        let _ = stream.shutdown().await;
    }

    /// Accept connections on `listener` until it fails fatally, serving each in its own
    /// task tagged with the peer and the channel it arrived on.
    async fn accept_loop(self: Arc<Self>, listener: TcpListener, sink: SessionSink) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                // One failed accept (fd limit, RST between accept and return) shouldn't
                // take the listener down; the next sender deserves a try.
                Err(e) => {
                    warn!(error = %e, "AirPlay accept failed");
                    continue;
                }
            };
            let this = Arc::clone(&self);
            let conn_sink = sink.with_instance(peer.to_string());
            tokio::spawn(async move { this.serve(stream, peer, conn_sink).await });
        }
    }
}

/// How a served connection finished, when it finished cleanly.
///
/// The two are not the same event and must not read as one in a log: a sender that sent
/// `TEARDOWN` processed everything we answered and chose to stop, while one that just
/// closed the socket rejected something we said. Which of those happened is the first
/// question asked of any session that dies in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PumpEnd {
    /// The peer closed the connection (clean EOF), saying nothing about why.
    PeerClosed,
    /// A request ended the session — `TEARDOWN`, normally.
    EndedByRequest,
}

/// Read requests until the peer closes, folding each through the session.
async fn pump(
    stream: &mut TcpStream,
    session: &mut AirPlaySession,
    transform: &mut dyn ByteTransform,
    sink: &SessionSink,
    peer: SocketAddr,
    audio_sockets: &mut Option<AudioSockets>,
    mirror_listener: &mut Option<TcpListener>,
) -> Result<PumpEnd, AirPlayError> {
    // The sending half of the mirror's audio channel, created with the video and filled
    // in later: a sender negotiates mirroring audio *after* its video is already
    // flowing, so the channel has to exist before there is anything to put in it.
    let mut mirror_audio_tx: Option<mpsc::Sender<EncodedFrame>> = None;
    // Shared by the mirror's two planes so they present on one timeline.
    let mirror_origin = Arc::new(StreamOrigin::new());
    // Counters every task in this session writes to, and a reporter reads. See
    // `diagnostics` for why this exists rather than debug logging.
    let diagnostics = Arc::new(SessionDiagnostics::new());
    let reporter = tokio::spawn(report_session(Arc::clone(&diagnostics)));
    // FLUSH arrives on the RTSP connection and has to reach the audio task, which is
    // elsewhere. `watch` rather than a channel: only the newest flush point matters, and
    // a task that missed one because it was busy should not then act on a stale one.
    let (flush_tx, flush_rx) = tokio::sync::watch::channel(None);
    let _reporter = ReporterGuard(reporter);
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| AirPlayError::Connection(e.to_string()))?;
        if n == 0 {
            return Ok(PumpEnd::PeerClosed);
        }
        // The raw control channel, for when nothing above the bytes explains what a
        // sender did. `trace` because it is a per-message hexdump of a live session —
        // this is the RE capture facility (ground rule 9), not diagnostics.
        trace!(%peer, bytes = %hex(&chunk[..n]), "airplay rx");
        // Decrypt per chunk, not per accumulated buffer: the transform is a stream
        // cipher with position, so re-running it over bytes already decrypted would
        // desynchronize it.
        let mut cleartext = chunk[..n].to_vec();
        transform
            .decrypt_inbound(&mut cleartext)
            .map_err(|e| AirPlayError::Connection(e.to_string()))?;
        buf.extend_from_slice(&cleartext);

        // A message that claims more than MAX_MESSAGE is never going to arrive; drop the
        // connection rather than buffering toward OOM.
        if buf.len() > MAX_MESSAGE {
            return Err(AirPlayError::Connection(format!(
                "message exceeds {MAX_MESSAGE} bytes"
            )));
        }

        while let Some((msg, consumed)) =
            substrate_rtsp::parse(&buf).map_err(|e| AirPlayError::Connection(e.to_string()))?
        {
            buf.drain(..consumed);
            let Some(reply) = dispatch(session, &msg, peer)? else {
                continue;
            };
            let mut bytes = substrate_rtsp::write(&reply.message)
                .map_err(|e| AirPlayError::Connection(e.to_string()))?;
            trace!(%peer, bytes = %hex(&bytes), "airplay tx");
            transform
                .encrypt_outbound(&mut bytes)
                .map_err(|e| AirPlayError::Connection(e.to_string()))?;
            stream
                .write_all(&bytes)
                .await
                .map_err(|e| AirPlayError::Connection(e.to_string()))?;
            stream
                .flush()
                .await
                .map_err(|e| AirPlayError::Connection(e.to_string()))?;

            // RECORD is the sender saying it is about to stream. Starting the audio
            // task is driven from here rather than from the session, because the pure
            // core owns no sockets and should not be handed a channel just to hand one
            // back (ground rule 3).
            if let Some(point) = session.take_flush() {
                let _ = flush_tx.send(Some(point));
            }

            if session.is_recording() {
                if let Some(sockets) = audio_sockets.take() {
                    start_audio(
                        session,
                        sockets,
                        flush_rx.clone(),
                        Arc::clone(&diagnostics),
                        sink,
                        peer,
                    )
                    .await;
                }
            }

            // A mirroring SETUP that named the stream leaves the keys behind; the
            // sender is about to dial the data port we advertised.
            // Audio negotiated alongside a mirroring session. It rides the same UDP
            // sockets the AirPlay 1 flow uses, and feeds the channel already handed to
            // the pipeline with the video — *not* a session of its own, which would
            // preempt the picture it belongs to.
            if let (Some(params), Some(tx)) = (session.take_mirror_audio(), mirror_audio_tx.take())
            {
                if let Some(sockets) = audio_sockets.take() {
                    let stream = AudioStream::new(&params);
                    info!(%peer, link = %params.describe(), "AirPlay mirroring audio starting");
                    let _ = sink
                        .emit(SessionEvent::SourceInfo(
                            castaway_core::SourceDescription::new().with_link(params.describe()),
                        ))
                        .await;
                    tokio::spawn(run_audio(
                        sockets,
                        stream,
                        tx,
                        peer.ip(),
                        session.sender_ports(),
                        flush_rx.clone(),
                        Arc::clone(&diagnostics),
                    ));
                }
            }

            if let Some(keys) = session.take_mirror_keys() {
                // One data channel per session: the listener is *moved* into the task.
                // Re-binding by address instead would race whatever took the port in
                // between, and we have already told the sender that number.
                if let Some(listener) = mirror_listener.take() {
                    mirror_audio_tx = start_mirroring(
                        *keys,
                        listener,
                        Arc::clone(&mirror_origin),
                        Arc::clone(&diagnostics),
                        sink,
                        peer,
                    )
                    .await;
                }
            }

            if let Some(event) = reply.event {
                let ended = matches!(event, SessionEvent::End);
                sink.emit(event)
                    .await
                    .map_err(|e| AirPlayError::Connection(e.to_string()))?;
                if ended {
                    return Ok(PumpEnd::EndedByRequest);
                }
            }
        }
    }
}

/// A serialized reply plus whatever event the session wants forwarded with it.
struct Reply {
    message: RtspMessage,
    event: Option<SessionEvent>,
}

/// Fold one parsed message through the session. Returns `None` for anything that isn't
/// a request we answer (responses a sender sent us, interleaved data).
fn dispatch(
    session: &mut AirPlaySession,
    msg: &RtspMessage,
    peer: SocketAddr,
) -> Result<Option<Reply>, AirPlayError> {
    let Message::Request(req) = msg else {
        // Senders don't send us responses, and interleaved RTP arrives on its own UDP
        // ports — either one means we misparsed or the peer is confused.
        debug!(%peer, "ignoring non-request RTSP message");
        return Ok(None);
    };
    let method = substrate_rtsp::method_name(req.method()).to_string();
    let path = substrate_rtsp::request_path(req);
    // Collected rather than borrowed: the protocol puts load-bearing values in headers
    // (`Transport`, `Apple-Challenge`, `RTP-Info`) and the pure core has to see them
    // without depending on `rtsp-types`.
    let headers: Vec<(String, String)> = req
        .headers()
        .map(|(name, value)| (name.as_str().to_string(), value.as_str().to_string()))
        .collect();
    let resp = session.handle(&AirPlayRequest {
        method: &method,
        path: &path,
        headers: &headers,
        body: req.body(),
    })?;

    let mut builder = Response::builder(Version::V1_0, StatusCode::from(resp.status))
        .header(SERVER, format!("{SERVER_HEADER_PREFIX}{SOURCE_VERSION}"));
    // Echoing CSeq is what lets the sender match this reply to its request; without it
    // every sender treats the exchange as timed out.
    if let Some(cseq) = substrate_rtsp::cseq(msg) {
        builder = builder.header(CSEQ, cseq.to_string());
    }
    for (name, value) in resp.headers {
        // `name` is `&'static str` by construction (see `AirPlayResponse::headers`), so
        // this allocates nothing and the ASCII check can only fail on a literal we wrote.
        let name = HeaderName::from_static_str(name)
            .map_err(|_| AirPlayError::Malformed("non-ASCII response header name"))?;
        builder = builder.header(name, value);
    }
    if let Some(content_type) = resp.content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    Ok(Some(Reply {
        message: builder.build(resp.body).into(),
        event: resp.event,
    }))
}

#[async_trait::async_trait]
impl SourceAdapter for AirPlayReceiver {
    fn kind(&self) -> ProtocolKind {
        ProtocolKind::AirPlay
    }

    fn advertisements(&self) -> Vec<Advertisement> {
        // The port comes from the socket that will actually be bound, not the advert's
        // default: an advertisement naming a port nothing answers is the failure this
        // prevents.
        [
            self.identity.airplay_service(),
            self.identity.raop_service(),
        ]
        .into_iter()
        .map(|svc| Advertisement::MdnsService {
            ty: svc.service_type,
            instance: svc.instance.into_string(),
            port: self.addr.port(),
            txt: svc.txt,
        })
        .collect()
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "registered: the airplay/tcp 7000 entry in crates/app/src/surface.rs"
        )]
        let listener = TcpListener::bind(self.addr)
            .await
            .map_err(|e| CoreError::Adapter(format!("binding AirPlay on {}: {e}", self.addr)))?;
        info!(addr = %self.addr, "AirPlay RTSP listener ready");
        self.accept_loop(listener, sink).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn identity() -> AirPlayIdentity {
        AirPlayIdentity {
            name: "Lab TV".into(),
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            host: "castaway".into(),
            pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
            offer_hevc: false,
            mirror_height: 1080,
        }
    }

    #[test]
    fn advertises_both_services_on_the_one_port_it_binds() {
        let r = AirPlayReceiver::new(identity(), MediaPorts::Ephemeral)
            .with_addr(SocketAddr::from(([0, 0, 0, 0], 17000)));
        let ads = r.advertisements();
        assert_eq!(ads.len(), 2);
        let ports: Vec<u16> = ads
            .iter()
            .map(|ad| match ad {
                Advertisement::MdnsService { port, .. } => *port,
                other => panic!("expected an mDNS advertisement, got {other:?}"),
            })
            .collect();
        // Both services, one port — and the port actually bound, not the default. An
        // advertisement naming a port nothing answers is the failure this prevents.
        assert_eq!(ports, vec![17000, 17000]);
    }

    #[test]
    fn raop_keeps_its_deviceid_at_name_instance_convention() {
        let r = AirPlayReceiver::new(identity(), MediaPorts::Ephemeral);
        let ads = r.advertisements();
        match &ads[1] {
            Advertisement::MdnsService { ty, instance, .. } => {
                assert_eq!(ty, crate::advert::RAOP_SERVICE);
                assert_eq!(instance, "AABBCCDDEEFF@Lab TV");
            }
            other => panic!("expected an mDNS advertisement, got {other:?}"),
        }
    }

    #[test]
    fn kind_is_airplay() {
        assert_eq!(
            AirPlayReceiver::new(identity(), MediaPorts::Ephemeral).kind(),
            ProtocolKind::AirPlay
        );
    }

    #[test]
    fn a_bare_path_get_info_is_answered_with_a_plist() {
        let mut session = AirPlaySession::new(identity());
        let raw = b"GET /info RTSP/1.0\r\nCSeq: 2\r\n\r\n";
        let (msg, _) = substrate_rtsp::parse(raw).unwrap().unwrap();
        let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
        let reply = dispatch(&mut session, &msg, peer).unwrap().unwrap();
        let bytes = substrate_rtsp::write(&reply.message).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("RTSP/1.0 200"), "{text}");
        // Without the echoed CSeq a real sender treats the exchange as timed out.
        assert!(text.contains("CSeq: 2"), "{text}");
        assert!(text.contains(crate::session::APPLE_PLIST_MIME), "{text}");
    }

    #[test]
    fn teardown_reports_the_end_of_the_session() {
        let mut session = AirPlaySession::new(identity());
        let raw = b"TEARDOWN rtsp://10.0.0.1:7000/1 RTSP/1.0\r\nCSeq: 9\r\n\r\n";
        let (msg, _) = substrate_rtsp::parse(raw).unwrap().unwrap();
        let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
        let reply = dispatch(&mut session, &msg, peer).unwrap().unwrap();
        assert!(matches!(reply.event, Some(SessionEvent::End)));
    }

    #[test]
    fn legacy_pair_setup_answers_with_our_identity() {
        // The advertisement promises bit 27, so this endpoint must answer — a 501 here
        // is the receiver refusing the regime it just asked for.
        let mut session = AirPlaySession::new(identity());
        let raw = b"POST /pair-setup RTSP/1.0\r\nCSeq: 3\r\n\r\n";
        let (msg, _) = substrate_rtsp::parse(raw).unwrap().unwrap();
        let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
        let reply = dispatch(&mut session, &msg, peer).unwrap().unwrap();
        let bytes = substrate_rtsp::write(&reply.message).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("RTSP/1.0 200"), "{text}");
    }

    #[test]
    fn homekit_pairing_is_still_refused_rather_than_faked() {
        // `X-Apple-HKP` marks a HomeKit flow, whose bits we do not advertise and whose
        // SRP/ChaCha channel does not exist here. 501 is the honest answer.
        let mut session = AirPlaySession::new(identity());
        let raw = b"POST /pair-setup RTSP/1.0\r\nCSeq: 3\r\nX-Apple-HKP: 4\r\n\r\n";
        let (msg, _) = substrate_rtsp::parse(raw).unwrap().unwrap();
        let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
        let reply = dispatch(&mut session, &msg, peer).unwrap().unwrap();
        let bytes = substrate_rtsp::write(&reply.message).unwrap();
        assert!(String::from_utf8_lossy(&bytes).starts_with("RTSP/1.0 501"));
    }
}

/// The three UDP sockets one RAOP audio session runs on.
///
/// Bound *before* the connection is served, so a `SETUP` can answer with ports that are
/// already listening. The alternative — answering with numbers we intend to bind — is
/// how a receiver ends up advertising a port nothing is on, and the sender's audio goes
/// into the void with the RTSP exchange looking perfect.
struct AudioSockets {
    audio: UdpSocket,
    control: UdpSocket,
    timing: UdpSocket,
}

impl AudioSockets {
    /// Bind all three on `host`, inside the receiver's media port policy.
    async fn bind(host: IpAddr, media_ports: MediaPorts) -> std::io::Result<(Self, ReceiverPorts)> {
        let audio = bind_udp_media(host, media_ports).await?;
        let control = bind_udp_media(host, media_ports).await?;
        let timing = bind_udp_media(host, media_ports).await?;
        let ports = ReceiverPorts {
            audio: audio.local_addr()?.port(),
            control: control.local_addr()?.port(),
            timing: timing.local_addr()?.port(),
        };
        Ok((
            Self {
                audio,
                control,
                timing,
            },
            ports,
        ))
    }
}

/// Whether a failed bind means "this port is taken, try the next one".
///
/// `AddrInUse` is the ordinary collision — a sibling socket from the same range, or an
/// unrelated process. `PermissionDenied` is Windows: WSAEACCES is what its excluded
/// port ranges (`netsh interface … show excludedportrange`) answer with, and treating
/// it as fatal would let one reserved port poison the whole range.
fn is_port_taken(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
    )
}

/// The error when every candidate port is taken.
fn range_exhausted(media_ports: MediaPorts) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!("no free port in the media port range {media_ports}"),
    )
}

/// Bind a UDP socket on the first free candidate port of `media_ports`.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the airplay/udp [media_ports] entry in crates/app/src/surface.rs"
)]
async fn bind_udp_media(host: IpAddr, media_ports: MediaPorts) -> std::io::Result<UdpSocket> {
    for port in media_ports.candidates() {
        match UdpSocket::bind(SocketAddr::new(host, port)).await {
            Ok(socket) => return Ok(socket),
            Err(e) if is_port_taken(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(range_exhausted(media_ports))
}

/// Bind a TCP listener on the first free candidate port of `media_ports`.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the airplay/tcp [media_ports] entry in crates/app/src/surface.rs"
)]
async fn bind_tcp_media(host: IpAddr, media_ports: MediaPorts) -> std::io::Result<TcpListener> {
    for port in media_ports.candidates() {
        match TcpListener::bind(SocketAddr::new(host, port)).await {
            Ok(listener) => return Ok(listener),
            Err(e) if is_port_taken(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(range_exhausted(media_ports))
}

/// Receive audio, control and timing datagrams until the session ends.
///
/// One task owns the depacketiser, rather than one per socket, so [`AudioStream`] needs
/// no lock: `select!` only decides which socket produced bytes: every protocol decision
/// still happens in the pure core (ground rule 3).
async fn run_audio(
    sockets: AudioSockets,
    mut stream: AudioStream,
    frames: mpsc::Sender<EncodedFrame>,
    peer_ip: IpAddr,
    sender_ports: Option<SenderPorts>,
    mut flush: tokio::sync::watch::Receiver<Option<crate::audio::FlushPoint>>,
    diagnostics: Arc<SessionDiagnostics>,
) {
    let mut timing = TimingClient::new();
    let mut resends = ResendTracker::new();
    let timing_peer = sender_ports.map(|p| SocketAddr::new(peer_ip, p.timing));
    let control_peer = sender_ports.map(|p| SocketAddr::new(peer_ip, p.control));
    let mut probe =
        tokio::time::interval(std::time::Duration::from_millis(timing.next_interval_ms()));
    // The first tick is immediate, which is what we want: nothing converts to local time
    // until a round trip completes, so the sooner the first probe goes the better.
    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut our_resend_seq: u16 = 0;
    // One buffer per socket: `select!` polls all three branches, so they cannot share
    // a single mutable borrow.
    let mut audio_buf = vec![0u8; 2048];
    let mut control_buf = vec![0u8; 2048];
    let mut timing_buf = vec![0u8; 2048];
    loop {
        // Which socket, and how many bytes. Nothing is parsed here.
        let (which, len) = tokio::select! {
            r = sockets.audio.recv_from(&mut audio_buf) => match r {
                Ok((n, _)) => (Socket::Audio, n),
                Err(e) => { warn!(error = %e, "AirPlay audio socket failed"); return; }
            },
            r = sockets.control.recv_from(&mut control_buf) => match r {
                Ok((n, _)) => (Socket::Control, n),
                Err(e) => { warn!(error = %e, "AirPlay control socket failed"); return; }
            },
            r = sockets.timing.recv_from(&mut timing_buf) => match r {
                Ok((n, _)) => (Socket::Timing, n),
                Err(e) => { warn!(error = %e, "AirPlay timing socket failed"); return; }
            },
            () = frames.closed() => {
                debug!("AirPlay audio consumer went away; stopping the receive loop");
                return;
            }
            Ok(()) = flush.changed() => {
                if let Some(point) = *flush.borrow_and_update() {
                    // Both halves of the fix: stop playing what the sender has left
                    // behind, and stop asking it to resend across a gap it made on
                    // purpose.
                    stream.flush(point);
                    resends.reset();
                    debug!(rtp = ?point.rtp, "AirPlay audio flushed");
                }
                continue;
            }
            _ = probe.tick() => {
                if let Some(target) = timing_peer {
                    let request = timing.build_request(now_ntp());
                    if let Err(e) = sockets.timing.send_to(&request, target).await {
                        debug!(error = %e, "could not send a timing request");
                    }
                    // The cadence tightens for the first few probes and then backs off.
                    probe = tokio::time::interval(std::time::Duration::from_millis(
                        timing.next_interval_ms(),
                    ));
                    probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    probe.reset();
                }
                continue;
            }
        };

        let datagram = match which {
            Socket::Audio => &audio_buf[..len],
            Socket::Control => &control_buf[..len],
            Socket::Timing => &timing_buf[..len],
        };
        let outcome = match which {
            Socket::Audio => stream.on_audio(datagram),
            Socket::Control => stream.on_control(datagram),
            Socket::Timing => stream.on_timing(datagram),
        };
        match outcome {
            Ok(AudioOutput::Frame {
                frame, sequence, ..
            }) => {
                // Ask for anything the gap revealed, before handing the frame on: the
                // sooner the request goes the likelier the sender still has the packet.
                if let (Some(gap), Some(target)) = (resends.on_packet(sequence), control_peer) {
                    our_resend_seq = our_resend_seq.wrapping_add(1);
                    let request =
                        crate::audio::resend_request(our_resend_seq, gap.first, gap.count);
                    if let Err(e) = sockets.control.send_to(&request, target).await {
                        debug!(error = %e, "could not ask for a resend");
                    } else {
                        diagnostics.resend(gap.count);
                        debug!(first = gap.first, count = gap.count, "asked for a resend");
                    }
                }
                // Latency beats freshness: a full channel means the decoder is behind,
                // and waiting here would stall sync and timing handling too.
                diagnostics.audio_frame(frame.pts);
                if frames.try_send(frame).is_err() {
                    diagnostics.audio_drop();
                    debug!("AirPlay audio buffer full; dropping a packet");
                }
            }
            Ok(AudioOutput::Sync(sync)) => {
                diagnostics.sender_latency(sync.latency_frames());
                debug!(latency = sync.latency_frames(), "AirPlay sync");
            }
            Ok(AudioOutput::TimingReply(reply)) => {
                if let Some(sample) = timing.on_reply(&reply, now_ntp()) {
                    diagnostics.timing_sample(
                        timing.offset_ns().unwrap_or(sample.offset_ns),
                        sample.delay_ns,
                    );
                    debug!(offset_ns = timing.offset_ns(), "AirPlay clock");
                }
            }
            // Neither of these is a fault: the sender saying "not yet", and audio that
            // arrived before a sync packet could place it on the shared timeline.
            Err(crate::audio::AudioError::Priming) => {}
            // Both of these are expected at session start or after a seek, and both are
            // worth counting: a figure that keeps climbing means something is wrong with
            // the anchor or the flush point rather than with one packet.
            Err(crate::audio::AudioError::AwaitingSync) => diagnostics.audio_awaiting_sync(),
            Err(crate::audio::AudioError::Stale) => diagnostics.audio_stale(),
            // One bad datagram off a radio link must not take the music down.
            Err(e) => debug!(error = %e, ?which, %peer_ip, "dropping a datagram"),
        }
    }
}

/// Which socket a datagram arrived on.
#[derive(Debug, Clone, Copy)]
enum Socket {
    Audio,
    Control,
    Timing,
}

/// Hand the negotiated stream to the pipeline and start receiving it.
async fn start_audio(
    session: &AirPlaySession,
    sockets: AudioSockets,
    flush: tokio::sync::watch::Receiver<Option<crate::audio::FlushPoint>>,
    diagnostics: Arc<SessionDiagnostics>,
    sink: &SessionSink,
    peer: SocketAddr,
) {
    let Some(params) = session.announced() else {
        warn!(%peer, "RECORD with nothing announced; not starting audio");
        return;
    };
    let ports = session.sender_ports();
    start_negotiated_audio(params, sockets, ports, flush, diagnostics, sink, peer).await;
}

/// Start an audio receive session for whatever was negotiated.
///
/// Shared between the AirPlay 1 flow and the audio that accompanies mirroring: the two
/// negotiate through completely different messages and then produce the same thing — a
/// codec, a key, and three sockets obeying identical payload rules.
async fn start_negotiated_audio(
    params: &crate::sdp::AnnounceParams,
    sockets: AudioSockets,
    sender_ports: Option<SenderPorts>,
    flush: tokio::sync::watch::Receiver<Option<crate::audio::FlushPoint>>,
    diagnostics: Arc<SessionDiagnostics>,
    sink: &SessionSink,
    peer: SocketAddr,
) {
    let codec = params.codec;
    let Some(format) = AudioFormat::from_hz(codec.sample_rate(), u16::from(codec.channels()))
    else {
        warn!(%peer, "announced format has a zero rate or channel count");
        return;
    };
    // ALAC and AAC-ELD cannot open a decoder without this; PCM needs no decoder at all.
    let config = codec.codec_config().map(bytes::Bytes::from);
    let link = params.describe();

    // Bounded: a full channel means the decoder is behind, and the receive loop drops
    // rather than stalling the sockets that carry sync and timing.
    let (tx, rx) = mpsc::channel(512);
    let stream = AudioStream::new(params);
    info!(%peer, %link, "AirPlay audio starting");
    // Say what was negotiated before the stream event, so the card is populated by the
    // time the first frame lands.
    let _ = sink
        .emit(SessionEvent::SourceInfo(
            castaway_core::SourceDescription::new().with_link(link),
        ))
        .await;
    if sink
        .emit(SessionEvent::Audio {
            source: FrameSource::Encoded(rx),
            format,
            config,
        })
        .await
        .is_err()
    {
        warn!(%peer, "session manager gone; not starting audio");
        return;
    }
    tokio::spawn(run_audio(
        sockets,
        stream,
        tx,
        peer.ip(),
        sender_ports,
        flush,
        diagnostics,
    ));
}

/// The wall clock as an NTP timestamp.
///
/// A wall clock rather than a monotonic one because the value goes on the wire and the
/// sender computes a difference against its own; a monotonic reading would be an offset
/// from an arbitrary boot instant, which is exactly what makes the *sender's* timestamps
/// unusable as absolute times.
fn now_ntp() -> NtpTime {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    NtpTime::from_unix_nanos(nanos)
}

/// Hand the mirroring stream to the pipeline and start receiving it.
///
/// The listener is not awaited here — a sender dials it a moment after reading the
/// `SETUP` reply, and blocking the RTSP pump until it does would stall the very
/// connection the sender is still talking on.
/// Returns the sender for the mirror's audio channel, for whenever it is negotiated.
async fn start_mirroring(
    keys: MirrorKeys,
    listener: TcpListener,
    origin: Arc<StreamOrigin>,
    diagnostics: Arc<SessionDiagnostics>,
    sink: &SessionSink,
    peer: SocketAddr,
) -> Option<mpsc::Sender<EncodedFrame>> {
    let (tx, rx) = mpsc::channel(8);
    // The audio channel is created now even though nothing will feed it until a later
    // SETUP — if one ever comes. A mirror with no audio simply leaves it silent, which
    // costs one idle channel and avoids the alternative: announcing the audio as its own
    // session, which preempts and tears down the picture.
    let (audio_tx, audio_rx) = mpsc::channel(512);
    let format = AudioFormat::from_hz(44_100, 2)?;
    info!(%peer, "AirPlay mirroring starting");
    if sink
        .emit(SessionEvent::Mirror {
            video: FrameSource::Encoded(rx),
            audio: Some(castaway_core::MirrorAudio {
                source: FrameSource::Encoded(audio_rx),
                format,
                // Mirroring offers exactly one audio codec, so unlike the SDP path there
                // is nothing to discover: AAC-ELD, and always this configuration.
                config: Some(bytes::Bytes::from(crate::sdp::AAC_ELD_CONFIG.to_vec())),
            }),
        })
        .await
        .is_err()
    {
        warn!(%peer, "session manager gone; not starting mirroring");
        return None;
    }
    tokio::spawn(run_mirroring(listener, keys, origin, tx, peer, diagnostics));
    Some(audio_tx)
}

/// Accept the sender's data connection and feed frames until it ends.
async fn run_mirroring(
    listener: TcpListener,
    keys: MirrorKeys,
    origin: Arc<StreamOrigin>,
    frames: mpsc::Sender<EncodedFrame>,
    peer: SocketAddr,
    diagnostics: Arc<SessionDiagnostics>,
) {
    let Ok((mut stream, from)) = listener.accept().await else {
        warn!(%peer, "no mirroring data connection arrived");
        return;
    };
    info!(%from, "AirPlay mirroring data channel connected");

    let mut mirror = MirrorStream::new(&keys, origin);
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 16);
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let n = tokio::select! {
            r = stream.read(&mut chunk) => match r {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => { debug!(error = %e, "mirroring read failed"); break }
            },
            () = frames.closed() => {
                debug!("mirroring consumer went away");
                break;
            }
        };
        buf.extend_from_slice(&chunk[..n]);

        let outputs = match mirror.feed(&mut buf) {
            Ok(o) => o,
            // Fatal by design: the keystream is continuous, so a message we cannot
            // frame means everything after it would be noise.
            Err(e) => {
                warn!(error = %e, "mirroring stream lost sync; ending it");
                break;
            }
        };
        for output in outputs {
            match output {
                MirrorOutput::Frame(frame) => {
                    diagnostics.video_frame(frame.pts);
                    // Drop late rather than stall: latency beats freshness, and this is
                    // safe *here* because the frame has already been decrypted — the
                    // keystream has moved on regardless of whether we keep the bytes.
                    if frames.try_send(*frame).is_err() {
                        diagnostics.video_drop();
                        debug!("mirroring buffer full; dropping a frame");
                    }
                }
                MirrorOutput::Geometry(g) => {
                    info!(
                        encoded = ?g.encoded,
                        source = ?g.source,
                        "AirPlay mirroring geometry"
                    );
                }
                MirrorOutput::Suspend => info!("AirPlay mirroring suspended by the sender"),
                MirrorOutput::Resume => info!("AirPlay mirroring resumed"),
            }
        }
    }
    info!(%peer, "AirPlay mirroring ended");
}

/// How often a live session reports itself.
///
/// Slow enough that a two-minute test is a dozen readable lines rather than a wall, and
/// fast enough that a skew that drifts is visibly drifting rather than one number at the
/// end.
const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Log the session's counters until the session ends.
async fn report_session(diagnostics: Arc<SessionDiagnostics>) {
    let mut tick = tokio::time::interval(REPORT_EVERY);
    tick.tick().await;
    loop {
        tick.tick().await;
        let snapshot = diagnostics.snapshot();
        // Nothing has happened on either plane: a control-only connection, which is most
        // of them. Reporting zeroes every five seconds would bury the sessions that
        // matter.
        if snapshot.video_frames == 0 && snapshot.audio_frames == 0 {
            continue;
        }
        snapshot.log();
    }
}

/// Stops the reporter when the connection it belongs to ends.
struct ReporterGuard(tokio::task::JoinHandle<()>);

impl Drop for ReporterGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}
