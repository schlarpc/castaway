//! Replay of the reference sender's captured transcripts (`tests/fixtures/`) through
//! the pure session + player: the wire behaviour of `fcast-sender-sdk` 0.3.0 — the
//! SDK Grayjay embeds — is the spec these assertions pin (ground rule 9).

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use proto_fcast::messages::PlayState;
use proto_fcast::player::Player;
use proto_fcast::session::{
    ReceiverIdentity, SenderCommand, Session, SessionContext, SessionVersion,
};
use proto_fcast::wire::{self, Frame, Opcode};

/// One transcript row.
struct Row {
    inbound: bool,
    t_ms: u64,
    frame: Frame,
    raw: Vec<u8>,
}

fn parse_fixture(jsonl: &str) -> Vec<Row> {
    jsonl
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            let raw = unhex(value["hex"].as_str().unwrap());
            let (frame, consumed) = wire::try_decode(&raw).unwrap().unwrap();
            assert_eq!(consumed, raw.len(), "fixture rows hold exactly one frame");
            Row {
                inbound: value["dir"] == "in",
                t_ms: value["t_ms"].as_u64().unwrap(),
                frame,
                raw,
            }
        })
        .collect()
}

fn unhex(s: &str) -> Vec<u8> {
    s.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hi = char::from(pair[0]).to_digit(16).unwrap();
            let lo = char::from(pair[1]).to_digit(16).unwrap();
            u8::try_from(hi * 16 + lo).unwrap()
        })
        .collect()
}

fn identity() -> ReceiverIdentity {
    ReceiverIdentity {
        display_name: "dma.space/screen".into(),
        app_name: "castaway".into(),
        app_version: "0.1.0".into(),
    }
}

/// Feed every inbound frame of a transcript through a fresh session (and the
/// player, when a frame carries a command). Returns the session, the player, and
/// every command the sender's bytes produced.
fn replay(jsonl: &str) -> (Session, Player, Vec<SenderCommand>) {
    let receiver = identity();
    let rows = parse_fixture(jsonl);
    let (mut session, greeting) = Session::new();

    // Our scripted capture receiver greeted with exactly the bytes our session
    // greets with — so the sender behaviour recorded after it applies to us.
    let scripted_greeting = rows
        .iter()
        .find(|r| !r.inbound)
        .expect("every transcript records the greeting");
    assert_eq!(
        wire::encode(&greeting).unwrap(),
        scripted_greeting.raw,
        "the capture harness greeted with different bytes than the shipped session"
    );

    let mut player = Player::new();
    let mut commands = Vec::new();
    for row in rows.iter().filter(|r| r.inbound) {
        let play_data = player.play_data().cloned();
        let ctx = SessionContext {
            wall_ms: 1_754_700_000_000 + row.t_ms,
            receiver: &receiver,
            play_data: play_data.as_ref(),
            volume: player.volume(),
        };
        let reaction = session
            .on_frame(Duration::from_millis(row.t_ms), &ctx, &row.frame)
            .unwrap_or_else(|e| panic!("frame {:?} faulted: {e}", row.frame.opcode));
        if let Some(command) = reaction.command {
            commands.push(command.clone());
            // Apply as the adapter would; refusals are scenario bugs here.
            match command {
                SenderCommand::Load(play) => {
                    player.load(*play).unwrap();
                }
                SenderCommand::Pause => {
                    player.pause();
                }
                SenderCommand::Resume => {
                    player.resume();
                }
                SenderCommand::Stop => {
                    player.stop();
                }
                SenderCommand::Seek(t) => {
                    player.seek(t);
                }
                SenderCommand::SetVolume(v) => {
                    player.set_volume(v);
                }
                SenderCommand::SetSpeed(_) | SenderCommand::SetPlaylistItem(_) => {
                    // Asserted per-scenario below; speed refusal is deliberate.
                }
            }
        }
    }
    (session, player, commands)
}

/// Every transcript begins with the SDK preamble: `Version {4}` (downgraded to a v3
/// session by our reply), `Initial`, and an automatic `MediaItemEnd` subscription.
fn assert_preamble(session: &Session) {
    assert_eq!(session.version(), Some(SessionVersion::V3));
    assert_eq!(
        session.peer().unwrap().app_name.as_deref(),
        Some("FCast Sender SDK v0.3.0")
    );
    assert!(
        session
            .subscriptions()
            .wants(proto_fcast::messages::MediaItemEventKind::End),
        "the SDK auto-subscribes to MediaItemEnd on every connection"
    );
}

#[test]
fn play_url() {
    let (session, player, commands) = replay(include_str!("fixtures/sdk-0.3.0-play-url.jsonl"));
    assert_preamble(&session);
    let Some(SenderCommand::Load(play)) = commands.last() else {
        panic!("expected a Load, got {commands:?}");
    };
    assert_eq!(play.container, "video/mp4");
    assert_eq!(
        play.url.as_deref(),
        Some("http://example.com/media/BigBuckBunny.mp4")
    );
    assert_eq!(play.time, Some(10.0));
    assert_eq!(play.speed, Some(1.25));
    assert_eq!(
        play.headers.as_ref().unwrap()["Authorization"],
        "Bearer sekrit"
    );
    assert_eq!(
        player.play_data().unwrap().url,
        play.url,
        "the load applied"
    );
}

/// The playlist the reference repository ships as its own example
/// (`video_playlist_example.json`), sent by the real sender as inline
/// `application/json` content, parses and loads at item 0.
#[test]
fn play_playlist() {
    let (session, player, commands) =
        replay(include_str!("fixtures/sdk-0.3.0-play-playlist.jsonl"));
    assert_preamble(&session);
    assert!(matches!(commands.last(), Some(SenderCommand::Load(_))));
    let snapshot = player.snapshot(None);
    assert_eq!(snapshot.state, PlayState::Playing);
    assert_eq!(snapshot.item_index, Some(0));
}

/// A different real implementation: nixpkgs' 2024 pre-SDK terminal client. No
/// `Version` frame at all — the `Play` verb *is* the hello — and every optional
/// field written as an explicit `null`. The session must conclude v1 and load
/// anyway.
#[test]
fn the_2024_client_speaks_implicit_v1() {
    let (session, player, commands) = replay(include_str!("fixtures/client-2024-play.jsonl"));
    assert_eq!(session.version(), Some(SessionVersion::V1));
    assert!(session.peer().is_none(), "v1 has no Initial to identify by");
    let Some(SenderCommand::Load(play)) = commands.last() else {
        panic!("expected a Load, got {commands:?}");
    };
    assert_eq!(play.url.as_deref(), Some("http://example.com/v.mp4"));
    assert_eq!(play.content, None, "explicit null parses as absent");
    assert!(player.play_data().is_some());
}

#[test]
fn transport_verbs() {
    for (fixture, expected) in [
        (
            include_str!("fixtures/sdk-0.3.0-pause.jsonl"),
            SenderCommand::Pause,
        ),
        (
            include_str!("fixtures/sdk-0.3.0-resume.jsonl"),
            SenderCommand::Resume,
        ),
        (
            include_str!("fixtures/sdk-0.3.0-stop.jsonl"),
            SenderCommand::Stop,
        ),
        (
            include_str!("fixtures/sdk-0.3.0-seek.jsonl"),
            SenderCommand::Seek(Duration::from_secs_f64(100.5)),
        ),
        (
            include_str!("fixtures/sdk-0.3.0-set-volume.jsonl"),
            SenderCommand::SetVolume(0.5),
        ),
        (
            include_str!("fixtures/sdk-0.3.0-set-speed.jsonl"),
            SenderCommand::SetSpeed(2.0),
        ),
        (
            include_str!("fixtures/sdk-0.3.0-set-playlist-item.jsonl"),
            SenderCommand::SetPlaylistItem(1),
        ),
    ] {
        let (session, _, commands) = replay(fixture);
        assert_preamble(&session);
        assert_eq!(commands.last(), Some(&expected));
    }
}

/// The `listen` transcript: the sender held the connection open for 9 s and sent no
/// `Ping` — the heartbeat is receiver-initiated in practice — and it accepted our
/// scripted `PlaybackUpdate`/`VolumeUpdate`/`PlaybackError` pushes, whose exact
/// bytes must match what the shipped session would send a v3 sender at the same
/// moments.
#[test]
fn listen_events_pushes_match_the_shipped_translation() {
    let fixture = include_str!("fixtures/sdk-0.3.0-listen-events.jsonl");
    let (session, _, commands) = replay(fixture);
    assert_preamble(&session);
    assert_eq!(commands, Vec::new(), "listen sends no transport commands");

    let rows = parse_fixture(fixture);
    assert!(
        rows.iter()
            .filter(|r| r.inbound)
            .all(|r| r.frame.opcode != Opcode::Ping),
        "the reference sender did not ping over a 9s idle window"
    );

    // Reconstruct the scripted pushes with the shipped translation layer.
    use proto_fcast::session::{PlaybackSnapshot, ReceiverUpdate};
    let receiver = identity();
    let (mut fresh, _) = Session::new();
    fresh
        .on_frame(
            Duration::ZERO,
            &SessionContext {
                wall_ms: 0,
                receiver: &receiver,
                play_data: None,
                volume: 1.0,
            },
            &Frame::with_body(Opcode::Version, br#"{"version":4}"#.to_vec()),
        )
        .unwrap();
    let pushed: Vec<&Row> = rows
        .iter()
        .filter(|r| !r.inbound && r.frame.opcode != Opcode::Version)
        .collect();
    let expected = [
        fresh.frame_update(
            1_754_700_000_000,
            &ReceiverUpdate::Playback(PlaybackSnapshot {
                state: PlayState::Playing,
                time: Some(12.5),
                duration: Some(596.5),
                speed: 1.0,
                item_index: None,
            }),
        ),
        fresh.frame_update(1_754_700_000_500, &ReceiverUpdate::Volume(0.75)),
        fresh.frame_update(
            0,
            &ReceiverUpdate::Error("synthetic error for capture".into()),
        ),
    ];
    for (row, expected) in pushed.iter().zip(expected) {
        let expected = expected.unwrap();
        assert_eq!(row.frame.opcode, expected.opcode);
        // Bodies must be JSON-equal (key order is serializer-defined).
        let sent: serde_json::Value = serde_json::from_slice(&row.frame.body).unwrap();
        let ours: serde_json::Value = serde_json::from_slice(&expected.body).unwrap();
        assert_eq!(sent, ours, "opcode {:?}", expected.opcode);
    }
}

// ---------------------------------------------------------------------------
// Protocol v4 transcripts (#248): the same sender through TLS, decrypted by the
// capture harness. Replayed at the message layer — the session/TLS plumbing has
// its own tests — so what is pinned here is that the real SDK's FlatBuffers
// parse into exactly the commands they should.
// ---------------------------------------------------------------------------

use proto_fcast::v4msg::{self, LoadSource, Parsed, V4Inbound};

/// The TLS-phase inbound frames of a v4 fixture, in order.
fn v4_frames(jsonl: &str) -> Vec<Row> {
    parse_fixture(jsonl)
        .into_iter()
        .filter(|r| r.inbound)
        .collect()
}

fn parse_v4(frame: &Frame) -> V4Inbound {
    match v4msg::parse_flatbuf(&frame.body).unwrap() {
        Parsed::Message(msg) => msg,
        Parsed::Reply(kind) => panic!("sender frame judged an error: {kind:?}"),
    }
}

/// Every v4 transcript opens with the SDK preamble the docs never mention:
/// `SenderIntroduction` naming the SDK, then an automatic `CompanionHelloRequest`.
#[test]
fn the_v4_preamble_parses() {
    let rows = v4_frames(include_str!("fixtures/sdk-0.3.0-v4-play-url.jsonl"));
    // rows[0] is the plaintext Version; TLS frames follow.
    let tls: Vec<&Row> = rows
        .iter()
        .filter(|r| r.frame.opcode == Opcode::Flatbuf)
        .collect();
    let V4Inbound::SenderIntroduction(info) = parse_v4(&tls[0].frame) else {
        panic!("expected SenderIntroduction first");
    };
    assert_eq!(info.app_name.as_deref(), Some("FCast Sender SDK v0.3.0"));
    assert!(matches!(
        parse_v4(&tls[1].frame),
        V4Inbound::CompanionHelloRequest
    ));
}

#[test]
fn v4_verbs_parse_to_their_commands() {
    let load = v4_frames(include_str!("fixtures/sdk-0.3.0-v4-play-url.jsonl"));
    let V4Inbound::Load { source, .. } = parse_v4(&load.last().unwrap().frame) else {
        panic!("expected Load last");
    };
    let LoadSource::Single(item) = source else {
        panic!("expected Single");
    };
    assert_eq!(item.source_url, "http://example.invalid/v4.mp4");
    assert_eq!(item.container, "video/mp4");
    assert_eq!(item.start_time, Some(Duration::from_secs(5)));
    assert!(
        item.headers.is_empty(),
        "the SDK drops headers on v4 Single"
    );

    for (fixture, want) in [
        (
            include_str!("fixtures/sdk-0.3.0-v4-pause.jsonl"),
            V4Inbound::PlaybackStateChanged(fcast_flatbuf::flat::PlaybackState::Paused),
        ),
        (
            include_str!("fixtures/sdk-0.3.0-v4-set-volume.jsonl"),
            V4Inbound::VolumeChanged(0.5),
        ),
        (
            include_str!("fixtures/sdk-0.3.0-v4-set-speed.jsonl"),
            V4Inbound::SpeedChanged(1.5),
        ),
        (
            include_str!("fixtures/sdk-0.3.0-v4-stop.jsonl"),
            V4Inbound::StopPlayback,
        ),
    ] {
        let rows = v4_frames(fixture);
        assert_eq!(parse_v4(&rows.last().unwrap().frame), want);
    }

    let seek = v4_frames(include_str!("fixtures/sdk-0.3.0-v4-seek.jsonl"));
    let V4Inbound::ProgressChanged { position } = parse_v4(&seek.last().unwrap().frame) else {
        panic!("expected ProgressChanged");
    };
    assert_eq!(position, Duration::from_secs_f64(42.5));
}

/// The SDK's `set-playlist-item` at v4 sends the raw **v3 JSON opcode** inside
/// the TLS session — captured, not hypothesized. A v4 session answers
/// `Error{InvalidOpcode}`; this pins that the frame really arrives as opcode 16.
#[test]
fn the_sdk_leaks_a_v3_opcode_into_v4() {
    let rows = v4_frames(include_str!(
        "fixtures/sdk-0.3.0-v4-set-playlist-item.jsonl"
    ));
    assert_eq!(rows.last().unwrap().frame.opcode, Opcode::SetPlaylistItem);
}
