// Measures how castLabs ECS actually obtains the Widevine CDM, and whether a machine
// with no internet can play protected video.
//
// This is G46's property under a new mechanism. The CEF path staged `WidevineCdm/` beside
// libcef because `DIR_COMPONENT_PREINSTALLED` was the only directory Chromium scanned; ECS
// instead exposes a `components` API. If that API fetches at runtime, then a panel that
// has never been online has no DRM — which is exactly the regression G46 was written to
// prevent, and it would be silent.
//
//   electron browser-host/widevine-probe.js
//
// Reports where the CDM came from and whether requestMediaKeySystemAccess succeeds. Run
// it twice: once with a fresh `--user-data-dir` and network, once with the same dir and
// no network (`unshare -n`). Second run passing is what "pre-stageable" means.
'use strict';

const { app, BrowserWindow, components, dialog } = require('electron');
const fs = require('fs');
const http = require('http');
const path = require('path');

dialog.showErrorBox = (t, c) => process.stderr.write(`widevine-probe: ${t}: ${c}\n`);

function findCdm(dir) {
  const hits = [];
  const walk = (d, depth) => {
    if (depth > 4) return;
    let entries;
    try {
      entries = fs.readdirSync(d, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) {
        if (/widevine/i.test(e.name)) hits.push(p);
        walk(p, depth + 1);
      } else if (/libwidevinecdm|widevinecdm\.dll/i.test(e.name)) {
        hits.push(p);
      }
    }
  };
  walk(dir, 0);
  return hits;
}

// Kept because it is what located the hang that this probe's own bug caused: without
// it, 'no output for 120s' names nothing. Opt-in so a normal run stays quiet.
const step = (m) => {
  if (process.env.WV_TRACE) process.stderr.write(`widevine-probe: step ${m}\n`);
};

app.whenReady().then(async () => {
  step('app-ready');
  const out = { userDataDir: app.getPath('userData') };

  const t0 = Date.now();
  let readyErr = null;
  try {
    // `components.whenReady()` blocks on a *network fetch* when the profile has no CDM —
    // measured, not assumed. On a panel with no route to Google that is an unbounded wait
    // on a path the receiver awaits, so it gets a deadline here and must get one in the
    // host app too. Losing the race is not fatal: a pre-staged CDM is already on disk,
    // and DRM-less playback is much better than a receiver that never finishes starting.
    const DEADLINE_MS = Number(process.env.WV_DEADLINE_MS || 20000);
    let timer;
    const deadline = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`whenReady exceeded ${DEADLINE_MS}ms`)), DEADLINE_MS);
    });
    try {
      await Promise.race([components.whenReady(), deadline]);
    } finally {
      clearTimeout(timer);
    }
  } catch (e) {
    readyErr = String(e);
  }
  step('components-done');
  out.whenReadyMs = Date.now() - t0;
  out.whenReadyError = readyErr;
  try {
    out.status = components.status();
  } catch (e) {
    out.status = `status() threw: ${e}`;
  }

  step('scanning-cdm');
  out.cdmPathsUnderUserData = findCdm(out.userDataDir);
  out.cdmPathsBesideBinary = findCdm(path.dirname(process.execPath));

  // EME is gated on a **secure context**, and a `data:` URL is not one — asking there
  // throws rather than reporting "no CDM", which reads exactly like a staging failure.
  // A loopback origin is trustworthy by definition, so the probe serves itself one.
  step('serving-loopback');
  const server = http.createServer((_req, res) => {
    res.writeHead(200, { 'Content-Type': 'text/html' });
    res.end('<title>wv</title>');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const origin = `http://127.0.0.1:${server.address().port}/`;

  step('creating-window');
  const win = new BrowserWindow({ show: false, webPreferences: { offscreen: true } });
  await win.loadURL(origin);
  // The question that actually matters: can a page get a Widevine key system?
  step('requesting-key-system');
  out.keySystemAccess = await win.webContents.executeJavaScript(`
    navigator.requestMediaKeySystemAccess('com.widevine.alpha', [{
      initDataTypes: ['cenc'],
      videoCapabilities: [{ contentType: 'video/mp4; codecs="avc1.42E01E"' }],
      audioCapabilities: [{ contentType: 'audio/mp4; codecs="mp4a.40.2"' }],
    }]).then(a => ({ ok: true, keySystem: a.keySystem }),
             e => ({ ok: false, error: String(e) }))
  `);

  out.origin = origin;
  server.close();
  process.stdout.write(JSON.stringify(out, null, 2) + '\n');
  app.exit(out.keySystemAccess && out.keySystemAccess.ok ? 0 : 1);
});
