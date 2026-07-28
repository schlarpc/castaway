// castaway's browser, as a subprocess (D36).
//
// Dependency-free by policy: Electron APIs only, no npm tree. That is what keeps "a Node
// runtime joined a Rust appliance" to a runtime rather than an ecosystem, and it is a
// condition of the decision that let JavaScript into this repo at all.
//
// This process owns no policy. It renders, reports, and asks — blocking decisions, page
// choice and scriptlet content all come from castaway over stdio. Everything here is
// mechanism, so that the parts worth testing stay in Rust where they can be
// fixture-tested (`pipeline::browser_proto`).
//
// The protocol is documented in crates/pipeline/src/browser_proto.rs. Two rules matter
// on this side:
//
//   1. A painted frame is *lent*, not given. `texture.release()` happens when castaway
//      says `release`, never before — it is still sampling. Frames beyond MAX_INFLIGHT
//      are dropped rather than queued, because for live output latency beats freshness.
//   2. stdout is the control channel. Nothing else may write to it, or framing
//      desynchronizes. Diagnostics go to stderr, which castaway inherits.

'use strict';

const { app, BrowserWindow, components, dialog, session } = require('electron');

// ---------------------------------------------------------------------------
// Fail loudly on stderr, never in a dialog. A modal on a wall-mounted panel is a
// receiver that has stopped, with nobody there to dismiss the reason.
// ---------------------------------------------------------------------------
dialog.showErrorBox = (title, content) => log('error', `${title}: ${content}`);
process.on('uncaughtException', (err) => {
  log('error', `uncaught: ${err && err.stack}`);
  app.exit(1);
});
process.on('unhandledRejection', (err) => log('error', `unhandled rejection: ${err}`));

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------
const MAX_INFLIGHT = 3;

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + '\n');
}
function log(level, message) {
  send({ type: 'log', level, message });
}

/** Frames lent to castaway, id → texture. */
const inflight = new Map();
let paintSeq = 0;
let drops = 0;

/** Pending adblock questions, id → resolve. */
const pendingBlock = new Map();
let blockSeq = 0;

let win = null;
/** Pending scriptlet questions, id → resolve. */
const pendingScriptlets = new Map();
let scriptletSeq = 0;
/** The CDP script registration, so a new blob replaces rather than stacks. */
let scriptletHandle = null;

const USER_AGENT = process.env.CASTAWAY_USER_AGENT || undefined;

// ---------------------------------------------------------------------------
// Widevine. Blocking here would be a hang on a path castaway awaits, so it is raced
// against a deadline: a pre-staged CDM makes this resolve instantly, and a box with no
// route to Google gets a receiver that starts without DRM rather than one that never
// starts. See OPEN-QUESTIONS Q42.
// ---------------------------------------------------------------------------
async function readyComponents() {
  const deadline = Number(process.env.CASTAWAY_CDM_DEADLINE_MS || 20000);
  let timer;
  try {
    await Promise.race([
      components.whenReady(),
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`components timed out after ${deadline}ms`)), deadline);
      }),
    ]);
    log('info', `widevine: ${JSON.stringify(components.status())}`);
  } catch (e) {
    log('warn', `widevine unavailable (${e}); protected playback will fail, the rest is unaffected`);
  } finally {
    clearTimeout(timer);
  }
}

// ---------------------------------------------------------------------------
// Ad blocking. The engine is castaway's — this only asks.
//
// Electron's webRequest callback is asynchronous, which is the whole reason the engine
// can live in Rust: CEF's equivalent had to answer synchronously in-process. A question
// that goes unanswered would stall the page, so every one has a deadline and defaults to
// *allowing* — a receiver that loads ads is worse than one that loads nothing, but only
// slightly, and a blank page is the failure nobody can diagnose from the room.
// ---------------------------------------------------------------------------
function installAdblock(ses) {
  ses.webRequest.onBeforeRequest({ urls: ['<all_urls>'] }, (details, callback) => {
    // Never gate the top-level document: a false positive there is a black panel.
    if (details.resourceType === 'mainFrame') {
      callback({ cancel: false });
      return;
    }
    const id = ++blockSeq;
    let settled = false;
    const finish = (block) => {
      if (settled) return;
      settled = true;
      pendingBlock.delete(id);
      callback({ cancel: block });
    };
    pendingBlock.set(id, finish);
    setTimeout(() => {
      if (!settled) log('warn', `adblock verdict ${id} timed out; allowing`);
      finish(false);
    }, 1000);
    send({
      type: 'adblock-query',
      id,
      url: details.url,
      source: (win && !win.isDestroyed() && win.webContents.getURL()) || '',
      kind: details.resourceType || 'other',
    });
  });
}

// ---------------------------------------------------------------------------
// Scriptlets. uBlock Origin's `##+js(...)` bodies must run in the page's **main world**
// and before any page script, because they patch globals the page then uses. A preload
// script cannot do it: under contextIsolation it runs in an isolated world, where the
// patches are invisible to the page, and turning contextIsolation off would trade the
// renderer sandbox for it. CDP's addScriptToEvaluateOnNewDocument is main-world and
// document-start, which is exactly the pair required.
// ---------------------------------------------------------------------------
async function applyScriptlets(contents, source) {
  if (!source) return;
  try {
    if (!contents.debugger.isAttached()) contents.debugger.attach('1.3');
  } catch (e) {
    log('warn', `could not attach the debugger; scriptlets will not inject: ${e}`);
    return;
  }
  try {
    if (scriptletHandle) {
      await contents.debugger.sendCommand('Page.removeScriptToEvaluateOnNewDocument', {
        identifier: scriptletHandle,
      });
    }
    const res = await contents.debugger.sendCommand('Page.addScriptToEvaluateOnNewDocument', {
      source,
      runImmediately: true,
    });
    scriptletHandle = res.identifier;
    log('info', `scriptlets armed (${source.length} bytes)`);
  } catch (e) {
    log('warn', `scriptlet injection failed: ${e}`);
  }
}

function requestScriptlets(url) {
  const id = ++scriptletSeq;
  return new Promise((resolve) => {
    let settled = false;
    const finish = (source) => {
      if (settled) return;
      settled = true;
      pendingScriptlets.delete(id);
      resolve(source || '');
    };
    pendingScriptlets.set(id, finish);
    // A late answer must not hold the navigation: an unblocked page is a much better
    // outcome than a panel that never loads one.
    setTimeout(() => finish(''), 2000);
    send({ type: 'scriptlet-query', id, url });
  });
}

// ---------------------------------------------------------------------------
// Input. `webContents.sendInputEvent` has no touch type at all, so touch goes through
// CDP too. Chromium's own gesture recognizer is on the far side of both, so scroll,
// fling and tap behave as they did embedded.
// ---------------------------------------------------------------------------
const touchPoints = new Map();

async function dispatchTouch(msg) {
  if (!win || win.isDestroyed()) return;
  const contents = win.webContents;
  if (!contents.debugger.isAttached()) return;

  const type =
    msg.phase === 'start' ? 'touchStart'
    : msg.phase === 'move' ? 'touchMove'
    : msg.phase === 'cancel' ? 'touchCancel'
    : 'touchEnd';

  if (msg.phase === 'end' || msg.phase === 'cancel') touchPoints.delete(msg.id);
  else touchPoints.set(msg.id, { x: msg.x, y: msg.y, id: msg.id });

  // touchEnd/Cancel carry the points that *remain*; every other type carries all of
  // them. Sending the lifted point in touchEnd makes Chromium think it is still down.
  const points = [...touchPoints.values()].map((p) => ({ x: p.x, y: p.y, id: p.id }));
  try {
    await contents.debugger.sendCommand('Input.dispatchTouchEvent', {
      type,
      touchPoints: type === 'touchEnd' || type === 'touchCancel' ? points : points,
    });
  } catch (e) {
    log('warn', `touch dispatch failed: ${e}`);
  }
}

function dispatchPointer(msg) {
  if (!win || win.isDestroyed()) return;
  const contents = win.webContents;
  const type = msg.kind === 'down' ? 'mouseDown' : msg.kind === 'up' ? 'mouseUp' : 'mouseMove';
  contents.sendInputEvent({ type, x: Math.round(msg.x), y: Math.round(msg.y), button: 'left', clickCount: 1 });
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------
function createWindow(width, height) {
  const w = new BrowserWindow({
    show: false,
    webPreferences: {
      offscreen: { useSharedTexture: true },
      // The renderer executes other people's pages, so it keeps every guard Chromium
      // offers. This is the posture CEF could not have on Windows at all (G86).
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
      backgroundThrottling: false,
    },
  });
  // Constructor sizes are clamped to the display work area even offscreen — asking for
  // 4K silently yields the panel's size instead. setContentSize is not clamped.
  w.setContentSize(width, height);
  if (USER_AGENT) w.webContents.setUserAgent(USER_AGENT);
  w.webContents.setFrameRate(60);
  // Audio plays through the system device, mixed by the OS with castaway's own output.
  // Electron exposes no PCM tap the way CEF's audio handler did, so this is the honest
  // arrangement rather than a chosen one; see GAPS for what it costs.
  w.webContents.setAudioMuted(false);

  w.webContents.on('paint', (event) => {
    if (!event.texture) {
      send({ type: 'no-texture', detail: 'paint arrived without a shared texture' });
      return;
    }
    const info = event.texture.textureInfo;
    const pixmap = info.handle && info.handle.nativePixmap;
    const shared = info.handle && info.handle.sharedTextureHandle;
    if (!pixmap && shared === undefined) {
      send({ type: 'no-texture', detail: `handle carries neither nativePixmap nor sharedTextureHandle: ${JSON.stringify(info.handle)}` });
      event.texture.release();
      return;
    }
    if (inflight.size >= MAX_INFLIGHT) {
      drops += 1;
      send({ type: 'dropped', total: drops });
      event.texture.release();
      return;
    }
    const id = ++paintSeq;
    inflight.set(id, event.texture);
    send({
      type: 'paint',
      id,
      format: info.pixelFormat === 'rgba' ? 'rgba' : 'bgra',
      width: info.codedSize.width,
      height: info.codedSize.height,
      // Linux: per-plane fds plus a DRM modifier. Windows: one NT handle and no
      // modifier, because the handle describes its own layout.
      modifier: pixmap ? String(pixmap.modifier) : null,
      planes: pixmap
        ? pixmap.planes.map((p) => ({ fd: p.fd, stride: p.stride, offset: p.offset }))
        : [{ fd: Number(shared), stride: 0, offset: 0 }],
    });
  });

  w.webContents.on('did-finish-load', () => {
    send({ type: 'load-end', url: w.webContents.getURL(), status: null });
  });
  w.webContents.on('did-fail-load', (_e, code, desc, url) => {
    // -3 is ERR_ABORTED, which a navigation that superseded another produces routinely.
    if (code === -3) return;
    send({ type: 'load-error', url: url || '', error: `${desc} (${code})` });
  });
  w.webContents.on('render-process-gone', (_e, details) => {
    send({ type: 'render-gone', reason: (details && details.reason) || 'unknown' });
  });

  return w;
}

// ---------------------------------------------------------------------------
// Commands from castaway
// ---------------------------------------------------------------------------
function handle(msg) {
  switch (msg.type) {
    case 'release': {
      const texture = inflight.get(msg.id);
      if (texture) {
        inflight.delete(msg.id);
        texture.release();
      }
      return;
    }
    case 'navigate': {
      if (!win || win.isDestroyed()) win = createWindow(msg.width, msg.height);
      else win.setContentSize(msg.width, msg.height);
      // Ask for this page's scriptlets and arm them *before* navigating: uBO rules are
      // domain-scoped, and arming after the load has already begun races the very page
      // scripts the patches exist to get in front of.
      requestScriptlets(msg.url)
        .then((source) => applyScriptlets(win.webContents, source))
        .catch((e) => log('warn', `scriptlets for ${msg.url}: ${e}`))
        .finally(() =>
          win.loadURL(msg.url).catch((e) =>
            send({ type: 'load-error', url: msg.url, error: String(e) })
          )
        );
      return;
    }
    case 'blank': {
      if (win && !win.isDestroyed()) win.loadURL('about:blank').catch(() => {});
      return;
    }
    case 'resize': {
      if (win && !win.isDestroyed()) win.setContentSize(msg.width, msg.height);
      return;
    }
    case 'touch':
      dispatchTouch(msg);
      return;
    case 'pointer':
      dispatchPointer(msg);
      return;
    case 'wheel': {
      if (!win || win.isDestroyed()) return;
      // Position *and* delta: Chromium scrolls what is under the cursor, so a delta with
      // no position scrolls whichever region happens to be focused.
      win.webContents.sendInputEvent({
        type: 'mouseWheel',
        x: Math.round(msg.x),
        y: Math.round(msg.y),
        deltaX: msg.dx,
        deltaY: msg.dy,
      });
      return;
    }
    case 'adblock-verdict': {
      const finish = pendingBlock.get(msg.id);
      if (finish) finish(Boolean(msg.block));
      return;
    }
    case 'scriptlet-source': {
      const finish = pendingScriptlets.get(msg.id);
      if (finish) finish(msg.source);
      return;
    }
    case 'quit':
      shutdown();
      return;
    default:
      log('warn', `unknown command ${msg.type}`);
  }
}

function shutdown() {
  // Return every lent frame before going: Chromium asserts on a texture destroyed while
  // still lent out, and an assert here is a crash rather than a clean exit.
  for (const [, texture] of inflight) {
    try {
      texture.release();
    } catch {
      /* already gone */
    }
  }
  inflight.clear();
  log('info', `shutting down; ${drops} frames dropped this session`);
  app.quit();
}

let buffered = '';
process.stdin.on('data', (chunk) => {
  buffered += chunk;
  let newline;
  while ((newline = buffered.indexOf('\n')) >= 0) {
    const line = buffered.slice(0, newline).trim();
    buffered = buffered.slice(newline + 1);
    if (!line) continue;
    try {
      handle(JSON.parse(line));
    } catch (e) {
      log('warn', `unparseable command: ${e}`);
    }
  }
});
// castaway vanishing is a quit, not an error loop — this is the backstop that stops an
// orphaned browser holding the GPU and the profile lock.
process.stdin.on('end', shutdown);

app.whenReady().then(async () => {
  await readyComponents();
  installAdblock(session.defaultSession);
  send({ type: 'ready', pid: process.pid });
});

app.on('window-all-closed', () => {
  /* the panel decides when we exit, not the window count */
});
