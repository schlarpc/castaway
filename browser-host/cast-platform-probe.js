// Runs Google's real receiver SDK against our platform server, and prints what happened.
//
// This is the oracle for `proto-cast::platform`. That module is a reimplementation of a
// protocol whose only specification is `cast_receiver.js` — so unit tests against message
// shapes we *believe* are right can only ever restate the belief. This loads the actual
// bundle the way a receiver page loads it (a `<script src>` in a real document), lets its
// `cast.receiver.IpcChannel` dial our WebSocket, and reports whether the SDK's own
// `onReady` fired and whether a message we relayed arrived at the application layer.
//
//   electron browser-host/cast-platform-probe.js --port 8008 --sdk /path/to/sdk [--caf]
//
// Prints one JSON object on stdout and exits non-zero if the SDK never came up. Driven by
// `crates/proto-cast/tests/receiver_sdk.rs`, which starts the server it talks to.
'use strict';

const { app, BrowserWindow, dialog } = require('electron');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

dialog.showErrorBox = (title, content) => {
  process.stderr.write(`cast-platform-probe: ${title}: ${content}\n`);
};

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 && i + 1 < process.argv.length ? process.argv[i + 1] : fallback;
}

const PORT = Number(arg('port', '8008'));
const SDK_DIR = arg('sdk', '');
const USE_CAF = process.argv.includes('--caf');
const NAMESPACE = arg('namespace', 'urn:x-cast:com.google.cast.media');
const TIMEOUT_MS = Number(arg('timeout', '20000'));

const BUNDLE = USE_CAF ? 'cast_receiver_framework.js' : 'cast_receiver.js';

let done = false;
function report(payload) {
  if (done) return;
  done = true;
  process.stdout.write(`${JSON.stringify(payload)}\n`);
  app.exit(payload.ok ? 0 : 1);
}

// The application half, as a receiver page would write it. Kept as source text because it
// goes into the document rather than through `executeJavaScript`: the SDK installs itself
// as a classic script and expects to be running in a page, not in an injected eval.
function appScript() {
  if (USE_CAF) {
    return `
      const context = cast.framework.CastReceiverContext.getInstance();
      context.addEventListener(cast.framework.system.EventType.READY, () => {
        probe.events.push({ type: 'ready' });
      });
      context.addEventListener(cast.framework.system.EventType.SENDER_CONNECTED, (e) => {
        probe.events.push({ type: 'senderconnected', senderId: e.senderId });
      });
      context.addCustomMessageListener(${JSON.stringify(NAMESPACE)}, (e) => {
        probe.appMessages.push({ senderId: e.senderId, data: JSON.stringify(e.data) });
        context.sendCustomMessage(${JSON.stringify(NAMESPACE)}, e.senderId, { type: 'PROBE_ACK', saw: e.data });
      });
      context.start({ statusText: 'probe up' });
    `;
  }
  return `
    const manager = cast.receiver.CastReceiverManager.getInstance();
    manager.onReady = () => {
      const data = manager.getApplicationData() || {};
      probe.events.push({
        type: 'ready',
        applicationId: data.id,
        sessionId: data.sessionId,
        launchingSenderId: data.launchingSenderId,
      });
    };
    manager.onSenderConnected = (event) => {
      probe.events.push({ type: 'senderconnected', senderId: event.senderId || event.data });
    };
    manager.onSystemVolumeChanged = (event) => {
      probe.events.push({ type: 'volumechanged', level: event.data.level, muted: event.data.muted });
    };
    const bus = manager.getCastMessageBus(
      ${JSON.stringify(NAMESPACE)},
      cast.receiver.CastMessageBus.MessageType.JSON
    );
    bus.onMessage = (event) => {
      probe.appMessages.push({ senderId: event.senderId, data: JSON.stringify(event.data) });
      // Answer, so the platform can prove the return path as well as the forward one.
      bus.send(event.senderId, { type: 'PROBE_ACK', saw: event.data });
    };
    manager.start({ statusText: 'probe up' });
  `;
}

app.whenReady().then(async () => {
  const bundle = path.join(SDK_DIR, BUNDLE);
  if (!fs.existsSync(bundle)) {
    report({ ok: false, reason: `no SDK bundle at ${bundle}` });
    return;
  }

  // A real document in a real directory. The SDK is loaded by `<script src>` and the
  // `__platform__` shim is installed before it, exactly as the browser host does it — so
  // what this measures is the arrangement the receiver actually ships, not an eval of it.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cast-platform-probe-'));
  fs.copyFileSync(bundle, path.join(dir, BUNDLE));
  fs.writeFileSync(
    path.join(dir, 'index.html'),
    `<!doctype html><meta charset="utf-8"><title>cast-platform-probe</title>
     <script>
       // The shim the browser host installs, in miniature.
       //
       // It hangs off window.cast, NOT window. Both SDK generations capture the cast
       // namespace object first and read the shim out of that:
       //   v2:  var n = this||self; var q = n.cast || {};   ... q.__platform__
       //   CAF: _.u.cast = _.u.cast || {}; cast = _.u.cast; ... cast.__platform__
       // On window it is simply never found, and the SDK falls back to its own hardcoded
       // 8008 with no complaint - which is exactly the silent failure this probe has to
       // be able to tell apart from a protocol error.
       //
       // It must also exist BEFORE the bundle evaluates: v2 captures n.cast once, at
       // load. Both SDKs preserve an existing object rather than replacing it.
       window.cast = window.cast || {};
       window.cast.__platform__ = {
         queryPlatformValue: (key) => (key === 'port-for-web-server' ? '${PORT}' : undefined),
         canDisplayType: () => false,
       };
       window.probe = { events: [], appMessages: [], error: null };
       window.onerror = (message, source, line) => {
         window.probe.error = message + ' @' + source + ':' + line;
       };
     </script>
     <script src="./${BUNDLE}"></script>
     <script>
       try { ${appScript()} } catch (e) { window.probe.error = String(e && e.stack || e); }
     </script>`
  );

  const win = new BrowserWindow({ show: false, webPreferences: { offscreen: true } });
  win.webContents.on('console-message', (_e, _level, message) => {
    process.stderr.write(`page: ${message}\n`);
  });
  await win.loadFile(path.join(dir, 'index.html'));

  // Poll rather than wait on a single promise: what is being measured is whether the SDK
  // reached `ready` at all, and a promise that never resolves would report a timeout with
  // nothing in it. This way the partial state is the diagnosis.
  const deadline = Date.now() + TIMEOUT_MS;
  let snapshot = { events: [], appMessages: [], error: null };
  while (Date.now() < deadline) {
    snapshot = await win.webContents.executeJavaScript(
      'JSON.parse(JSON.stringify(window.probe))'
    );
    if (snapshot.error) break;
    if (snapshot.events.some((e) => e.type === 'ready') && snapshot.appMessages.length > 0) {
      break;
    }
    await new Promise((r) => setTimeout(r, 100));
  }

  const ready = snapshot.events.find((e) => e.type === 'ready');
  report({
    ok: Boolean(ready),
    sdk: USE_CAF ? 'caf-v3' : 'v2',
    reason: snapshot.error || undefined,
    ready: ready || null,
    events: snapshot.events,
    appMessages: snapshot.appMessages,
  });
});
