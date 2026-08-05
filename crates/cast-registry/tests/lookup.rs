//! The resolver against a real socket, over the whole diverse fixture set.
//!
//! The unit tests in `entry` prove the parse and the ones in `resolver` prove the cache;
//! neither ever opens a connection. This drives `resolve()` end to end — HTTP request,
//! response body, parse, cache write — against a local server replaying the captured
//! registry responses, so the composition is tested without the internet (ground rule 6).

// A test's own fixture server is not part of the receiver's network surface, so it has
// no entry in `crates/app/src/surface.rs` to point the lint at.
#![allow(clippy::disallowed_methods)]
#![allow(clippy::unwrap_used)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;

use cast_registry::{AppSurface, Registry, RegistryError};

/// Serve `tests/fixtures/registry/<a>.json` for `GET /?a=<a>`, one request per
/// connection, until dropped.
///
/// Hand-rolled rather than pulled from a crate: the whole server is "read a request line,
/// write a file", and a test dependency that brings an HTTP stack in to do it would be a
/// larger surface than the thing under test.
struct FixtureServer {
    port: u16,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Which app ids were actually asked for, so a test can prove a cache hit made *no*
    /// request rather than merely returning the right answer.
    asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl FixtureServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

        let stop = std::sync::Arc::clone(&shutdown);
        let log = std::sync::Arc::clone(&asked);
        let handle = std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        let mut reader =
                            std::io::BufReader::new(stream.try_clone().expect("clone"));
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() {
                            continue;
                        }
                        let app_id = line
                            .split_whitespace()
                            .nth(1)
                            .and_then(|target| target.split("a=").nth(1))
                            .unwrap_or("")
                            .to_owned();
                        if let Ok(mut seen) = log.lock() {
                            seen.push(app_id.clone());
                        }
                        let body = fixture(&app_id);
                        let (status, body) = match body {
                            Some(bytes) => ("200 OK", bytes),
                            // What the real endpoint does with an id it does not know.
                            None => ("404 Not Found", b"<h1>Not Found</h1>\n".to_vec()),
                        };
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                        let _ = stream.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            shutdown,
            handle: Some(handle),
            asked,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/cast/chromecast/device/app", self.port)
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn fixture(app_id: &str) -> Option<Vec<u8>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/registry")
        .join(format!("{}.json", app_id.to_ascii_uppercase()));
    let mut body = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .read_to_end(&mut body)
        .ok()?;
    // The captured 404 is stored as the HTML the endpoint really returns; serve it as
    // the 404 it was rather than as a 200 with an HTML body.
    if body.starts_with(b"<h1>") {
        return None;
    }
    Some(body)
}

fn registry(server: &FixtureServer, cache: Option<std::path::PathBuf>) -> Registry {
    Registry::with_cache_path(cache)
        .with_endpoint(server.endpoint())
        .with_timeout(std::time::Duration::from_secs(5))
}

/// Every app id we have a fixture for, resolved over a socket, and each one landing on
/// the surface its `url` field implies. This is the "diverse app ids" claim as a test:
/// two first-party pages, two third-party pages, a sample, all three native mirroring
/// ids, and one that does not exist.
#[tokio::test]
async fn the_whole_fixture_set_resolves_over_a_real_socket() {
    let server = FixtureServer::start();
    let registry = registry(&server, None);

    let pages = [
        ("CC1AD845", "gstatic.com/cast/sdk/default_receiver"),
        ("233637DE", "youtube.com/tv?castv=2.0"),
        ("9AC194DC", "app.plex.tv/cast"),
        ("CA5E8412", "netflix.com"),
        ("4F8B3483", "storage.googleapis.com/cast-reference-receiver"),
        ("B3419EF5", "default_receiver"),
    ];
    for (app_id, expect) in pages {
        let surface = registry
            .resolve(app_id)
            .await
            .unwrap_or_else(|e| panic!("{app_id}: {e}"));
        let url = surface
            .page_url()
            .unwrap_or_else(|| panic!("{app_id} resolved to {surface:?}, not a page"));
        assert!(url.contains(expect), "{app_id} resolved to {url}");
    }

    for app_id in ["0F5096E8", "85CDB22F", "674A0243"] {
        let surface = registry.resolve(app_id).await.expect(app_id);
        assert!(
            matches!(surface, AppSurface::Native { .. }),
            "{app_id} resolved to {surface:?}; a mirroring id must never become a page"
        );
    }

    let err = registry.resolve("DEADBEEF").await.unwrap_err();
    assert!(
        matches!(
            err,
            RegistryError::Lookup { .. } | RegistryError::NotRegistryJson(_)
        ),
        "{err:?}"
    );
}

/// A resolution is asked for once. The second `LAUNCH` of the same app — which is what
/// actually happens in a room, over and over — must not put a third party in the path of
/// starting a video.
#[tokio::test]
async fn a_second_launch_of_the_same_app_asks_nobody() {
    let server = FixtureServer::start();
    let registry = registry(&server, None);

    for _ in 0..5 {
        let surface = registry.resolve("233637DE").await.expect("resolve");
        assert_eq!(
            surface.page_url(),
            Some("https://www.youtube.com/tv?castv=2.0")
        );
    }
    assert_eq!(
        server.asked(),
        vec!["233637DE".to_owned()],
        "the registry was asked more than once for an app it had already resolved"
    );
}

/// The disk cache across a restart, over the real fetch path: resolve with the server
/// up, then bring a fresh `Registry` up against a *dead* endpoint and still launch.
#[tokio::test]
async fn what_was_resolved_before_a_restart_still_launches_after_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("registry.json");

    {
        let server = FixtureServer::start();
        let registry = registry(&server, Some(path.clone()));
        registry.resolve("9AC194DC").await.expect("resolve");
        // The cache write is spawned; give it the runtime tick it needs to land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // The server is gone with the scope above, so its port refuses instantly.
    let after = Registry::with_cache_path(Some(path))
        .with_endpoint("http://127.0.0.1:1/gone")
        .with_timeout(std::time::Duration::from_millis(100));
    let surface = after.resolve("9AC194DC").await.expect("resolve from cache");
    assert_eq!(surface.page_url(), Some("https://app.plex.tv/cast"));
}
