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

use std::net::SocketAddr;
use std::sync::Arc;

use castaway_core::{
    Advertisement, CoreError, ProtocolKind, SessionEvent, SessionSink, SourceAdapter,
};
use substrate_rtsp::rtsp_types::headers::{HeaderName, CONTENT_TYPE, CSEQ, SERVER};
use substrate_rtsp::rtsp_types::{Message, Response, StatusCode, Version};
use substrate_rtsp::{ByteTransform, Identity, RtspMessage};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::advert::{AirPlayIdentity, AIRPLAY_PORT, SOURCE_VERSION};
use crate::error::AirPlayError;
use crate::session::AirPlaySession;

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

/// A listening AirPlay receiver: one TCP listener, one [`AirPlaySession`] per
/// connection.
pub struct AirPlayReceiver {
    identity: AirPlayIdentity,
    addr: SocketAddr,
}

impl AirPlayReceiver {
    /// Build a receiver for `identity` on the default AirPlay port.
    #[must_use]
    pub fn new(identity: AirPlayIdentity) -> Self {
        Self {
            identity,
            addr: SocketAddr::from(([0, 0, 0, 0], AIRPLAY_PORT)),
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

        let mut session = AirPlaySession::new(self.identity.clone());
        // Identity today: the encrypted control channel starts only after pair-verify,
        // which is not implemented (Q1). The seam is here so landing pairing is a swap
        // of this transform, not a rewrite of the loop.
        let mut transform: Box<dyn ByteTransform> = Box::new(Identity);

        if let Err(e) = pump(&mut stream, &mut session, &mut *transform, &sink, peer).await {
            warn!(%peer, error = %e, "AirPlay connection ended with an error");
        }
        // A dropped connection is a finished session, however it ended: tell the manager
        // so the pipeline doesn't hold the screen for a sender that walked away.
        let _ = sink.emit(SessionEvent::End).await;
        let _ = stream.shutdown().await;
        info!(%peer, "AirPlay sender disconnected");
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

/// Read requests until the peer closes, folding each through the session.
async fn pump(
    stream: &mut TcpStream,
    session: &mut AirPlaySession,
    transform: &mut dyn ByteTransform,
    sink: &SessionSink,
    peer: SocketAddr,
) -> Result<(), AirPlayError> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 4096];
    loop {
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| AirPlayError::Connection(e.to_string()))?;
        if n == 0 {
            return Ok(()); // clean EOF
        }
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

            if let Some(event) = reply.event {
                let ended = matches!(event, SessionEvent::End);
                sink.emit(event)
                    .await
                    .map_err(|e| AirPlayError::Connection(e.to_string()))?;
                if ended {
                    return Ok(());
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
    let resp = session.handle(&method, &path, req.body())?;

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
            instance: svc.instance,
            port: self.addr.port(),
            txt: svc.txt,
        })
        .collect()
    }

    async fn run(self: Arc<Self>, sink: SessionSink) -> Result<(), CoreError> {
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
        }
    }

    #[test]
    fn advertises_both_services_on_the_one_port_it_binds() {
        let r = AirPlayReceiver::new(identity()).with_addr(SocketAddr::from(([0, 0, 0, 0], 17000)));
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
        let r = AirPlayReceiver::new(identity());
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
            AirPlayReceiver::new(identity()).kind(),
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
    fn pairing_is_refused_rather_than_faked() {
        let mut session = AirPlaySession::new(identity());
        let raw = b"POST /pair-setup RTSP/1.0\r\nCSeq: 3\r\n\r\n";
        let (msg, _) = substrate_rtsp::parse(raw).unwrap().unwrap();
        let peer = SocketAddr::from(([10, 0, 0, 9], 5000));
        let reply = dispatch(&mut session, &msg, peer).unwrap().unwrap();
        let bytes = substrate_rtsp::write(&reply.message).unwrap();
        assert!(String::from_utf8_lossy(&bytes).starts_with("RTSP/1.0 501"));
    }
}
