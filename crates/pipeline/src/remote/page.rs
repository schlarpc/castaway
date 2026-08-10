//! The player that drives the panel, for the landing page (#18).
//!
//! Lives with the transport rather than with the routes that serve it: it is the *other
//! half* of the same protocol, and the half that has to be reachable by a test with a real
//! browser in it. Compiled in every build, feature or no feature, because a build that
//! cannot answer an offer should still serve something that says so.
//!
//! **Not a page of its own.** It is a fragment the landing page embeds, so there is one
//! place to look at the panel rather than a viewer at `/` and a driver somewhere else.
//!
//! ## Stopped until asked
//!
//! It starts as an empty stage and a play button, and nothing is negotiated or encoded
//! until that button is pressed — the same discipline `/stream/*` has, where fetching the
//! playlist is what starts the encoder. A landing page left open in a tab costs the panel
//! nothing.
//!
//! ## Playing is what makes it an input surface
//!
//! Before the press it is a video with a control on it. After, the control is gone and the
//! whole picture is a capture layer: there is nothing to pause or scrub, because what is on
//! it is the panel *now* and every press on it is a finger on the glass. The two states are
//! exclusive by construction — the capture layer is `display:none` until the track arrives,
//! so it cannot shadow the play button and the button cannot shadow it.

/// The player, as a fragment: markup, style, and enough script to negotiate.
///
/// One constant rather than an assembled template — it is markup and script and neither
/// wants interpolation, and the one URL it needs is spelled the same on both sides (a test
/// checks that).
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
/// - **The keyboard is a field, not a key relay (#260).** A phone's IME composes —
///   autocorrect, swipe typing, CJK — so by the time script sees anything there is no
///   key stream, only the field's value. What travels is the value's *diff*: insertions
///   as `text` messages, deletions as `backspace` keys, and nothing at all while
///   `isComposing`. Only the strokes text cannot say — Enter, arrows, backspace on an
///   empty field — travel as `key` messages.
pub const PLAYER: &str = r##"
<section id="remote">
  <h2>The panel</h2>
  <div id="stage" data-state="idle">
    <video id="panel" autoplay muted playsinline></video>
    <div id="grab"></div>
    <button id="play" type="button" aria-label="Play, and take control of the panel">
      <span id="play-mark"></span><span>Take control</span>
    </button>
    <p id="note"></p>
    <div id="bar">
      <button id="home" type="button">Home</button>
      <button id="keys" type="button">Keyboard</button>
      <button id="full" type="button">Fullscreen</button>
      <button id="stop" type="button">Stop</button>
    </div>
    <div id="kbd">
      <input id="typein" type="text" autocomplete="off" autocapitalize="off"
             autocorrect="off" spellcheck="false" placeholder="Type on the panel">
      <button data-key="backspace" type="button" aria-label="Backspace">&#9003;</button>
      <button data-key="enter" type="button" aria-label="Enter">&#9166;</button>
      <button data-key="up" type="button" aria-label="Up">&#8593;</button>
      <button data-key="down" type="button" aria-label="Down">&#8595;</button>
      <button data-key="left" type="button" aria-label="Left">&#8592;</button>
      <button data-key="right" type="button" aria-label="Right">&#8594;</button>
    </div>
  </div>
  <p id="hint">Live, with your touches going through. Nothing is encoded until you press
  play, and it stops again when you do.</p>
</section>
<style>
  #stage { position:relative; width:100%; aspect-ratio:16/9; background:#000;
           border-radius:.4rem; overflow:hidden; }
  #stage video { width:100%; height:100%; object-fit:contain; display:block;
                 background:#000; }
  /* The capture layer only exists once there is a picture to touch. While the stage is
     idle it is not in the way of the play button, and there would be nothing to send to. */
  #grab  { position:absolute; inset:0; touch-action:none; user-select:none;
           -webkit-user-select:none; -webkit-touch-callout:none; display:none; }
  #stage[data-state="live"] #grab { display:block; }
  #play  { position:absolute; inset:0; margin:auto; width:12rem; height:3rem;
           display:flex; align-items:center; justify-content:center; gap:.6rem;
           font:inherit; color:#eee; background:#0009; border:1px solid #555;
           border-radius:2rem; cursor:pointer; touch-action:manipulation; }
  #play:hover { background:#000c; border-color:#888; }
  #play-mark { width:0; height:0; border-left:.9rem solid #eee;
               border-top:.55rem solid transparent; border-bottom:.55rem solid transparent; }
  #stage:not([data-state="idle"]) #play { display:none; }
  /* The bar must not swallow presses: it sits over the bottom of the picture, which is
     exactly where the panel draws its transport strip. Only the buttons take input. */
  #bar   { position:absolute; left:0; right:0; bottom:0; display:none; gap:.5rem;
           padding:.5rem; pointer-events:none; }
  #stage[data-state="live"] #bar { display:flex; }
  #bar button { font:inherit; color:#eee; background:#222c; border:1px solid #444;
                border-radius:.4rem; padding:.4rem .8rem; touch-action:manipulation;
                pointer-events:auto; cursor:pointer; }
  #bar button:active { background:#333c; }
  /* The keyboard tray: only reachable while live and toggled, so it can never shadow
     the play button, and closing it gives the picture back its bottom band. */
  #kbd   { position:absolute; left:0; right:0; bottom:2.9rem; display:none; gap:.4rem;
           padding:.4rem .5rem; background:#000b; align-items:center; }
  #stage[data-state="live"][data-kbd="on"] #kbd { display:flex; }
  #kbd input { flex:1; min-width:6rem; font:inherit; color:#eee; background:#111;
               border:1px solid #444; border-radius:.3rem; padding:.35rem .5rem; }
  #kbd button { font:inherit; color:#eee; background:#222c; border:1px solid #444;
                border-radius:.3rem; padding:.35rem .6rem; touch-action:manipulation;
                cursor:pointer; }
  #kbd button:active { background:#333c; }
  #note  { position:absolute; top:.5rem; left:.5rem; right:.5rem; margin:0; color:#bbb;
           font-size:.85rem; text-shadow:0 1px 2px #000; pointer-events:none; }
  #hint  { color:#999; font-size:.85rem; }
</style>
<script>
(function () {
  var video = document.getElementById('panel');
  var grab  = document.getElementById('grab');
  var stage = document.getElementById('stage');
  var note  = document.getElementById('note');
  var say = function (t) { note.textContent = t || ''; };
  var state = function (s) { stage.dataset.state = s; };

  var pc = null, channel = null, ping = null;
  var live = {};   // pointerId -> true, for contacts the panel has been told about

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

  // ---- Keyboard (#260) -----------------------------------------------------------
  // The design constraint is the phone's IME: autocorrect, swipe typing and CJK input
  // compose, so there is no key stream to forward — only the field's value. So the tray
  // holds a real text field, and what travels is the *diff* of its value: typed text as
  // a `text` message, deletions as `backspace` keys. The keys that cannot be said as
  // text — Enter, arrows, backspace on an empty field — are explicit buttons and an
  // explicit keydown path.
  var field = document.getElementById('typein');
  var lastValue = '';

  // Send what changed since the last look. Deletions are counted in code points, not
  // UTF-16 units, because the panel's Backspace deletes characters — two backspaces for
  // one deleted emoji would eat a neighbour.
  function syncField() {
    var v = field.value;
    var common = 0;
    while (common < lastValue.length && common < v.length &&
           lastValue.charCodeAt(common) === v.charCodeAt(common)) { common += 1; }
    // Never split a surrogate pair at the diff boundary.
    if (common > 0 &&
        lastValue.charCodeAt(common - 1) >= 0xD800 &&
        lastValue.charCodeAt(common - 1) <= 0xDBFF) { common -= 1; }
    var removed = Array.from(lastValue.slice(common)).length;
    for (var i = 0; i < removed; i++) { send({ type: 'key', key: 'backspace' }); }
    if (v.length > common) { send({ type: 'text', text: v.slice(common) }); }
    lastValue = v;
  }

  field.addEventListener('input', function (e) {
    // Mid-composition values are provisional — the IME may replace them wholesale on
    // commit. Sending them would type the candidates as well as the choice.
    if (e.isComposing) { return; }
    syncField();
  });
  field.addEventListener('compositionend', syncField);

  field.addEventListener('keydown', function (e) {
    if (e.key === 'Enter') {
      syncField();   // anything the IME committed but input has not delivered yet
      send({ type: 'key', key: 'enter' });
      field.value = ''; lastValue = '';
      e.preventDefault();
    } else if (e.key === 'Backspace' && field.value.length === 0) {
      // An empty field has no text to diff, but the panel's field may not be empty.
      send({ type: 'key', key: 'backspace' });
      e.preventDefault();
    }
  });

  var kbd = document.getElementById('kbd');
  Array.prototype.forEach.call(kbd.querySelectorAll('button[data-key]'), function (b) {
    // Keep focus in the field, or every arrow press would drop the phone's keyboard.
    b.addEventListener('pointerdown', function (e) { e.preventDefault(); });
    b.addEventListener('click', function () { send({ type: 'key', key: b.dataset.key }); });
  });

  function resetKeyboard() {
    stage.dataset.kbd = 'off';
    field.value = '';
    lastValue = '';
  }

  document.getElementById('keys').addEventListener('click', function () {
    var on = stage.dataset.kbd === 'on';
    stage.dataset.kbd = on ? 'off' : 'on';
    if (!on) { field.focus(); }
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

  // Stopping is what retires the encoder: the tap goes when the last peer does, so a tab
  // left open on the landing page costs the panel nothing again.
  function stop() {
    releaseAll();
    resetKeyboard();
    if (ping) { clearInterval(ping); ping = null; }
    if (pc) { try { pc.close(); } catch (e) {} pc = null; }
    channel = null;
    video.srcObject = null;
    state('idle');
    say('');
  }
  document.getElementById('stop').addEventListener('click', stop);

  function start() {
    state('starting');
    say('Starting the encoder…');
    var self = new RTCPeerConnection({ iceServers: [] });
    pc = self;
    // Default options: ordered and reliable. A lost `up` after a `down` would strand a
    // contact on the panel for the rest of the session.
    channel = self.createDataChannel('input');
    channel.onopen = function () { say(''); };

    self.addTransceiver('video', { direction: 'recvonly' });
    self.ontrack = function (e) {
      video.srcObject = e.streams[0] || new MediaStream([e.track]);
      // The moment it stops being a video and starts being the panel.
      state('live');
      say('');
    };
    self.onconnectionstatechange = function () {
      if (pc !== self) { return; }
      if (self.connectionState === 'failed' || self.connectionState === 'disconnected') {
        stop();
        say('The panel went away.');
      }
    };

    self.createOffer().then(function (offer) {
      return self.setLocalDescription(offer);
    }).then(function () {
      // Non-trickle: the panel answers with every candidate at once, so we send ours the
      // same way rather than keeping a signalling channel open for nothing.
      return new Promise(function (done) {
        if (self.iceGatheringState === 'complete') { return done(); }
        var timer = setTimeout(done, 3000);
        self.onicegatheringstatechange = function () {
          if (self.iceGatheringState === 'complete') { clearTimeout(timer); done(); }
        };
      });
    }).then(function () {
      return fetch('/remote/whep', {
        method: 'POST',
        headers: { 'Content-Type': 'application/sdp' },
        body: self.localDescription.sdp,
        cache: 'no-store'
      });
    }).then(function (r) {
      if (!r.ok) { return r.text().then(function (t) { throw new Error(t.trim()); }); }
      return r.text();
    }).then(function (sdp) {
      return self.setRemoteDescription({ type: 'answer', sdp: sdp });
    }).then(function () {
      // Keep the flow alive through anything that reaps idle UDP, and give the panel a
      // reason to believe someone is still watching.
      ping = setInterval(function () { send({ type: 'ping' }); }, 5000);
    }).catch(function (e) {
      say(String(e.message || e));
      state('idle');
    });
  }

  document.getElementById('play').addEventListener('click', start);
})();
</script>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_stopped_until_somebody_presses_it() {
        // The whole reason it can sit on the landing page: nothing is negotiated and
        // nothing is encoded until the button is pressed, so a tab left open costs the
        // panel nothing. Same discipline as `/stream/*`, where fetching the playlist is
        // what starts the encoder.
        assert!(PLAYER.contains(r#"data-state="idle""#));
        let (before, after) = PLAYER.split_once("function start()").expect("a start");
        assert!(
            !before.contains("new RTCPeerConnection"),
            "the connection is made before anything asked for it"
        );
        assert!(after.contains("new RTCPeerConnection"));
        assert!(PLAYER.contains("getElementById('play').addEventListener('click', start)"));
    }

    #[test]
    fn the_capture_layer_does_not_exist_until_there_is_a_picture() {
        // The two states have to be exclusive by construction rather than by careful
        // ordering: an invisible capture layer over the idle stage would swallow the
        // press meant for the play button, and the player would look broken.
        assert!(PLAYER.contains("#grab  { position:absolute; inset:0;"));
        let grab_rule = PLAYER
            .split("#grab  {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .expect("a #grab rule");
        assert!(
            grab_rule.contains("display:none"),
            "the capture layer must start hidden: {grab_rule}"
        );
        assert!(PLAYER.contains(r#"#stage[data-state="live"] #grab { display:block; }"#));
    }

    #[test]
    fn playing_is_what_makes_it_an_input_surface() {
        // `ontrack` is the moment there is something to touch, so it is the moment the
        // stage flips. Before it there is a video with a control on it; after it there is
        // the panel.
        let ontrack = PLAYER
            .split("self.ontrack")
            .nth(1)
            .and_then(|r| r.split("};").next())
            .expect("an ontrack handler");
        assert!(ontrack.contains("state('live')"), "{ontrack}");
    }

    #[test]
    fn it_is_never_a_video_with_controls() {
        // A `controls` attribute would offer pause and scrub over a live panel, neither of
        // which means anything, and would put native chrome exactly where the capture
        // layer needs to be.
        assert!(!PLAYER.contains("controls"));
        assert!(PLAYER.contains("playsinline"));
    }

    #[test]
    fn the_video_element_is_never_fullscreened() {
        // A native fullscreen video takes the pointer events with it and the remote
        // silently becomes view-only. The container is what goes fullscreen.
        assert!(PLAYER.contains("stage.requestFullscreen"));
        assert!(!PLAYER.contains("video.requestFullscreen"));
        assert!(!PLAYER.contains("webkitEnterFullscreen"));
    }

    #[test]
    fn the_capture_layer_takes_the_gestures_the_browser_would_eat() {
        // Without `touch-action: none` the browser claims a drag as scroll or pinch and no
        // pointermove is ever delivered — the single most load-bearing line of CSS here.
        assert!(PLAYER.contains("touch-action:none"));
        assert!(PLAYER.contains("setPointerCapture"));
    }

    #[test]
    fn every_way_a_contact_can_end_is_handled() {
        // A contact that never ends leaves the panel believing a finger is down for the
        // rest of the session. There are four ways out and all of them are needed.
        for hook in ["pointerup", "pointercancel", "visibilitychange", "pagehide"] {
            assert!(PLAYER.contains(hook), "{hook} is not handled");
        }
    }

    #[test]
    fn stopping_releases_what_it_was_holding() {
        // Stop is a way out mid-drag as much as a way to retire the encoder.
        let stop = PLAYER
            .split("function stop()")
            .nth(1)
            .and_then(|r| r.split("\n  }").next())
            .expect("a stop");
        assert!(stop.contains("releaseAll()"), "{stop}");
        assert!(stop.contains("state('idle')"), "{stop}");
    }

    #[test]
    fn the_control_bar_does_not_shadow_the_panels_own_transport_strip() {
        // The bar sits over the bottom of the picture, which is where the panel draws its
        // transport strip. If the container took pointer events the whole bottom band
        // would be untouchable and nothing would say why.
        let bar = PLAYER
            .split("#bar   {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .expect("a #bar rule");
        assert!(bar.contains("pointer-events:none"), "{bar}");
        assert!(PLAYER.contains("pointer-events:auto"));
    }

    #[test]
    fn typing_travels_as_text_and_never_while_composing() {
        // The IME constraint (#260): composed input has no key stream to forward, so
        // insertions are `text` messages, and a mid-composition value is provisional —
        // forwarding it would type the candidates as well as the choice.
        assert!(PLAYER.contains("type: 'text'"));
        assert!(PLAYER.contains("if (e.isComposing) { return; }"));
        assert!(PLAYER.contains("compositionend"));
    }

    #[test]
    fn the_strokes_text_cannot_say_are_keys() {
        // Enter, arrows and an empty-field backspace have no diff to travel as. The
        // spellings here are the wire contract `input_touch::wire` parses.
        assert!(PLAYER.contains("type: 'key'"));
        for key in ["backspace", "enter", "up", "down", "left", "right"] {
            assert!(
                PLAYER.contains(&format!("data-key=\"{key}\"")),
                "{key} has no button"
            );
        }
        // Backspace when the field is empty still has to reach the panel.
        assert!(PLAYER.contains("e.key === 'Backspace' && field.value.length === 0"));
    }

    #[test]
    fn deletions_are_counted_in_code_points() {
        // The panel's Backspace deletes characters; UTF-16 units would send two
        // backspaces for one deleted emoji and eat its neighbour.
        assert!(PLAYER.contains("Array.from(lastValue.slice(common)).length"));
    }

    #[test]
    fn the_keyboard_tray_only_exists_live_and_asked_for() {
        // Exclusive by construction like the capture layer: a hidden tray over the idle
        // stage would shadow the play button.
        assert!(
            PLAYER.contains(r##"#stage[data-state="live"][data-kbd="on"] #kbd { display:flex; }"##)
        );
        let kbd_rule = PLAYER
            .split("#kbd   {")
            .nth(1)
            .and_then(|r| r.split('}').next())
            .expect("a #kbd rule");
        assert!(kbd_rule.contains("display:none"), "{kbd_rule}");
    }

    #[test]
    fn the_field_does_not_fight_the_phones_ime() {
        // Autocapitalize/autocorrect on a URL bar or a search box types things nobody
        // typed; the field is a conduit, so the phone's own mangling is turned off.
        assert!(PLAYER.contains(r#"autocapitalize="off""#));
        assert!(PLAYER.contains(r#"autocomplete="off""#));
    }

    #[test]
    fn enter_flushes_composition_before_it_submits() {
        // An Enter racing ahead of the text it submits would submit the previous value.
        let keydown = PLAYER
            .split("e.key === 'Enter'")
            .nth(1)
            .and_then(|r| r.split("preventDefault").next())
            .expect("an Enter branch");
        let sync = keydown.find("syncField()").expect("a flush");
        let sent = keydown.find("key: 'enter'").expect("a send");
        assert!(sync < sent, "the flush must come before the key");
    }

    #[test]
    fn there_is_a_way_home_that_is_not_a_gesture() {
        // The panel's home gesture is a left-edge swipe, which the phone's own OS eats.
        assert!(PLAYER.contains("type: 'home'"));
        assert!(PLAYER.contains(r#"getElementById('home')"#));
    }
}
