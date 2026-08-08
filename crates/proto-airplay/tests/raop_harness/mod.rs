//! The RAOP session harness: a scripted AirPlay 1 sender over real sockets.
//!
//! Shared between `proto-airplay`'s own end-to-end tests and the decode-seam test in
//! `crates/app` (#189), which reaches it by `#[path]` the way the Bluetooth seam test
//! reaches its fixtures — one copy, because a harness that drifts from the tests it
//! spawned stops proving they exercise the same session.
//!
//! In a subdirectory rather than next to the test files so Cargo does not compile it
//! as a test target of its own.

// Each including test binary uses the slice of this it needs.
#![allow(dead_code)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use castaway_core::{MediaPorts, ProtocolKind, SessionSink, SourceAdapter as _, SourceId};
use proto_airplay::{AirPlayIdentity, AirPlayReceiver};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// The `a=fmtp:` integers an iPhone announces: 352-sample frames, 16-bit, the
/// `40 10 14` Rice parameters, stereo, 44.1 kHz.
pub const IPHONE_FMTP: &str = "352 0 16 40 10 14 2 255 0 0 44100";

/// The SDP a sender announces unencrypted ALAC with, for the given `a=fmtp:` integers.
#[must_use]
pub fn announce_sdp(fmtp: &str) -> String {
    format!(
        "v=0\r\n\
         o=iTunes 3696222840 0 IN IP4 127.0.0.1\r\n\
         s=iTunes\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=audio 0 RTP/AVP 96\r\n\
         a=rtpmap:96 AppleLossless\r\n\
         a=fmtp:96 {fmtp}\r\n\
         a=min-latency:11025\r\n"
    )
}

/// The receiver identity every harness session advertises.
#[must_use]
pub fn identity() -> AirPlayIdentity {
    AirPlayIdentity {
        name: "Test TV".into(),
        device_id: "AA:BB:CC:DD:EE:FF".into(),
        host: "castaway".into(),
        pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
        offer_hevc: false,
        mirror_height: 1080,
    }
}

/// Bind an ephemeral loopback port, start a receiver on it, and return the address.
///
/// The bind-then-drop dance is how the test learns the port before the adapter owns it.
pub async fn spawn_receiver(media_ports: MediaPorts, sink: SessionSink) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let receiver = Arc::new(AirPlayReceiver::new(identity(), media_ports).with_addr(addr));
    tokio::spawn(async move {
        let _ = receiver.run(sink).await;
    });
    addr
}

/// Connect to a receiver that is still in the middle of binding.
pub async fn connect(addr: SocketAddr) -> TcpStream {
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(addr).await {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the receiver never started listening on {addr}");
}

/// A receiver up and connected: the RTSP control stream and the event channel.
pub async fn start(
    media_ports: MediaPorts,
) -> (TcpStream, mpsc::Receiver<castaway_core::SourceMessage>) {
    let (tx, events) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::AirPlay, "test"), tx);
    let addr = spawn_receiver(media_ports, sink).await;
    (connect(addr).await, events)
}

/// Send one RTSP request and read the response head.
pub async fn request(
    stream: &mut TcpStream,
    line: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    cseq: u32,
) -> String {
    let mut req = format!("{line} RTSP/1.0\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .expect("receiver answered in time")
        .unwrap();
    String::from_utf8_lossy(&buf[..n]).to_string()
}

/// Pull `server_port=NNNN` out of a Transport header.
#[must_use]
pub fn server_port(response: &str) -> u16 {
    response
        .split("server_port=")
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse().ok())
        })
        .unwrap_or_else(|| panic!("no server_port in:\n{response}"))
}

/// An RTP audio packet around a payload.
#[must_use]
pub fn audio_packet(sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, 0x60];
    p.extend_from_slice(&sequence.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// Drive the whole RAOP audio negotiation — OPTIONS, ANNOUNCE with `fmtp`, SETUP,
/// RECORD — and return the advertised audio port and the RECORD response.
///
/// Panics on any non-200, so a caller holds a session that is genuinely up.
pub async fn negotiate(stream: &mut TcpStream, fmtp: &str) -> (u16, String) {
    let options = request(stream, "OPTIONS *", &[], &[], 1).await;
    assert!(options.starts_with("RTSP/1.0 200"), "{options}");

    let announce = request(
        stream,
        "ANNOUNCE rtsp://127.0.0.1/1",
        &[("Content-Type", "application/sdp")],
        announce_sdp(fmtp).as_bytes(),
        2,
    )
    .await;
    assert!(announce.starts_with("RTSP/1.0 200"), "{announce}");

    // A sender's own control/timing ports; unused by the harness, but SETUP is refused
    // without them.
    let setup = request(
        stream,
        "SETUP rtsp://127.0.0.1/1",
        &[(
            "Transport",
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port=6001;timing_port=6002",
        )],
        &[],
        3,
    )
    .await;
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let audio_port = server_port(&setup);
    assert_ne!(audio_port, 0, "a zero server_port means no socket is bound");

    let record = request(stream, "RECORD rtsp://127.0.0.1/1", &[], &[], 4).await;
    assert!(record.starts_with("RTSP/1.0 200"), "{record}");
    (audio_port, record)
}
