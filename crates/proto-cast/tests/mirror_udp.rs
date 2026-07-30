//! End-to-end test of the mirroring UDP actor.
//!
//! `openscreen_stream.rs` proves the pure core agrees with openscreen. This proves the
//! actor around it is wired up: the same openscreen-generated datagrams go in over a
//! real UDP socket, and the decrypted frames come out of the [`FrameSource`] the
//! pipeline would be handed — with the RTCP feedback a sender needs arriving back at
//! the address the packets came from.
//!
//! Nothing here needs hardware or a human, per ground rule 6. Loopback is enough.

#![allow(clippy::unwrap_used)]

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use castaway_core::FrameSource;
use common::{datagrams, expected_frames, mirror_config, RECEIVER_SSRC, SENDER_SSRC};
use proto_cast::rtp_actor::MirrorSocket;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Everything a test needs to talk to a running actor: a socket that plays the sender,
/// and the frame channel the pipeline would own.
struct Harness {
    sender: UdpSocket,
    receiver_addr: SocketAddr,
    frames: mpsc::Receiver<castaway_core::EncodedFrame>,
}

async fn start() -> Harness {
    let socket = MirrorSocket::bind(LOCALHOST, castaway_core::MediaPorts::Ephemeral)
        .await
        .unwrap();
    let receiver_addr = SocketAddr::new(LOCALHOST, socket.port());

    let (video, audio, rtp) = socket.start(&mirror_config(receiver_addr.port()));
    assert!(audio.is_none(), "the fixture stream is video only");
    let FrameSource::Encoded(frames) = video else {
        panic!("mirroring must hand the pipeline encoded frames, not a URL");
    };
    tokio::spawn(rtp.run());

    Harness {
        sender: UdpSocket::bind(SocketAddr::new(LOCALHOST, 0))
            .await
            .unwrap(),
        receiver_addr,
        frames,
    }
}

/// Wait for `count` frames, failing rather than hanging if the actor stops producing.
async fn collect(harness: &mut Harness, count: usize) -> Vec<castaway_core::EncodedFrame> {
    let mut out = Vec::with_capacity(count);
    while out.len() < count {
        let frame = tokio::time::timeout(Duration::from_secs(5), harness.frames.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out after {} of {count} frames", out.len()))
            .expect("the actor closed the frame channel early");
        out.push(frame);
    }
    out
}

#[tokio::test]
async fn frames_sent_over_udp_arrive_decrypted_at_the_pipeline() {
    let mut harness = start().await;
    for datagram in datagrams() {
        harness
            .sender
            .send_to(&datagram, harness.receiver_addr)
            .await
            .unwrap();
    }

    let want = expected_frames();
    let got = collect(&mut harness, want.len()).await;

    for (got, want) in got.iter().zip(want.iter()) {
        assert_eq!(
            got.data.as_ref(),
            want.payload.as_slice(),
            "frame {} came out of the socket path with the wrong bytes",
            want.frame_id
        );
        assert_eq!(got.video_codec, Some(castaway_core::VideoCodec::Vp8));
        assert_eq!(got.audio_codec, None);
    }

    // Frame 0 is the fixture's key frame; only it should be flagged, or the pipeline
    // cannot tell where it is allowed to start decoding.
    assert!(got[0].keyframe);
    assert!(got[1..].iter().all(|frame| !frame.keyframe));

    // Presentation times are the stream's 90 kHz ticks measured from the first frame.
    // The generator spaces frames 3000 ticks apart, which is 1/30 s.
    assert_eq!(got[0].pts, Duration::ZERO);
    assert_eq!(got[1].pts, Duration::from_nanos(33_333_333));
    assert_eq!(got[2].pts, Duration::from_nanos(66_666_666));
}

#[tokio::test]
async fn a_missing_packet_is_nacked_back_to_wherever_the_stream_came_from() {
    let mut harness = start().await;
    let all = datagrams();

    // Hold back one packet from the middle of frame 1, as a lossy link would.
    let held = all[2].clone();
    for (index, datagram) in all.iter().enumerate() {
        if index != 2 {
            harness
                .sender
                .send_to(datagram, harness.receiver_addr)
                .await
                .unwrap();
        }
    }

    // Frame 0 is complete and independent, so it comes through even though frame 1 is
    // stuck behind the hole.
    let first = collect(&mut harness, 1).await;
    assert_eq!(first[0].data.as_ref(), expected_frames()[0].payload);

    // The actor learned our address from the datagrams themselves — the ANSWER never
    // carried it — so the report must come back here.
    let mut buf = [0u8; 1500];
    let (len, from) =
        tokio::time::timeout(Duration::from_secs(5), harness.sender.recv_from(&mut buf))
            .await
            .expect("no RTCP feedback within 5s; the sender would stop retransmitting")
            .unwrap();
    assert_eq!(from, harness.receiver_addr);

    let report = &buf[..len];
    assert_eq!(len % 4, 0, "RTCP packets are 32-bit word aligned");
    assert!(
        report.windows(4).any(|word| word == b"CAST"),
        "feedback must carry the Cast-specific block, or the sender learns nothing"
    );

    // Retransmit, and the rest of the stream falls out.
    harness
        .sender
        .send_to(&held, harness.receiver_addr)
        .await
        .unwrap();
    let rest = collect(&mut harness, expected_frames().len() - 1).await;
    for (got, want) in rest.iter().zip(expected_frames()[1..].iter()) {
        assert_eq!(got.data.as_ref(), want.payload.as_slice());
    }
}

#[tokio::test]
async fn datagrams_for_another_ssrc_are_ignored_rather_than_misparsed() {
    let mut harness = start().await;

    // The receiver's own SSRC shows up on this port in real sessions: senders put their
    // RTCP sender reports there. Nothing addressed to it may reach the video stream.
    let mut stray = datagrams()[0].to_vec();
    stray[8..12].copy_from_slice(&RECEIVER_SSRC.to_be_bytes());
    harness
        .sender
        .send_to(&stray, harness.receiver_addr)
        .await
        .unwrap();

    // Then the real frame 0, under the sender's SSRC.
    assert_ne!(SENDER_SSRC, RECEIVER_SSRC);
    harness
        .sender
        .send_to(&datagrams()[0], harness.receiver_addr)
        .await
        .unwrap();

    let got = collect(&mut harness, 1).await;
    assert_eq!(got[0].data.as_ref(), expected_frames()[0].payload);
    assert!(
        harness.frames.try_recv().is_err(),
        "the stray datagram produced a frame it had no business producing"
    );
}

#[tokio::test]
async fn the_receive_loop_stops_when_the_pipeline_drops_its_end() {
    let socket = MirrorSocket::bind(LOCALHOST, castaway_core::MediaPorts::Ephemeral)
        .await
        .unwrap();
    let addr = SocketAddr::new(LOCALHOST, socket.port());
    let (video, _audio, rtp) = socket.start(&mirror_config(addr.port()));
    let task = tokio::spawn(rtp.run());

    // The pipeline going away is the loop's shutdown signal; there is no separate
    // channel for it, so if this does not end the task, a finished session leaks one.
    drop(video);

    let sender = UdpSocket::bind(SocketAddr::new(LOCALHOST, 0))
        .await
        .unwrap();
    for datagram in datagrams() {
        // Sending is what makes the loop notice: it only learns the channel is closed
        // when it has a frame to push into it.
        let _ = sender.send_to(&datagram, addr).await;
    }

    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the RTP loop outlived the pipeline that owned it")
        .unwrap();
}

/// The declared media port range is respected: the socket lands inside it, taken ports
/// are skipped rather than fatal, and exhaustion is an error instead of a silent fall
/// back to an ephemeral port — which no firewall rule could have named.
#[tokio::test]
async fn a_declared_range_is_honoured_skipping_taken_ports() {
    use castaway_core::{MediaPorts, PortRange};

    let range = MediaPorts::Range(PortRange::new(42500, 42502).unwrap());

    // Occupy the first candidate so the bind has something to skip.
    let blocker = UdpSocket::bind(SocketAddr::new(LOCALHOST, 42500))
        .await
        .unwrap();

    let socket = MirrorSocket::bind(LOCALHOST, range).await.unwrap();
    assert!(
        (42501..=42502).contains(&socket.port()),
        "expected the next free port in the range, got {}",
        socket.port()
    );

    // Take whatever is left; the range is now full.
    let second = MirrorSocket::bind(LOCALHOST, range).await.unwrap();
    assert_ne!(second.port(), socket.port());

    let exhausted = MirrorSocket::bind(LOCALHOST, range).await;
    assert!(
        exhausted.is_err(),
        "an exhausted range must refuse, not fall back to an ephemeral port"
    );
    drop(blocker);
}
