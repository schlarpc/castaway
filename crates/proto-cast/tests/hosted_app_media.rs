//! A hosted Cast application playing **real media**, end to end.
//!
//! `receiver_sdk.rs` proves the platform protocol against Google's own SDK. This proves
//! the thing the protocol is *for*: a sender launches an application it did not write,
//! the receiver resolves the id to a page, the page comes up in the real browser runtime,
//! a `LOAD` crosses the platform channel, and a video with real H.264 and AAC in it
//! actually plays. Every layer is the real one except the sender, which is this test.
//!
//! What is deliberately local rather than real: the registry endpoint and the receiver
//! page. Pointing them at Google and at YouTube would make the test a measurement of
//! somebody else's uptime — and the *shape* is identical, because the registry response
//! is the captured one and the page loads the pinned SDK exactly as youtube.com/tv does.
//!
//! Needs `CASTAWAY_ELECTRON`, `CASTAWAY_CAST_RECEIVER_SDK` and `ffmpeg`, which the dev
//! shell provides and the `cast-app-hosting` flake check supplies. With no browser at all
//! it skips — audibly, on stderr, for the reason in `receiver_sdk.rs`.

#![allow(clippy::unwrap_used)]
// A test's own fixture server is not part of the receiver's network surface.
#![allow(clippy::disallowed_methods)]

use std::io::{BufRead as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cast_registry::Registry;
use proto_cast::messages::{App, AppCatalogue, LaunchRefusal};
use proto_cast::platform::{AppIdentity, DeviceCapabilities};
use proto_cast::platform_actor::{HostEvent, PlatformServer};
use proto_cast::session::CastSession;
use proto_cast::CastMessage;
use tokio::sync::mpsc;

const MEDIA_NS: &str = "urn:x-cast:com.google.cast.media";
/// The app id this test's registry serves. Not a real one — a real id would resolve to a
/// real page, which is the thing being kept out.
const APP_ID: &str = "ABCD1234";

/// Synthesise two seconds of H.264 + AAC, once per test run.
///
/// The pair matters: those are the codecs every commercial Cast receiver streams, and
/// the pair D36 moved the browser runtime to get (`browser-host/codec-probe.js`). A test
/// that played VP9 would pass on a build that cannot serve a single real receiver.
fn media_file() -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("castaway-hosted-media-{}.mp4", std::process::id()));
    if path.exists() {
        return path;
    }
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=320x240:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-movflags",
            "+faststart",
            "-shortest",
        ])
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("ffmpeg is needed for the real-media test; the dev shell provides it");
    assert!(status.success(), "ffmpeg would not produce the fixture");
    path
}

/// Everything the receiver and the page fetch: the registry entry, the receiver page,
/// the pinned SDK, and the media itself.
struct Origin {
    port: u16,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    fetched: Arc<Mutex<Vec<String>>>,
}

impl Origin {
    fn start(sdk_dir: PathBuf, media: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fetched = Arc::new(Mutex::new(Vec::new()));

        let stop = Arc::clone(&shutdown);
        let log = Arc::clone(&fetched);
        let handle = std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let sdk = sdk_dir.clone();
                        let media = media.clone();
                        let log = Arc::clone(&log);
                        // One thread per connection: a page fetches its script and its
                        // media at once, and a serial server would deadlock on that.
                        std::thread::spawn(move || serve_one(stream, &sdk, &media, port, &log));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            shutdown,
            handle: Some(handle),
            fetched,
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn fetched(&self) -> Vec<String> {
        self.fetched.lock().map(|f| f.clone()).unwrap_or_default()
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_one(
    mut stream: std::net::TcpStream,
    sdk: &std::path::Path,
    media: &std::path::Path,
    port: u16,
    log: &Arc<Mutex<Vec<String>>>,
) {
    stream.set_nonblocking(false).ok();
    let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
    let mut request = String::new();
    if reader.read_line(&mut request).is_err() {
        return;
    }
    // Drain the headers; a Range header is answered with the whole file, which is legal
    // and which Chromium accepts (it asks `bytes=0-` for a first play anyway).
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line.trim().is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }
    let target = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
    if let Ok(mut seen) = log.lock() {
        seen.push(target.clone());
    }

    let (kind, body): (&str, Vec<u8>) = if target.starts_with("/app?") {
        // The captured registry shape, with the url pointed at this server. Same fields,
        // same anti-hijacking prefix; only the host differs from Google's.
        (
            "application/json",
            format!(
                "{}{{\"resolution_height\":0,\"uses_ipc\":true,\"display_name\":\"Local Receiver\",\
                 \"app_id\":\"{APP_ID}\",\"url\":\"http://127.0.0.1:{port}/receiver.html\"}}",
                ")]}'"
            )
            .into_bytes(),
        )
    } else if target.starts_with("/receiver.html") {
        ("text/html", receiver_page(port).into_bytes())
    } else if target.starts_with("/cast_receiver.js") {
        (
            "application/javascript",
            std::fs::read(sdk.join("cast_receiver.js")).expect("the pinned SDK"),
        )
    } else if target.starts_with("/media.mp4") {
        (
            "video/mp4",
            std::fs::read(media).expect("the media fixture"),
        )
    } else {
        ("text/plain", b"not found".to_vec())
    };

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nAccept-Ranges: none\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// A receiver page, written the way a real one is: load the SDK, open the media bus,
/// and play what a `LOAD` names.
fn receiver_page(port: u16) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><title>local receiver</title><body>
<script src="http://127.0.0.1:{port}/cast_receiver.js"></script>
<script>
  window.probe = {{ events: [], appMessages: [], error: null, media: null }};
  window.onerror = (m, s, l) => {{ window.probe.error = m + ' @' + s + ':' + l; }};
  const manager = cast.receiver.CastReceiverManager.getInstance();
  manager.onReady = () => {{
    const d = manager.getApplicationData() || {{}};
    probe.events.push({{ type: 'ready', applicationId: d.id, sessionId: d.sessionId }});
  }};
  const bus = manager.getCastMessageBus(
    'urn:x-cast:com.google.cast.media',
    cast.receiver.CastMessageBus.MessageType.JSON
  );
  bus.onMessage = (event) => {{
    probe.appMessages.push({{ senderId: event.senderId, data: JSON.stringify(event.data) }});
    if (!event.data || event.data.type !== 'LOAD') return;
    const url = event.data.media && event.data.media.contentId;
    probe.media = {{ url, state: 'loading', currentTime: 0, error: null }};
    const video = document.createElement('video');
    video.src = url;
    video.muted = true;   // there is no audio device in an offscreen renderer
    video.addEventListener('playing', () => {{ probe.media.state = 'playing'; }});
    video.addEventListener('timeupdate', () => {{ probe.media.currentTime = video.currentTime; }});
    video.addEventListener('error', () => {{
      probe.media.state = 'error';
      probe.media.error = video.error ? video.error.code : 'unknown';
    }});
    document.body.appendChild(video);
    video.play().catch((e) => {{ probe.media.state = 'error'; probe.media.error = String(e); }});
    // What a real receiver answers, and what a sender's UI reads.
    bus.send(event.senderId, {{
      type: 'MEDIA_STATUS',
      requestId: event.data.requestId || 0,
      status: [{{ playerState: 'PLAYING', currentTime: 0 }}],
    }});
  }};
  manager.start({{ statusText: 'local receiver' }});
</script>"#
    )
}

#[derive(Debug, serde::Deserialize)]
struct Report {
    ok: bool,
    #[serde(default)]
    media: Option<serde_json::Value>,
}

/// The browser environment, or a reason there is none.
fn browser_env() -> Option<(String, PathBuf)> {
    let electron = std::env::var("CASTAWAY_ELECTRON").ok()?;
    let sdk = std::env::var("CASTAWAY_CAST_RECEIVER_SDK").ok()?;
    Some((electron, PathBuf::from(sdk)))
}

/// The whole path: a sender's `LAUNCH` resolves through the registry, the page comes up
/// in the real browser against our platform, a `LOAD` crosses to it, and real H.264+AAC
/// plays — with the application's own `MEDIA_STATUS` coming back out to the sender.
#[tokio::test(flavor = "multi_thread")]
async fn a_hosted_application_plays_real_media_and_answers_the_sender() {
    let Some((electron, sdk)) = browser_env() else {
        // Loud, and on stderr: a green run that never opened a browser must not read
        // like a green run that did.
        eprintln!(
            "\n*** NOT RUN: the hosted-application media test needs a browser \
             (CASTAWAY_ELECTRON / CASTAWAY_CAST_RECEIVER_SDK).\n\
             *** Whether a hosted Cast application actually plays has not been measured. \
             Run it with\n\
             ***   nix develop -c cargo nextest run -p proto-cast -E 'binary(hosted_app_media)'\n"
        );
        return;
    };
    let origin = Origin::start(sdk, media_file());

    // The receiver's own registry, pointed at this test's endpoint. Everything else
    // about it — the parse, the cache, the shape of the response — is the real thing.
    let registry =
        Arc::new(Registry::with_cache_path(None).with_endpoint(format!("{}/app", origin.base())));

    let (host, task) = PlatformServer::new(DeviceCapabilities::default())
        .with_port(0)
        .bind()
        .await
        .unwrap();
    tokio::spawn(task);

    // The sender's half, folded through the real session state machine.
    let mut catalogue = AppCatalogue::new(true);
    catalogue.record(APP_ID, true);
    let mut session = CastSession::new(None);
    session.observe_catalogue(catalogue.clone());

    assert_eq!(
        App::classify(APP_ID, &catalogue),
        App::Page,
        "a resolved web receiver must classify as a page"
    );

    let launch = CastMessage::json(
        "sender-0",
        "receiver-0",
        proto_cast::ns::RECEIVER,
        format!(r#"{{"requestId":3,"type":"LAUNCH","appId":"{APP_ID}"}}"#),
    );
    let reaction = session.handle(&launch).unwrap();
    let pending = reaction
        .launch_page
        .expect("a LAUNCH for a page must defer rather than answer");
    assert!(
        reaction.outgoing.is_empty(),
        "nothing may be claimed before the page exists"
    );

    // What the actor does with it: resolve, then host.
    let surface = registry.resolve(&pending.app_id).await.expect("resolve");
    let url = surface
        .page_url()
        .expect("the registry entry names a page")
        .to_owned();
    assert_eq!(surface.display_name(), Some("Local Receiver"));

    let (events_tx, mut events) = mpsc::channel(64);
    host.start(
        AppIdentity {
            application_id: pending.app_id.clone(),
            application_name: "Local Receiver".into(),
            session_id: pending.session_id.clone(),
            launching_sender_id: pending.sender.clone(),
            icon_url: None,
        },
        session.output_volume(),
        events_tx,
    )
    .await
    .unwrap();
    host.sender_connected(&pending.sender, "test-sender/1.0")
        .await;

    // The browser, pointed at the page the registry named and at the platform port.
    let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../browser-host/cast-page-probe.js");
    // A panic rather than a skip, for the reason in `receiver_sdk.rs`: a configured
    // browser with no probe is a broken environment, and skipping it is how a green run
    // comes to mean nothing.
    let probe = probe.canonicalize().unwrap_or_else(|e| {
        panic!(
            "CASTAWAY_ELECTRON is set but there is no probe at {}: {e}",
            probe.display()
        )
    });
    let child = tokio::process::Command::new(&electron)
        // Chromium's setuid/namespace sandbox cannot start inside the Nix build sandbox
        // (`zygote_host_impl_linux.cc: Check failed: Invalid argument`). Dropping it is
        // safe here and only here: the renderer's whole world is a local fixture server
        // and a pinned script, and the alternative is a test that cannot run in CI.
        .arg("--no-sandbox")
        // No GPU and no compositor in the build sandbox; the page is rasterised on the
        // CPU, which is all an offscreen probe needs.
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        // No display server either. Ozone's headless platform is what lets Chromium
        // initialise without one; without it Electron exits at `aura/env.cc: The
        // platform failed to initialize`.
        .arg("--ozone-platform=headless")
        .arg(&probe)
        .arg("--port")
        .arg(host.port().to_string())
        .arg("--url")
        .arg(&url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawning electron");

    // Drive the session from the platform's events, exactly as the actor does.
    let media_url = format!("{}/media.mp4", origin.base());
    let driver = tokio::spawn({
        let host = host.clone();
        let media_url = media_url.clone();
        async move {
            let mut status_to_sender = None;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
            loop {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                match tokio::time::timeout(left, events.recv()).await {
                    Ok(Some(HostEvent::Ready(ready))) => {
                        let outgoing = session.page_hosted(
                            &pending,
                            "Local Receiver",
                            ready.active_namespaces.clone(),
                        );
                        // The sender's answer, at last, and only now.
                        assert!(!outgoing.is_empty(), "the sender was never told");
                        // The application owns the media namespace, so a LOAD from the
                        // sender is folded into a relay rather than into our own player.
                        let load = CastMessage::json(
                            "sender-0",
                            &pending.transport_id,
                            MEDIA_NS,
                            format!(
                                r#"{{"requestId":8,"type":"LOAD","media":{{"contentId":"{media_url}","contentType":"video/mp4","streamType":"BUFFERED"}}}}"#
                            ),
                        );
                        let r = session.handle(&load).unwrap();
                        assert!(
                            r.events.is_empty() && r.outgoing.is_empty(),
                            "we must not answer a LOAD the application owns: {r:?}"
                        );
                        let relay = r.to_page.first().expect("the LOAD must be relayed");
                        host.to_page(&relay.namespace, &relay.sender, &relay.data)
                            .await;
                    }
                    Ok(Some(HostEvent::ToSender {
                        namespace,
                        sender_id,
                        data,
                    })) => {
                        if data.contains("MEDIA_STATUS") {
                            status_to_sender =
                                Some(session.from_page(&namespace, &sender_id, &data));
                            break;
                        }
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) | Err(_) => break,
                }
            }
            status_to_sender
        }
    });

    let output = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("the probe did not exit")
        .expect("reading the probe");
    let status_to_sender = driver.await.unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with('{'))
        .unwrap_or_else(|| panic!("no report.\nstdout:\n{stdout}\nstderr:\n{stderr}"));
    let report: Report = serde_json::from_str(line).expect("a readable report");

    assert!(
        report.ok,
        "the receiver page never came up: {report:?}\n{stderr}"
    );
    let Some(media) = report.media.clone() else {
        panic!("the page never got a LOAD: {report:?}\n{stderr}");
    };
    assert_eq!(
        media["url"].as_str(),
        Some(media_url.as_str()),
        "the application played something other than what the sender sent"
    );
    assert_eq!(
        media["state"], "playing",
        "the media did not play: {media}\n{stderr}"
    );
    let reached = media["currentTime"].as_f64().unwrap_or_default();
    assert!(
        reached > 0.0,
        "the picture never moved — a <video> that errored and one that is playing look \
         identical until the clock advances: {media}"
    );

    // …and the application's own status reached the sender, from the session's transport
    // id, which is what the sender opened its virtual connection to.
    let status = status_to_sender.expect("the application's MEDIA_STATUS never came back");
    assert_eq!(status.namespace, MEDIA_NS);
    assert_eq!(status.destination_id, "sender-0");
    assert!(status.source_id.starts_with("transport-"));

    // The page really was fetched from the URL the registry named, and the media really
    // was fetched by the page. Without this the assertions above could be satisfied by a
    // page that had the URL baked in.
    let fetched = origin.fetched();
    assert!(
        fetched.iter().any(|f| f.starts_with("/receiver.html")),
        "the receiver page was never fetched: {fetched:?}"
    );
    assert!(
        fetched.iter().any(|f| f.starts_with("/media.mp4")),
        "the media was never fetched: {fetched:?}"
    );
}

/// The other half of "diverse app ids": what happens to one that resolves to something
/// no browser can host. The registry answers and the launch is declined — with the
/// sender's own error rather than silence.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_id_that_is_not_a_page_is_declined_with_a_reason() {
    let mut catalogue = AppCatalogue::new(true);
    catalogue.record("0F5096E8", false);
    let mut session = CastSession::new(None);
    session.observe_catalogue(catalogue);

    // A mirroring id is not a page and never becomes one; it stays on the RTP path.
    let launch = CastMessage::json(
        "sender-0",
        "receiver-0",
        proto_cast::ns::RECEIVER,
        r#"{"requestId":1,"type":"LAUNCH","appId":"0F5096E8"}"#.to_owned(),
    );
    let reaction = session.handle(&launch).unwrap();
    assert!(
        reaction.launch_page.is_none(),
        "a mirroring id must not be sent to the browser"
    );

    // And an id the registry has, that has no page: declined, in the sender's words.
    let mut catalogue = AppCatalogue::new(true);
    catalogue.record("AAAAAAAA", true);
    let mut session = CastSession::new(None);
    session.observe_catalogue(catalogue);
    let launch = CastMessage::json(
        "sender-0",
        "receiver-0",
        proto_cast::ns::RECEIVER,
        r#"{"requestId":2,"type":"LAUNCH","appId":"AAAAAAAA"}"#.to_owned(),
    );
    let pending = session
        .handle(&launch)
        .unwrap()
        .launch_page
        .expect("a page launch");
    let refusal = session.page_refused(&pending, LaunchRefusal::NotFound);
    let payload: serde_json::Value =
        serde_json::from_str(refusal[0].payload_utf8.as_deref().unwrap()).unwrap();
    assert_eq!(payload["type"], "LAUNCH_ERROR");
    assert_eq!(payload["reason"], "NOT_FOUND");
    assert_eq!(payload["requestId"], 2);
}
