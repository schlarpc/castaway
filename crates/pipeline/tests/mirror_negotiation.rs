//! Receiving a WebRTC mirror, negotiated and carried for real (#248).
//!
//! The reassembly fixtures in `mirror_in::assemble` prove that RTP payloads become
//! frames, and they would pass unchanged if the peer connection never answered an offer
//! at all — that half depends on a codec being registered, a port being bound, DTLS
//! completing and gathering finishing, none of which is visible in the types.
//!
//! So the last test here drives the whole thing: a second, real peer offers a track,
//! `MirrorReceiver` answers it, the offerer writes samples, and the assertion is that
//! decodable frames come out of the [`castaway_core::FrameSource`] the session event
//! carries. "Connection healthy, carries nothing" is the failure this is for.
//!
//! Ports come from a range well clear of `[remote.ice_ports]`, so running this against a
//! live panel cannot take a socket out from under a connected sender.

#![cfg(feature = "remote")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use castaway_core::{FrameSource, MirrorBackend as _};
use pipeline::ice_ports::PortPool;
use pipeline::mirror_in::{MirrorReceiver, MirrorReceiverConfig};

/// Clear of both the 41032–41063 the config defaults to and the 45032+ the
/// remote-control negotiation tests use. Every test here takes its own slice: they share
/// a process and run concurrently, and two pools over one range would each pick their
/// lowest free port and the second bind would fail.
const BASE: u16 = 45532;

/// The payload types the offer numbers its codecs with.
///
/// Deliberately **not** the 102/111 this end registers, for the same reason
/// `remote_negotiation.rs` avoids its own numbers: an answerer adopts the offerer's
/// numbering, and equal numbers hide a whole class of bug.
const OFFERED_H264: u8 = 98;
const OFFERED_OPUS: u8 = 110;

fn receiver(ports: (u16, u16)) -> Arc<MirrorReceiver> {
    MirrorReceiver::new(MirrorReceiverConfig {
        ice_ports: Arc::new(PortPool::new(ports.0, ports.1)),
        bind_ips: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
    })
    .expect("a tokio runtime is available in a test")
}

/// An offer of the shape a sender's screen-share makes: one **sendonly** video m-line,
/// and an audio one when asked for.
///
/// Hand-written rather than captured for the same reason as the remote-control tests:
/// what matters is the shape, and a captured offer's fingerprint and ufrag expire into
/// noise. `audio_port` of `"0"` is a *rejected* section, which is what a sender sends
/// after renegotiating its microphone away.
fn sender_offer(audio_port: Option<&str>) -> String {
    let mut lines = vec![
        "v=0".to_owned(),
        "o=- 4611731400430051336 2 IN IP4 127.0.0.1".to_owned(),
        "s=-".to_owned(),
        "t=0 0".to_owned(),
        "a=group:BUNDLE 0".to_owned(),
        "a=msid-semantic: WMS".to_owned(),
        format!("m=video 9 UDP/TLS/RTP/SAVPF {OFFERED_H264}"),
        "c=IN IP4 0.0.0.0".to_owned(),
        "a=rtcp:9 IN IP4 0.0.0.0".to_owned(),
        "a=ice-ufrag:sTuV".to_owned(),
        "a=ice-pwd:0123456789abcdef0123456789".to_owned(),
        "a=ice-options:trickle".to_owned(),
        "a=fingerprint:sha-256 \
         11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:\
11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00"
            .to_owned(),
        "a=setup:actpass".to_owned(),
        "a=mid:0".to_owned(),
        "a=sendonly".to_owned(),
        "a=rtcp-mux".to_owned(),
        format!("a=rtpmap:{OFFERED_H264} H264/90000"),
        format!(
            "a=fmtp:{OFFERED_H264} \
             level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        ),
    ];
    if let Some(port) = audio_port {
        lines.extend([
            format!("m=audio {port} UDP/TLS/RTP/SAVPF {OFFERED_OPUS}"),
            "c=IN IP4 0.0.0.0".to_owned(),
            "a=ice-ufrag:sTuV".to_owned(),
            "a=ice-pwd:0123456789abcdef0123456789".to_owned(),
            "a=fingerprint:sha-256 \
             11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:\
11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00"
                .to_owned(),
            "a=setup:actpass".to_owned(),
            "a=mid:1".to_owned(),
            "a=sendonly".to_owned(),
            "a=rtcp-mux".to_owned(),
            format!("a=rtpmap:{OFFERED_OPUS} opus/48000/2"),
        ]);
        lines[4] = "a=group:BUNDLE 0 1".to_owned();
    }
    lines.join("\r\n") + "\r\n"
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offer_is_answered_with_something_a_sender_could_use() {
    let receiver = receiver((BASE, BASE + 3));
    let answer = receiver
        .answer(&sender_offer(Some("9")))
        .await
        .expect("the offer should be answerable");

    // The parts a sender will refuse to proceed without. Checked individually rather than
    // against a golden string: the ufrag, the fingerprint and the candidate all change
    // every run.
    assert!(answer.sdp.starts_with("v=0"), "{}", answer.sdp);
    assert!(answer.sdp.contains("a=fingerprint:"), "no DTLS fingerprint");
    assert!(answer.sdp.contains("a=ice-ufrag:"), "no ICE credentials");
    assert!(answer.sdp.contains("m=video"), "no video m-line");
    assert!(answer.sdp.contains("m=audio"), "no audio m-line");
    // The offerer's numbering, not ours — this end registers 102 and 111.
    assert!(
        answer
            .sdp
            .contains(&format!("a=rtpmap:{OFFERED_H264} H264/90000")),
        "the answer should carry H.264 under the offer's payload type: {}",
        answer.sdp
    );
    // Non-trickle: FCast signals the answer once and has no second message, so an answer
    // with no candidate leaves the sender waiting for a connection that never comes.
    assert!(
        answer.sdp.contains("a=candidate:"),
        "gathering produced no candidate: {}",
        answer.sdp
    );
    // The direction that makes this the *receiving* end.
    assert!(
        answer.sdp.contains("a=recvonly") || answer.sdp.contains("a=inactive"),
        "we add no track of our own, so the answer must not claim to send: {}",
        answer.sdp
    );
    receiver.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_audio_section_produces_a_silent_mirror() {
    let receiver = receiver((BASE + 10, BASE + 13));
    let with_audio = receiver.answer(&sender_offer(Some("9"))).await.unwrap();
    assert!(
        with_audio.audio.is_some(),
        "an offered microphone means the session event carries an audio half"
    );

    let rejected = receiver.answer(&sender_offer(Some("0"))).await.unwrap();
    assert!(
        rejected.audio.is_none(),
        "a section offered on port 0 is a rejected one; promising an audio track that \
         never carries a packet leaves the mixer waiting on silence"
    );

    let video_only = receiver.answer(&sender_offer(None)).await.unwrap();
    assert!(video_only.audio.is_none());
    receiver.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answer_names_a_port_from_the_declared_range() {
    // The point of the pool, and what the firewall depends on: a candidate outside the
    // range is one the deployed box drops, and the session would negotiate and then carry
    // nothing.
    const PORTS: (u16, u16) = (BASE + 20, BASE + 23);
    let receiver = receiver(PORTS);
    let answer = receiver.answer(&sender_offer(None)).await.unwrap();

    let ports: Vec<u16> = answer
        .sdp
        .lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter_map(|port| port.parse().ok())
        .collect();
    assert!(!ports.is_empty(), "no candidate ports in {}", answer.sdp);
    for port in ports {
        assert!(
            (PORTS.0..=PORTS.1).contains(&port),
            "candidate port {port} is outside {PORTS:?}, which the firewall would drop"
        );
    }
    receiver.shutdown().await;
}

/// A second peer offers a real track, and the pictures it writes come out the other end
/// as frames the decoder could open.
///
/// The one test that would fail if DTLS never completed, if the codec were registered as
/// the wrong kind, or if the depacketizer were wired to the wrong track — none of which
/// the SDP assertions above can see.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_peers_pictures_arrive_as_frames() {
    use rtc::interceptor::Registry;
    use rtc::media_stream::MediaStreamTrack;
    use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
    use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264};
    use rtc::peer_connection::configuration::RTCConfigurationBuilder;
    use rtc::peer_connection::sdp::RTCSessionDescription;
    use rtc::rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    };
    use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
    use webrtc::media_stream::track_local::TrackLocal;
    use webrtc::peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    };
    use webrtc::runtime::default_runtime;

    const PORTS: (u16, u16) = (BASE + 30, BASE + 33);
    const OFFERER_PORT: u16 = BASE + 40;
    const SSRC: u32 = 0x1234_5678;

    struct Gathered(Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Gathered {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                self.0.notify_waiters();
            }
        }
    }

    let receiver = receiver(PORTS);

    // The sender: one H.264 track and nothing else.
    let codec = RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                .to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: OFFERED_H264,
    };
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(codec.clone(), RtpCodecKind::Video)
        .unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media_engine).unwrap();
    let gathered = Arc::new(tokio::sync::Notify::new());
    let offerer = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Gathered(Arc::clone(&gathered))))
        .with_runtime(default_runtime().unwrap())
        .with_udp_addrs(vec![std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            OFFERER_PORT,
        )])
        .build()
        .await
        .unwrap();
    let offerer: Arc<dyn PeerConnection> = Arc::from(Box::new(offerer) as Box<_>);

    let track = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
            "sender".to_owned(),
            "screen".to_owned(),
            "screen".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(SSRC),
                    ..Default::default()
                },
                codec: codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ))
        .unwrap(),
    );
    offerer
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await
        .unwrap();

    let offer = offerer.create_offer(None).await.unwrap();
    offerer.set_local_description(offer).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), gathered.notified()).await;
    let offer = offerer.local_description().await.unwrap();

    let answered = receiver.answer(&offer.sdp).await.expect("answerable");
    offerer
        .set_remote_description(RTCSessionDescription::answer(answered.sdp).unwrap())
        .await
        .unwrap();

    let FrameSource::Encoded(mut frames) = answered.video else {
        panic!("a mirror's video must arrive as encoded frames");
    };

    // Keep writing until the transport comes up: `write_sample` fails until the track is
    // bound, and how long DTLS takes is not ours to decide. One IDR NAL per sample, so
    // every frame that arrives is decodable on its own.
    let idr: bytes::Bytes = bytes::Bytes::from_static(&[
        0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x10, 0xff, 0xfe,
    ]);
    let writer = tokio::spawn(async move {
        let mut sent = 0usize;
        for _ in 0..400 {
            let sample = rtc::media::Sample {
                data: idr.clone(),
                duration: Duration::from_millis(33),
                ..Default::default()
            };
            if track
                .write_sample(SSRC, OFFERED_H264, &sample, &[])
                .await
                .is_ok()
            {
                sent += 1;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sent
    });

    let frame = tokio::time::timeout(Duration::from_secs(10), frames.recv())
        .await
        .expect("no frame arrived from a connected peer")
        .expect("the frame channel closed before a frame arrived");
    writer.abort();

    assert_eq!(frame.video_codec, Some(castaway_core::VideoCodec::H264));
    assert!(frame.audio_codec.is_none());
    assert!(frame.keyframe, "an IDR access unit must be flagged as one");
    assert!(
        frame.data.starts_with(&[0, 0, 0, 1]),
        "the decoder is opened for Annex-B: {:02x?}",
        &frame.data[..frame.data.len().min(8)]
    );
    assert_eq!(
        frame.data[4] & 0x1f,
        5,
        "the NAL that went in is the NAL that came out"
    );

    receiver.shutdown().await;
    let _ = offerer.close().await;
}
