//! The whole adapter over real loopback sockets: a scripted sender dials the
//! listener, and the assertions are on what came out the other two ends — the
//! `SessionEvent` channel toward the pipeline, and the bytes written back to the
//! socket. No sleeps stand in for conditions (#236): everything is awaited with the
//! shared deadline poll.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use castaway_core::{
    ControlTxn, ProtocolKind, SessionEvent, SessionSink, SourceAdapter, SourceId, SourceMessage,
};
use castaway_test_support::eventually;
use proto_fcast::wire::{self, Frame, Opcode};
use proto_fcast::FCastReceiver;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Start an adapter on an ephemeral loopback port; returns its address and the
/// pipeline-facing event channel.
async fn started() -> (std::net::SocketAddr, mpsc::Receiver<SourceMessage>) {
    let (tx, rx) = mpsc::channel(32);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::FCast, "test"), tx);
    let receiver =
        Arc::new(FCastReceiver::new("Test Panel").with_listen(([127, 0, 0, 1], 0).into()));
    let adapter = Arc::clone(&receiver);
    tokio::spawn(async move {
        let _ = adapter.run(sink).await;
    });
    let addr = eventually("the listener to bind", || receiver.bound_addr()).await;
    (addr, rx)
}

/// A scripted sender connection with a local read buffer.
struct Sender {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Sender {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(addr).await.unwrap(),
            buf: Vec::new(),
        }
    }

    async fn send(&mut self, frame: &Frame) {
        self.stream
            .write_all(&wire::encode(frame).unwrap())
            .await
            .unwrap();
    }

    async fn send_json(&mut self, opcode: Opcode, body: &str) {
        self.send(&Frame::with_body(opcode, body.as_bytes().to_vec()))
            .await;
    }

    /// Read frames until one matches `want`, skipping interleaved broadcasts, with
    /// a deadline that reports what it was waiting for.
    async fn expect(&mut self, want: Opcode) -> Frame {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            if let Some((frame, consumed)) = wire::try_decode(&self.buf).unwrap() {
                self.buf.drain(..consumed);
                if frame.opcode == want {
                    return frame;
                }
                continue;
            }
            let mut chunk = [0u8; 1024];
            let read = tokio::time::timeout_at(deadline, self.stream.read(&mut chunk))
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for a {want:?} frame"));
            match read {
                Ok(0) => panic!("EOF while waiting for a {want:?} frame"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) => panic!("read failed while waiting for a {want:?} frame: {e}"),
            }
        }
    }

    /// Await EOF — the adapter hung up.
    async fn expect_eof(&mut self) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        let mut chunk = [0u8; 256];
        loop {
            let read = tokio::time::timeout_at(deadline, self.stream.read(&mut chunk))
                .await
                .expect("timed out waiting for the adapter to hang up");
            match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {} // drain whatever was in flight
            }
        }
    }
}

const DEADLINE: Duration = Duration::from_secs(5);

async fn next_event(rx: &mut mpsc::Receiver<SourceMessage>) -> SessionEvent {
    tokio::time::timeout(DEADLINE, rx.recv())
        .await
        .expect("timed out waiting for a session event")
        .expect("the event channel closed")
        .event
}

/// The full v3 session a real SDK sender runs (its exact preamble is pinned in
/// `tests/fixtures/`): greeting exchange, `Initial` back, then a `Play` that must
/// come out the pipeline side as `Play` + `NowPlaying` + `ControlSurface`, and a
/// `Pause` that must come out as an absolute `Control(Pause)`.
#[tokio::test]
async fn a_v3_sender_plays_and_pauses() {
    let (addr, mut rx) = started().await;
    let mut sender = Sender::connect(addr).await;

    let greeting = sender.expect(Opcode::Version).await;
    assert_eq!(greeting.body, br#"{"version":3}"#.to_vec());
    sender.send_json(Opcode::Version, r#"{"version":4}"#).await;
    let initial = sender.expect(Opcode::Initial).await;
    let body: serde_json::Value = serde_json::from_slice(&initial.body).unwrap();
    assert_eq!(body["displayName"], "Test Panel");

    sender
        .send_json(
            Opcode::Play,
            r#"{"container":"video/mp4","url":"http://h/v.mp4","time":10.0}"#,
        )
        .await;
    // PlayChanged reaches the *sender* side too, as a v3 PlayUpdate broadcast.
    let update = sender.expect(Opcode::PlayUpdate).await;
    let body: serde_json::Value = serde_json::from_slice(&update.body).unwrap();
    assert_eq!(body["playData"]["url"], "http://h/v.mp4");

    // The pipeline side: Play (with the start offset), then the control surface
    // right behind it — the manager only accepts a surface from the active source.
    let play = next_event(&mut rx).await;
    let SessionEvent::Play { source, start } = play else {
        panic!("expected Play, got {play:?}");
    };
    assert_eq!(source.url().as_str(), "http://h/v.mp4");
    assert_eq!(start, Some(Duration::from_secs(10)));
    let surface = next_event(&mut rx).await;
    assert!(matches!(surface, SessionEvent::ControlSurface(_)));
    // NowPlaying, the volume-apply, and finally the sender's identity: drain to
    // the SourceInfo that closes a load's event train.
    loop {
        match next_event(&mut rx).await {
            SessionEvent::SourceInfo(_) => break,
            SessionEvent::Control(ControlTxn::Volume(_)) | SessionEvent::NowPlaying(_) => {}
            other => panic!("unexpected event {other:?}"),
        }
    }

    sender.send(&Frame::bare(Opcode::Pause)).await;
    let paused = next_event(&mut rx).await;
    assert!(matches!(paused, SessionEvent::Control(ControlTxn::Pause)));
    // And the pause is broadcast back as a state-2 PlaybackUpdate — behind however
    // many state-1 updates the play already queued, so read until it lands.
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no paused PlaybackUpdate arrived"
        );
        let update = sender.expect(Opcode::PlaybackUpdate).await;
        let body: serde_json::Value = serde_json::from_slice(&update.body).unwrap();
        if body["state"] == 2 {
            break;
        }
        assert_eq!(body["state"], 1, "only play-time updates may precede it");
    }
}

/// Multi-sender sync (the v3 feature the issue calls out): a second phone joining
/// mid-session is told what is playing in its `Initial`, and both hear a
/// `PlayUpdate` when either loads something new.
#[tokio::test]
async fn a_second_sender_joins_in_sync() {
    let (addr, mut rx) = started().await;
    let mut first = Sender::connect(addr).await;
    first.expect(Opcode::Version).await;
    first.send_json(Opcode::Version, r#"{"version":3}"#).await;
    first.expect(Opcode::Initial).await;
    first
        .send_json(
            Opcode::Play,
            r#"{"container":"video/mp4","url":"http://h/first.mp4"}"#,
        )
        .await;
    first.expect(Opcode::PlayUpdate).await;

    let mut second = Sender::connect(addr).await;
    second.expect(Opcode::Version).await;
    second.send_json(Opcode::Version, r#"{"version":3}"#).await;
    let initial = second.expect(Opcode::Initial).await;
    let body: serde_json::Value = serde_json::from_slice(&initial.body).unwrap();
    assert_eq!(
        body["playData"]["url"], "http://h/first.mp4",
        "a late joiner must see the running session"
    );

    second
        .send_json(
            Opcode::Play,
            r#"{"container":"video/mp4","url":"http://h/second.mp4"}"#,
        )
        .await;
    let update = first.expect(Opcode::PlayUpdate).await;
    let body: serde_json::Value = serde_json::from_slice(&update.body).unwrap();
    assert_eq!(
        body["playData"]["url"], "http://h/second.mp4",
        "the first sender must hear the takeover"
    );
    while !matches!(next_event(&mut rx).await, SessionEvent::Play { .. }) {}
}

/// A v1 sender — no `Version`, verbs only — still plays, and the broadcasts it gets
/// are in its own dialect: `{"time","state"}` with no `generationTime`.
#[tokio::test]
async fn a_v1_sender_is_spoken_to_in_v1() {
    let (addr, mut rx) = started().await;
    let mut sender = Sender::connect(addr).await;
    sender.expect(Opcode::Version).await;

    sender
        .send_json(
            Opcode::Play,
            r#"{"container":"video/mp4","url":"http://h/old.mp4"}"#,
        )
        .await;
    while !matches!(next_event(&mut rx).await, SessionEvent::Play { .. }) {}

    let update = sender.expect(Opcode::PlaybackUpdate).await;
    let body: serde_json::Value = serde_json::from_slice(&update.body).unwrap();
    assert_eq!(body["state"], 1);
    assert!(
        body.get("generationTime").is_none(),
        "a v1 sender must get the v1 shape, got {body}"
    );
}

/// A frame outside the protocol we agreed to — opcode 20 is v4's FlatBuffers surface
/// — closes the connection (decline, don't guess), and takes nothing else down: a
/// session on another socket keeps working.
#[tokio::test]
async fn a_v4_frame_is_declined_by_disconnect() {
    let (addr, mut rx) = started().await;
    let mut healthy = Sender::connect(addr).await;
    healthy.expect(Opcode::Version).await;
    healthy.send_json(Opcode::Version, r#"{"version":3}"#).await;
    healthy.expect(Opcode::Initial).await;

    let mut confused = Sender::connect(addr).await;
    confused.expect(Opcode::Version).await;
    confused
        .stream
        .write_all(&[0x01, 0x00, 0x00, 0x00, 20])
        .await
        .unwrap();
    confused.expect_eof().await;

    // The healthy session is unaffected.
    healthy
        .send_json(
            Opcode::Play,
            r#"{"container":"video/mp4","url":"http://h/v.mp4"}"#,
        )
        .await;
    while !matches!(next_event(&mut rx).await, SessionEvent::Play { .. }) {}
}

/// A refused load answers the asking sender with `PlaybackError` and leaves the
/// connection up — refusal is an answer, not a fault.
#[tokio::test]
async fn a_refused_load_is_an_error_reply_not_a_disconnect() {
    let (addr, _rx) = started().await;
    let mut sender = Sender::connect(addr).await;
    sender.expect(Opcode::Version).await;
    sender.send_json(Opcode::Version, r#"{"version":3}"#).await;
    sender.expect(Opcode::Initial).await;

    sender
        .send_json(
            Opcode::Play,
            r#"{"container":"application/dash+xml","content":"<MPD/>"}"#,
        )
        .await;
    let error = sender.expect(Opcode::PlaybackError).await;
    let body: serde_json::Value = serde_json::from_slice(&error.body).unwrap();
    assert!(
        body["message"].as_str().unwrap().contains("inline content"),
        "the refusal names the problem: {body}"
    );

    // Still connected and still working.
    sender.send(&Frame::bare(Opcode::Ping)).await;
    sender.expect(Opcode::Pong).await;
}
