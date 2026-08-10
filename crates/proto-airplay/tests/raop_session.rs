//! A whole AirPlay 1 audio session, over real sockets, with no hardware.
//!
//! The unit tests prove each piece in isolation; this proves they are wired together —
//! that the ports a `SETUP` advertises are ports something is actually listening on, and
//! that audio sent to them comes out the other end as decodable frames. That seam is
//! exactly where a receiver can look perfect on the RTSP exchange and still play
//! nothing, so it gets a test that uses the sockets rather than mocking them.

#![allow(clippy::unwrap_used)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::time::Duration;

use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use castaway_core::{FrameSource, MediaPorts, ProtocolKind, SessionEvent, SessionSink, SourceId};
use proto_airplay::{MirrorKeys, StreamConnectionId};
use tokio::io::AsyncWriteExt as _;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

#[path = "raop_harness/mod.rs"]
mod harness;
use harness::{
    audio_packet, mirror_message, plist_body, request, unhex, FP_EKEY, FP_EXPECTED_AES_KEY,
    FP_KEY_MESSAGE, IPHONE_FMTP, MIRROR_STREAM_ID,
};

#[tokio::test]
async fn a_raop_session_negotiates_and_audio_reaches_the_pipeline() {
    let (mut stream, mut events) = harness::start(MediaPorts::Ephemeral).await;
    let (audio_port, record) = harness::negotiate(&mut stream, IPHONE_FMTP).await;
    assert!(record.contains("Audio-Latency"), "{record}");

    // The description reaches the panel, naming the generation and codec — *after* the
    // stream event, because the session manager drops a description for a source that is
    // not active yet, and the stream event is what makes it active (#81).
    let mut described = None;
    let mut frames = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        match msg.event {
            SessionEvent::SourceInfo(d) => {
                described = Some(d);
                if frames.is_some() {
                    break;
                }
            }
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
            }
            _ => {}
        }
    }
    let mut frames = frames.expect("RECORD should have started an audio session");
    let described = described.expect("the panel should be told what was negotiated");
    assert_eq!(
        described.link.as_deref(),
        Some("AirPlay 1 · ALAC · 44.1 kHz · stereo")
    );
    // A RAOP sender never names itself, so the address is all the card has to identify
    // which phone in the room this is.
    assert_eq!(described.address.as_deref(), Some("127.0.0.1"));

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
    let (mut stream, _events) = harness::start(MediaPorts::Ephemeral).await;

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

#[tokio::test]
async fn a_mirroring_session_delivers_both_video_and_its_audio() {
    let (mut stream, mut events) = harness::start(MediaPorts::Ephemeral).await;

    // `/fp-setup` with the captured key message, the key-material SETUP, and the
    // type-110 stream SETUP — the reply has to carry a data port that is listening.
    let data_port = harness::negotiate_mirror(&mut stream).await;

    // The pipeline is told to expect a mirror — and its audio arrives *with* it, not as
    // a session of its own, which would preempt the picture it belongs to.
    let mut frames = None;
    let mut audio_frames = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        if let SessionEvent::Mirror { video, audio } = msg.event {
            let FrameSource::Encoded(rx) = video else {
                panic!("expected encoded frames")
            };
            frames = Some(rx);
            let audio = audio.expect("a mirror announces its audio channel up front");
            // AAC-ELD will not open a decoder without its AudioSpecificConfig.
            assert_eq!(
                audio.config.expect("a codec config").as_ref(),
                &[0xf8, 0xe8, 0x50, 0x00]
            );
            assert_eq!(audio.format.sample_rate(), 44_100);
            let FrameSource::Encoded(arx) = audio.source else {
                panic!("expected encoded audio frames")
            };
            audio_frames = Some(arx);
            break;
        }
    }
    let mut frames = frames.expect("SETUP should have started a mirroring session");
    let mut audio_frames = audio_frames.expect("the mirror should carry an audio channel");

    // Now encrypt video with the key the real derivation must have produced.
    let aes_key: [u8; 16] = unhex(FP_EXPECTED_AES_KEY).try_into().unwrap();
    let keys = MirrorKeys::derive(
        &aes_key,
        StreamConnectionId::from_plist_signed(MIRROR_STREAM_ID),
    );
    let mut cipher = ctr::Ctr128BE::<aes::Aes128>::new(&keys.key.into(), &keys.iv.into());

    let mut data = tokio::net::TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], data_port)))
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
    let hd = (1920.0f32, 1080.0f32);
    let mut out = mirror_message(1, 7000, &record, hd);
    for (i, ts) in [7000u64, 24_000].into_iter().enumerate() {
        let nal_type = if i == 0 { 5u8 } else { 1 };
        let mut nal = vec![nal_type];
        nal.extend_from_slice(b"an-access-unit-payload");
        let mut au = u32::try_from(nal.len()).unwrap().to_be_bytes().to_vec();
        au.extend_from_slice(&nal);
        cipher.apply_keystream(&mut au);
        out.extend_from_slice(&mirror_message(0, ts, &au, hd));
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

    // --- and now the audio that rides alongside it ---

    // A heartbeat first, because a real sender sends dozens of them between negotiating
    // the video and negotiating the audio — and *that* is what used to break mirror
    // audio. The actor took the audio channel out of its slot while evaluating a tuple
    // pattern that then did not match, so every unrelated request in between quietly
    // threw the channel away and the audio arrived with nowhere to go. Without a request
    // in this gap the test passes with the bug present.
    let feedback = request(&mut stream, "POST /feedback", &[], &[], 4).await;
    assert!(feedback.starts_with("RTSP/1.0 200"), "{feedback}");

    let mut s0 = plist::Dictionary::new();
    s0.insert("type".into(), plist::Value::Integer(96i64.into()));
    s0.insert("ct".into(), plist::Value::Integer(8i64.into()));
    s0.insert("spf".into(), plist::Value::Integer(480i64.into()));
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
    );
    let reply = harness::request_bytes(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        5,
    )
    .await;
    let reply = harness::response_plist(&reply);
    let audio_port = harness::stream_port(&reply, "dataPort");
    let control_port = harness::stream_port(&reply, "controlPort");

    // The audio key is the FairPlay one with the `eiv` verbatim — no SHA-512 derivation,
    // which is the video stream's alone. Encrypt the way a sender does and check it lands.
    let sender = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();

    // The sync packet first, which is what a real sender emits at stream start. Mirror
    // audio shares its origin with mirror video, so it cannot place a frame on that
    // timeline until a sync has anchored it to the sender's clock — a packet before the
    // anchor is `AwaitingSync` and dropped, deliberately. 20 bytes, and *not* an RTP
    // header plus payload: the NTP stamp sits where an SSRC would.
    let mut sync = vec![0x90u8, 0x80 | 84, 0, 7];
    sync.extend_from_slice(&0u32.to_be_bytes()); // rtp now, less latency
    sync.extend_from_slice(&0x0001_0000_0000_0000u64.to_be_bytes()); // sender NTP
    sync.extend_from_slice(&0u32.to_be_bytes()); // rtp anchor
    sender
        .send_to(&sync, SocketAddr::from(([127, 0, 0, 1], control_port)))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut plain = vec![0x8cu8];
    plain.extend_from_slice(b"an-aac-eld-access-unit!!");
    let mut cipher_text = plain.clone();
    let n = cipher_text.len() - (cipher_text.len() % 16);
    let mut enc = cbc::Encryptor::<aes::Aes128>::new(&aes_key.into(), &[0u8; 16].into());
    let chunks = aes::cipher::inout::InOutBuf::from(&mut cipher_text[..n])
        .into_chunks::<aes::cipher::consts::U16>()
        .0
        .into_out();
    use aes::cipher::BlockEncryptMut as _;
    enc.encrypt_blocks_mut(chunks);

    let mut packet = vec![0x80, 0x60, 0, 1];
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&cipher_text);
    sender
        .send_to(&packet, SocketAddr::from(([127, 0, 0, 1], audio_port)))
        .await
        .unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), audio_frames.recv())
        .await
        .expect("an audio frame arrived")
        .expect("the channel is open");
    assert_eq!(frame.data.as_ref(), plain.as_slice());

    // --- and now the end of it ---
    //
    // Stopping a mirror is the sender closing the data channel. It is *not* a `TEARDOWN`
    // (iOS tears down named streams mid-session to renegotiate, which `session.rs`
    // deliberately does not treat as an end, with two tests pinning that) and it does not
    // require the RTSP control connection to close — that stays up here, as it does on a
    // real phone.
    //
    // Nothing consumed that close. The picture stopped and the session never ended, so
    // `Pipeline::stop` never ran, the video surface stayed claimed, and the panel held the
    // last decoded frame with no way back Home. Observed on the Dell, 2026-07-31.
    drop(data);

    let mut ended = false;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        if matches!(msg.event, SessionEvent::End) {
            ended = true;
            break;
        }
    }
    assert!(
        ended,
        "closing the mirror data channel must end the session — otherwise the panel keeps \
         the last frame forever"
    );
}

/// With a declared media range, the ports a `SETUP` advertises come from that range —
/// the property the firewall depends on: every port a sender is told to hit is one the
/// deployment could have opened ahead of time.
#[tokio::test]
async fn setup_advertises_ports_from_the_declared_media_range() {
    // Eight ports: one connection takes four (mirror data TCP + audio/control/timing UDP).
    let range = MediaPorts::Range(castaway_core::PortRange::new(42510, 42517).unwrap());
    let (mut stream, _events) = harness::start(range).await;

    let (audio_port, _) = harness::negotiate(&mut stream, IPHONE_FMTP).await;
    assert!(
        (42510..=42517).contains(&audio_port),
        "SETUP advertised {audio_port}, outside the declared range 42510-42517"
    );
}

/// A sender casting an app's *media* rather than its screen: key material, then one
/// audio stream and no video stream at all.
///
/// Captured from an iPhone AirPlaying from YouTube (2026-07-31). The whole session used
/// to negotiate cleanly and then start nothing: the audio was only ever started by being
/// handed the mirror's channel, and there is no mirror here. On the phone that looks
/// like "connected" and no sound; in the log it was one line —
/// `no active session for source airplay/…` — as the sender's metadata was dropped on
/// the floor.
#[tokio::test]
async fn an_audio_only_session_starts_without_a_picture_to_belong_to() {
    let (mut stream, mut events) = harness::start(MediaPorts::Ephemeral).await;

    let fp = request(
        &mut stream,
        "POST /fp-setup",
        &[("Content-Type", "application/octet-stream")],
        &unhex(FP_KEY_MESSAGE),
        1,
    )
    .await;
    assert!(fp.starts_with("RTSP/1.0 200"), "{fp}");

    // The key material — with no `isScreenMirroringSession`, which is what says this is
    // a media session rather than a mirror.
    let mut d = plist::Dictionary::new();
    d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
    d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
    d.insert("timingProtocol".into(), plist::Value::String("NTP".into()));
    // The sender introduces itself here and nowhere else in the session.
    d.insert("name".into(), plist::Value::String("Chaz's iPhone".into()));
    d.insert("model".into(), plist::Value::String("iPhone17,1".into()));
    let setup1 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        2,
    )
    .await;
    assert!(setup1.starts_with("RTSP/1.0 200"), "{setup1}");

    // One stream: ALAC audio, exactly as the capture has it.
    let mut s0 = plist::Dictionary::new();
    s0.insert("type".into(), plist::Value::Integer(96i64.into()));
    s0.insert("ct".into(), plist::Value::Integer(2i64.into()));
    s0.insert("spf".into(), plist::Value::Integer(352i64.into()));
    s0.insert("sr".into(), plist::Value::Integer(44100i64.into()));
    s0.insert("isMedia".into(), plist::Value::Boolean(true));
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
    );
    let setup2 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        3,
    )
    .await;
    assert!(setup2.starts_with("RTSP/1.0 200"), "{setup2}");

    // The point of the test: an audio session reaches the pipeline, with the codec the
    // sender named rather than mirroring's, and a decoder config it can open.
    let mut described = None;
    let mut started = false;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        match msg.event {
            SessionEvent::SourceInfo(d) => {
                described = Some(d);
                if started {
                    break;
                }
            }
            SessionEvent::Audio { format, config, .. } => {
                assert_eq!(format.sample_rate(), 44_100);
                let config = config.expect("ALAC must carry its magic cookie");
                assert_eq!(config.len(), 36, "the 36-byte ALACSpecificConfig");
                assert_eq!(&config[4..8], b"alac");
                // The frame length the sender asked for, not mirroring's 480.
                assert_eq!(&config[12..16], &352u32.to_be_bytes());
                started = true;
            }
            _ => {}
        }
    }
    assert!(started, "an audio-only session must start an audio session");
    let described = described.expect("the panel should be told who is casting, and how");
    assert_eq!(
        described.link.as_deref(),
        Some("AirPlay mirroring · ALAC · 44.1 kHz · stereo")
    );
    // The name the sender gave in its first SETUP, three requests ago. This is the whole
    // of "Unknown device": every fact was on the wire and none of it was kept (#81).
    assert_eq!(described.display_name.as_deref(), Some("Chaz's iPhone"));
    assert_eq!(described.address.as_deref(), Some("127.0.0.1"));
}

/// Poll the session counters until `cond` holds, or fail naming what never happened.
///
/// A poll with a deadline rather than a sleep (ground rule 6): an expired poll reports
/// which counter it was waiting for, where an insufficient sleep reports a wrong number
/// and blames the code under test.
async fn wait_for_counters(
    diagnostics: &proto_airplay::SessionDiagnostics,
    what: &str,
    cond: impl Fn(&proto_airplay::Snapshot) -> bool,
) -> proto_airplay::Snapshot {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let snapshot = diagnostics.snapshot();
        if cond(&snapshot) {
            return snapshot;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "waited 2 s for {what}; counters: {snapshot:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Keep answering timing probes on `timing` so a reply that raced the probe cadence
/// (superseded requests are dropped by design) cannot starve the assertion.
fn answer_timing_probes(timing: UdpSocket) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        while let Ok((n, from)) = timing.recv_from(&mut buf).await {
            if n == 32 {
                let _ = timing
                    .send_to(&harness::timing_reply(&buf[..n]), from)
                    .await;
            }
        }
    })
}

/// The T1 timing test, RAOP shape (issue #176): the synthetic sender binds a real UDP
/// socket on the timing port its `Transport` header declares, and a type-82 request
/// must arrive there within 2 s of `RECORD` — then a valid type-83 reply must be
/// *folded in*, which only `clock_samples` can prove. This is the exchange the seven
/// passing unit tests in `clock.rs` exercised without it ever running.
#[tokio::test]
async fn a_raop_session_probes_the_declared_timing_peer_and_folds_in_the_reply() {
    let (mut stream, mut events, diagnostics) = harness::start_watched(MediaPorts::Ephemeral).await;

    // Bound *before* they are declared, so the receiver's first probe lands on sockets
    // the test is watching rather than ports nothing answers.
    let timing = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let control = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let timing_port = timing.local_addr().unwrap().port();
    let control_port = control.local_addr().unwrap().port();

    let ports =
        harness::negotiate_declaring_ports(&mut stream, IPHONE_FMTP, control_port, timing_port)
            .await;

    let mut buf = [0u8; 64];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), timing.recv_from(&mut buf))
        .await
        .expect("a timing request must arrive within 2 s of RECORD")
        .unwrap();
    assert_eq!(n, 32, "a timing request is exactly 32 bytes");
    assert_eq!(buf[1] & 0x7F, 82, "payload type 82");
    // From the receiver's own timing socket, which is where the reply must go back to.
    assert_eq!(from.port(), ports.timing);

    timing
        .send_to(&harness::timing_reply(&buf[..n]), from)
        .await
        .unwrap();
    let responder = answer_timing_probes(timing);
    let snapshot =
        wait_for_counters(&diagnostics, "clock_samples >= 1", |s| s.clock_samples >= 1).await;
    assert!(
        snapshot.clock_offset_ns.is_some(),
        "a folded-in round trip must yield an offset: {snapshot:?}"
    );

    // And the sender's declared lead is read rather than dropped on the floor: a sync
    // packet declaring 77175 frames (1.75 s at 44.1 kHz — the value every live log
    // showed) reaches the counters.
    let sync = harness::sync_packet(1_000, 0x0001_0000_0000_0000, 78_175);
    control
        .send_to(&sync, SocketAddr::from(([127, 0, 0, 1], ports.control)))
        .await
        .unwrap();
    wait_for_counters(&diagnostics, "sender_latency_frames == 77175", |s| {
        s.sender_latency_frames == 77_175
    })
    .await;

    // …and consumed, not merely counted (#176): the same figure leaves the adapter as
    // a session event, converted at the boundary — 77175 frames of 44.1 kHz is exactly
    // 1.75 s — which is what the pipeline adopts as the mixer's target buffer depth.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let declared = loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .expect("waited 2 s for a SessionEvent::AudioLatency that never came");
        let Ok(Some(msg)) = tokio::time::timeout(remaining, events.recv()).await else {
            panic!("the event channel closed before the declared latency was emitted");
        };
        if let SessionEvent::AudioLatency(latency) = msg.event {
            break latency;
        }
    };
    assert_eq!(declared.duration(), Duration::from_millis(1750));
    responder.abort();
}

/// The same T1 test in the plist shape — the one that failed before #176: a session
/// negotiated through the two-phase plist `SETUP` (here the captured `isMedia` flow)
/// declares its timing peer in `timingPort` and its control peer in the stream entry,
/// and got neither read, so no probe ever left and `clock_samples` stayed 0 for the
/// life of every mirroring and media session.
#[tokio::test]
async fn a_plist_session_probes_the_declared_timing_peer_and_folds_in_the_reply() {
    let (mut stream, _events, diagnostics) = harness::start_watched(MediaPorts::Ephemeral).await;

    let timing = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let timing_port = timing.local_addr().unwrap().port();
    let control = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let control_port = control.local_addr().unwrap().port();

    let fp = request(
        &mut stream,
        "POST /fp-setup",
        &[("Content-Type", "application/octet-stream")],
        &unhex(FP_KEY_MESSAGE),
        1,
    )
    .await;
    assert!(fp.starts_with("RTSP/1.0 200"), "{fp}");

    // First SETUP: key material, and the sender's own NTP service — the fact the
    // Transport-header path gets from `timing_port=` and this path used to drop.
    let mut d = plist::Dictionary::new();
    d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
    d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
    d.insert("timingProtocol".into(), plist::Value::String("NTP".into()));
    d.insert(
        "timingPort".into(),
        plist::Value::Integer(i64::from(timing_port).into()),
    );
    let setup1 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        2,
    )
    .await;
    assert!(setup1.starts_with("RTSP/1.0 200"), "{setup1}");

    // A real phone sends RECORD here, between the two SETUPs.
    let record = request(&mut stream, "RECORD rtsp://127.0.0.1/1", &[], &[], 3).await;
    assert!(record.starts_with("RTSP/1.0 200"), "{record}");

    // Second SETUP: the media-audio stream, verbatim from the capture — ALAC, its
    // declared latency bounds, and the sender's control port.
    let mut s0 = plist::Dictionary::new();
    s0.insert("type".into(), plist::Value::Integer(96i64.into()));
    s0.insert("ct".into(), plist::Value::Integer(2i64.into()));
    s0.insert("spf".into(), plist::Value::Integer(352i64.into()));
    s0.insert("sr".into(), plist::Value::Integer(44100i64.into()));
    s0.insert("latencyMin".into(), plist::Value::Integer(11025i64.into()));
    s0.insert("latencyMax".into(), plist::Value::Integer(88200i64.into()));
    s0.insert(
        "controlPort".into(),
        plist::Value::Integer(i64::from(control_port).into()),
    );
    s0.insert("isMedia".into(), plist::Value::Boolean(true));
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
    );
    let setup2 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        4,
    )
    .await;
    assert!(setup2.starts_with("RTSP/1.0 200"), "{setup2}");

    // The probe that never left before #176.
    let mut buf = [0u8; 64];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), timing.recv_from(&mut buf))
        .await
        .expect("a plist session must probe its declared timing peer within 2 s")
        .unwrap();
    assert_eq!(n, 32);
    assert_eq!(buf[1] & 0x7F, 82, "payload type 82");

    timing
        .send_to(&harness::timing_reply(&buf[..n]), from)
        .await
        .unwrap();
    let responder = answer_timing_probes(timing);
    let snapshot =
        wait_for_counters(&diagnostics, "clock_samples >= 1", |s| s.clock_samples >= 1).await;
    assert!(
        snapshot.clock_offset_ns.is_some(),
        "a folded-in round trip must yield an offset: {snapshot:?}"
    );
    responder.abort();
}

/// The resend path at socket level (issue #176): the shape test passed for months while
/// no resend request had ever been observed leaving a socket. Here a sequence gap must
/// produce a type-85 datagram on the *sender's* control socket, and serving the
/// retransmit must heal the stream — every payload reaches the pipeline exactly once.
#[tokio::test]
async fn a_sequence_gap_sends_a_resend_request_out_the_socket_and_the_retransmit_plays() {
    let (mut stream, mut events, diagnostics) = harness::start_watched(MediaPorts::Ephemeral).await;

    let control = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let timing = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let control_port = control.local_addr().unwrap().port();
    let timing_port = timing.local_addr().unwrap().port();

    let ports =
        harness::negotiate_declaring_ports(&mut stream, IPHONE_FMTP, control_port, timing_port)
            .await;

    // The frame channel, so the retransmit can be seen coming out the other end.
    let mut frames = None;
    for _ in 0..8 {
        let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(5), events.recv()).await
        else {
            break;
        };
        if let SessionEvent::Audio { source, .. } = msg.event {
            let FrameSource::Encoded(rx) = source else {
                panic!("expected encoded frames")
            };
            frames = Some(rx);
            break;
        }
    }
    let mut frames = frames.expect("RECORD should have started an audio session");

    // Sequence 0 arrives, then 3: packets 1 and 2 are missing.
    let sender = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let audio_target = SocketAddr::from(([127, 0, 0, 1], ports.audio));
    sender
        .send_to(&audio_packet(0, 0, b"packet-zero....."), audio_target)
        .await
        .unwrap();
    sender
        .send_to(&audio_packet(3, 3 * 352, b"packet-three...."), audio_target)
        .await
        .unwrap();

    // The request actually leaves a socket — the datagram that had never been observed.
    let mut buf = [0u8; 64];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), control.recv_from(&mut buf))
        .await
        .expect("a resend request must leave within 2 s of the gap")
        .unwrap();
    assert_eq!(n, 8, "a resend request is exactly 8 bytes");
    assert_eq!(buf[1] & 0x7F, 85, "payload type 85");
    assert_eq!(
        u16::from_be_bytes([buf[4], buf[5]]),
        1,
        "first missing sequence"
    );
    assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 2, "missing count");
    // From the receiver's control socket, which is where the retransmit goes back to.
    assert_eq!(from.port(), ports.control);

    // Serve it, wrapped the way senders wrap a retransmit: four prefix bytes, then the
    // complete original packet.
    for (seq, ts, payload) in [
        (1u16, 352u32, &b"packet-one......"[..]),
        (2, 704, &b"packet-two......"[..]),
    ] {
        let mut wrapped = vec![0x80, 0x80 | 86, 0, 0];
        wrapped.extend_from_slice(&audio_packet(seq, ts, payload));
        control.send_to(&wrapped, from).await.unwrap();
    }

    // Every payload comes out: the two that arrived, and the two the resend recovered.
    let mut delivered = Vec::new();
    for _ in 0..4 {
        let frame = tokio::time::timeout(Duration::from_secs(5), frames.recv())
            .await
            .expect("a frame arrived")
            .expect("the channel is open");
        delivered.push(frame.data);
    }
    for payload in [
        &b"packet-zero....."[..],
        &b"packet-three...."[..],
        &b"packet-one......"[..],
        &b"packet-two......"[..],
    ] {
        assert!(
            delivered.iter().any(|d| d.as_ref() == payload),
            "{payload:?} never reached the pipeline; got {delivered:?}"
        );
    }
    assert_eq!(
        diagnostics.snapshot().resends_sent,
        2,
        "the counter counts packets asked for"
    );
}

/// A pipeline that keeps the last description it was told, and nothing else.
///
/// The other tests in this file read events straight off the sink, which is one seam
/// short of the truth: the session manager *gates* descriptions on the source being the
/// active one, so a `SourceInfo` emitted before the stream event is dropped and never
/// reaches a pipeline at all. That gate is only visible with a real manager behind it.
struct CardPipeline(mpsc::UnboundedSender<castaway_core::SourceDescription>);

#[async_trait::async_trait]
impl castaway_core::Pipeline for CardPipeline {
    async fn play(
        &self,
        _source: castaway_core::MediaRequest,
        _start: Option<Duration>,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn mirror(
        &self,
        _video: FrameSource,
        _audio: Option<castaway_core::MirrorAudio>,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn play_audio(
        &self,
        _source: FrameSource,
        _format: castaway_core::AudioFormat,
        _config: Option<bytes::Bytes>,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn now_playing(
        &self,
        _snapshot: castaway_core::NowPlaying,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn up_next(
        &self,
        _items: Vec<castaway_core::QueueItem>,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn source_info(
        &self,
        source: castaway_core::SourceDescription,
    ) -> Result<(), castaway_core::CoreError> {
        let _ = self.0.send(source);
        Ok(())
    }
    async fn controls(
        &self,
        _capabilities: castaway_core::ControlCapabilities,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn control(
        &self,
        _txn: castaway_core::ControlTxn,
    ) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), castaway_core::CoreError> {
        Ok(())
    }
}

#[tokio::test]
async fn the_card_learns_who_is_casting_once_the_session_is_the_active_one() {
    // #81, at the seam that produced it. Every fact about the sender — its name from the
    // first SETUP, its address, the codec from the stream entry — is known before there
    // is a session to attach it to, and the manager drops a description from a source
    // that is not active yet. Emitted in the wrong order, all of it lands in a
    // "dropped an event" warning and the card reads "Unknown device".
    let (cards_tx, mut cards) = mpsc::unbounded_channel();
    let (tx, rx) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::AirPlay, "test"), tx);
    tokio::spawn(
        castaway_core::SessionManager::new(
            CardPipeline(cards_tx),
            None,
            castaway_core::SessionConfig::default(),
        )
        .run(rx),
    );

    let addr = harness::spawn_receiver(MediaPorts::Ephemeral, sink).await;
    let mut stream = harness::connect(addr).await;

    let fp = request(
        &mut stream,
        "POST /fp-setup",
        &[("Content-Type", "application/octet-stream")],
        &unhex(FP_KEY_MESSAGE),
        1,
    )
    .await;
    assert!(fp.starts_with("RTSP/1.0 200"), "{fp}");

    let mut d = plist::Dictionary::new();
    d.insert("ekey".into(), plist::Value::Data(unhex(FP_EKEY)));
    d.insert("eiv".into(), plist::Value::Data(vec![0u8; 16]));
    d.insert("name".into(), plist::Value::String("Chaz's iPhone".into()));
    d.insert("model".into(), plist::Value::String("iPhone17,1".into()));
    let setup1 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        2,
    )
    .await;
    assert!(setup1.starts_with("RTSP/1.0 200"), "{setup1}");

    let mut s0 = plist::Dictionary::new();
    s0.insert("type".into(), plist::Value::Integer(96i64.into()));
    s0.insert("ct".into(), plist::Value::Integer(2i64.into()));
    s0.insert("spf".into(), plist::Value::Integer(352i64.into()));
    s0.insert("sr".into(), plist::Value::Integer(44100i64.into()));
    s0.insert("isMedia".into(), plist::Value::Boolean(true));
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
    );
    let setup2 = request(
        &mut stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        3,
    )
    .await;
    assert!(setup2.starts_with("RTSP/1.0 200"), "{setup2}");

    let card = tokio::time::timeout(Duration::from_secs(5), cards.recv())
        .await
        .expect("the pipeline should be told who is casting")
        .expect("the manager should still be running");
    assert_eq!(card.display_name.as_deref(), Some("Chaz's iPhone"));
    assert_eq!(card.address.as_deref(), Some("127.0.0.1"));
    assert_eq!(
        card.link.as_deref(),
        Some("AirPlay mirroring · ALAC · 44.1 kHz · stereo")
    );
    // And it is what a person reads, rather than the placeholder.
    assert_eq!(card.label(), Some("Chaz's iPhone"));
}
