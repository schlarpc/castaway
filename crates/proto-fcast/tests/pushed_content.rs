//! Media a sender *hands over* rather than points at (#249), end to end.
//!
//! Two of the three shapes FCast has that are not URLs live here — inline content, and a
//! playlist fetched from a URL — driven through the real adapter over real sockets, and
//! checked at both ends: the `Play` the pipeline is handed names a URL, and fetching that
//! URL off the receiver's own HTTP host returns what the sender pushed. The third,
//! `fcomp://`, needs a v4 session and lives with the rest of them in
//! `adapter_v4_loopback.rs`.
//!
//! The assertion that matters is the second one. A load that produces a plausible URL and
//! then 404s is the shape this whole path exists to avoid.

#![allow(clippy::unwrap_used)]
// Ephemeral loopback sockets for the test's own listener and its stand-in media server;
// the registry (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use castaway_core::{
    ProtocolKind, SessionEvent, SessionSink, SourceAdapter, SourceId, SourceMessage,
};
use castaway_test_support::eventually;
use proto_fcast::wire::{self, Frame, Opcode};
use proto_fcast::FCastReceiver;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const DEADLINE: Duration = Duration::from_secs(5);

/// An adapter with its HTTP surface actually served, since that is half of what is under
/// test: a URL nothing answers is not a resolution.
async fn started() -> (std::net::SocketAddr, mpsc::Receiver<SourceMessage>, String) {
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", http.local_addr().unwrap());

    let (tx, rx) = mpsc::channel(32);
    let sink = SessionSink::new(SourceId::new(ProtocolKind::FCast, "test"), tx);
    let receiver = Arc::new(
        FCastReceiver::new("Test Panel")
            .with_listen(([127, 0, 0, 1], 0).into())
            .with_local_host(&base),
    );
    let router = receiver.router();
    tokio::spawn(async move {
        let _ = axum::serve(http, router).await;
    });
    let adapter = Arc::clone(&receiver);
    tokio::spawn(async move {
        let _ = adapter.run(sink).await;
    });
    let addr = eventually("the listener to bind", || receiver.bound_addr()).await;
    (addr, rx, base)
}

/// A one-line HTTP/1.1 GET, so the test drives the route the way libavformat would rather
/// than by calling the handler directly.
async fn get(url: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let rest = url.strip_prefix("http://").expect("an http url");
    let (authority, path) = rest.split_once('/').expect("a path");
    let mut stream = TcpStream::connect(authority).await.unwrap();
    let request = format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(DEADLINE, stream.read_to_end(&mut raw))
        .await
        .expect("the receiver's HTTP host did not answer")
        .unwrap();

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete header block");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    (status, headers, raw[split + 4..].to_vec())
}

async fn next_event(rx: &mut mpsc::Receiver<SourceMessage>) -> SessionEvent {
    tokio::time::timeout(DEADLINE, rx.recv())
        .await
        .expect("timed out waiting for a session event")
        .expect("the event channel closed")
        .event
}

/// The URL the pipeline was told to play.
async fn played(rx: &mut mpsc::Receiver<SourceMessage>) -> String {
    loop {
        if let SessionEvent::Play { source, .. } = next_event(rx).await {
            return source.uri().to_string();
        }
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(held, _)| held == name)
        .map(|(_, value)| value.as_str())
}

/// The terminal sender's `cat dash.mpd | fcast play --mime-type application/dash+xml`.
///
/// Before #249 this was refused with an honest "unsupported"; now the manifest is served
/// back off our own host — which is the point, because that gives its relative segment
/// references a real base URL to resolve against, where a `data:` URI would give them none.
#[tokio::test]
async fn inline_content_is_served_back_as_a_url() {
    let (addr, mut rx, base) = started().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let manifest = r#"<MPD><Period><BaseURL>seg/</BaseURL></Period></MPD>"#;
    let play = serde_json::json!({
        "container": "application/dash+xml",
        "content": manifest,
    });
    stream
        .write_all(
            &wire::encode(&Frame::with_body(
                Opcode::Play,
                play.to_string().into_bytes(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();

    let url = played(&mut rx).await;
    assert!(
        url.starts_with(&format!("{base}/fcast/content/")),
        "the decoder should be pointed at our own host: {url}"
    );

    let (status, headers, body) = get(&url).await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/dash+xml"),
        "the declared container is what tells the demuxer probe what it is looking at"
    );
    assert_eq!(String::from_utf8(body).unwrap(), manifest);
}

/// A playlist the sender *pointed at*: the items are not known until it has been fetched,
/// and the player cannot do I/O, so the fetch happens at the actor boundary.
#[tokio::test]
async fn a_playlist_by_url_is_fetched_and_its_first_item_plays() {
    let (addr, mut rx, _base) = started().await;

    // A stand-in media server holding the playlist. One connection, one answer.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let playlist_url = format!("http://{}/list.json", listener.local_addr().unwrap());
    let playlist = serde_json::json!({
        "contentType": 0,
        "items": [
            {"container": "video/mp4", "url": "http://h/first.mp4"},
            {"container": "video/mp4", "url": "http://h/second.mp4"},
        ],
    })
    .to_string();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
        let head = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            playlist.len()
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(playlist.as_bytes());
        let _ = sock.flush();
    });

    let mut stream = TcpStream::connect(addr).await.unwrap();
    let play = serde_json::json!({
        "container": "application/json",
        "url": playlist_url,
    });
    stream
        .write_all(
            &wire::encode(&Frame::with_body(
                Opcode::Play,
                play.to_string().into_bytes(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(played(&mut rx).await, "http://h/first.mp4");
}

/// A playlist whose *items* were pushed inline rather than named: each becomes its own
/// URL, and the document round-trips otherwise untouched.
#[tokio::test]
async fn a_playlists_inline_items_each_get_their_own_url() {
    let (addr, mut rx, base) = started().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();

    let play = serde_json::json!({
        "container": "application/json",
        "content": serde_json::json!({
            "contentType": 0,
            "items": [
                {"container": "application/dash+xml", "content": "<MPD id='one'/>"},
                {"container": "video/mp4", "url": "http://h/plain.mp4"},
            ],
        }).to_string(),
    });
    stream
        .write_all(
            &wire::encode(&Frame::with_body(
                Opcode::Play,
                play.to_string().into_bytes(),
            ))
            .unwrap(),
        )
        .await
        .unwrap();

    let url = played(&mut rx).await;
    assert!(url.starts_with(&format!("{base}/fcast/content/")), "{url}");
    let (status, headers, body) = get(&url).await;
    assert_eq!(status, 200);
    assert_eq!(
        header(&headers, "content-type"),
        Some("application/dash+xml")
    );
    assert_eq!(String::from_utf8(body).unwrap(), "<MPD id='one'/>");
}

/// Content that was never published — or has been evicted — is a 404 rather than an
/// empty body, so the decoder reports a failed fetch instead of "no video stream".
#[tokio::test]
async fn an_unknown_content_id_is_a_404() {
    let (_addr, _rx, base) = started().await;
    let (status, _, _) = get(&format!("{base}/fcast/content/999")).await;
    assert_eq!(status, 404);
}
