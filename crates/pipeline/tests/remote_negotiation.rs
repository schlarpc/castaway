//! The remote transport, negotiated for real (#18).
//!
//! Every other test of this feature is sans-I/O: the wire parse in `input-touch`, the
//! coalescing queue, the router's contact bookkeeping. All valuable, and none of them
//! would notice if the WebRTC stack never answered an offer at all — which is the one
//! thing that cannot be reasoned about from the types, since it depends on a codec being
//! registered, a port being bound, and gathering finishing.
//!
//! So this drives the real `RemoteService` with a real browser-shaped offer over real
//! sockets, and checks the answer is one a browser could use. No GPU: the feed is never
//! published to, because what is under test is the negotiation and not the pictures.
//!
//! Ports come from a range well clear of `[remote.ice_ports]`, so running this against a
//! live panel cannot take a socket out from under a connected peer.

#![cfg(feature = "remote")]
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use input_touch::RemoteInputQueue;
use pipeline::remote::{RemoteConfig, RemoteService};
use pipeline::stream::feed::LiveFeed;

/// Clear of the 41032–41063 the config defaults to, so running this against a live panel
/// cannot take a socket out from under a connected peer.
///
/// Every test gets its own slice of it. They share a process and cargo runs them
/// concurrently, so two services over one range would each pick its lowest free port —
/// their own pools know nothing of each other — and the second bind would fail. That is a
/// property of the test harness, not of the pool, which is single-service by design.
const BASE: u16 = 45032;

/// The payload type the offer numbers H.264 with.
///
/// Deliberately **not** the 102 this end registers. An answerer must use the *offerer's*
/// numbering, and a browser picks its own — Chromium's H.264 is usually not 102. This file
/// used to offer 102, which made the two numbers accidentally equal and hid a bug where
/// every packet went out stamped with ours: the transceiver rejected all of them as
/// "unsupported codec type", at frame rate, with the connection otherwise healthy.
const OFFERED_PAYLOAD_TYPE: u8 = 96;

/// A minimal offer of the shape a browser sends: one recvonly video m-line and one
/// application m-line for the data channel.
///
/// Hand-written rather than captured because what matters is the *shape* — a receiver
/// asking for H.264 under its own payload type, and a data channel — and a captured one
/// would carry a fingerprint and ufrag that expire into noise.
fn browser_offer() -> String {
    [
        "v=0",
        "o=- 4611731400430051336 2 IN IP4 127.0.0.1",
        "s=-",
        "t=0 0",
        "a=group:BUNDLE 0 1",
        "a=msid-semantic: WMS",
        &format!("m=video 9 UDP/TLS/RTP/SAVPF {OFFERED_PAYLOAD_TYPE}"),
        "c=IN IP4 0.0.0.0",
        "a=rtcp:9 IN IP4 0.0.0.0",
        "a=ice-ufrag:sTuV",
        "a=ice-pwd:0123456789abcdef0123456789",
        "a=ice-options:trickle",
        "a=fingerprint:sha-256 \
         11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:\
11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00",
        "a=setup:actpass",
        "a=mid:0",
        "a=recvonly",
        "a=rtcp-mux",
        &format!("a=rtpmap:{OFFERED_PAYLOAD_TYPE} H264/90000"),
        &format!(
            "a=fmtp:{OFFERED_PAYLOAD_TYPE} \
             level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
        ),
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
        "c=IN IP4 0.0.0.0",
        "a=ice-ufrag:sTuV",
        "a=ice-pwd:0123456789abcdef0123456789",
        "a=fingerprint:sha-256 \
         11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:\
11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00",
        "a=setup:actpass",
        "a=mid:1",
        "a=sctp-port:5000",
    ]
    .join("\r\n")
        + "\r\n"
}

fn service(ports: (u16, u16), accept_input: bool) -> (Arc<RemoteService>, Arc<RemoteInputQueue>) {
    let feed = Arc::new(LiveFeed::new());
    let input = Arc::new(RemoteInputQueue::new(castaway_core::Waker::new()));
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&started);
    let service = RemoteService::new(
        RemoteConfig {
            ice_ports: ports,
            bind_ips: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
            accept_input,
        },
        feed,
        Arc::clone(&input),
        Arc::new(move || flag.store(true, std::sync::atomic::Ordering::Release)),
    )
    .expect("a tokio runtime is available in a test");
    (service, input)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offer_is_answered_with_something_a_browser_could_use() {
    let (service, _input) = service((BASE, BASE + 3), true);
    let answer = service
        .answer(&browser_offer())
        .await
        .expect("the offer should be answerable");

    // The parts a browser will refuse to proceed without. Checked individually rather
    // than against a golden string: the ufrag, the fingerprint and the candidate all
    // change every run, so a fixture would only ever test the parts that do not matter.
    assert!(answer.starts_with("v=0"), "{answer}");
    assert!(answer.contains("a=fingerprint:"), "no DTLS fingerprint");
    assert!(answer.contains("a=ice-ufrag:"), "no ICE credentials");
    assert!(answer.contains("m=video"), "no video m-line");
    assert!(
        answer.contains("m=application"),
        "no data channel: input would have nowhere to go"
    );
    assert!(
        answer.contains("H264/90000"),
        "the codec we register is not in the answer: {answer}"
    );
    // Non-trickle is the whole reason one request is the whole negotiation. An answer
    // with no candidate in it would leave the peer waiting for a channel that never
    // opens.
    assert!(
        answer.contains("a=candidate:"),
        "gathering produced no candidate: {answer}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answer_speaks_the_offerers_payload_type_not_ours() {
    // The bug this file did not catch, because it used to offer the same number this end
    // registers. An answerer must adopt the offerer's numbering; stamping our own on every
    // packet gave a healthy connection that carried nothing but rejections.
    let (service, _input) = service((BASE + 30, BASE + 33), true);
    let answer = service.answer(&browser_offer()).await.unwrap();

    assert!(
        answer.contains(&format!("a=rtpmap:{OFFERED_PAYLOAD_TYPE} H264/90000")),
        "the answer should carry H.264 under the offer's payload type, not ours: {answer}"
    );
    let video_line = answer
        .lines()
        .find(|line| line.starts_with("m=video"))
        .expect("a video m-line");
    assert!(
        video_line
            .split_whitespace()
            .any(|f| f == OFFERED_PAYLOAD_TYPE.to_string()),
        "the m-line should list the offered payload type: {video_line}"
    );

    // …and, the part that actually broke: what the *pump* will stamp on every packet. The
    // SDP above was always correct — webrtc-rs generates it. The bug was writing samples
    // under our own registered number regardless of what the SDP had just agreed.
    let stamped = service.peer_payload_types().await;
    assert_eq!(stamped.len(), 1);
    assert_eq!(
        stamped[0].1,
        Some(OFFERED_PAYLOAD_TYPE),
        "packets would go out under our number instead of the negotiated one"
    );
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_answer_names_a_port_from_the_declared_range() {
    // The point of the pool, and the thing the firewall depends on. A candidate outside
    // the range is one the deployed box drops, and the connection would negotiate and
    // then carry nothing — the worst shape a networking bug has.
    const PORTS: (u16, u16) = (BASE + 10, BASE + 13);
    let (service, _input) = service(PORTS, true);
    let answer = service.answer(&browser_offer()).await.unwrap();

    let ports: Vec<u16> = answer
        .lines()
        .filter(|line| line.starts_with("a=candidate:"))
        .filter_map(|line| line.split_whitespace().nth(5))
        .filter_map(|port| port.parse().ok())
        .collect();
    assert!(!ports.is_empty(), "no candidate ports in {answer}");
    for port in ports {
        assert!(
            (PORTS.0..=PORTS.1).contains(&port),
            "candidate port {port} is outside {PORTS:?}, which the firewall would drop"
        );
    }
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn answering_does_not_wait_out_the_gather_timeout() {
    // A regression test for a bug that was invisible because it *worked*: gathering was
    // signalled with `notify_waiters`, which wakes only tasks already parked. On a LAN the
    // candidates are gathered before the answer path gets as far as waiting, so the
    // notification went on the floor and every connection sat out the full three-second
    // timeout before answering. Nothing failed — the panel was just unusably slow to
    // connect, and no assertion in this file noticed.
    //
    // The bound is deliberately loose. What is being caught is a timeout being waited out,
    // which is an order of magnitude away, not a slow CI box.
    let (service, _input) = service((BASE + 20, BASE + 23), true);
    let started = std::time::Instant::now();
    service.answer(&browser_offer()).await.unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_millis(750),
        "answering took {took:?}, which means gathering was waited out rather than noticed"
    );
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn several_peers_get_different_ports() {
    let (service, _input) = service((45100, 45115), true);
    let first = service.answer(&browser_offer()).await.unwrap();
    let second = service.answer(&browser_offer()).await.unwrap();
    assert_eq!(service.peer_count(), 2);

    let port_of = |sdp: &str| -> u16 {
        sdp.lines()
            .find(|line| line.starts_with("a=candidate:"))
            .and_then(|line| line.split_whitespace().nth(5))
            .and_then(|port| port.parse().ok())
            .expect("a candidate port")
    };
    assert_ne!(
        port_of(&first),
        port_of(&second),
        "two peers on one socket would have their traffic demuxed into each other"
    );
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exhausted_range_is_refused_rather_than_bound_anyway() {
    // One port, two peers. The second must be told no: binding outside the range would
    // produce a candidate the firewall drops, which fails silently later instead of
    // loudly now.
    let (service, _input) = service((45200, 45200), true);
    assert!(service.answer(&browser_offer()).await.is_ok());
    let refused = service.answer(&browser_offer()).await;
    assert!(refused.is_err(), "the second peer should be refused");
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_offer_that_is_not_an_offer_is_an_error_and_keeps_its_port() {
    // Whatever a stranger on the LAN POSTs. The port matters as much as the error: a run
    // of bad offers that each leaked one would exhaust the range and refuse the next real
    // peer.
    let (service, _input) = service((45300, 45301), true);
    for body in ["", "not an sdp at all", "v=0\r\n", "v=0\r\nm=video\r\n"] {
        let _ = service.answer(body).await;
    }
    assert!(
        service.answer(&browser_offer()).await.is_ok(),
        "a real offer after four bad ones should still find a free port"
    );
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_goes_away_cancels_its_contacts_and_returns_its_port() {
    // The failure the whole origin-tracking design exists to prevent, at the layer that
    // has to trigger it.
    let (service, input) = service((45400, 45400), true);
    service.answer(&browser_offer()).await.unwrap();
    assert_eq!(service.peer_count(), 1);

    service.shutdown().await;
    assert_eq!(service.peer_count(), 0);

    // …and the single port is free again, which it would not be if shutdown had only
    // closed the connection.
    let (service2, _) = service_reusing(input, (45400, 45400));
    // Give the OS a moment to release the socket the closed peer held.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        service2.answer(&browser_offer()).await.is_ok(),
        "the port should be bindable again"
    );
    service2.shutdown().await;
}

fn service_reusing(
    input: Arc<RemoteInputQueue>,
    ports: (u16, u16),
) -> (Arc<RemoteService>, Arc<RemoteInputQueue>) {
    let service = RemoteService::new(
        RemoteConfig {
            ice_ports: ports,
            bind_ips: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
            accept_input: true,
        },
        Arc::new(LiveFeed::new()),
        Arc::clone(&input),
        Arc::new(|| {}),
    )
    .unwrap();
    (service, input)
}

#[tokio::test(flavor = "multi_thread")]
async fn connecting_starts_the_encoder() {
    // A peer never fetches a playlist, so nothing else would wake the tap — the panel
    // would negotiate a perfectly good connection and send no pictures down it.
    let feed = Arc::new(LiveFeed::new());
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&started);
    let service = RemoteService::new(
        RemoteConfig {
            ice_ports: (45500, 45501),
            bind_ips: vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)],
            accept_input: true,
        },
        feed,
        Arc::new(RemoteInputQueue::new(castaway_core::Waker::new())),
        Arc::new(move || flag.store(true, std::sync::atomic::Ordering::Release)),
    )
    .unwrap();

    assert!(!started.load(std::sync::atomic::Ordering::Acquire));
    service.answer(&browser_offer()).await.unwrap();
    assert!(
        started.load(std::sync::atomic::Ordering::Acquire),
        "the first peer should have started the encoder"
    );
    service.shutdown().await;
}
