//! A DLNA cast, end to end, with nothing faked between the SOAP envelope and the frames.
//!
//! This is the test G67 was about. The media plane had never been exercised through DLNA
//! by anything: the tier-2 VM test casts `http://example.invalid/clip.mp4` at a build with
//! no decoder in it, so nothing had ever been fetched or decoded through this path in CI —
//! and G61, G62 and G63 all lived in exactly that blind spot, which is three separate ways
//! for a cast to produce silence, a slideshow, or a pause that did not pause.
//!
//! So everything here is the real thing: `proto-dlna`'s router parsing a real envelope,
//! `castaway_core`'s session manager arbitrating, `pipeline`'s `RenderPipeline` opening a
//! real HTTP URL with a real demuxer, and the completion report coming back up to move the
//! transport state. The one part that is not real is the compositor — the frames are taken
//! off the render channel rather than presented, because a GPU is the one thing a CI
//! sandbox does not have, and it is the half GAPS records as still needing a nix lift.
//!
//! Needs the `render` feature (for `RenderPipeline`) and the `ffmpeg` CLI on `PATH` (to
//! make the clip). Without either it skips rather than fails, so a build that cannot run
//! it says so instead of going red for the wrong reason.
#![cfg(feature = "render")]
#![allow(clippy::unwrap_used)]
// Tests bind ephemeral loopback sockets that never face the LAN; the registry
// (crates/app/src/surface.rs) governs production binds.
#![allow(clippy::disallowed_methods)]

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use castaway_core::{
    ProtocolKind, SessionConfig, SessionManager, SessionSink, SourceId, SourceMessage,
};
use pipeline::{RenderCommand, RenderPipeline};
use proto_dlna::DlnaService;
use tokio::sync::mpsc;
use tower::ServiceExt as _;

/// Build a short A/V clip.
///
/// Panics rather than skipping when ffmpeg is missing, and that is the point (#98). This
/// used to `eprintln!` and return, which meant a CI job that turned the `render` feature on
/// without putting the ffmpeg *binary* on `PATH` would report success having proved
/// nothing — the same shape of failure as the missing gate itself, one level up. A test
/// whose prerequisite is absent has not passed.
///
/// The dev shell and the `media-plane` check both supply it, so there is no honest
/// environment where this fires.
fn make_clip() -> std::path::PathBuf {
    let path = std::env::temp_dir().join("castaway-dlna-plane.mp4");
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=160x120:rate=10",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
        .args(["-c:v", "libx264", "-c:a", "aac"])
        .arg(&path)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "this test needs the ffmpeg CLI on PATH to build its clip, and running it \
                 failed: {e}. Use `nix develop`, or run the `media-plane` flake check."
            )
        });
    assert!(
        status.success() && path.exists(),
        "ffmpeg ran but produced no clip (exit {status}); this build of ffmpeg may lack \
         libx264 or the aac encoder"
    );
    path
}

/// Serve `body` over HTTP on an ephemeral port, and return the URL.
///
/// A real socket rather than a `file://` path, because that is the only thing a control
/// point ever hands us — and because libavformat's protocol set is a build-time choice, so
/// an ffmpeg without `http` decodes every local file perfectly and fails every real cast.
///
/// Never joined. The decoder decides how many times it opens the URL, so a server that the
/// test waits for is a test that hangs whenever the decoder needed one connection fewer
/// than the server was told to expect; it is left to die with the process instead.
fn serve(body: Vec<u8>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // libavformat may probe and then fetch, and is free to reopen; each connection
        // gets the whole file.
        while let Ok((mut sock, _)) = listener.accept() {
            let body = body.clone();
            std::thread::spawn(move || {
                let mut scratch = [0u8; 2048];
                let _ = sock.read(&mut scratch);
                let header = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nContent-Type: video/mp4\r\n\
                     Accept-Ranges: none\r\n\r\n",
                    body.len()
                );
                if sock.write_all(header.as_bytes()).is_ok() && sock.write_all(&body).is_ok() {
                    let _ = sock.flush();
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}/clip.mp4")
}

fn envelope(action: &str, args: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0"?><s:Envelope "#,
            r#"xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>"#,
            r#"<u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#,
            "<InstanceID>0</InstanceID>{args}",
            "</u:{action}></s:Body></s:Envelope>",
        ),
        action = action,
        args = args,
    )
}

/// The whole path: a control point sets a URI and presses play, pixels and sound come out
/// of a real decoder, the position it polls is a real position, and when the item ends the
/// transport says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_cast_from_a_control_point_decodes_and_then_reports_that_it_finished() {
    let clip = make_clip();
    let url = serve(std::fs::read(&clip).unwrap());

    // The pipeline, wired exactly as `main` wires it: a handle for the position, a channel
    // for the end report, and the frame receiver the kiosk would otherwise own.
    let (pipe, frames) = RenderPipeline::new(8);
    let (ends_tx, ends_rx) = castaway_core::playback::end_channel();
    pipe.set_playback_ends(ends_tx);
    let playback: Arc<dyn castaway_core::PlaybackReport> = Arc::new(pipe.playback_handle());

    let (event_tx, event_rx) = mpsc::channel::<SourceMessage>(32);
    let manager =
        SessionManager::new(pipe, None, SessionConfig::default()).with_playback_ends(ends_rx);
    tokio::spawn(manager.run(event_rx));

    let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "test"), event_tx);
    let dlna = DlnaService::new("Test TV", "abcd-1234", sink).with_playback(playback);
    let app = dlna.router();

    // Count what the compositor would have been given, on its own thread: the channel is a
    // `std` one because the render loop is not async, and draining it is also what stops a
    // full queue from silently dropping the frames this test is about.
    //
    // Counted into atomics rather than returned from a join. The sender lives inside the
    // session manager, which runs for as long as the process does, so the channel never
    // closes and a join here would wait for something that is not going to happen.
    let video = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cleared = Arc::new(std::sync::atomic::AtomicBool::new(false));
    std::thread::spawn({
        let (video, cleared) = (Arc::clone(&video), Arc::clone(&cleared));
        move || {
            while let Some(cmd) = frames.recv_timeout(Duration::from_secs(60)) {
                match cmd {
                    RenderCommand::Video(_) => {
                        video.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    RenderCommand::ClearVideo => {
                        cleared.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
    });

    let post = |body: String| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dlna/control/AVTransport")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    let set = post(envelope(
        "SetAVTransportURI",
        &format!("<CurrentURI>{url}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"),
    ))
    .await;
    assert_eq!(set.status(), StatusCode::OK);

    let play = post(envelope("Play", "<Speed>1</Speed>")).await;
    assert_eq!(play.status(), StatusCode::OK);

    // While it plays, the position a control point polls is a real one. This is the whole
    // of G69: it used to be the `NOT_IMPLEMENTED` sentinel for the life of every item, so
    // every control point drew no progress bar at all.
    let mut moved = None;
    for _ in 0..300 {
        let info = body_of(post(envelope("GetPositionInfo", "")).await).await;
        if let Some(rel) = arg_of(&info, "RelTime") {
            if rel != "NOT_IMPLEMENTED" {
                moved = Some(rel);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rel = moved.expect("the control point was never told a position");
    assert!(
        rel.starts_with("0:00:"),
        "a two-second clip reported {rel}, which is not a position in it"
    );

    // The item is two seconds long and paced to real time, so it ends on its own — and
    // when it does, the transport has to stop saying PLAYING. Before the completion
    // channel existed it never did, so a queued playlist waited for the life of the
    // process.
    let mut stopped = false;
    for _ in 0..400 {
        let info = body_of(post(envelope("GetTransportInfo", "")).await).await;
        if arg_of(&info, "CurrentTransportState").as_deref() == Some("STOPPED") {
            // …and a clip that played through is not an error.
            assert_eq!(
                arg_of(&info, "CurrentTransportStatus").as_deref(),
                Some("OK"),
                "an item that finished normally must not read as a failure"
            );
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        stopped,
        "the item ended and the control point was never told"
    );

    let frames_seen = video.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        frames_seen >= 10,
        "only {frames_seen} frames reached the compositor from a two-second 10fps clip",
    );
    assert!(
        cleared.load(std::sync::atomic::Ordering::Relaxed),
        "the screen was never handed back at the end",
    );
}

/// A URL the box cannot fetch, which is indistinguishable from a healthy cast at the phone
/// unless the receiver says otherwise.
#[tokio::test(flavor = "multi_thread")]
async fn a_cast_at_a_url_that_is_not_there_comes_back_as_an_error() {
    let (pipe, frames) = RenderPipeline::new(8);
    let (ends_tx, ends_rx) = castaway_core::playback::end_channel();
    pipe.set_playback_ends(ends_tx);
    // Drained so a full render channel cannot be what ends the decode.
    std::thread::spawn(move || while frames.recv_timeout(Duration::from_secs(60)).is_some() {});

    let (event_tx, event_rx) = mpsc::channel::<SourceMessage>(32);
    let manager =
        SessionManager::new(pipe, None, SessionConfig::default()).with_playback_ends(ends_rx);
    tokio::spawn(manager.run(event_rx));

    let sink = SessionSink::new(SourceId::new(ProtocolKind::Dlna, "test"), event_tx);
    let app = DlnaService::new("Test TV", "abcd-1234", sink).router();

    // A port nothing is listening on: refused at once, so this does not depend on a DNS
    // lookup or on the network being absent in a particular way.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let url = format!("http://127.0.0.1:{port}/gone.mp4");

    for body in [
        envelope(
            "SetAVTransportURI",
            &format!("<CurrentURI>{url}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"),
        ),
        envelope("Play", "<Speed>1</Speed>"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dlna/control/AVTransport")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let mut said = None;
    for _ in 0..400 {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/dlna/control/AVTransport")
                    .body(Body::from(envelope("GetTransportInfo", "")))
                    .unwrap(),
            )
            .await
            .unwrap();
        let info = body_of(resp).await;
        if arg_of(&info, "CurrentTransportStatus").as_deref() == Some("ERROR_OCCURRED") {
            assert_eq!(
                arg_of(&info, "CurrentTransportState").as_deref(),
                Some("STOPPED")
            );
            said = Some(());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        said.is_some(),
        "a URL the box could not reach read as PLAYING / OK, which is what a healthy \
         session reads as",
    );
}

async fn body_of(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The text of one output argument in a SOAP response.
///
/// A substring scan rather than a parser: the responses are ours, the shape is fixed, and
/// a second XML reader here would be testing `quick-xml` rather than the receiver.
fn arg_of(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].to_string())
}
