//! `/remote/*`: driving the panel from a browser (#18).
//!
//! The sibling of `/stream/*` and the answer to a different want. That one duplicates the
//! panel so you can *see* it; this one makes the duplicate touchable, which needs a
//! transport HLS cannot provide — three to six seconds of segment latency is fine for
//! watching and unusable for control.
//!
//! Two routes and a page. `POST /remote/whep` takes an SDP offer and returns an answer;
//! everything after that is UDP between the peer and `pipeline::remote`, and this host
//! sees none of it. Non-trickle, so one request is the whole negotiation — there is no
//! session to keep, no polling, and nothing to clean up if the peer never comes back.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;

/// The handle on the remote-control service, or an uninhabited stand-in.
///
/// The same trick as `Stream` in `stream_http`: making the type uninhabited rather than
/// `cfg`-ing the routes away means the endpoints still exist and still explain themselves
/// in a build that cannot serve them.
#[cfg(feature = "remote")]
pub type Remote = std::sync::Arc<pipeline::remote::RemoteService>;
#[cfg(not(feature = "remote"))]
pub type Remote = std::convert::Infallible;

/// Where the page lives.
pub const PAGE_PATH: &str = "/remote/";

/// Where an offer goes.
pub const WHEP_PATH: &str = "/remote/whep";

/// The largest SDP offer that will be read.
///
/// A real one is two or three kilobytes. This is generous enough that no browser will hit
/// it and small enough that a stranger on the LAN cannot make the panel allocate.
const MAX_OFFER: usize = 64 * 1024;

/// Mount `/remote/*`.
pub fn routes(remote: Option<Remote>) -> Router {
    Router::new()
        .route(PAGE_PATH, get(page_route))
        .route(WHEP_PATH, post(whep_route))
        .with_state(remote)
}

/// `GET /remote/` — the control page.
///
/// Served in every build, including one with no transport behind it: the page then loads
/// and says what is wrong, which is a better answer than a 404 for the URL somebody typed
/// off the panel.
async fn page_route() -> Html<&'static str> {
    Html(PAGE)
}

/// `POST /remote/whep` — an SDP offer in, an SDP answer out.
#[cfg(feature = "remote")]
async fn whep_route(State(remote): State<Option<Remote>>, offer: String) -> Response {
    let Some(remote) = remote else {
        return unavailable();
    };
    if offer.len() > MAX_OFFER {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            [(header::CACHE_CONTROL, "no-store")],
            "that offer is not an offer\n",
        )
            .into_response();
    }
    match remote.answer(&offer).await {
        Ok(answer) => (
            StatusCode::CREATED,
            [
                (header::CONTENT_TYPE, "application/sdp"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            answer,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "remote: could not answer an offer");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CACHE_CONTROL, "no-store")],
                format!("the panel could not accept that: {e}\n"),
            )
                .into_response()
        }
    }
}

#[cfg(not(feature = "remote"))]
#[allow(clippy::unused_async)]
async fn whep_route(State(_): State<Option<Remote>>, offer: String) -> Response {
    let _ = (offer, MAX_OFFER);
    unavailable()
}

/// 503 with the reason, not 404 — "no such endpoint" and "this binary has no transport in
/// it" are different problems and only one of them is worth chasing.
fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        "this build cannot serve the remote UI; rebuild with the `render` feature\n",
    )
        .into_response()
}

/// The control page.
///
/// One constant rather than an assembled template: it is markup, style and script, none of
/// which wants interpolation, and the two URLs it needs are spelled the same on both sides
/// (a test checks that).
///
/// The load-bearing decisions in it, none of which are obvious from reading the code:
///
/// - **The `<video>` is never fullscreened.** A native fullscreen video hands control to
///   the browser's — on iOS, the OS's — media player, which cannot be overlaid and will
///   not deliver pointer events. The *container* goes fullscreen instead, and the capture
///   layer over the video is an ordinary DOM element that keeps working.
/// - **Pointer capture on `pointerdown`.** Without it a drag that leaves the video element
///   stops delivering `pointermove`, so every gesture that wanders would freeze halfway.
///   With it, leaving the element clamps to the edge rather than cancelling — a drag that
///   goes out of frame and comes back is one gesture, and the panel clamps identically.
/// - **Coordinates are measured against the rendered video box, not the element.**
///   `object-fit: contain` letterboxes, so using the element's rectangle offsets every
///   coordinate by the size of the bars and makes the whole thing feel subtly broken.
/// - **`touch-action: none` is mandatory.** Without it the browser claims drags as scroll
///   or pinch and no `pointermove` is ever delivered.
/// - **There is a Home button.** The panel's way home is a left-edge swipe, which on
///   Android is the system back gesture and in iOS Safari is swipe-to-go-back — the
///   browser eats it before the page sees it. A remote that could only pass gestures
///   through would have no way home, so this one does not try.
pub const PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no,viewport-fit=cover">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-capable" content="yes">
<title>castaway · remote</title>
<style>
  :root { color-scheme: dark; }
  html,body { margin:0; height:100%; background:#000; color:#eee;
              font:15px/1.4 system-ui,sans-serif; overscroll-behavior:none; }
  #stage { position:relative; width:100%; height:100%; display:flex;
           align-items:center; justify-content:center; background:#000; }
  video  { width:100%; height:100%; object-fit:contain; display:block; background:#000; }
  /* The capture layer. Covers the stage rather than the video because a finger that
     leaves the picture must keep being delivered — pointer capture does that, and this
     keeps the press that starts outside the letterbox from selecting the page instead. */
  #grab  { position:absolute; inset:0; touch-action:none; user-select:none;
           -webkit-user-select:none; -webkit-touch-callout:none; }
  #bar   { position:absolute; left:0; right:0; bottom:0; display:flex; gap:.5rem;
           padding:.5rem; background:linear-gradient(transparent,#000a); }
  button { font:inherit; color:#eee; background:#222c; border:1px solid #444;
           border-radius:.4rem; padding:.5rem .9rem; touch-action:manipulation; }
  button:active { background:#333c; }
  #note  { position:absolute; top:.5rem; left:.5rem; right:.5rem; color:#bbb;
           font-size:.85rem; text-shadow:0 1px 2px #000; pointer-events:none; }
</style>
</head><body>
<div id="stage">
  <video id="panel" autoplay muted playsinline></video>
  <div id="grab"></div>
  <p id="note">Connecting…</p>
  <div id="bar">
    <button id="home" type="button">Home</button>
    <button id="full" type="button">Fullscreen</button>
  </div>
</div>
<script>
(function () {
  var video = document.getElementById('panel');
  var grab  = document.getElementById('grab');
  var stage = document.getElementById('stage');
  var note  = document.getElementById('note');
  var say = function (t) { note.textContent = t; };

  var channel = null;
  var live = {};   // pointerId -> true, for contacts we have told the panel about

  function send(msg) {
    if (channel && channel.readyState === 'open') {
      try { channel.send(JSON.stringify(msg)); } catch (e) { /* closing */ }
    }
  }

  // Where the picture actually is inside the element. `object-fit: contain` letterboxes,
  // so the element's rectangle is not the video's — using it would offset every
  // coordinate by the size of the bars.
  function box() {
    var r = video.getBoundingClientRect();
    var vw = video.videoWidth, vh = video.videoHeight;
    if (!vw || !vh) { return r; }
    var scale = Math.min(r.width / vw, r.height / vh);
    var w = vw * scale, h = vh * scale;
    return { left: r.left + (r.width - w) / 2, top: r.top + (r.height - h) / 2,
             width: w, height: h };
  }

  // Clamped, not rejected: a drag that leaves the picture and comes back is one gesture,
  // and pinning it to the edge is what the panel does with a finger dragged off the
  // glass. The panel clamps too, so both ends agree on where the contact is.
  function at(e) {
    var b = box();
    var x = b.width  > 0 ? (e.clientX - b.left) / b.width  : 0;
    var y = b.height > 0 ? (e.clientY - b.top)  / b.height : 0;
    return { x: Math.min(1, Math.max(0, x)), y: Math.min(1, Math.max(0, y)) };
  }

  function touch(e, phase) {
    var p = at(e);
    send({ type: 'touch', id: e.pointerId >>> 0, phase: phase, x: p.x, y: p.y,
           pressure: e.pressure, width: e.width, height: e.height,
           pointer: e.pointerType });
  }

  grab.addEventListener('pointerdown', function (e) {
    // Capture is what keeps `pointermove` coming once the finger leaves this element.
    // Without it every gesture that wandered would freeze where it crossed the edge.
    try { grab.setPointerCapture(e.pointerId); } catch (err) { /* not capturable */ }
    live[e.pointerId] = true;
    touch(e, 'down');
    e.preventDefault();
  });

  grab.addEventListener('pointermove', function (e) {
    if (!live[e.pointerId]) { return; }   // hover is the panel's own cursor, not ours
    touch(e, 'move');
    e.preventDefault();
  });

  function end(e, phase) {
    if (!live[e.pointerId]) { return; }
    delete live[e.pointerId];
    touch(e, phase);
    e.preventDefault();
  }
  grab.addEventListener('pointerup',     function (e) { end(e, 'up'); });
  // Not an error path. The browser takes the pointer away for a system gesture, a
  // notification shade, a fullscreen change — and a contact that never ends leaves the
  // panel believing a finger is down.
  grab.addEventListener('pointercancel', function (e) { end(e, 'cancel'); });

  grab.addEventListener('wheel', function (e) {
    var p = at(e);
    // deltaMode 1 is lines and 2 is pages; the panel's wheel is specified in pixels.
    var k = e.deltaMode === 1 ? 16 : (e.deltaMode === 2 ? window.innerHeight : 1);
    send({ type: 'wheel', x: p.x, y: p.y, dx: e.deltaX * k, dy: -e.deltaY * k });
    e.preventDefault();
  }, { passive: false });

  grab.addEventListener('contextmenu', function (e) { e.preventDefault(); });

  // A phone that locks or is backgrounded mid-drag would otherwise strand every contact
  // it had down. Cancelling proactively beats waiting for the connection to time out.
  function releaseAll() {
    Object.keys(live).forEach(function (id) {
      send({ type: 'touch', id: (id >>> 0), phase: 'cancel', x: 0, y: 0 });
      delete live[id];
    });
  }
  document.addEventListener('visibilitychange', function () {
    if (document.hidden) { releaseAll(); }
  });
  window.addEventListener('pagehide', releaseAll);

  document.getElementById('home').addEventListener('click', function () {
    send({ type: 'home' });
  });

  // The container, never the video. A native fullscreen video hands control to the
  // browser's own player UI, which cannot be overlaid and delivers no pointer events —
  // so the capture layer would simply stop existing.
  document.getElementById('full').addEventListener('click', function () {
    if (document.fullscreenElement) { document.exitFullscreen(); return; }
    var go = stage.requestFullscreen || stage.webkitRequestFullscreen;
    if (go) {
      Promise.resolve(go.call(stage)).then(function () {
        // Android honours this; iOS ignores it. A 4K landscape panel on a portrait phone
        // is otherwise a small strip between two large black bars.
        if (screen.orientation && screen.orientation.lock) {
          screen.orientation.lock('landscape').catch(function () {});
        }
      }).catch(function () { say('This browser will not go fullscreen.'); });
    } else {
      // iPhone Safari has no Fullscreen API for elements. Add to Home Screen gets a
      // standalone window — and loses the swipe-back gesture that fights the panel's own.
      say('Add this page to your Home Screen for a fullscreen remote.');
    }
  });

  // Keep the flow alive through anything that reaps idle UDP, and give the panel a
  // reason to believe someone is still watching.
  setInterval(function () { send({ type: 'ping' }); }, 5000);

  function connect() {
    var pc = new RTCPeerConnection({ iceServers: [] });
    // Default options: ordered and reliable. A lost `up` after a `down` would strand a
    // contact on the panel for the rest of the session.
    channel = pc.createDataChannel('input');
    channel.onopen  = function () { say(''); };
    channel.onclose = function () { say('Input channel closed.'); };

    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.ontrack = function (e) { video.srcObject = e.streams[0] || new MediaStream([e.track]); };
    pc.onconnectionstatechange = function () {
      if (pc.connectionState === 'failed' || pc.connectionState === 'disconnected') {
        say('Reconnecting…');
        releaseAll();
        try { pc.close(); } catch (e) {}
        setTimeout(connect, 1000);
      }
    };

    pc.createOffer().then(function (offer) {
      return pc.setLocalDescription(offer);
    }).then(function () {
      // Non-trickle: the panel answers with every candidate at once, so we send ours the
      // same way rather than keeping a signalling channel open for nothing.
      return new Promise(function (done) {
        if (pc.iceGatheringState === 'complete') { return done(); }
        var timer = setTimeout(done, 3000);
        pc.onicegatheringstatechange = function () {
          if (pc.iceGatheringState === 'complete') { clearTimeout(timer); done(); }
        };
      });
    }).then(function () {
      say('Connecting…');
      return fetch('/remote/whep', {
        method: 'POST',
        headers: { 'Content-Type': 'application/sdp' },
        body: pc.localDescription.sdp,
        cache: 'no-store'
      });
    }).then(function (r) {
      if (!r.ok) { return r.text().then(function (t) { throw new Error(t.trim()); }); }
      return r.text();
    }).then(function (sdp) {
      return pc.setRemoteDescription({ type: 'answer', sdp: sdp });
    }).catch(function (e) {
      say(String(e.message || e));
      setTimeout(connect, 2000);
    });
  }

  connect();
})();
</script>
</body></html>
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

    #[tokio::test]
    async fn the_offer_endpoint_exists_even_with_nothing_behind_it() {
        // Same reason `/stream/*` does: a 404 sends whoever is debugging to look for a
        // typo in the URL, and a 503 with a sentence sends them to the build.
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(WHEP_PATH)
            .body(axum::body::Body::from("v=0\r\n"))
            .unwrap();
        let (status, text) = body(routes(None).oneshot(request).await.unwrap()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(text.contains("cannot serve"), "{text}");
    }

    #[tokio::test]
    async fn the_page_is_served_by_a_build_that_cannot_answer_an_offer() {
        // The page's own error handling is what explains the problem, and it cannot do
        // that if the page itself 404s.
        let request = axum::http::Request::builder()
            .uri(PAGE_PATH)
            .body(axum::body::Body::empty())
            .unwrap();
        let (status, text) = body(routes(None).oneshot(request).await.unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains("<video"), "{text}");
    }

    #[test]
    fn the_page_posts_to_the_route_this_module_serves() {
        // Two constants that have to be the same string, and nothing but this checks it.
        assert!(PAGE.contains(WHEP_PATH));
    }

    #[test]
    fn the_page_does_not_fullscreen_the_video_element() {
        // A native fullscreen video takes the pointer events with it, and the remote
        // silently becomes view-only. The container is what goes fullscreen.
        assert!(PAGE.contains("stage.requestFullscreen"));
        assert!(!PAGE.contains("video.requestFullscreen"));
        assert!(!PAGE.contains("webkitEnterFullscreen"));
        // `playsinline` is the other half on iOS: without it, playing the video enters
        // the native player by itself and there is nothing to overlay.
        assert!(PAGE.contains("playsinline"));
    }

    #[test]
    fn the_capture_layer_takes_the_gestures_the_browser_would_eat() {
        // Without `touch-action: none` the browser claims a drag as scroll or pinch and
        // no pointermove is ever delivered — the single most load-bearing line of CSS
        // on the page.
        assert!(PAGE.contains("touch-action:none"));
        assert!(PAGE.contains("setPointerCapture"));
        assert!(PAGE.contains("pointercancel"));
    }

    #[test]
    fn every_way_a_contact_can_end_is_handled() {
        // A contact that never ends leaves the panel believing a finger is down for the
        // rest of the session. There are four ways out and the page needs all of them.
        for hook in ["pointerup", "pointercancel", "visibilitychange", "pagehide"] {
            assert!(PAGE.contains(hook), "{hook} is not handled");
        }
    }

    #[test]
    fn the_page_offers_a_way_home_that_is_not_a_gesture() {
        // The panel's home gesture is a left-edge swipe, which the phone's own OS eats.
        assert!(PAGE.contains(r#"'home'"#));
        assert!(PAGE.contains("type: 'home'"));
    }
}
