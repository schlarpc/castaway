// Loads a receiver page in the real browser runtime, with the Cast platform shim armed,
// and reports what the page's own `window.probe` says.
//
// The companion to `cast-platform-probe.js`, and the division is deliberate: that one
// supplies the application half itself, to measure the *protocol*. This one supplies
// nothing but the shim and loads somebody else's page — which is what the receiver
// actually does — so the application half under test is real page code fetched over HTTP.
//
//   electron browser-host/cast-page-probe.js --port 8008 --url http://.../receiver.html
//
// Prints one JSON object on stdout. Driven by
// `crates/proto-cast/tests/hosted_app_media.rs`.
'use strict';

const { app, BrowserWindow, dialog } = require('electron');

dialog.showErrorBox = (title, content) => {
  process.stderr.write(`cast-page-probe: ${title}: ${content}\n`);
};

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : fallback;
}

const PORT = Number(arg('port', '8008'));
const URL_ = arg('url', '');
const TIMEOUT_MS = Number(arg('timeout', '40000'));

let done = false;
function report(payload) {
  if (done) return;
  done = true;
  process.stdout.write(`${JSON.stringify(payload)}\n`);
  app.exit(payload.ok ? 0 : 1);
}

// A watchdog, because a probe that hangs is worse than one that fails: the caller sees a
// timeout with nothing in it, where a failure carries a reason.
setTimeout(() => report({ ok: false, reason: `watchdog fired after ${TIMEOUT_MS + 15000}ms` }), TIMEOUT_MS + 15000);

app.whenReady().then(async () => {
  if (!URL_) {
    report({ ok: false, reason: 'no --url' });
    return;
  }

  const win = new BrowserWindow({ show: false, webPreferences: { offscreen: true } });
  win.webContents.on('console-message', (_e, _level, message) => {
    process.stderr.write(`page: ${message}\n`);
  });

  // A first navigation before the debugger, because a CDP command sent to a webContents
  // with no target never answers — the same trap `browser-host/main.js` wraps every one
  // of its commands in a deadline for. Without this the probe hangs with no error.
  await win.loadURL('about:blank');

  // The shim, at document-start and in the page's main world — the same two properties
  // the receiver's own injection has, reached here through the simplest API that has
  // them. It hangs off `cast`, not `window`: both SDK generations capture the `cast`
  // namespace object and read the shim out of it, and on `window` it is never found.
  win.webContents.debugger.attach('1.3');
  await win.webContents.debugger.sendCommand('Page.enable');
  await win.webContents.debugger.sendCommand('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      window.cast = window.cast || {};
      window.cast.__platform__ = {
        queryPlatformValue: (key) => (key === 'port-for-web-server' ? '${PORT}' : undefined),
        canDisplayType: (mime) => {
          try {
            return Boolean(window.MediaSource && window.MediaSource.isTypeSupported(mime));
          } catch (e) {
            return false;
          }
        },
      };
    `,
    runImmediately: true,
  });

  await win.loadURL(URL_);

  const deadline = Date.now() + TIMEOUT_MS;
  let snapshot = null;
  while (Date.now() < deadline) {
    snapshot = await win.webContents
      .executeJavaScript('window.probe ? JSON.parse(JSON.stringify(window.probe)) : null')
      .catch(() => null);
    if (snapshot && snapshot.error) break;
    // Done when the picture has actually moved, or failed to. A <video> that errored and
    // one that is playing look identical until the clock advances.
    if (
      snapshot &&
      snapshot.media &&
      (snapshot.media.currentTime > 0 || snapshot.media.state === 'error')
    ) {
      break;
    }
    await new Promise((r) => setTimeout(r, 100));
  }

  const ready = snapshot && snapshot.events && snapshot.events.some((e) => e.type === 'ready');
  report({
    ok: Boolean(ready),
    reason: (snapshot && snapshot.error) || undefined,
    events: (snapshot && snapshot.events) || [],
    appMessages: (snapshot && snapshot.appMessages) || [],
    media: (snapshot && snapshot.media) || null,
  });
}).catch((e) => report({ ok: false, reason: `probe threw: ${e && e.stack ? e.stack : e}` }));
