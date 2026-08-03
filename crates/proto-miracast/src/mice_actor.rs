//! The socket shell for the [MS-MICE] control channel.
//!
//! Everything protocol-shaped is in [`crate::mice`]; this owns only the listener, the
//! framing, and the ordering (ground rule 3). Nothing is parsed inside the `select!`.
//!
//! ## The shape, which is the mirror image of [`crate::actor`]
//!
//! Wi-Fi Direct Miracast has the sink dial the source's RTSP port once the P2P group is
//! up. MICE keeps that — the RTSP half is unchanged, which is the whole reason this is a
//! small addition rather than a second protocol — and puts a *listener* in front of it:
//! the source connects to us on 7250 to say where it is listening, and then we dial it
//! exactly as before.
//!
//! So this module is the part that answers "which address and port", and `run_session` is
//! the part that has always known what to do with the answer.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::BytesMut;
use castaway_core::SessionSink;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use crate::error::{MiceError, MiracastError};
use crate::mice::{
    CloseReason, MiceMessage, MiceOutput, MiceSession, CONTROL_PORT, ESTABLISHMENT_TIMEOUT,
};
use crate::params::SinkCapabilities;

/// The largest control message we will buffer.
///
/// A friendly name caps at 520 bytes and a DTLS token is the only variable field that
/// could be large — and we do not offer DTLS. Anything past this is not a message.
const MAX_MESSAGE: usize = 4096;

/// How often the establishment timer is advanced while waiting on a peer.
const TICK: Duration = Duration::from_millis(250);

/// Serve the MICE control channel until the listener fails.
///
/// One projection at a time, which is the spec's own guidance ([MS-MICE] §3.1.5.8: a
/// second connection while one is established *"SHOULD reject"*) and also this panel's
/// policy — a second source is a second thing on one screen.
///
/// # Errors
/// [`MiracastError::Connection`] if the listener itself fails. A single peer misbehaving
/// closes that peer's channel and is not an error here: the next source should still be
/// able to project.
pub async fn serve(
    listener: TcpListener,
    friendly_name: String,
    caps: SinkCapabilities,
    sink: SessionSink,
) -> Result<(), MiracastError> {
    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|e| MiracastError::Connection(format!("mice accept: {e}")))?;
        info!(%peer, "mice: a source opened the control channel");
        // Awaited rather than spawned, which *is* the one-at-a-time policy: while a
        // projection is running nothing else is accepted, so a second source finds the
        // backlog rather than a second session on the same panel.
        if let Err(e) = serve_one(stream, peer, &friendly_name, &caps, &sink).await {
            warn!(%peer, error = %e, "mice: the control channel ended");
        }
    }
}

/// Drive one control channel to its end.
async fn serve_one(
    mut stream: TcpStream,
    peer: SocketAddr,
    friendly_name: &str,
    caps: &SinkCapabilities,
    sink: &SessionSink,
) -> Result<(), MiracastError> {
    // The control channel's messages are small and each one gates the next step, so
    // coalescing them costs a round trip for nothing.
    let _ = stream.set_nodelay(true);

    let mut session = MiceSession::new(friendly_name);
    let mut buf = BytesMut::with_capacity(MAX_MESSAGE);
    let mut read = vec![0u8; 1024];
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let mut outputs = Vec::new();
        tokio::select! {
            got = stream.read(&mut read) => match got {
                Ok(0) => {
                    debug!(%peer, "mice: the source closed the control channel");
                    return Ok(());
                }
                Ok(n) => {
                    if buf.len() + n > MAX_MESSAGE {
                        return Err(MiracastError::Connection(format!(
                            "mice: {peer} sent more than {MAX_MESSAGE} bytes without a whole \
                             message"
                        )));
                    }
                    buf.extend_from_slice(&read[..n]);
                    // Parsing happens here, after the read resolved — never inside the
                    // `select!`, where a cancelled branch would lose it.
                    outputs.extend(drain(&mut buf, &mut session, peer)?);
                }
                Err(e) => {
                    return Err(MiracastError::Connection(format!("mice read from {peer}: {e}")));
                }
            },
            _ = ticker.tick() => {
                outputs.extend(session.tick(TICK));
            }
        }

        for output in outputs {
            match output {
                MiceOutput::Send(message) => {
                    let bytes = message
                        .encode()
                        .map_err(|e| MiracastError::Connection(format!("mice encode: {e}")))?;
                    stream
                        .write_all(&bytes)
                        .await
                        .map_err(|e| MiracastError::Connection(format!("mice write: {e}")))?;
                }
                MiceOutput::Project {
                    rtsp_port,
                    friendly_name,
                } => {
                    // The whole point of the channel, reached. From here it is the
                    // ordinary WFD session — the source's *address* is the one that
                    // connected to us, which is why this is not read from any TLV: an
                    // address a peer states about itself is one it can state wrongly.
                    let source = SocketAddr::new(peer.ip(), rtsp_port);
                    info!(
                        %source,
                        name = friendly_name.as_deref().unwrap_or("<unnamed>"),
                        "mice: projecting over infrastructure"
                    );
                    session.rtsp_established();
                    project(source, peer.ip(), friendly_name, caps.clone(), sink).await?;
                    // The RTSP session ended, so the projection is over whatever the
                    // control channel thinks.
                    for out in session.stop() {
                        if let MiceOutput::Send(message) = out {
                            if let Ok(bytes) = message.encode() {
                                let _ = stream.write_all(&bytes).await;
                            }
                        }
                    }
                    return Ok(());
                }
                MiceOutput::Close(reason) => {
                    match &reason {
                        // Not a fault: the ordinary end of a projection, and of a control
                        // channel that was only ever going to carry one.
                        CloseReason::SourceStopped => info!(%peer, "mice: {reason}"),
                        other => warn!(%peer, "mice: {other}"),
                    }
                    return Ok(());
                }
            }
        }
    }
}

/// Pull every whole message out of `buf` and feed it to the session.
fn drain(
    buf: &mut BytesMut,
    session: &mut MiceSession,
    peer: SocketAddr,
) -> Result<Vec<MiceOutput>, MiracastError> {
    let mut out = Vec::new();
    loop {
        let size = match MiceMessage::framed_len(buf) {
            Ok(size) => size,
            // Not yet a whole header. Wait for more rather than treating it as an error —
            // this is a stream, and a short read is the ordinary case.
            Err(MiceError::Truncated) => return Ok(out),
            Err(e) => {
                return Err(MiracastError::Connection(format!(
                    "mice framing from {peer}: {e}"
                )))
            }
        };
        if buf.len() < size {
            return Ok(out);
        }
        let message = MiceMessage::decode(&buf[..size]);
        let _ = buf.split_to(size);
        match message {
            Ok(message) => {
                debug!(%peer, message = message.name(), "mice: message");
                out.extend(session.on_message(&message));
            }
            // A message we cannot understand is exactly what the spec says to tear the
            // channel down for, and saying which one is the difference between a
            // diagnosable failure and a source that "just doesn't work".
            Err(e) => {
                return Err(MiracastError::Connection(format!(
                    "mice message from {peer}: {e}"
                )))
            }
        }
        if out
            .iter()
            .any(|o| matches!(o, MiceOutput::Close(_) | MiceOutput::Project { .. }))
        {
            // Nothing after a close or a projection belongs to this channel's read loop.
            return Ok(out);
        }
    }
}

/// Dial the source's RTSP port and run the session, exactly as Wi-Fi Direct does.
async fn project(
    source: SocketAddr,
    peer_ip: IpAddr,
    friendly_name: Option<String>,
    caps: SinkCapabilities,
    sink: &SessionSink,
) -> Result<(), MiracastError> {
    // The port we advertise in M3 has to be one we are already listening on — the RTP
    // socket is bound before the RTSP session starts for the same reason it is on the
    // Wi-Fi Direct path, and it comes from the same capability table.
    let rtp = crate::actor::bind_rtp(caps.client_rtp_ports.port()).await?;
    let control =
        tokio::time::timeout(ESTABLISHMENT_TIMEOUT, crate::actor::connect_control(source))
            .await
            .map_err(|_| {
                MiracastError::Connection(format!("mice: {source} did not accept RTSP in time"))
            })??;
    // Named by whatever the source called itself, falling back to its address: on the
    // panel this is the line that says whose screen is up.
    let described = sink
        .clone()
        .with_instance(friendly_name.unwrap_or_else(|| peer_ip.to_string()));
    crate::actor::run_session(control, rtp, caps, described).await
}

/// Bind the control listener on [`CONTROL_PORT`].
///
/// # Errors
/// [`MiracastError::Connection`] if the port is taken, which on this port almost always
/// means a second receiver on the same box rather than an unrelated service — 7250 has no
/// other common occupant.
#[expect(
    clippy::disallowed_methods,
    reason = "registered: the miracast/tcp 7250 entry in crates/app/src/surface.rs"
)]
pub async fn bind() -> Result<TcpListener, MiracastError> {
    TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, CONTROL_PORT))
        .await
        .map_err(|e| MiracastError::Connection(format!("binding mice control port: {e}")))
}
