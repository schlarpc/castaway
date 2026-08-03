//! A scripted Windows source drives the MICE control channel over real sockets.
//!
//! Tier 1 of ground rule 6 for #166: the real listener, the real framing, the real state
//! machine, and no Windows box. What is proven here is the half MICE actually adds — that
//! a source connecting to 7250 and saying `SOURCE_READY` causes the sink to dial the RTSP
//! port it named, on the address the connection came from.
//!
//! The RTSP half is deliberately *not* re-proven: it is the same `run_session` the Wi-Fi
//! Direct path uses and `actor.rs`'s own tests already drive it from a transcript. What
//! this asserts is the hand-off.

#![allow(clippy::unwrap_used)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

use std::time::Duration;

use proto_miracast::mice::{
    Capability, MiceMessage, MiceOutput, MiceSession, SecurityOptions, SourceId,
};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// A source id that is not all zeroes, so a field left unfilled would be visible.
const SOURCE_ID: [u8; 16] = [
    0x91, 0xF4, 0xAB, 0xE9, 0xEF, 0xF5, 0x46, 0x4A, 0xAE, 0xE2, 0x69, 0x72, 0x2A, 0xED, 0x11, 0xB5,
];

#[tokio::test]
async fn a_source_ready_makes_the_sink_dial_the_rtsp_port_it_named() {
    // The whole of MICE's contribution, end to end: the source tells us where it is
    // listening, and the sink connects there. Everything after this point is the RTSP
    // session that already existed.
    //
    // The source's RTSP listener is bound *first* and its real port used, so this cannot
    // pass by connecting to something that happens to be up.
    let rtsp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rtsp_port = rtsp.local_addr().unwrap().port();

    let control = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_addr = control.local_addr().unwrap();

    // The sink's half: accept the control channel, run the pure session, and act on what
    // it says. This is `mice_actor::serve_one` in miniature — the module's own function
    // hands off into `run_session`, which needs a whole RTSP peer, so the hand-off itself
    // is what is reproduced here.
    let sink = tokio::spawn(async move {
        let (mut stream, peer) = control.accept().await.unwrap();
        let mut session = MiceSession::new("castaway");
        let mut buf = vec![0u8; 1024];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut buf)
                .await
                .unwrap();
            assert!(n > 0, "the source closed without saying anything");
            let message = MiceMessage::decode(&buf[..n]).unwrap();
            for output in session.on_message(&message) {
                if let MiceOutput::Project { rtsp_port, .. } = output {
                    // The address is the connection's, not any TLV's: an address a peer
                    // states about itself is one it can state wrongly.
                    let target = std::net::SocketAddr::new(peer.ip(), rtsp_port);
                    return TcpStream::connect(target)
                        .await
                        .unwrap()
                        .peer_addr()
                        .unwrap();
                }
            }
        }
    });

    let mut source = TcpStream::connect(control_addr).await.unwrap();
    let ready = MiceMessage::SourceReady {
        friendly_name: Some("Dummy1-Kabylake".into()),
        rtsp_port,
        source_id: SourceId::new(SOURCE_ID),
    };
    source.write_all(&ready.encode().unwrap()).await.unwrap();

    let dialled = tokio::time::timeout(Duration::from_secs(5), sink)
        .await
        .expect("the sink must dial the source's RTSP port")
        .unwrap();
    assert_eq!(
        dialled.port(),
        rtsp_port,
        "the sink dialled the wrong port: {dialled}"
    );

    // …and the source's listener really did get the connection.
    let (_, from) = tokio::time::timeout(Duration::from_secs(5), rtsp.accept())
        .await
        .expect("the source's RTSP listener must see the connection")
        .unwrap();
    assert_eq!(from.ip(), std::net::Ipv4Addr::LOCALHOST);
}

#[tokio::test]
async fn a_message_split_across_reads_is_still_one_message() {
    // The control channel is a stream. A source that writes its header and its TLVs in
    // separate segments — which any of them may, and Nagle makes more likely on a busy
    // link — must not be treated as two messages or as a framing error.
    let control = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = control.local_addr().unwrap();

    let sink = tokio::spawn(async move {
        let (mut stream, _) = control.accept().await.unwrap();
        let mut session = MiceSession::new("castaway");
        let mut buf = bytes::BytesMut::new();
        let mut read = vec![0u8; 8];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut stream, &mut read)
                .await
                .unwrap();
            assert!(n > 0, "the source closed mid-message");
            buf.extend_from_slice(&read[..n]);
            let Ok(size) = MiceMessage::framed_len(&buf) else {
                continue;
            };
            if buf.len() < size {
                continue;
            }
            let message = MiceMessage::decode(&buf[..size]).unwrap();
            return session.on_message(&message);
        }
    });

    let mut source = TcpStream::connect(addr).await.unwrap();
    let bytes = MiceMessage::SourceReady {
        friendly_name: Some("Dummy1-Kabylake".into()),
        rtsp_port: 7236,
        source_id: SourceId::new(SOURCE_ID),
    }
    .encode()
    .unwrap();
    // Deliberately mid-TLV.
    source.write_all(&bytes[..7]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    source.write_all(&bytes[7..]).await.unwrap();

    let outputs = tokio::time::timeout(Duration::from_secs(5), sink)
        .await
        .expect("a split message must still arrive")
        .unwrap();
    assert_eq!(
        outputs,
        vec![MiceOutput::Project {
            rtsp_port: 7236,
            friendly_name: Some("Dummy1-Kabylake".into()),
        }]
    );
}

#[tokio::test]
async fn two_messages_in_one_segment_are_both_seen() {
    // The other direction of the same thing: a source that coalesces `SOURCE_READY` and
    // `STOP_PROJECTION` into one write must not have the second silently dropped, or a
    // teardown is lost and the panel keeps a projection nobody is sending.
    let mut session = MiceSession::new("castaway");
    let mut both = MiceMessage::SourceReady {
        friendly_name: None,
        rtsp_port: 7236,
        source_id: SourceId::new(SOURCE_ID),
    }
    .encode()
    .unwrap()
    .to_vec();
    let first_len = both.len();
    both.extend_from_slice(
        &MiceMessage::StopProjection {
            friendly_name: None,
            source_id: SourceId::new(SOURCE_ID),
        }
        .encode()
        .unwrap(),
    );

    let mut outputs = Vec::new();
    let mut offset = 0;
    while offset < both.len() {
        let size = MiceMessage::framed_len(&both[offset..]).unwrap();
        let message = MiceMessage::decode(&both[offset..offset + size]).unwrap();
        outputs.extend(session.on_message(&message));
        offset += size;
    }
    assert_eq!(offset, both.len(), "the second message was not consumed");
    assert!(first_len < both.len());
    assert!(
        matches!(outputs.first(), Some(MiceOutput::Project { .. })),
        "{outputs:?}"
    );
    assert!(
        matches!(outputs.last(), Some(MiceOutput::Close(_))),
        "the teardown must not be lost: {outputs:?}"
    );
}

#[tokio::test]
async fn a_source_asking_for_encryption_is_told_no_rather_than_left_waiting() {
    // We advertise `0x05` — MICE on, encryption off — so a source asking for DTLS has
    // ignored what it read. The cost of not answering is not zero: it would sit through
    // the whole thirty-second establishment timer first.
    let mut session = MiceSession::new("castaway");
    let out = session.on_message(&MiceMessage::SessionRequest {
        friendly_name: "Dummy1-Kabylake".into(),
        source_id: SourceId::new(SOURCE_ID),
        security: SecurityOptions {
            dtls: true,
            pin: false,
        },
    });
    assert!(matches!(out.as_slice(), [MiceOutput::Close(_)]), "{out:?}");
    assert_eq!(
        Capability::insecure().bits(),
        0x05,
        "and the byte that says so is the one the spec gives"
    );
}
