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

use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use castaway_core::{
    FrameSource, ProtocolKind, SessionEvent, SessionSink, SourceAdapter, SourceId,
};
use proto_airplay::{AirPlayIdentity, AirPlayReceiver, MirrorKeys, StreamConnectionId};
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

/// A real FairPlay v3 vector: the 164-byte `/fp-setup` SETUP2 body, the 72-byte `ekey`
/// a `SETUP` would carry, and the AES key they derive to.
///
/// Using a genuine one rather than a synthetic pair is what makes this a test of the
/// whole chain: the session runs the real derivation, and the test can encrypt frames
/// with the key it *knows* must come out. A wrong derivation fails here as garbage
/// rather than as a mismatch nobody notices.
const FP_KEY_MESSAGE: &str = "46504c590301030000000098008f1a9ca548fdd57560a52926ff399f2eb154d0a7a0fffc997f58e27e00499eb9f310110d019e550e328047aea54308ab71b647041406878af96e06cf74127ae35941dceb58931b5543b39903f9f76a376248ee52e3656b561e1c1a0106ec6608df0ab4f2df528e65db6d622d3892d5b49c6c025606a574f19ebea7d93500bdd69db23333f22edcb3ccf7a6acde7389f2facabfa61b0b50";
const FP_EKEY: &str = "46504c59010201000000003c000000006d44ba12b91f48e061eb230fc53abfa2000000108a1060465d51b808df112d08b604501f9e3ea29ce0902f3c43b81d5319d0575f78517e01";
const FP_EXPECTED_AES_KEY: &str = "0496a612172f41e0fd71912acc33fc54";

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A binary plist body from a dictionary.
fn plist_body(dict: plist::Dictionary) -> Vec<u8> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &plist::Value::Dictionary(dict)).unwrap();
    buf
}

/// One framed mirroring message.
fn mirror_message(kind: u8, timestamp: u64, payload: &[u8]) -> Vec<u8> {
    let mut m = vec![0u8; 128];
    m[0..4].copy_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    m[4] = kind;
    m[8..16].copy_from_slice(&timestamp.to_le_bytes());
    m[56..60].copy_from_slice(&1920.0f32.to_le_bytes());
    m[60..64].copy_from_slice(&1080.0f32.to_le_bytes());
    m.extend_from_slice(payload);
    m
}

#[tokio::test]
async fn a_mirroring_session_negotiates_and_video_reaches_the_pipeline() {
    let (tx, mut events) = mpsc::channel(64);
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

    // `/fp-setup` SETUP2 carries the key message the derivation needs. The session keeps
    // it, exactly as it would from a real handshake.
    let fp = request(
        &mut stream,
        "POST /fp-setup",
        &[("Content-Type", "application/octet-stream")],
        &unhex(FP_KEY_MESSAGE),
        1,
    )
    .await;
    assert!(fp.starts_with("RTSP/1.0 200"), "{fp}");

    // First mirroring SETUP: the wrapped key.
    let mut d = plist::Dictionary::new();
    d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
    d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
    d.insert("timingProtocol".into(), plist::Value::String("NTP".into()));
    let setup1 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        2,
    )
    .await;
    assert!(setup1.starts_with("RTSP/1.0 200"), "{setup1}");

    // Second: name the stream. The reply has to carry a data port that is listening.
    let stream_id: i64 = 4_964_383_553_955_644_435;
    let mut s0 = plist::Dictionary::new();
    s0.insert("type".into(), plist::Value::Integer(110i64.into()));
    s0.insert(
        "streamConnectionID".into(),
        plist::Value::Integer(stream_id.into()),
    );
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
    );
    let body = plist_body(d);
    let mut req = format!(
        "SETUP rtsp://127.0.0.1/1 RTSP/1.0\r\nCSeq: 3\r\nContent-Type: \
         application/x-apple-binary-plist\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(&body);
    stream.write_all(&req).await.unwrap();
    stream.flush().await.unwrap();

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(head.starts_with("RTSP/1.0 200"), "{head}");

    // The data port comes back inside the plist body, so parse it rather than scraping.
    let body_at = head.find("\r\n\r\n").expect("a header/body split") + 4;
    let reply: plist::Value = plist::from_bytes(&buf[body_at..n]).expect("a plist reply");
    let data_port = reply
        .as_dictionary()
        .and_then(|d| d.get("streams"))
        .and_then(plist::Value::as_array)
        .and_then(|a| a.first())
        .and_then(plist::Value::as_dictionary)
        .and_then(|d| d.get("dataPort"))
        .and_then(plist::Value::as_unsigned_integer)
        .expect("a dataPort in the reply");
    assert_ne!(data_port, 0, "a zero dataPort means nothing is listening");

    // The pipeline is told to expect a mirror.
    let mut frames = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        if let SessionEvent::Mirror { video, .. } = msg.event {
            let FrameSource::Encoded(rx) = video else {
                panic!("expected encoded frames")
            };
            frames = Some(rx);
            break;
        }
    }
    let mut frames = frames.expect("SETUP should have started a mirroring session");

    // Now encrypt video with the key the real derivation must have produced.
    let aes_key: [u8; 16] = unhex(FP_EXPECTED_AES_KEY).try_into().unwrap();
    let keys = MirrorKeys::derive(&aes_key, StreamConnectionId::from_plist_signed(stream_id));
    let mut cipher = ctr::Ctr128BE::<aes::Aes128>::new(&keys.key.into(), &keys.iv.into());

    let mut data = tokio::net::TcpStream::connect(SocketAddr::from((
        [127, 0, 0, 1],
        u16::try_from(data_port).unwrap(),
    )))
    .await
    .expect("the advertised data port accepts");

    // A codec config, then an IDR at the same timestamp, then a second frame.
    let record = [
        &[1u8, 100, 0xc0, 40, 0xff, 0xe1][..],
        &3u16.to_be_bytes()[..],
        &[0x67, 0x42, 0x00][..],
        &[1u8][..],
        &2u16.to_be_bytes()[..],
        &[0x68, 0xce][..],
    ]
    .concat();
    let mut out = mirror_message(1, 7000, &record);
    for (i, ts) in [7000u64, 24_000].into_iter().enumerate() {
        let nal_type = if i == 0 { 5u8 } else { 1 };
        let mut nal = vec![nal_type];
        nal.extend_from_slice(b"an-access-unit-payload");
        let mut au = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
        au.extend_from_slice(&nal);
        cipher.apply_keystream(&mut au);
        out.extend_from_slice(&mirror_message(0, ts, &au));
    }
    data.write_all(&out).await.unwrap();
    data.flush().await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(5), frames.recv())
        .await
        .expect("a frame arrived")
        .expect("the channel is open");
    // If the FairPlay derivation, the SHA-512 stream-key derivation, the keystream and
    // the AVCC rewrite are all right, this is the access unit we sent — with the SPS and
    // PPS from the codec-config packet prepended in-band, which is what the decoder wants.
    assert!(first.keyframe, "the first access unit is an IDR");
    assert_eq!(&first.data[..4], &[0, 0, 0, 1]);
    assert_eq!(&first.data[4..7], &[0x67, 0x42, 0x00], "SPS in-band");
    assert!(
        first.data.ends_with(b"an-access-unit-payload"),
        "the payload should survive decryption"
    );

    let second = tokio::time::timeout(Duration::from_secs(5), frames.recv())
        .await
        .expect("a second frame")
        .expect("the channel is open");
    assert!(!second.keyframe);
    // The keystream ran on, so the second frame only decrypts if it was never restarted.
    assert!(second.data.ends_with(b"an-access-unit-payload"));
    assert_eq!(second.pts, Duration::from_nanos(17_000));
}
