//! A whole AirPlay 1 audio session, over real sockets, with no hardware.
//!
//! The unit tests prove each piece in isolation; this proves they are wired together —
//! that the ports a `SETUP` advertises are ports something is actually listening on, and
//! that audio sent to them comes out the other end as decodable frames. That seam is
//! exactly where a receiver can look perfect on the RTSP exchange and still play
//! nothing, so it gets a test that uses the sockets rather than mocking them.

#![allow(clippy::unwrap_used)]

use std::net::SocketAddr;
use std::time::Duration;

use castaway_core::{
    FrameSource, ProtocolKind, SessionEvent, SessionSink, SourceAdapter, SourceId,
};
use proto_airplay::{AirPlayIdentity, AirPlayReceiver};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

/// The SDP an iOS sender announces unencrypted ALAC with.
const ANNOUNCE_SDP: &str = "v=0\r\n\
    o=iTunes 3696222840 0 IN IP4 127.0.0.1\r\n\
    s=iTunes\r\n\
    c=IN IP4 127.0.0.1\r\n\
    t=0 0\r\n\
    m=audio 0 RTP/AVP 96\r\n\
    a=rtpmap:96 AppleLossless\r\n\
    a=fmtp:96 352 0 16 40 10 14 2 255 0 0 44100\r\n\
    a=min-latency:11025\r\n";

fn identity() -> AirPlayIdentity {
    AirPlayIdentity {
        name: "Test TV".into(),
        device_id: "AA:BB:CC:DD:EE:FF".into(),
        host: "castaway".into(),
        pairing_id: "de159742-c022-4514-915b-203cb99f8b71".into(),
    }
}

/// Send one RTSP request and read the response head.
async fn request(
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
fn server_port(response: &str) -> u16 {
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

/// An RTP audio packet with an ALAC-shaped payload.
fn audio_packet(sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, 0x60];
    p.extend_from_slice(&sequence.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&0u32.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

#[tokio::test]
async fn a_raop_session_negotiates_and_audio_reaches_the_pipeline() {
    let (tx, mut events) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::AirPlay, "test"), tx);

    // Bind explicitly so the test knows the port before the adapter starts serving.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let receiver = std::sync::Arc::new(AirPlayReceiver::new(identity()).with_addr(addr));

    tokio::spawn({
        let receiver = std::sync::Arc::clone(&receiver);
        async move {
            let _ = receiver.run(sink).await;
        }
    });

    // Wait for the listener to come up.
    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(addr).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut stream = stream.expect("the receiver started listening");

    // OPTIONS, then the audio negotiation in order.
    let options = request(&mut stream, "OPTIONS *", &[], &[], 1).await;
    assert!(options.starts_with("RTSP/1.0 200"), "{options}");

    let announce = request(
        &mut stream,
        "ANNOUNCE rtsp://127.0.0.1/1",
        &[("Content-Type", "application/sdp")],
        ANNOUNCE_SDP.as_bytes(),
        2,
    )
    .await;
    assert!(announce.starts_with("RTSP/1.0 200"), "{announce}");

    // A sender's own control/timing ports; we do not use them here, but SETUP is
    // refused without them, which is the point.
    let setup = request(
        &mut stream,
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

    let record = request(&mut stream, "RECORD rtsp://127.0.0.1/1", &[], &[], 4).await;
    assert!(record.starts_with("RTSP/1.0 200"), "{record}");
    assert!(record.contains("Audio-Latency"), "{record}");

    // The description reaches the panel, naming the generation and codec.
    let mut described = None;
    let mut frames = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        match msg.event {
            SessionEvent::SourceInfo(d) => described = d.link,
            SessionEvent::Audio {
                source,
                format,
                config,
            } => {
                assert_eq!(format.sample_rate(), 44_100);
                assert_eq!(format.channels(), 2);
                // ALAC will not open a decoder without its magic cookie.
                let config = config.expect("ALAC must carry its magic cookie");
                assert_eq!(config.len(), 36);
                assert_eq!(&config[4..8], b"alac");
                let FrameSource::Encoded(rx) = source else {
                    panic!("expected encoded frames")
                };
                frames = Some(rx);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        described.as_deref(),
        Some("AirPlay 1 · ALAC · 44.1 kHz · stereo"),
        "the panel should be told what was negotiated"
    );
    let mut frames = frames.expect("RECORD should have started an audio session");

    // Now the part only real sockets can prove: audio sent to the advertised port comes
    // out as frames.
    let sender = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let target = SocketAddr::from(([127, 0, 0, 1], audio_port));
    for i in 0..4u16 {
        let packet = audio_packet(i, u32::from(i) * 352, b"an ALAC frame..");
        sender.send_to(&packet, target).await.unwrap();
    }

    let mut received = 0;
    for _ in 0..4 {
        match tokio::time::timeout(Duration::from_secs(5), frames.recv()).await {
            Ok(Some(frame)) => {
                assert_eq!(frame.data.as_ref(), b"an ALAC frame..");
                received += 1;
            }
            _ => break,
        }
    }
    assert_eq!(
        received, 4,
        "every packet sent should have arrived as a frame"
    );
}

#[tokio::test]
async fn a_setup_before_announce_is_refused_over_a_real_socket() {
    let (tx, _events) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::AirPlay, "test"), tx);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let receiver = std::sync::Arc::new(AirPlayReceiver::new(identity()).with_addr(addr));
    tokio::spawn(async move {
        let _ = receiver.run(sink).await;
    });

    let mut stream = None;
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(addr).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut stream = stream.expect("the receiver started listening");

    let setup = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[(
            "Transport",
            "RTP/AVP/UDP;unicast;mode=record;control_port=6001;timing_port=6002",
        )],
        &[],
        1,
    )
    .await;
    assert!(
        setup.starts_with("RTSP/1.0 451"),
        "a SETUP with nothing announced must be refused, got:\n{setup}"
    );
}
