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
use proto_airplay::{AirPlayIdentity, AirPlayReceiver, NtpTime, SessionDiagnostics};
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
    spawn_configured_receiver(media_ports, sink, None).await
}

/// [`spawn_receiver`], optionally sharing the session counters with the test.
///
/// The timing exchange and the resend path are invisible from outside the process
/// except through those counters — `clock_samples >= 1` is what proves a type-83 reply
/// was *folded in* rather than merely delivered (#176).
pub async fn spawn_configured_receiver(
    media_ports: MediaPorts,
    sink: SessionSink,
    diagnostics: Option<Arc<SessionDiagnostics>>,
) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut receiver = AirPlayReceiver::new(identity(), media_ports).with_addr(addr);
    if let Some(diagnostics) = diagnostics {
        receiver = receiver.with_diagnostics(diagnostics);
    }
    let receiver = Arc::new(receiver);
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

/// [`start`], with the session counters shared so a test can assert on them.
pub async fn start_watched(
    media_ports: MediaPorts,
) -> (
    TcpStream,
    mpsc::Receiver<castaway_core::SourceMessage>,
    Arc<SessionDiagnostics>,
) {
    let (tx, events) = mpsc::channel(64);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::AirPlay, "test"), tx);
    let diagnostics = Arc::new(SessionDiagnostics::new());
    let addr = spawn_configured_receiver(media_ports, sink, Some(Arc::clone(&diagnostics))).await;
    (connect(addr).await, events, diagnostics)
}

/// Send one RTSP request and read the raw response — head and body, as bytes.
///
/// For the replies that carry a binary plist body, which `request`'s lossy string
/// conversion would mangle.
pub async fn request_bytes(
    stream: &mut TcpStream,
    line: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    cseq: u32,
) -> Vec<u8> {
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
    buf.truncate(n);
    buf
}

/// Send one RTSP request and read the response head.
pub async fn request(
    stream: &mut TcpStream,
    line: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    cseq: u32,
) -> String {
    String::from_utf8_lossy(&request_bytes(stream, line, headers, body, cseq).await).to_string()
}

/// Pull `server_port=NNNN` out of a Transport header.
#[must_use]
pub fn server_port(response: &str) -> u16 {
    transport_port(response, "server_port=")
}

/// Pull any `key=NNNN` port out of a Transport header.
#[must_use]
pub fn transport_port(response: &str, key: &str) -> u16 {
    response
        .split(key)
        .nth(1)
        .and_then(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse().ok())
        })
        .unwrap_or_else(|| panic!("no {key} in:\n{response}"))
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
    negotiate_with_sdp(stream, &announce_sdp(fmtp)).await
}

/// The ports one negotiated session runs on: the receiver's three, as its `SETUP`
/// reply named them.
pub struct Negotiated {
    /// Where the sender streams audio.
    pub audio: u16,
    /// Where the sender's sync packets and our resend replies go.
    pub control: u16,
    /// Where the sender's timing replies go.
    pub timing: u16,
}

/// [`negotiate`], with the *sender's* control and timing ports declared for real —
/// the ports a scripted sender has actually bound, so the receiver's timing probes
/// and resend requests land on sockets the test is watching (#176).
pub async fn negotiate_declaring_ports(
    stream: &mut TcpStream,
    fmtp: &str,
    sender_control: u16,
    sender_timing: u16,
) -> Negotiated {
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

    let transport = format!(
        "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;\
         control_port={sender_control};timing_port={sender_timing}"
    );
    let setup = request(
        stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Transport", &transport)],
        &[],
        3,
    )
    .await;
    assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");
    let negotiated = Negotiated {
        audio: transport_port(&setup, "server_port="),
        control: transport_port(&setup, "control_port="),
        timing: transport_port(&setup, "timing_port="),
    };

    let record = request(stream, "RECORD rtsp://127.0.0.1/1", &[], &[], 4).await;
    assert!(record.starts_with("RTSP/1.0 200"), "{record}");
    negotiated
}

/// A well-formed type-83 timing reply to `request`, as a sender's NTP service answers:
/// our transmit stamp echoed at 8..16, the sender's receive and transmit times filling
/// the rest.
#[must_use]
pub fn timing_reply(request: &[u8]) -> [u8; 32] {
    assert_eq!(request.len(), 32, "a timing request is exactly 32 bytes");
    let now = NtpTime::from_unix_nanos(
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap(),
    );
    let mut r = [0u8; 32];
    r[0] = 0x80;
    r[1] = 0x80 | 83;
    r[2..4].copy_from_slice(&request[2..4]);
    r[8..16].copy_from_slice(&request[24..32]); // echo the requester's transmit time
    r[16..24].copy_from_slice(&now.raw().to_be_bytes());
    r[24..32].copy_from_slice(&now.raw().to_be_bytes());
    r
}

/// The 20-byte sync packet a sender emits on its control link: "RTP `anchor` is *now*,
/// and I intend to run `anchor - rtp_now_less_latency` frames ahead of you".
#[must_use]
pub fn sync_packet(rtp_now_less_latency: u32, sender_ntp_raw: u64, rtp_anchor: u32) -> Vec<u8> {
    let mut p = vec![0x90u8, 0x80 | 84, 0, 7];
    p.extend_from_slice(&rtp_now_less_latency.to_be_bytes());
    p.extend_from_slice(&sender_ntp_raw.to_be_bytes());
    p.extend_from_slice(&rtp_anchor.to_be_bytes());
    p
}

// --- The mirroring half of the harness ---

/// A real FairPlay v3 vector: the 164-byte `/fp-setup` SETUP2 body, the 72-byte `ekey`
/// a `SETUP` would carry, and the AES key they derive to.
///
/// Using a genuine one rather than a synthetic pair is what makes tests built on this a
/// test of the whole chain: the session runs the real derivation, and the test can
/// encrypt frames with the key it *knows* must come out. A wrong derivation fails as
/// garbage rather than as a mismatch nobody notices.
pub const FP_KEY_MESSAGE: &str = "46504c590301030000000098008f1a9ca548fdd57560a52926ff399f2eb154d0a7a0fffc997f58e27e00499eb9f310110d019e550e328047aea54308ab71b647041406878af96e06cf74127ae35941dceb58931b5543b39903f9f76a376248ee52e3656b561e1c1a0106ec6608df0ab4f2df528e65db6d622d3892d5b49c6c025606a574f19ebea7d93500bdd69db23333f22edcb3ccf7a6acde7389f2facabfa61b0b50";

/// The `ekey` matching [`FP_KEY_MESSAGE`].
pub const FP_EKEY: &str = "46504c59010201000000003c000000006d44ba12b91f48e061eb230fc53abfa2000000108a1060465d51b808df112d08b604501f9e3ea29ce0902f3c43b81d5319d0575f78517e01";

/// The AES key the two must derive to.
pub const FP_EXPECTED_AES_KEY: &str = "0496a612172f41e0fd71912acc33fc54";

/// The `streamConnectionID` [`negotiate_mirror`] names, as a plist's signed integer.
pub const MIRROR_STREAM_ID: i64 = 4_964_383_553_955_644_435;

/// Decode a hex string.
#[must_use]
pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A binary plist body from a dictionary.
#[must_use]
pub fn plist_body(dict: plist::Dictionary) -> Vec<u8> {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &plist::Value::Dictionary(dict)).unwrap();
    buf
}

/// One framed mirroring message, with its encoded-geometry fields naming `encoded`.
#[must_use]
pub fn mirror_message(kind: u8, timestamp: u64, payload: &[u8], encoded: (f32, f32)) -> Vec<u8> {
    let mut m = vec![0u8; 128];
    m[0..4].copy_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    m[4] = kind;
    m[8..16].copy_from_slice(&timestamp.to_le_bytes());
    m[56..60].copy_from_slice(&encoded.0.to_le_bytes());
    m[60..64].copy_from_slice(&encoded.1.to_le_bytes());
    m.extend_from_slice(payload);
    m
}

/// Pull a named port out of the first stream entry of a plist `SETUP` reply.
#[must_use]
pub fn stream_port(reply: &plist::Value, key: &str) -> u16 {
    let port = reply
        .as_dictionary()
        .and_then(|d| d.get("streams"))
        .and_then(plist::Value::as_array)
        .and_then(|a| a.first())
        .and_then(plist::Value::as_dictionary)
        .and_then(|d| d.get(key))
        .and_then(plist::Value::as_unsigned_integer)
        .unwrap_or_else(|| panic!("no {key} in the stream reply"));
    assert_ne!(port, 0, "a zero {key} means nothing is listening");
    u16::try_from(port).unwrap()
}

/// The plist body of a response, parsed.
#[must_use]
pub fn response_plist(raw: &[u8]) -> plist::Value {
    let head = String::from_utf8_lossy(raw);
    assert!(head.starts_with("RTSP/1.0 200"), "{head}");
    let body_at = head.find("\r\n\r\n").expect("a header/body split") + 4;
    plist::from_bytes(&raw[body_at..]).expect("a plist reply")
}

/// Drive the whole mirroring negotiation — `/fp-setup` with the captured key message,
/// the key-material `SETUP`, and the type-110 stream `SETUP` naming
/// [`MIRROR_STREAM_ID`] — and return the advertised data port. Uses CSeq 1–3.
///
/// The stream keys are `MirrorKeys::derive` over [`FP_EXPECTED_AES_KEY`] and
/// [`MIRROR_STREAM_ID`], which is what lets a test encrypt like the sender.
pub async fn negotiate_mirror(stream: &mut TcpStream) -> u16 {
    let fp = request(
        stream,
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
        stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        2,
    )
    .await;
    assert!(setup1.starts_with("RTSP/1.0 200"), "{setup1}");

    // Second: name the stream. The reply has to carry a data port that is listening.
    let mut s0 = plist::Dictionary::new();
    s0.insert("type".into(), plist::Value::Integer(110i64.into()));
    s0.insert(
        "streamConnectionID".into(),
        plist::Value::Integer(MIRROR_STREAM_ID.into()),
    );
    let mut d = plist::Dictionary::new();
    d.insert(
        "streams".into(),
        plist::Value::Array(vec![plist::Value::Dictionary(s0)]),
    );
    let reply = request_bytes(
        stream,
        "SETUP rtsp://127.0.0.1/1",
        &[("Content-Type", "application/x-apple-binary-plist")],
        &plist_body(d),
        3,
    )
    .await;
    stream_port(&response_plist(&reply), "dataPort")
}

/// [`negotiate`], with the whole SDP supplied — for a sender that announces more than
/// the codec line, like the `a=rsaaeskey:`/`a=aesiv:` of an encrypting one.
pub async fn negotiate_with_sdp(stream: &mut TcpStream, sdp: &str) -> (u16, String) {
    let options = request(stream, "OPTIONS *", &[], &[], 1).await;
    assert!(options.starts_with("RTSP/1.0 200"), "{options}");

    let announce = request(
        stream,
        "ANNOUNCE rtsp://127.0.0.1/1",
        &[("Content-Type", "application/sdp")],
        sdp.as_bytes(),
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
