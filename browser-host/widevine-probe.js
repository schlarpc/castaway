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

app.whenReady().then(async () => {
  const out = { userDataDir: app.getPath('userData') };

  const t0 = Date.now();
  let readyErr = null;
  try {
    // The API the ECS docs point at. If this blocks on a network fetch, the elapsed
    // time on a cold profile says so.
    await components.whenReady();
  } catch (e) {
    readyErr = String(e);
  }
  out.whenReadyMs = Date.now() - t0;
  out.whenReadyError = readyErr;
  try {
    out.status = components.status();
  } catch (e) {
    out.status = `status() threw: ${e}`;
  }

  out.cdmPathsUnderUserData = findCdm(out.userDataDir);
  out.cdmPathsBesideBinary = findCdm(path.dirname(process.execPath));

  const win = new BrowserWindow({ show: false, webPreferences: { offscreen: true } });
  await win.loadURL('data:text/html,<title>wv</title>');
  // The question that actually matters: can a page get a Widevine key system?
  out.keySystemAccess = await win.webContents.executeJavaScript(`
    navigator.requestMediaKeySystemAccess('com.widevine.alpha', [{
      initDataTypes: ['cenc'],
      videoCapabilities: [{ contentType: 'video/mp4; codecs="avc1.42E01E"' }],
      audioCapabilities: [{ contentType: 'audio/mp4; codecs="mp4a.40.2"' }],
    }]).then(a => ({ ok: true, keySystem: a.keySystem }),
             e => ({ ok: false, error: String(e) }))
  `);

  process.stdout.write(JSON.stringify(out, null, 2) + '\n');
  app.exit(out.keySystemAccess && out.keySystemAccess.ok ? 0 : 1);
});
