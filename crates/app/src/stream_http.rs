//! `/stream/*`: the panel's output, live, in a browser (#101).
//!
//! The sibling of `/screenshot.png` and the answer to the same want — see what the panel
//! is showing without standing in front of it — except continuously. Everything below the
//! HTTP surface is `pipeline::stream`; this is the routes, the cache headers, and the
//! small player the landing page carries.
//!
//! **Fetching the playlist is what starts the encoder.** Nothing is encoded and no frame
//! is read back until a request arrives, and the tap retires ten seconds after the last
//! one — so an unattended panel with nobody watching costs exactly nothing, and a browser
//! tab left open costs one readback per stream frame and no more. That also means the
//! first playlist request usually 503s while the encoder opens, which is why the player
//! below retries rather than giving up.
//!
//! Two players are served by one set of routes. Safari plays HLS natively, so it is handed
//! the playlist URL and nothing else happens. Everything else gets ~80 lines of Media
//! Source Extensions, which is enough *because we are both ends*: the playlist this serves
//! is a fixed shape, so there is no general HLS parser to vendor half a megabyte of.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

/// The handle on the output stream, or an uninhabited stand-in.
///
/// The same trick as `Screenshot` in `main`: making the type uninhabited rather than
/// `cfg`-ing the routes away means the endpoints still exist and still explain themselves,
/// and the "this build cannot encode" branch is the only representable state, so the
/// compiler agrees rather than a comment claiming it.
#[cfg(feature = "stream")]
pub type Stream = pipeline::StreamHandle;
#[cfg(not(feature = "stream"))]
pub type Stream = std::convert::Infallible;

/// Where the playlist lives. Named once, because the landing page's player and the
/// documentation both quote it.
pub const PLAYLIST_PATH: &str = "/stream/live.m3u8";

/// Where the segments live.
///
/// The `.m4s` is not decoration and the whole filename is one path parameter for a
/// reason. ffmpeg's HLS demuxer keeps an `allowed_segment_extensions` whitelist and
/// refuses a segment URI whose extension is not on it — a playlist pointing at `seg/3`
/// fails to open with "not in allowed_segment_extensions", which takes ffplay, VLC and
/// anything else built on libavformat out. So the sequence number arrives here with its
/// suffix attached and [`segment_sequence`] takes it off.
pub const SEGMENT_ROUTE: &str = "/stream/seg/{name}";

/// The sequence number in a segment filename, or `None` if that is not one.
///
/// A stranger on the LAN types whatever they like into this, so it is a lookup that
/// misses rather than a parse that fails.
///
/// Compiled in every build even though only the encoding one calls it: the route it spells
/// exists in every build too, and a helper that vanished with the feature would let the two
/// halves of that URL drift apart unnoticed.
#[cfg_attr(not(feature = "stream"), allow(dead_code))]
#[must_use]
pub fn segment_sequence(name: &str) -> Option<u32> {
    name.strip_suffix(".m4s")?.parse().ok()
}

/// Mount `/stream/*`.
pub fn routes(stream: Option<Stream>) -> Router {
    Router::new()
        .route(PLAYLIST_PATH, get(playlist_route))
        .route("/stream/init.mp4", get(init_route))
        .route(SEGMENT_ROUTE, get(segment_route))
        .route("/stream/status.json", get(status_route))
        .with_state(stream)
}

/// Nothing here may be cached, by anyone, ever.
///
/// The playlist changes every second by design. The segments look immutable and are not:
/// a stream that stops and restarts renumbers from one, so a cached `seg/1` is a *previous
/// stream's* first second, which a player would splice into the timeline and show as a
/// glitch rather than an error.
#[cfg(feature = "stream")]
fn uncacheable(content_type: &'static str, body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store, max-age=0"),
        ],
        body,
    )
        .into_response()
}

#[cfg(feature = "stream")]
mod live {
    use std::sync::Arc;
    use std::time::Instant;

    use super::{
        header, uncacheable, IntoResponse as _, Path, Response, State, StatusCode, Stream,
    };
    use pipeline::stream::{LiveStream, StreamStatus};

    /// How a playlist URI spells a segment, relative to the playlist itself.
    fn segment_uri(sequence: u32) -> String {
        format!("seg/{sequence}.m4s")
    }

    /// `GET /stream/live.m3u8` — the media playlist, and the request that starts the
    /// encoder.
    pub async fn playlist_route(State(handle): State<Option<Stream>>) -> Response {
        let Some(handle) = handle else {
            return super::no_encoder();
        };
        handle.ensure_running();
        let stream = handle.stream();
        if let Some(text) = stream.playlist("init.mp4", &segment_uri) {
            return uncacheable("application/vnd.apple.mpegurl", text.into_bytes());
        }
        // No segments yet. Which of the two reasons that is matters to whoever is waiting:
        // one resolves itself in a second and the other never will.
        match stream.status() {
            StreamStatus::Failed(why) => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                format!("the panel cannot encode: {why}\n"),
            )
                .into_response(),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (header::RETRY_AFTER, "1"),
                ],
                "the encoder is starting; ask again\n",
            )
                .into_response(),
        }
    }

    /// `GET /stream/init.mp4` — the initialisation segment.
    pub async fn init_route(State(handle): State<Option<Stream>>) -> Response {
        let Some(handle) = handle else {
            return super::no_encoder();
        };
        touch(handle.stream());
        handle.stream().init_segment().map_or_else(
            || (StatusCode::NOT_FOUND, "no stream yet\n").into_response(),
            |bytes| uncacheable("video/mp4", bytes.to_vec()),
        )
    }

    /// `GET /stream/seg/{n}.m4s` — one media segment, while it is still in the window.
    pub async fn segment_route(
        State(handle): State<Option<Stream>>,
        Path(name): Path<String>,
    ) -> Response {
        let Some(handle) = handle else {
            return super::no_encoder();
        };
        touch(handle.stream());
        super::segment_sequence(&name)
            .and_then(|sequence| handle.stream().segment(sequence))
            .map_or_else(
                // A player that asks for one this old has fallen out of the window, which is a
                // 404 rather than an error: reloading the playlist is the cure and every
                // player already does it.
                || (StatusCode::NOT_FOUND, "that segment has expired\n").into_response(),
                |bytes| uncacheable("video/iso.segment", bytes.to_vec()),
            )
    }

    /// `GET /stream/status.json` — what the stream is doing, and the codec string the
    /// player needs to open a `SourceBuffer` with.
    ///
    /// Deliberately does *not* start the encoder: this is what a monitoring script or a
    /// person poking at the box asks, and neither should be able to pin the render loop at
    /// display rate by curling a status endpoint.
    pub async fn status_route(State(handle): State<Option<Stream>>) -> Response {
        let Some(handle) = handle else {
            return super::no_encoder();
        };
        let stream = handle.stream();
        let body = match stream.status() {
            StreamStatus::Idle => serde_json::json!({ "state": "idle" }),
            StreamStatus::Starting => serde_json::json!({ "state": "starting" }),
            StreamStatus::Live {
                encoder,
                width,
                height,
                codec,
            } => serde_json::json!({
                "state": "live",
                "encoder": encoder,
                "width": width,
                "height": height,
                "codec": codec,
            }),
            StreamStatus::Failed(why) => serde_json::json!({
                "state": "failed",
                "error": why,
            }),
        };
        uncacheable("application/json", body.to_string().into_bytes())
    }

    /// Every fetch is a vote to keep the stream alive. Counting requests rather than
    /// tracking viewers is what makes "nobody is watching" self-correcting: there is no
    /// subscriber count anyone has to remember to decrement when a tab closes.
    fn touch(stream: &Arc<LiveStream>) {
        stream.touch(Instant::now());
    }
}

#[cfg(feature = "stream")]
use live::{init_route, playlist_route, segment_route, status_route};

/// The same four endpoints in a build that cannot encode.
///
/// One function per route rather than one shared handler, so the router above is identical
/// in both builds and cannot drift.
#[cfg(not(feature = "stream"))]
mod dead {
    use super::{Path, Response, State, Stream};

    pub async fn playlist_route(State(_): State<Option<Stream>>) -> Response {
        super::no_encoder()
    }
    pub async fn init_route(State(_): State<Option<Stream>>) -> Response {
        super::no_encoder()
    }
    pub async fn segment_route(State(_): State<Option<Stream>>, Path(_): Path<String>) -> Response {
        super::no_encoder()
    }
    pub async fn status_route(State(_): State<Option<Stream>>) -> Response {
        super::no_encoder()
    }
}

#[cfg(not(feature = "stream"))]
use dead::{init_route, playlist_route, segment_route, status_route};

/// 503 with the reason, not 404. "No such endpoint" and "this binary has no encoder in it"
/// are different problems and only one of them is worth chasing.
fn no_encoder() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        "this build cannot encode; rebuild with the `render` feature\n",
    )
        .into_response()
}

/// The landing page's player: a `<video>`, and enough script to feed it.
///
/// Kept as one constant rather than assembled, because it is markup and script and neither
/// wants interpolation — the one URL it needs is [`PLAYLIST_PATH`], and it is spelled the
/// same on both sides.
pub const PLAYER: &str = r##"
<section id="stream">
  <h2>Live</h2>
  <video id="panel" autoplay muted playsinline controls></video>
  <p id="stream-note">Connecting…</p>
</section>
<script>
(function () {
  var video = document.getElementById('panel');
  var note = document.getElementById('stream-note');
  var say = function (text) { note.textContent = text; };

  // Safari plays HLS natively, so there is nothing to do but point it at the playlist.
  // Asking for the playlist is also what starts the encoder, which is why this is enough.
  if (video.canPlayType('application/vnd.apple.mpegurl')) {
    video.src = '/stream/live.m3u8';
    say('');
    return;
  }
  if (!window.MediaSource) { say('This browser cannot play the stream.'); return; }

  var buffer = null;
  var pending = [];
  var have = {};

  function pump() {
    if (buffer && !buffer.updating && pending.length) { buffer.appendBuffer(pending.shift()); }
  }

  function bytes(url) {
    return fetch(url, { cache: 'no-store' }).then(function (r) {
      if (!r.ok) { throw new Error(url + ' → ' + r.status); }
      return r.arrayBuffer();
    });
  }

  // The playlist 503s until the first segment exists, which is a second or so after the
  // first request starts the encoder. Retrying is the normal path here, not error handling.
  function playlist(tries) {
    return fetch('/stream/live.m3u8', { cache: 'no-store' }).then(function (r) {
      if (r.ok) { return r.text(); }
      if (r.status !== 503 || tries <= 0) { return r.text().then(function (t) { throw new Error(t); }); }
      say('Starting the encoder…');
      return new Promise(function (go) { setTimeout(go, 500); }).then(function () {
        return playlist(tries - 1);
      });
    });
  }

  // Only the URI lines matter, and every other line in what we serve begins with '#'.
  function segments(text) {
    return text.split('\n').filter(function (line) {
      return line.length > 0 && line.charAt(0) !== '#';
    });
  }

  function follow() {
    playlist(0).then(function (text) {
      var wanted = segments(text);
      // Fetched in playlist order and appended in fetch order, because a SourceBuffer in
      // 'segments' mode places each fragment by its own `tfdt` — so arriving out of order
      // is a gap the player waits out rather than a reorder it fixes.
      var chain = Promise.resolve();
      wanted.forEach(function (uri) {
        if (have[uri]) { return; }
        have[uri] = true;
        chain = chain.then(function () {
          return bytes('/stream/' + uri).then(function (buf) {
            pending.push(new Uint8Array(buf));
            pump();
          });
        });
      });
      return chain;
    }).catch(function (e) {
      say(String(e.message || e));
    }).then(function () {
      // Stay near the live edge. A tab that was backgrounded comes back with the whole
      // window buffered, and playing it out from the front means watching the last ten
      // seconds of the panel instead of the panel.
      if (video.buffered.length) {
        var edge = video.buffered.end(video.buffered.length - 1);
        if (edge - video.currentTime > 4) { video.currentTime = edge - 1; }
      }
      setTimeout(follow, 500);
    });
  }

  playlist(60).then(function () {
    return fetch('/stream/status.json', { cache: 'no-store' }).then(function (r) { return r.json(); });
  }).then(function (status) {
    var mime = 'video/mp4; codecs="' + status.codec + '"';
    if (!window.MediaSource.isTypeSupported(mime)) { throw new Error('unsupported: ' + mime); }
    var source = new MediaSource();
    video.src = URL.createObjectURL(source);
    return new Promise(function (open) {
      source.addEventListener('sourceopen', function () { open(source); }, { once: true });
    }).then(function (ms) {
      ms.duration = Infinity;
      buffer = ms.addSourceBuffer(mime);
      buffer.mode = 'segments';
      buffer.addEventListener('updateend', pump);
      return bytes('/stream/init.mp4');
    }).then(function (init) {
      pending.push(new Uint8Array(init));
      pump();
      say(status.encoder + ' · ' + status.width + '×' + status.height);
      follow();
    });
  }).catch(function (e) {
    say(String(e.message || e));
  });
})();
</script>
"##;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use tower::ServiceExt as _;

    async fn body(response: Response) -> (StatusCode, String) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get_path(path: &str) -> (StatusCode, String) {
        let router = routes(None);
        let request = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        body(router.oneshot(request).await.unwrap()).await
    }

    #[tokio::test]
    async fn every_stream_route_exists_even_with_nothing_behind_it() {
        // The reason the handle type is uninhabited rather than the routes being `cfg`ed
        // away: a 404 sends whoever is debugging to look for a typo in the URL, and a 503
        // with a sentence sends them to the build.
        for path in [
            PLAYLIST_PATH,
            "/stream/init.mp4",
            "/stream/seg/1.m4s",
            "/stream/status.json",
        ] {
            let (status, text) = get_path(path).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert!(text.contains("cannot encode"), "{path}: {text}");
        }
    }

    #[test]
    fn a_segment_name_that_is_not_a_segment_is_a_miss_not_a_panic() {
        // The path parameter is whatever a stranger on the LAN typed.
        assert_eq!(segment_sequence("12.m4s"), Some(12));
        for name in [
            "nonsense",
            "12",
            "12.mp4",
            "",
            "-1.m4s",
            "99999999999999.m4s",
        ] {
            assert_eq!(segment_sequence(name), None, "{name}");
        }
    }

    #[test]
    fn the_segments_are_named_with_an_extension_libavformat_will_accept() {
        // ffmpeg's HLS demuxer refuses a segment URI whose extension is not on its
        // `allowed_segment_extensions` list, so a playlist pointing at `seg/3` opens
        // nowhere — no ffplay, no VLC, nothing built on libavformat. Found by pointing
        // ffmpeg at a live panel.
        assert!(SEGMENT_ROUTE.ends_with(".m4s") || segment_sequence("1.m4s").is_some());
        assert!(PLAYER.contains(".m4s") || PLAYER.contains("'/stream/' + uri"));
    }

    #[test]
    fn the_player_asks_for_the_playlist_this_module_actually_serves() {
        // The script is a constant and the route is a constant, and nothing but this
        // checks that they are the same string.
        assert!(PLAYER.contains(PLAYLIST_PATH));
        assert!(PLAYER.contains("/stream/init.mp4"));
        assert!(PLAYER.contains("/stream/status.json"));
        // The playlist's own segment URIs are relative, and the player resolves them
        // against `/stream/` — so the prefix it prepends has to match `segment_uri`.
        assert!(PLAYER.contains("'/stream/' + uri"));
    }
}
